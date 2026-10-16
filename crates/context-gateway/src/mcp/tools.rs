//! The catalogue of the hub and the named servers (ADR-N-025 2.2, ADR-N-043): each tool's name,
//! the operations that make it worth listing, its description and its published argument
//! schema (T-3648).
//!
//! Discovery stays the gateway's: a tool is listed when the PDP grants one of its operations on
//! an Endpoint of the caller's list, the same PDP that decides every call, so discovery and
//! enforcement cannot drift (SP-15, SP-16). The call itself is the member view's at the broker
//! (T-3808), which checks the arguments again under its own schema; the two describe tools are
//! answered by [`crate::mcp::describe`] and their arguments are checked here first (AG-31).
//! Every schema says `additionalProperties: false`, so an unknown field is an error rather than
//! a parameter that is quietly dropped (AG-21).

use crate::app::Gateway;
use crate::mcp::jsonrpc::refused;
use crate::pdp::evaluator::{Request as PolicyRequest, Subject, Verdict};
use crate::resolver::Endpoint;
use jc_core::kinds::Operation;
use serde_json::{json, Map, Value};
use std::sync::{Arc, LazyLock};

/// An NGSI-LD entity type name, as the models of this platform spell it: a term, never a
/// path, a URL or an escape sequence (AG-21).
///
/// A value outside this is a bad request and is refused by name. Answering it with an empty
/// result would tell the caller the argument was accepted, which is how a traversal probe
/// reads `[]`: as "the parameter went through and there was nothing there".
const TYPE_NAME: &str = "^[A-Za-z][A-Za-z0-9_-]*$";

/// An NGSI-LD entity id, which on this platform is always the URN of ADR 001.
const ENTITY_URN: &str = "^urn:ngsi-ld:[^\\s]+$";

/// An NGSI-LD attribute name: a term of the model, or the IRI a term expands to.
///
/// Wider than [`TYPE_NAME`], because an attribute may be sent expanded, and narrow enough that
/// no separator of a query string — `&`, `=`, `;`, `%`, a newline — can ride inside one and
/// arrive at the broker as a second parameter (AG-21, T-2299).
const ATTR_NAME: &str = "^[A-Za-z][A-Za-z0-9_\\-.:/#]*$";

/// One tool of the catalogue (Architecture/07 section 2).
pub(crate) struct Tool {
    pub(crate) name: &'static str,
    /// The operations that make this tool worth advertising: the caller holding any one of
    /// them sees it. Empty is a tool that describes the endpoint rather than its data, and
    /// that every caller the endpoint admits may call (EP-55).
    pub(crate) operations: &'static [Operation],
    pub(crate) description: &'static str,
    pub(crate) shape: Shape,
    /// What the broker's answer is called in the structured half of the result. MCP's
    /// `structuredContent` is an object matching the tool's output schema, so a bare list
    /// is read as nothing by every client that reads it by name (T-0946, AG-13).
    pub(crate) result_key: &'static str,
}

/// How a tool's arguments are published: a broker tool's from its argument table, the
/// gateway's own describe tools from their schema.
pub(crate) enum Shape {
    Broker(&'static [Arg]),
    Own(fn() -> Value),
}

impl Tool {
    /// Whether the tool only reads, from the operations it stands for rather than from a
    /// boolean somebody set beside them (T-1066, AG-07).
    ///
    /// This is what an agent runtime reads before it decides whether a person has to confirm
    /// the call, so it has to state what the tool does. A tool that writes at all is not
    /// read-only: *any* write is a write, and a rule that asked whether *any* operation reads
    /// would announce a mixed tool as a read.
    fn read_only(&self) -> bool {
        !self.operations.iter().any(Operation::is_write)
    }

    /// Whether a call can remove or overwrite what is there, as the broker annotates it: the
    /// three writes that only add are not (Antares `docs/src/mcp.md` section 2).
    fn destructive(&self) -> bool {
        !self.read_only()
            && !matches!(
                self.name,
                "create_entity" | "create_subscription" | "batch_create"
            )
    }

    /// The four annotations a directory reviewer and an agent runtime read (MCP 2025-06-18
    /// `ToolAnnotations`, T-4315).
    pub(crate) fn annotations(&self) -> Value {
        annotations(self.name, self.read_only(), self.destructive())
    }

    /// The tool's published JSON Schema.
    pub(crate) fn input_schema(&self) -> Value {
        match self.shape {
            Shape::Broker(args) => Value::Object(input_schema(args)),
            Shape::Own(schema) => schema(),
        }
    }
}

/// The tools a caller without a token may list and call, on every MCP surface: the hub, a named
/// server, an Endpoint's and a Context Space's (T-4168). An allowlist rather than
/// [`Tool::read_only`]: a tool the catalogue or the broker gains later is a write to an anonymous
/// caller until it is named here, so an unknown name is refused, never taken for a read.
pub(crate) const ANONYMOUS_TOOLS: &[&str] = &[
    "list_endpoints",
    "list_types",
    "get_type",
    "list_type_details",
    "list_attributes",
    "list_attribute_details",
    "get_attribute",
    "query_entities",
    "get_entity",
    "get_attribute_value",
    "query_temporal",
    "retrieve_temporal",
    "describe_schema",
    "describe_access",
];

/// Whether a caller without a token may list and call `name` (T-4168).
pub(crate) fn anonymous_may(name: &str) -> bool {
    ANONYMOUS_TOOLS.contains(&name)
}

/// The JSON-RPC error a caller without a token gets for any other tool: what it may call instead,
/// and that a token opens the rest under the Endpoint's own Policy.
pub(crate) fn anonymous_refusal(id: Value, name: &str) -> Value {
    crate::mcp::jsonrpc::error(
        id,
        -32602,
        &format!(
            "`{name}` is not offered without a token; an anonymous caller may use {} only. \
             Sign in for the tools the Endpoint's Policy grants you",
            ANONYMOUS_TOOLS.join(", ")
        ),
    )
}

/// The widest history window an anonymous temporal read may ask for (T-4261).
const ANONYMOUS_WINDOW_DAYS: i64 = 31;
/// The `lastN` an anonymous temporal read gets when it names none, and the most it may name.
const ANONYMOUS_LAST_N: u64 = 100;
/// The page cap every `limit` argument already publishes.
const PAGE_CAP: u64 = 1000;
/// The shortest aggregation period an anonymous read may ask for, in seconds (`PT1H`).
const ANONYMOUS_AGGR_SECONDS: f64 = 3600.0;

/// Bounds what one anonymous temporal read makes the broker read (T-4261, Architecture/07 §2):
/// a window of at most 31 days, `type` or `id`, `lastN` at most 100 and 100 when absent,
/// `limit` within the page cap, and aggregation periods of an hour or more. Any other tool
/// passes untouched; the error names the bound it broke.
pub(crate) fn anonymous_bounds(
    name: &str,
    arguments: &mut Map<String, Value>,
) -> Result<(), String> {
    if !matches!(name, "query_temporal" | "retrieve_temporal") {
        return Ok(());
    }
    let named = |key: &str| {
        arguments.get(key).is_some_and(|value| match value {
            Value::Array(items) => !items.is_empty(),
            Value::String(text) => !text.is_empty(),
            Value::Null => false,
            _ => true,
        })
    };
    if !named("type") && !named("id") {
        return Err(format!(
            "`{name}` without a token names `type` or `id`: a history read over every Entity is not offered anonymously"
        ));
    }
    let instant = |key: &str| {
        arguments
            .get(key)
            .and_then(Value::as_str)
            .and_then(|text| chrono::DateTime::parse_from_rfc3339(text).ok())
    };
    let window = match (
        arguments.get("timerel").and_then(Value::as_str),
        instant("timeAt"),
        instant("endTimeAt"),
    ) {
        (Some("between"), Some(start), Some(end)) if end > start => end - start,
        _ => {
            return Err(format!(
                "`{name}` without a token reads a window: timerel between, with timeAt before endTimeAt, \
                 both ISO 8601 instants, at most {ANONYMOUS_WINDOW_DAYS} days apart"
            ))
        }
    };
    if window > chrono::Duration::days(ANONYMOUS_WINDOW_DAYS) {
        return Err(format!(
            "`{name}` without a token reads at most {ANONYMOUS_WINDOW_DAYS} days between timeAt and endTimeAt"
        ));
    }
    match arguments.get("lastN") {
        None => {
            arguments.insert("lastN".to_owned(), Value::from(ANONYMOUS_LAST_N));
        }
        Some(n)
            if n.as_u64()
                .is_some_and(|n| (1..=ANONYMOUS_LAST_N).contains(&n)) => {}
        Some(_) => {
            return Err(format!(
                "`lastN` without a token is 1 to {ANONYMOUS_LAST_N}"
            ))
        }
    }
    if let Some(limit) = arguments.get("limit") {
        if !limit.as_u64().is_some_and(|n| (1..=PAGE_CAP).contains(&n)) {
            return Err(format!("`limit` is 1 to {PAGE_CAP}"));
        }
    }
    if arguments.contains_key("aggrMethods") {
        let period = arguments
            .get("aggrPeriodDuration")
            .and_then(Value::as_str)
            .and_then(duration_seconds);
        if !period.is_some_and(|seconds| seconds >= ANONYMOUS_AGGR_SECONDS) {
            return Err(
                "`aggrMethods` without a token needs an aggrPeriodDuration of at least PT1H"
                    .to_owned(),
            );
        }
    }
    Ok(())
}

/// The seconds of an ISO 8601 duration (`P1DT2H`, `PT1.5H`, `P2W`), a year taken as 365 days
/// and a month as 30; `None` when it is not one.
fn duration_seconds(text: &str) -> Option<f64> {
    let rest = text.strip_prefix('P')?;
    let (date, time) = match rest.split_once('T') {
        Some((date, time)) if !time.is_empty() => (date, Some(time)),
        Some(_) => return None,
        None => (rest, None),
    };
    let day = 86_400.0;
    let mut total = 0.0;
    let mut any = false;
    for (part, units) in [
        (
            date,
            &[
                ('Y', 365.0 * day),
                ('M', 30.0 * day),
                ('W', 7.0 * day),
                ('D', day),
            ][..],
        ),
        (
            time.unwrap_or(""),
            &[('H', 3600.0), ('M', 60.0), ('S', 1.0)][..],
        ),
    ] {
        let mut number = String::new();
        let mut next = 0;
        for c in part.chars() {
            if c.is_ascii_digit() || c == '.' {
                number.push(c);
                continue;
            }
            // Each unit once, in its order.
            let at = units[next..].iter().position(|(unit, _)| *unit == c)? + next;
            total += number.parse::<f64>().ok()? * units[at].1;
            number.clear();
            next = at + 1;
            any = true;
        }
        if !number.is_empty() {
            return None;
        }
    }
    any.then_some(total)
}

/// Keeps in a `tools/list` answer only what [`anonymous_may`] admits; an entry without a name
/// is dropped too.
pub(crate) fn keep_anonymous(tools: &mut Vec<Value>) {
    tools.retain(|tool| tool["name"].as_str().is_some_and(anonymous_may));
}

/// The words `describe_access` and the `access://` resource answer in (EP-56, EP-57, EP-58).
pub(crate) const ACCESS_FORMATS: [&str; 3] = ["permissions", "odrl", "grant-ast"];

/// One argument of a broker tool, as Antares `crates/antares-mcp/src/schema.rs` declares it.
pub(crate) struct Arg {
    name: &'static str,
    kind: Kind,
    required: bool,
}

const fn req(name: &'static str, kind: Kind) -> Arg {
    Arg {
        name,
        kind,
        required: true,
    }
}

const fn opt(name: &'static str, kind: Kind) -> Arg {
    Arg {
        name,
        kind,
        required: false,
    }
}

/// The broker's argument kinds (Antares `schema.rs` `Kind`). The hub spans many views, so a type
/// or an attribute is published by its pattern, as the broker publishes one of a view with more
/// names than an enum holds.
#[derive(Clone, Copy)]
enum Kind {
    Id,
    Ids,
    Type,
    Types,
    Attr,
    Attrs,
    Text(usize),
    Word(&'static [&'static str]),
    Int(u64, u64),
    Flag,
    Coordinates,
    Entity,
    Entities,
    Fragment,
    Attribute,
    Object(&'static [Arg]),
    ListOf(&'static [Arg], usize),
    Endpoint,
}

/// The broker's bounds on a list (Antares `schema.rs`).
const MAX_ITEMS: usize = 100;
const MAX_TYPES: usize = 50;
/// The broker's longest cursor (Antares `cursor.rs`).
const CURSOR_LEN: usize = 2048;

fn schema_of(kind: Kind) -> Value {
    let name = |pattern: &str| json!({"type": "string", "pattern": pattern, "maxLength": 256});
    let members = |core: &[&str]| -> Value {
        let mut properties = Map::new();
        for member in core {
            let kind = if *member == "id" {
                Kind::Id
            } else {
                Kind::Type
            };
            properties.insert((*member).to_owned(), schema_of(kind));
        }
        Value::Object(properties)
    };
    match kind {
        Kind::Id => json!({"type": "string", "pattern": ENTITY_URN, "maxLength": 512}),
        Kind::Ids => json!({"type": "array", "maxItems": MAX_ITEMS, "items": schema_of(Kind::Id)}),
        Kind::Type => name(TYPE_NAME),
        Kind::Types => {
            json!({"type": "array", "minItems": 1, "maxItems": MAX_TYPES, "items": schema_of(Kind::Type)})
        }
        Kind::Attr => name(ATTR_NAME),
        Kind::Attrs => {
            json!({"type": "array", "minItems": 1, "maxItems": MAX_ITEMS, "items": schema_of(Kind::Attr)})
        }
        Kind::Text(max) => json!({"type": "string", "maxLength": max}),
        Kind::Word(words) => json!({"type": "string", "enum": words}),
        Kind::Int(min, max) => json!({"type": "integer", "minimum": min, "maximum": max}),
        Kind::Flag => json!({"type": "boolean"}),
        Kind::Coordinates => json!({"type": "array", "maxItems": 1000}),
        Kind::Entity => json!({
            "type": "object",
            "required": ["id", "type"],
            "properties": members(&["id", "type"]),
            "propertyNames": {"pattern": ATTR_NAME, "maxLength": 256},
        }),
        Kind::Entities => {
            json!({"type": "array", "minItems": 1, "maxItems": MAX_ITEMS, "items": schema_of(Kind::Entity)})
        }
        Kind::Fragment => json!({
            "type": "object",
            "minProperties": 1,
            "properties": members(&["type"]),
            "propertyNames": {"pattern": ATTR_NAME, "maxLength": 256},
        }),
        Kind::Attribute => json!({"type": "object"}),
        Kind::Object(args) => Value::Object(input_schema(args)),
        Kind::ListOf(args, max) => {
            json!({"type": "array", "maxItems": max, "items": input_schema(args)})
        }
        Kind::Endpoint => json!({"type": "string", "maxLength": 2048}),
    }
}

/// One line of NGSI-LD grammar per argument, so a model reads how to fill it where it fills it
/// (EP-116). A description changes no name and no type, which is all EP-112 compares.
fn grammar(name: &str) -> Option<&'static str> {
    Some(match name {
        "type" => "Entity types, e.g. [\"AirQualityObserved\"]",
        "id" => "Entity ids, NGSI-LD URNs urn:ngsi-ld:{Type}:{id}",
        "idPattern" => "A regular expression over entity ids, e.g. ^urn:ngsi-ld:Parking:.*",
        "attrs" => "Attributes to return; with no q, also those an entity must have",
        "pick" | "omit" => "Members to keep or drop, id and type included",
        "q" => "NGSI-LD query: ; is AND, | is OR; == != > >= < <= ~= (regex) !~=; a..b ranges; a.b sub-attribute; a[b] member; !a absent. E.g. no2>40;status==\"ok\"",
        "scopeQ" => "Scope query, e.g. /city/centre or /city/# ; is AND, , is OR",
        "georel" => "near;maxDistance==<m>, near;minDistance==<m>, within, contains, intersects, equals, disjoint, overlaps",
        "coordinates" => "GeoJSON coordinates of `geometry`, longitude first, e.g. [19.15, 48.74]",
        "geoproperty" => "The GeoProperty the geo query tests; location when absent",
        "orderBy" => "attr[;asc|desc], comma-separated (CIM 009 4.23); local queries only",
        "format" => "Representation: normalized (default, values with metadata), concise, keyValues (plain values); on history temporalValues or aggregatedValues",
        "sysAttrs" => "true adds createdAt and modifiedAt: when the entity or attribute last changed",
        "timerel" => "before, after or between timeAt (and endTimeAt)",
        "timeAt" | "endTimeAt" => "An ISO 8601 instant, e.g. 2026-10-12T00:00:00Z",
        "timeproperty" => "The instant history is ordered by: observedAt (default), modifiedAt, createdAt, or deletedAt for removals",
        "lastN" => "Keep the newest N instances of each attribute",
        "aggrMethods" => "Comma-separated: avg, min, max, sum, totalCount, distinctCount, stddev, sumsq",
        "aggrPeriodDuration" => "ISO 8601 duration of each aggregation period, e.g. PT1H or P1D",
        "limit" => "Entities per page, 1 to 1000",
        "count" => "true answers the total number of matches",
        _ => return None,
    })
}

/// The input schema of a broker tool with `args`, as the broker publishes it, each argument with
/// its one line of grammar.
fn input_schema(args: &[Arg]) -> Map<String, Value> {
    let properties: Map<String, Value> = args
        .iter()
        .map(|arg| {
            let mut schema = schema_of(arg.kind);
            if let (Value::Object(object), Some(line)) = (&mut schema, grammar(arg.name)) {
                object.insert("description".to_owned(), json!(line));
            }
            (arg.name.to_owned(), schema)
        })
        .collect();
    let required: Vec<&str> = args
        .iter()
        .filter(|arg| arg.required)
        .map(|arg| arg.name)
        .collect();
    let mut schema = Map::new();
    schema.insert("type".into(), json!("object"));
    schema.insert("properties".into(), Value::Object(properties));
    if !required.is_empty() {
        schema.insert("required".into(), json!(required));
    }
    schema.insert("additionalProperties".into(), json!(false));
    schema
}

// The argument tables of Antares `crates/antares-mcp/src/tools.rs`, one to one (EP-112).
const GEOMETRIES: &[&str] = &[
    "Point",
    "MultiPoint",
    "LineString",
    "MultiLineString",
    "Polygon",
    "MultiPolygon",
];
const JOINS: &[&str] = &["flat", "inline"];
const TIMERELS: &[&str] = &["before", "after", "between"];
const TIMEPROPERTIES: &[&str] = &["observedAt", "createdAt", "modifiedAt", "deletedAt"];
/// The entity representations (CIM 009 6.3.7); `sysAttrs` is the broker's flag for
/// `options=sysAttrs` (T-4258).
const FORMATS: &[&str] = &["normalized", "concise", "keyValues"];
const TEMPORAL_FORMATS: &[&str] = &["normalized", "temporalValues", "aggregatedValues"];

const GET_ENTITY: &[Arg] = &[
    req("id", Kind::Id),
    opt("attrs", Kind::Attrs),
    opt("pick", Kind::Attrs),
    opt("omit", Kind::Attrs),
    opt("lang", Kind::Text(64)),
    opt("join", Kind::Word(JOINS)),
    opt("joinLevel", Kind::Int(1, 3)),
    opt("format", Kind::Word(FORMATS)),
    opt("sysAttrs", Kind::Flag),
];
const QUERY: &[Arg] = &[
    opt("type", Kind::Types),
    opt("id", Kind::Ids),
    opt("idPattern", Kind::Text(256)),
    opt("attrs", Kind::Attrs),
    opt("pick", Kind::Attrs),
    opt("omit", Kind::Attrs),
    opt("q", Kind::Text(4096)),
    opt("scopeQ", Kind::Text(1024)),
    opt("georel", Kind::Text(256)),
    opt("geometry", Kind::Word(GEOMETRIES)),
    opt("coordinates", Kind::Coordinates),
    opt("geoproperty", Kind::Attr),
    opt("local", Kind::Flag),
    opt("lang", Kind::Text(64)),
    opt("join", Kind::Word(JOINS)),
    opt("joinLevel", Kind::Int(1, 3)),
    opt("format", Kind::Word(FORMATS)),
    opt("sysAttrs", Kind::Flag),
    opt("orderBy", Kind::Text(1024)),
    opt("count", Kind::Flag),
    opt("limit", Kind::Int(1, 1000)),
    opt("cursor", Kind::Text(CURSOR_LEN)),
];
const BATCH_QUERY: &[Arg] = &[
    req("type", Kind::Types),
    opt("id", Kind::Ids),
    opt("attrs", Kind::Attrs),
    opt("pick", Kind::Attrs),
    opt("omit", Kind::Attrs),
    opt("q", Kind::Text(4096)),
    opt("scopeQ", Kind::Text(1024)),
    opt("georel", Kind::Text(256)),
    opt("geometry", Kind::Word(GEOMETRIES)),
    opt("coordinates", Kind::Coordinates),
    opt("geoproperty", Kind::Attr),
    opt("local", Kind::Flag),
    opt("lang", Kind::Text(64)),
    opt("count", Kind::Flag),
    opt("limit", Kind::Int(1, 1000)),
    opt("cursor", Kind::Text(CURSOR_LEN)),
];
const TEMPORAL_ONE: &[Arg] = &[
    req("id", Kind::Id),
    opt("attrs", Kind::Attrs),
    req("timerel", Kind::Word(TIMERELS)),
    req("timeAt", Kind::Text(64)),
    opt("endTimeAt", Kind::Text(64)),
    opt("timeproperty", Kind::Word(TIMEPROPERTIES)),
    opt("lastN", Kind::Int(1, 100)),
    opt("aggrMethods", Kind::Text(256)),
    opt("aggrPeriodDuration", Kind::Text(64)),
    opt("format", Kind::Word(TEMPORAL_FORMATS)),
    opt("sysAttrs", Kind::Flag),
];
const TEMPORAL_QUERY: &[Arg] = &[
    opt("type", Kind::Types),
    opt("id", Kind::Ids),
    opt("attrs", Kind::Attrs),
    opt("q", Kind::Text(4096)),
    opt("scopeQ", Kind::Text(1024)),
    opt("georel", Kind::Text(256)),
    opt("geometry", Kind::Word(GEOMETRIES)),
    opt("coordinates", Kind::Coordinates),
    opt("geoproperty", Kind::Attr),
    req("timerel", Kind::Word(TIMERELS)),
    req("timeAt", Kind::Text(64)),
    opt("endTimeAt", Kind::Text(64)),
    opt("timeproperty", Kind::Word(TIMEPROPERTIES)),
    opt("lastN", Kind::Int(1, 100)),
    opt("aggrMethods", Kind::Text(256)),
    opt("aggrPeriodDuration", Kind::Text(64)),
    opt("format", Kind::Word(TEMPORAL_FORMATS)),
    opt("sysAttrs", Kind::Flag),
    opt("count", Kind::Flag),
    opt("limit", Kind::Int(1, 1000)),
    opt("cursor", Kind::Text(CURSOR_LEN)),
];
/// The POST form: its conditions go into the Query document, which has no representation.
const BATCH_TEMPORAL_QUERY: &[Arg] = &[
    opt("type", Kind::Types),
    opt("id", Kind::Ids),
    opt("attrs", Kind::Attrs),
    opt("q", Kind::Text(4096)),
    opt("scopeQ", Kind::Text(1024)),
    opt("georel", Kind::Text(256)),
    opt("geometry", Kind::Word(GEOMETRIES)),
    opt("coordinates", Kind::Coordinates),
    opt("geoproperty", Kind::Attr),
    req("timerel", Kind::Word(TIMERELS)),
    req("timeAt", Kind::Text(64)),
    opt("endTimeAt", Kind::Text(64)),
    opt("timeproperty", Kind::Word(TIMEPROPERTIES)),
    opt("lastN", Kind::Int(1, 100)),
    opt("aggrMethods", Kind::Text(256)),
    opt("aggrPeriodDuration", Kind::Text(64)),
    opt("count", Kind::Flag),
    opt("limit", Kind::Int(1, 1000)),
    opt("cursor", Kind::Text(CURSOR_LEN)),
];
const ID_ONLY: &[Arg] = &[req("id", Kind::Text(512))];
const PAGE_ONLY: &[Arg] = &[
    opt("count", Kind::Flag),
    opt("limit", Kind::Int(1, 1000)),
    opt("cursor", Kind::Text(CURSOR_LEN)),
];
const NONE: &[Arg] = &[];
const ATTR_VALUE: &[Arg] = &[req("id", Kind::Id), req("attrId", Kind::Attr)];
const TYPE_ONLY: &[Arg] = &[req("type", Kind::Type)];
const ATTR_ONLY: &[Arg] = &[req("attrId", Kind::Attr)];
const ENTITY_ID: &[Arg] = &[req("id", Kind::Id)];
const ENTITY: &[Arg] = &[req("entity", Kind::Entity)];
const REPLACE: &[Arg] = &[req("id", Kind::Id), req("entity", Kind::Entity)];
const ATTRIBUTES: &[Arg] = &[req("id", Kind::Id), req("attributes", Kind::Fragment)];
const APPEND: &[Arg] = &[
    req("id", Kind::Id),
    req("attributes", Kind::Fragment),
    opt("noOverwrite", Kind::Flag),
];
const MERGE: &[Arg] = &[req("id", Kind::Id), req("fragment", Kind::Fragment)];
const ONE_ATTRIBUTE: &[Arg] = &[
    req("id", Kind::Id),
    req("attrId", Kind::Attr),
    req("attribute", Kind::Attribute),
];
const DELETE_ATTRIBUTE: &[Arg] = &[
    req("id", Kind::Id),
    req("attrId", Kind::Attr),
    opt("datasetId", Kind::Text(512)),
    opt("deleteAll", Kind::Flag),
];
const BATCH: &[Arg] = &[req("entities", Kind::Entities)];
const BATCH_UPSERT: &[Arg] = &[
    req("entities", Kind::Entities),
    opt("options", Kind::Word(&["update", "replace"])),
];
const BATCH_UPDATE: &[Arg] = &[
    req("entities", Kind::Entities),
    opt("noOverwrite", Kind::Flag),
];
const BATCH_DELETE: &[Arg] = &[req("ids", Kind::Ids)];
const ENDPOINT: &[Arg] = &[
    req("uri", Kind::Endpoint),
    opt(
        "accept",
        Kind::Word(&["application/json", "application/ld+json"]),
    ),
];
const NOTIFICATION: &[Arg] = &[
    opt("attributes", Kind::Attrs),
    opt(
        "format",
        Kind::Word(&["normalized", "concise", "keyValues"]),
    ),
    req("endpoint", Kind::Object(ENDPOINT)),
];
const SELECTOR: &[Arg] = &[
    req("type", Kind::Type),
    opt("id", Kind::Id),
    opt("idPattern", Kind::Text(256)),
];
const SUBSCRIPTION: &[Arg] = &[
    req("type", Kind::Word(&["Subscription"])),
    opt("subscriptionName", Kind::Text(256)),
    opt("description", Kind::Text(1024)),
    opt("entities", Kind::ListOf(SELECTOR, 50)),
    opt("watchedAttributes", Kind::Attrs),
    opt("q", Kind::Text(4096)),
    req("notification", Kind::Object(NOTIFICATION)),
    opt("expiresAt", Kind::Text(64)),
    opt("throttling", Kind::Int(0, 86_400)),
    opt("isActive", Kind::Flag),
];
const SUBSCRIPTION_FRAGMENT: &[Arg] = &[
    opt("subscriptionName", Kind::Text(256)),
    opt("description", Kind::Text(1024)),
    opt("entities", Kind::ListOf(SELECTOR, 50)),
    opt("watchedAttributes", Kind::Attrs),
    opt("q", Kind::Text(4096)),
    opt("notification", Kind::Object(NOTIFICATION)),
    opt("expiresAt", Kind::Text(64)),
    opt("throttling", Kind::Int(0, 86_400)),
    opt("isActive", Kind::Flag),
];
const CREATE_SUBSCRIPTION: &[Arg] = &[req("subscription", Kind::Object(SUBSCRIPTION))];
const UPDATE_SUBSCRIPTION: &[Arg] = &[
    req("id", Kind::Text(512)),
    req("fragment", Kind::Object(SUBSCRIPTION_FRAGMENT)),
];

/// A broker tool: its name, the operation of the gateway's Policy that grants it, its
/// description, its arguments and its result key, as the broker's view lists it (EP-112).
const fn broker(
    name: &'static str,
    operations: &'static [Operation],
    description: &'static str,
    args: &'static [Arg],
    result_key: &'static str,
) -> Tool {
    Tool {
        name,
        operations,
        description,
        shape: Shape::Broker(args),
        result_key,
    }
}

/// Every tool of the broker's view (Antares `crates/antares-mcp/src/tools.rs`, sorted by name),
/// then the gateway's two describe tools. The hub and the named servers list these names with
/// these arguments, so a call that works on an Endpoint's own MCP works on the hub with
/// `endpoint` beside it, and the hub hands the arguments on as they came (EP-112).
pub(crate) const TOOLS: &[Tool] = &[
    broker(
        "append_attributes",
        &[Operation::AppendAttrs],
        "Append Attributes to one Entity (TS 104 175 10.2.4); noOverwrite keeps those it has. The tool asks the person to confirm before it writes.",
        APPEND,
        "result",
    ),
    broker(
        "batch_create",
        &[Operation::CreateBatch],
        "Create several Entities (TS 104 175 10.3.2). The tool asks the person to confirm before it writes.",
        BATCH,
        "result",
    ),
    broker(
        "batch_delete",
        &[Operation::DeleteBatch],
        "Delete several Entities by id (TS 104 175 10.3.6). The tool asks the person to confirm before it writes.",
        BATCH_DELETE,
        "result",
    ),
    broker(
        "batch_merge",
        &[Operation::MergeBatch],
        "Merge into several Entities (TS 104 175 10.3.5). The tool asks the person to confirm before it writes.",
        BATCH,
        "result",
    ),
    broker(
        "batch_query",
        &[Operation::QueryBatch],
        "Query Entities with a Query document (TS 104 175 10.4.3, POST form): the Entities of the given types that match the conditions, a page at a time.",
        BATCH_QUERY,
        "entities",
    ),
    broker(
        "batch_query_temporal",
        &[Operation::QueryTemporal],
        "Query the history of Entities with a Query document (TS 104 175 11.3.3, POST form), a page of Entities at a time.",
        BATCH_TEMPORAL_QUERY,
        "entities",
    ),
    broker(
        "batch_update",
        &[Operation::UpdateBatch],
        "Update Attributes of several Entities (TS 104 175 10.3.4). The tool asks the person to confirm before it writes.",
        BATCH_UPDATE,
        "result",
    ),
    broker(
        "batch_upsert",
        &[Operation::UpsertBatch],
        "Create several Entities or update those that exist (TS 104 175 10.3.3). The tool asks the person to confirm before it writes.",
        BATCH_UPSERT,
        "result",
    ),
    broker(
        "create_entity",
        &[Operation::CreateEntity],
        "Create one Entity (TS 104 175 10.2.2). The tool asks the person to confirm before it writes.",
        ENTITY,
        "created",
    ),
    broker(
        "create_subscription",
        &[Operation::CreateSubscription],
        "Subscribe an endpoint the deployment allows to changes of Entities (TS 104 175 10.5.2). The tool asks the person to confirm before it writes.",
        CREATE_SUBSCRIPTION,
        "created",
    ),
    broker(
        "delete_attribute",
        &[Operation::DeleteAttrs],
        "Delete one Attribute of one Entity (TS 104 175 10.2.7). The tool asks the person to confirm before it writes.",
        DELETE_ATTRIBUTE,
        "result",
    ),
    broker(
        "delete_entity",
        &[Operation::DeleteEntity],
        "Delete one Entity (TS 104 175 10.2.8). The tool asks the person to confirm before it writes.",
        ENTITY_ID,
        "result",
    ),
    broker(
        "delete_subscription",
        &[Operation::DeleteSubscription],
        "Delete one Subscription (TS 104 175 10.5.6). The tool asks the person to confirm before it writes.",
        ID_ONLY,
        "result",
    ),
    broker(
        "get_attribute",
        &[Operation::RetrieveAttrTypeInfo],
        "Describe one Attribute: the types that carry it and how many instances exist (TS 104 175 10.6.8).",
        ATTR_ONLY,
        "attribute",
    ),
    broker(
        "get_attribute_value",
        &[Operation::RetrieveEntity],
        "Read one Attribute of one Entity (TS 104 175 10.4.4).",
        ATTR_VALUE,
        "attribute",
    ),
    broker(
        "get_entity",
        &[Operation::RetrieveEntity],
        "Read one Entity by its id (TS 104 175 10.4.2); attrs, pick and omit narrow what it returns.",
        GET_ENTITY,
        "entity",
    ),
    broker(
        "get_subscription",
        &[Operation::RetrieveSubscription],
        "Read one Subscription made through this endpoint (TS 104 175 10.5.4).",
        ID_ONLY,
        "subscription",
    ),
    broker(
        "get_type",
        &[Operation::RetrieveEntityTypeInfo],
        "Describe one Entity type: its Attributes and how many Entities it has (TS 104 175 10.6.5).",
        TYPE_ONLY,
        "entityType",
    ),
    broker(
        "list_attribute_details",
        &[Operation::RetrieveAttrTypeDetails],
        "List the Attributes with the types that carry each (TS 104 175 10.6.7).",
        NONE,
        "attributes",
    ),
    broker(
        "list_attributes",
        &[Operation::RetrieveAttrTypes],
        "List the names of the Attributes this endpoint serves (TS 104 175 10.6.6).",
        NONE,
        "attributes",
    ),
    broker(
        "list_subscriptions",
        &[Operation::QuerySubscription],
        "List the Subscriptions made through this endpoint (TS 104 175 10.5.5), a page at a time.",
        PAGE_ONLY,
        "subscriptions",
    ),
    broker(
        "list_type_details",
        &[Operation::RetrieveEntityTypeDetails],
        "List the Entity types with the Attributes of each (TS 104 175 10.6.4).",
        NONE,
        "entityTypes",
    ),
    broker(
        "list_types",
        &[Operation::RetrieveEntityTypes],
        "List the names of the Entity types this endpoint serves (TS 104 175 10.6.3).",
        NONE,
        "entityTypes",
    ),
    broker(
        "merge_entity",
        &[Operation::MergeEntity],
        "Merge a fragment into one Entity (TS 104 175 10.2.9). The tool asks the person to confirm before it writes.",
        MERGE,
        "result",
    ),
    broker(
        "query_entities",
        &[Operation::QueryEntity],
        "Query Entities (TS 104 175 10.4.3): by type, id, q, scopeQ or area, a page at a time. Give at least one of type, attrs, q, georel or local.",
        QUERY,
        "entities",
    ),
    broker(
        "query_temporal",
        &[Operation::QueryTemporal],
        "Query the history of Entities (TS 104 175 11.3.3) between timeAt and endTimeAt, a page of Entities at a time; lastN bounds each Attribute's instances.",
        TEMPORAL_QUERY,
        "entities",
    ),
    broker(
        "replace_attribute",
        &[Operation::ReplaceAttrs],
        "Replace one Attribute of one Entity (TS 104 175 10.2.11). The tool asks the person to confirm before it writes.",
        ONE_ATTRIBUTE,
        "result",
    ),
    broker(
        "replace_entity",
        &[Operation::ReplaceEntity],
        "Replace one Entity whole (TS 104 175 10.2.10). The tool asks the person to confirm before it writes.",
        REPLACE,
        "result",
    ),
    broker(
        "retrieve_temporal",
        &[Operation::RetrieveTemporal],
        "Read the history of one Entity (TS 104 175 11.3.2) between timeAt and endTimeAt; lastN bounds each Attribute's instances.",
        TEMPORAL_ONE,
        "entity",
    ),
    broker(
        "update_attribute",
        &[Operation::UpdateAttrs],
        "Update part of one Attribute of one Entity (TS 104 175 10.2.5). The tool asks the person to confirm before it writes.",
        ONE_ATTRIBUTE,
        "result",
    ),
    broker(
        "update_attributes",
        &[Operation::UpdateAttrs],
        "Update Attributes one Entity has (TS 104 175 10.2.3). The tool asks the person to confirm before it writes.",
        ATTRIBUTES,
        "result",
    ),
    broker(
        "update_subscription",
        &[Operation::UpdateSubscription],
        "Update one Subscription (TS 104 175 10.5.3). The tool asks the person to confirm before it writes.",
        UPDATE_SUBSCRIPTION,
        "result",
    ),
    Tool {
        name: "describe_access",
        operations: &[],
        description:
            "The caller's effective grants here: operations, attributes, residual constraints. \
             `format` picks the language: `permissions` (AuthZEN, the default), `odrl` (ODRL 2.2) \
             or `grant-ast` (UCAST); the grants are the same in all three (EP-60).",
        shape: Shape::Own(|| {
            json!({
                "type": "object",
                "properties": {
                    "format": { "type": "string", "enum": ACCESS_FORMATS },
                },
                "additionalProperties": false,
            })
        }),
        result_key: "access",
    },
    Tool {
        name: "describe_schema",
        // The schema is the description of what a read returns, so any read is enough to
        // be shown it; what it then contains is projected to the grant (EP-47).
        operations: &[
            Operation::QueryEntity,
            Operation::RetrieveEntity,
            Operation::RetrieveEntityTypes,
        ],
        description:
            "Inspect the data model of this context space, narrowed to the caller's grant. \
             Read linkml first: it carries the classes, their slots and their descriptions in \
             one small document. The default `summary` lists every artifact with its size and \
             digest and says which to read.",
        shape: Shape::Own(|| {
            json!({
                "type": "object",
                "properties": {
                    "format": {
                        "type": "string",
                        "enum": [
                            "summary", "json-schema", "context",
                            "linkml", "shacl", "owl", "rdf", "markdown", "qb",
                        ],
                        "description": "summary (the default) lists the models and every artifact with its format, size and digest, linkml first; the others render one, the text formalisms as {format, mediaType, document}; qb only where the model is a data cube",
                    },
                    "entityType": {
                        "oneOf": [
                            schema_of(Kind::Type),
                            {
                                "type": "array",
                                "items": schema_of(Kind::Type),
                                "minItems": 1,
                                "maxItems": 64,
                            },
                        ],
                        "description": "One entity type, or the list of the classes you need",
                    },
                    "version": { "type": "integer", "minimum": 1, "description": "Model major version" },
                },
                "additionalProperties": false,
            })
        }),
        result_key: "schema",
    },
];

/// One compiled validator per tool, in the order of `TOOLS` (AG-31).
///
/// Compiled once rather than per call: the schemas are constants, and an agent that asks
/// twice pays for the compilation once. A schema that does not compile leaves its tool
/// without a validator, and `validate` then refuses every call to it rather than letting
/// unvalidated arguments through.
static VALIDATORS: LazyLock<Vec<Option<jsonschema::Validator>>> = LazyLock::new(|| {
    TOOLS
        .iter()
        .map(|tool| jsonschema::draft7::new(&tool.input_schema()).ok())
        .collect()
});

/// Checks one tool's arguments against its published JSON Schema before anything is built
/// from them (AG-21, AG-31).
///
/// `additionalProperties: false` is in every schema, so an unknown field is an error here
/// rather than a parameter that is silently dropped and an answer that quietly means
/// something else.
pub(crate) fn validate(index: usize, arguments: &Map<String, Value>) -> Result<(), String> {
    let Some(Some(validator)) = VALIDATORS.get(index) else {
        return Err(
            "this tool's schema does not compile, so its arguments cannot be checked".to_owned(),
        );
    };
    let instance = Value::Object(arguments.clone());
    let problems: Vec<String> = validator
        .iter_errors(&instance)
        .map(|error| {
            let at = error.instance_path().to_string();
            let rule = broken_rule(&error);
            match at.is_empty() {
                true => rule,
                false => format!("{at}: {rule}"),
            }
        })
        .collect();
    match problems.is_empty() {
        true => Ok(()),
        false => Err(problems.join("; ")),
    }
}

/// Names the rule an argument broke, without repeating what the caller sent (GW31, AG-21).
///
/// A validator writes the offending value into its own message, and a tool error is read by a
/// model: a refusal that quotes the argument carries whatever was typed into the next prompt.
/// The place and the rule are what a caller needs to correct the call; the value it already has.
fn broken_rule(error: &jsonschema::ValidationError<'_>) -> String {
    use jsonschema::error::ValidationErrorKind as Rule;
    match error.kind() {
        Rule::AdditionalProperties { unexpected } => {
            format!(
                "no argument of this tool is called {}",
                unexpected.join(", ")
            )
        }
        Rule::Required { property } => format!("{property} is required"),
        // A list of alternatives, each one argument the call has to name (CIM 009 5.7.2.4): the
        // names are what the caller needs, not the keyword (T-3044).
        Rule::AnyOf { context } => {
            let names: Vec<&str> = context
                .iter()
                .filter_map(|branch| match branch.as_slice() {
                    [only] => match only.kind() {
                        Rule::Required { property } => property.as_str(),
                        _ => None,
                    },
                    _ => None,
                })
                .collect();
            match !names.is_empty() && names.len() == context.len() {
                true => format!(
                    "names none of {}: one of them is required",
                    names.join(", ")
                ),
                false => "matches none of the shapes the schema allows".to_owned(),
            }
        }
        Rule::Type { .. } => "is not of the type the schema declares".to_owned(),
        Rule::Pattern { pattern } => format!("does not match {pattern}"),
        Rule::MaxLength { limit } => format!("is longer than {limit} characters"),
        Rule::MinLength { limit } => format!("is shorter than {limit} characters"),
        Rule::MaxItems { limit } => format!("has more than {limit} items"),
        Rule::MinItems { limit } => format!("has fewer than {limit} items"),
        Rule::Maximum { limit } => format!("is above {limit}"),
        Rule::Minimum { limit } => format!("is below {limit}"),
        Rule::Enum { options } => format!("is not one of {options}"),
        Rule::Format { format } => format!("is not a {format}"),
        _ => {
            let keyword = error.schema_path().to_string();
            let keyword = keyword.rsplit('/').next().unwrap_or_default().to_owned();
            match keyword.is_empty() {
                true => "is not what the schema allows".to_owned(),
                false => format!("breaks the schema's {keyword}"),
            }
        }
    }
}

/// Whether the caller holds any of the tool's operations, asked of the PDP that also
/// enforces them, so discovery and enforcement cannot drift (SP-16).
pub(crate) fn granted(
    gateway: &Gateway,
    endpoint: &Endpoint,
    subject: &Subject,
    operations: &[Operation],
) -> bool {
    operations.is_empty()
        || operations.iter().any(|operation| {
            !matches!(
                gateway
                    .pdp
                    .decide(subject, *operation, &PolicyRequest::default(), endpoint),
                Verdict::Deny
            )
        })
}

/// Whether the caller may read anything on this Endpoint: a data tool its grants cover. An
/// Endpoint that answers only the describing tools is not one the hub lists (ADR-N-025 2.1).
pub(crate) fn reads_anything(gateway: &Gateway, endpoint: &Endpoint, subject: &Subject) -> bool {
    TOOLS.iter().any(|tool| {
        !tool.operations.is_empty() && granted(gateway, endpoint, subject, tool.operations)
    })
}

/// The hub's catalogue (ADR-N-025 2.2, 2.8): each tool once, listed when at least one Endpoint of
/// the caller's list grants it, with the one required `endpoint` argument whose enum is that list.
/// `tools/list` never multiplies by Endpoint.
pub(crate) fn hub_tools(gateway: &Gateway, reach: &[(Arc<Endpoint>, Subject)]) -> Vec<Value> {
    let slugs: Vec<&str> = reach
        .iter()
        .map(|(endpoint, _)| endpoint.slug.as_str())
        .collect();
    TOOLS
        .iter()
        .filter(|tool| {
            reach
                .iter()
                .any(|(endpoint, subject)| granted(gateway, endpoint, subject, tool.operations))
        })
        .map(|tool| {
            let mut schema = tool.input_schema();
            if let Value::Object(object) = &mut schema {
                if let Some(Value::Object(properties)) = object.get_mut("properties") {
                    properties.insert(
                        "endpoint".to_owned(),
                        json!({
                            "type": "string",
                            "enum": slugs,
                            "description": "The Endpoint this call reads or writes, by the slug \
                                            list_endpoints answers. One per call.",
                        }),
                    );
                }
                match object.get_mut("required") {
                    Some(Value::Array(required)) => required.push(json!("endpoint")),
                    _ => {
                        object.insert("required".to_owned(), json!(["endpoint"]));
                    }
                }
            }
            json!({
                "name": tool.name,
                "description": format!(
                    "{} Name the Endpoint in `endpoint`; the call is that Endpoint's own.",
                    tool.description
                ),
                "inputSchema": schema,
                "outputSchema": output_schema(tool),
                "annotations": tool.annotations(),
            })
        })
        .collect()
}

/// `query_entities` → `title: "Query entities"`, the hints beside it. Every tool reads or
/// writes this platform's own data, never an open world (`openWorldHint: false`).
pub(crate) fn annotations(name: &str, read_only: bool, destructive: bool) -> Value {
    let mut title = name.replace('_', " ");
    if let Some(first) = title.get_mut(0..1) {
        first.make_ascii_uppercase();
    }
    json!({
        "title": title,
        "readOnlyHint": read_only,
        "destructiveHint": destructive,
        "openWorldHint": false,
    })
}

/// How the hub stands with one tool on the Endpoint a call names (ADR-N-025 2.8, SP-17, SP-20).
pub(crate) enum HubTool {
    /// No such tool, or none of the caller's Endpoints grants it: the hub never listed it.
    Unknown,
    /// Listed, because another Endpoint of the caller's list grants it, and not granted here:
    /// the tool error that names the Endpoint and the operation.
    Ungranted(Value),
    /// The Endpoint grants it; the call goes on as that Endpoint's own.
    Granted,
}

/// Where `name` stands on `endpoint` for a caller whose hub list is `reach`.
pub(crate) fn hub_tool(
    gateway: &Gateway,
    endpoint: &Endpoint,
    subject: &Subject,
    reach: &[(Arc<Endpoint>, Subject)],
    name: &str,
) -> HubTool {
    let Some(tool) = TOOLS.iter().find(|tool| tool.name == name) else {
        return HubTool::Unknown;
    };
    if granted(gateway, endpoint, subject, tool.operations) {
        return HubTool::Granted;
    }
    let listed = reach
        .iter()
        .any(|(other, holder)| granted(gateway, other, holder, tool.operations));
    if !listed {
        return HubTool::Unknown;
    }
    let operations: Vec<&str> = tool.operations.iter().map(Operation::as_str).collect();
    HubTool::Ungranted(refused(
        &format!(
            "the Endpoint `{}` does not grant {} to this caller; `{name}` answers only on the \
             Endpoints whose Policy grants it (SP-17)",
            endpoint.slug,
            operations.join(" or ")
        ),
        &Value::Null,
    ))
}

/// What a tool's `structuredContent` looks like, published beside its input schema so a
/// client can validate the half it parses (MCP `outputSchema`).
///
/// The payload's own shape is the data model's, not MCP's — a schema here would be a second
/// copy of it that drifts — so the output schema names what the object carries and leaves the
/// value to `describe_schema`.
pub(crate) fn output_schema(tool: &Tool) -> Value {
    json!({
        "type": "object",
        "properties": {
            tool.result_key: { "description": tool.description },
            "restricted": {
                "type": "boolean",
                "description": "Present when the policy narrowed this answer (AG-13, R22)",
            },
            "jc:source": {
                "type": "array",
                "items": { "type": "string" },
                "description": "The registrations this answer is a union over, by name (EP-71)",
            },
        },
        "required": [tool.result_key],
    })
}

#[cfg(test)]
mod tests {
    use super::{
        anonymous_bounds, anonymous_may, anonymous_refusal, duration_seconds, keep_anonymous,
        Shape, Tool, ANONYMOUS_TOOLS, TOOLS,
    };
    use jc_core::kinds::Operation;
    use serde_json::{json, Map, Value};

    /// A day of AirQualityObserved history, the one shape an anonymous caller is let through.
    fn day() -> Map<String, Value> {
        json!({
            "type": ["AirQualityObserved"],
            "timerel": "between",
            "timeAt": "2026-10-12T00:00:00Z",
            "endTimeAt": "2026-10-13T00:00:00Z",
        })
        .as_object()
        .cloned()
        .expect("an object")
    }

    fn bounded(name: &str, change: Value) -> Result<Map<String, Value>, String> {
        let mut arguments = day();
        for (key, value) in change.as_object().expect("an object") {
            if value.is_null() {
                arguments.remove(key);
            } else {
                arguments.insert(key.clone(), value.clone());
            }
        }
        anonymous_bounds(name, &mut arguments).map(|()| arguments)
    }

    /// T-4261: a window of 31 days passes, one second more is refused, by its bound.
    #[test]
    fn an_anonymous_window_over_31_days_is_refused() {
        assert!(bounded(
            "query_temporal",
            json!({ "endTimeAt": "2026-11-12T00:00:00Z" })
        )
        .is_ok());
        let refused = bounded(
            "query_temporal",
            json!({ "endTimeAt": "2026-11-12T00:00:01Z" }),
        )
        .expect_err("32 days are refused");
        assert!(refused.contains("at most 31 days"), "{refused}");
    }

    /// T-4261: `before`, `after`, a missing end, an end before the start and an unparsable
    /// instant are no window.
    #[test]
    fn an_anonymous_read_without_a_window_is_refused() {
        for change in [
            json!({ "timerel": "before", "endTimeAt": null }),
            json!({ "timerel": "after", "endTimeAt": null }),
            json!({ "endTimeAt": null }),
            json!({ "endTimeAt": "2026-10-11T00:00:00Z" }),
            json!({ "timeAt": "yesterday" }),
            json!({ "timerel": null }),
        ] {
            let refused = bounded("retrieve_temporal", change.clone()).expect_err("no window");
            assert!(refused.contains("timerel between"), "{change}: {refused}");
        }
    }

    /// T-4261, decided: `lastN` above 100 is refused, not clamped, so the caller learns why its
    /// answer is short; an absent one becomes 100.
    #[test]
    fn an_anonymous_last_n_is_100_when_absent_and_more_is_refused() {
        assert_eq!(
            bounded("query_temporal", json!({})).expect("ok")["lastN"],
            json!(100)
        );
        assert_eq!(
            bounded("query_temporal", json!({ "lastN": 7 })).expect("ok")["lastN"],
            json!(7)
        );
        for n in [json!(1000), json!(101), json!(0), json!("5")] {
            let refused = bounded("query_temporal", json!({ "lastN": n })).expect_err("refused");
            assert!(
                refused.contains("`lastN` without a token is 1 to 100"),
                "{refused}"
            );
        }
    }

    /// T-4261: a history read over every Entity names neither `type` nor `id`.
    #[test]
    fn an_anonymous_read_without_type_or_id_is_refused() {
        for change in [
            json!({ "type": null }),
            json!({ "type": [] }),
            json!({ "type": null, "id": [] }),
        ] {
            let refused = bounded("query_temporal", change).expect_err("refused");
            assert!(refused.contains("names `type` or `id`"), "{refused}");
        }
        assert!(bounded(
            "query_temporal",
            json!({ "type": null, "id": ["urn:ngsi-ld:AirQualityObserved:1"] })
        )
        .is_ok());
        assert!(bounded("query_temporal", json!({ "limit": 1000 })).is_ok());
        assert!(bounded("query_temporal", json!({ "limit": 1001 })).is_err());
    }

    /// T-4261: aggregation periods under an hour, and aggregation with no period, are refused.
    #[test]
    fn an_anonymous_aggregation_period_under_an_hour_is_refused() {
        for period in [
            json!("PT1M"),
            json!("PT59M59S"),
            json!("P"),
            json!("1H"),
            Value::Null,
        ] {
            let refused = bounded(
                "query_temporal",
                json!({ "aggrMethods": "avg", "aggrPeriodDuration": period }),
            )
            .expect_err("refused");
            assert!(refused.contains("at least PT1H"), "{refused}");
        }
        for period in ["PT1H", "PT60M", "P1D", "P1W", "P1M", "PT0.5H30M"] {
            assert!(
                bounded(
                    "query_temporal",
                    json!({ "aggrMethods": "avg", "aggrPeriodDuration": period })
                )
                .is_ok(),
                "{period}"
            );
        }
    }

    #[test]
    fn durations_parse_as_iso_8601_and_nothing_else() {
        assert_eq!(duration_seconds("PT1H"), Some(3600.0));
        assert_eq!(duration_seconds("P1DT1M"), Some(86_460.0));
        assert_eq!(duration_seconds("PT1.5S"), Some(1.5));
        for bad in ["", "P", "PT", "P1H", "PT1D", "PT1M1H", "P1", "PTH", "P-1D"] {
            assert_eq!(duration_seconds(bad), None, "{bad}");
        }
    }

    /// T-4261: the bounds are the temporal tools' alone.
    #[test]
    fn the_bounds_leave_every_other_tool_untouched() {
        let mut arguments = Map::new();
        assert!(anonymous_bounds("query_entities", &mut arguments).is_ok());
        assert!(arguments.is_empty());
    }

    /// T-4168: every tool an anonymous caller may use reads, and every tool that writes, or that
    /// nobody named, is kept from it.
    #[test]
    fn an_anonymous_caller_may_use_the_named_reads_alone() {
        for name in ANONYMOUS_TOOLS
            .iter()
            .filter(|name| **name != "list_endpoints")
        {
            let tool = TOOLS.iter().find(|tool| tool.name == *name);
            assert!(
                tool.is_some_and(Tool::read_only),
                "{name} is not a read of the catalogue"
            );
        }
        for tool in TOOLS.iter().filter(|tool| !tool.read_only()) {
            assert!(!anonymous_may(tool.name), "{} writes", tool.name);
        }
        for name in [
            "get_type",
            "list_type_details",
            "list_attributes",
            "list_attribute_details",
            "get_attribute",
            "query_temporal",
        ] {
            assert!(
                anonymous_may(name),
                "{name} is a read an anonymous caller is refused"
            );
        }
        for name in [
            "create_subscription",
            "delete_entity",
            "a_tool_added_later",
            "",
        ] {
            assert!(!anonymous_may(name), "{name} is admitted");
        }
    }

    #[test]
    fn an_anonymous_listing_keeps_the_reads_and_drops_writes_and_nameless_entries() {
        let mut tools = vec![
            json!({"name": "query_entities"}),
            json!({"name": "create_subscription"}),
            json!({"name": "update_subscription"}),
            json!({"name": "a_tool_added_later"}),
            json!({"title": "no name"}),
            json!({"name": 7}),
            json!({"name": "describe_access"}),
        ];
        keep_anonymous(&mut tools);
        assert_eq!(
            tools,
            [
                json!({"name": "query_entities"}),
                json!({"name": "describe_access"})
            ]
        );
    }

    #[test]
    fn an_anonymous_refusal_is_a_json_rpc_error_naming_what_it_may_call() {
        let refusal = anonymous_refusal(json!(4), "delete_subscription");
        assert_eq!(refusal["id"], 4);
        assert_eq!(refusal["error"]["code"], -32602);
        let message = refusal["error"]["message"].as_str().unwrap_or_default();
        assert!(message.contains("`delete_subscription`"), "{message}");
        assert!(message.contains("query_entities"), "{message}");
        assert!(refusal.get("result").is_none());
    }

    /// T-1066, AG-07: `read_only` is the `readOnlyHint` an agent runtime reads before it asks a
    /// person, so it is derived from the operations rather than set beside them; and it says what
    /// the broker's view says of the same tool (EP-112).
    #[test]
    fn a_tool_that_writes_at_all_is_not_read_only() {
        for tool in TOOLS {
            let writes = tool.operations.iter().any(Operation::is_write);
            assert_eq!(
                tool.read_only(),
                !writes,
                "{} announces the wrong side of readOnlyHint",
                tool.name
            );
            assert_eq!(
                tool.read_only(),
                READS.contains(&tool.name) || tool.name.starts_with("describe_"),
                "{} reads on one surface and writes on the other",
                tool.name
            );
        }
        let adding: Vec<&str> = TOOLS
            .iter()
            .filter(|tool| !tool.read_only() && !tool.destructive())
            .map(|tool| tool.name)
            .collect();
        assert_eq!(
            adding,
            ["batch_create", "create_entity", "create_subscription"]
        );
    }

    /// The broker's read tools (Antares `docs/src/mcp.md` section 2).
    const READS: &[&str] = &[
        "batch_query",
        "batch_query_temporal",
        "get_attribute",
        "get_attribute_value",
        "get_entity",
        "get_subscription",
        "get_type",
        "list_attribute_details",
        "list_attributes",
        "list_subscriptions",
        "list_type_details",
        "list_types",
        "query_entities",
        "query_temporal",
        "retrieve_temporal",
    ];

    /// EP-112: the hub lists the broker's tools by the broker's names and argument names, so the
    /// call it hands on is the caller's as sent. The pin is Antares `crates/antares-mcp/src/
    /// tools.rs` at dev 2026-10-12; a broker tool renamed or an argument added there fails here
    /// first, and the conformance diff of the four surfaces catches what this copy misses.
    #[test]
    fn every_tool_is_the_broker_s_with_the_broker_s_arguments() {
        let pinned: &[(&str, &[&str])] = &[
            ("append_attributes", &["id", "attributes", "noOverwrite"]),
            ("batch_create", &["entities"]),
            ("batch_delete", &["ids"]),
            ("batch_merge", &["entities"]),
            (
                "batch_query",
                &[
                    "type",
                    "id",
                    "attrs",
                    "pick",
                    "omit",
                    "q",
                    "scopeQ",
                    "georel",
                    "geometry",
                    "coordinates",
                    "geoproperty",
                    "local",
                    "lang",
                    "count",
                    "limit",
                    "cursor",
                ],
            ),
            (
                "batch_query_temporal",
                &[
                    "type",
                    "id",
                    "attrs",
                    "q",
                    "scopeQ",
                    "georel",
                    "geometry",
                    "coordinates",
                    "geoproperty",
                    "timerel",
                    "timeAt",
                    "endTimeAt",
                    "timeproperty",
                    "lastN",
                    "aggrMethods",
                    "aggrPeriodDuration",
                    "count",
                    "limit",
                    "cursor",
                ],
            ),
            ("batch_update", &["entities", "noOverwrite"]),
            ("batch_upsert", &["entities", "options"]),
            ("create_entity", &["entity"]),
            ("create_subscription", &["subscription"]),
            (
                "delete_attribute",
                &["id", "attrId", "datasetId", "deleteAll"],
            ),
            ("delete_entity", &["id"]),
            ("delete_subscription", &["id"]),
            ("get_attribute", &["attrId"]),
            ("get_attribute_value", &["id", "attrId"]),
            (
                "get_entity",
                &[
                    "id",
                    "attrs",
                    "pick",
                    "omit",
                    "lang",
                    "join",
                    "joinLevel",
                    "format",
                    "sysAttrs",
                ],
            ),
            ("get_subscription", &["id"]),
            ("get_type", &["type"]),
            ("list_attribute_details", &[]),
            ("list_attributes", &[]),
            ("list_subscriptions", &["count", "limit", "cursor"]),
            ("list_type_details", &[]),
            ("list_types", &[]),
            ("merge_entity", &["id", "fragment"]),
            (
                "query_entities",
                &[
                    "type",
                    "id",
                    "idPattern",
                    "attrs",
                    "pick",
                    "omit",
                    "q",
                    "scopeQ",
                    "georel",
                    "geometry",
                    "coordinates",
                    "geoproperty",
                    "local",
                    "lang",
                    "join",
                    "joinLevel",
                    "format",
                    "sysAttrs",
                    "orderBy",
                    "count",
                    "limit",
                    "cursor",
                ],
            ),
            (
                "query_temporal",
                &[
                    "type",
                    "id",
                    "attrs",
                    "q",
                    "scopeQ",
                    "georel",
                    "geometry",
                    "coordinates",
                    "geoproperty",
                    "timerel",
                    "timeAt",
                    "endTimeAt",
                    "timeproperty",
                    "lastN",
                    "aggrMethods",
                    "aggrPeriodDuration",
                    "format",
                    "sysAttrs",
                    "count",
                    "limit",
                    "cursor",
                ],
            ),
            ("replace_attribute", &["id", "attrId", "attribute"]),
            ("replace_entity", &["id", "entity"]),
            (
                "retrieve_temporal",
                &[
                    "id",
                    "attrs",
                    "timerel",
                    "timeAt",
                    "endTimeAt",
                    "timeproperty",
                    "lastN",
                    "aggrMethods",
                    "aggrPeriodDuration",
                    "format",
                    "sysAttrs",
                ],
            ),
            ("update_attribute", &["id", "attrId", "attribute"]),
            ("update_attributes", &["id", "attributes"]),
            ("update_subscription", &["id", "fragment"]),
        ];
        let ours: Vec<(&str, Vec<&str>)> = TOOLS
            .iter()
            .filter_map(|tool| match tool.shape {
                Shape::Broker(args) => Some((tool.name, args.iter().map(|arg| arg.name).collect())),
                Shape::Own(_) => None,
            })
            .collect();
        let pinned: Vec<(&str, Vec<&str>)> = pinned
            .iter()
            .map(|(name, args)| (*name, args.to_vec()))
            .collect();
        assert_eq!(ours, pinned);
        let own: Vec<&str> = TOOLS
            .iter()
            .filter(|tool| matches!(tool.shape, Shape::Own(_)))
            .map(|tool| tool.name)
            .collect();
        assert_eq!(own, crate::mcp::describe::DESCRIBE_TOOLS);
    }

    /// T-4258, EP-116: the reads take the standard's representation and ordering, with the
    /// temporal formats on the history tools, and the POST history form takes none.
    #[test]
    fn the_reads_take_format_sys_attrs_and_order_by() {
        let schema = |name: &str| {
            TOOLS
                .iter()
                .find(|tool| tool.name == name)
                .expect("the tool")
                .input_schema()
        };
        for name in ["get_entity", "query_entities"] {
            let properties = &schema(name)["properties"];
            assert_eq!(
                properties["format"]["enum"],
                json!(["normalized", "concise", "keyValues"]),
                "{name}"
            );
            assert_eq!(properties["sysAttrs"]["type"], "boolean", "{name}");
        }
        for name in ["query_temporal", "retrieve_temporal"] {
            let properties = &schema(name)["properties"];
            assert_eq!(
                properties["format"]["enum"],
                json!(["normalized", "temporalValues", "aggregatedValues"]),
                "{name}"
            );
            assert_eq!(properties["sysAttrs"]["type"], "boolean", "{name}");
        }
        assert_eq!(
            schema("query_entities")["properties"]["orderBy"]["type"],
            "string"
        );
        assert!(schema("batch_query_temporal")["properties"]
            .get("format")
            .is_none());
        let query = schema("query_entities");
        for argument in ["q", "idPattern", "scopeQ", "georel", "orderBy", "format"] {
            assert!(
                query["properties"][argument]["description"].is_string(),
                "{argument} carries no grammar"
            );
        }
        assert!(
            schema("query_temporal")["properties"]["timerel"]["description"]
                .as_str()
                .is_some_and(|line| line.contains("between"))
        );
    }

    /// EP-116: the hub and every named server say they speak NGSI-LD and teach its language, and
    /// the primer names nothing a caller could use to reach around the hub.
    #[test]
    fn the_primer_teaches_ngsi_ld_and_names_no_slug_token_or_host() {
        let primer = crate::mcp::PRIMER;
        for word in [
            "NGSI-LD",
            "@context",
            "`q`",
            "idPattern",
            "timerel",
            "scopeQ",
            "orderBy",
            "sysAttrs",
            "lastN",
            "aggrMethods",
            "deletedAt",
            "list_endpoints",
            "describe_schema",
            "DCAT-AP",
        ] {
            assert!(primer.contains(word), "the primer does not mention {word}");
        }
        assert!(primer.split_whitespace().count() < 600);
        for leak in [
            "http://",
            "https://",
            ".svc",
            "Bearer",
            "joinedcontext.com",
            "/api/",
        ] {
            assert!(!primer.contains(leak), "the primer carries {leak}");
        }
        let named = crate::mcp::server::initialized(None);
        assert!(named["instructions"]
            .as_str()
            .is_some_and(|text| text.ends_with(primer)));
    }

    /// The broker's types: `type` of a query is a list, a cursor is text, a single entity's id
    /// is the URN (Antares `schema.rs`), so the published schema says what the view takes.
    #[test]
    fn a_published_schema_has_the_broker_s_types() {
        let schema = |name: &str| {
            TOOLS
                .iter()
                .find(|tool| tool.name == name)
                .expect("the tool")
                .input_schema()
        };
        let query = schema("query_entities");
        assert_eq!(query["properties"]["type"]["type"], "array");
        assert_eq!(query["properties"]["cursor"]["type"], "string");
        assert_eq!(query["additionalProperties"], false);
        assert!(query.get("required").is_none());
        let batch = schema("batch_query");
        assert_eq!(batch["required"], serde_json::json!(["type"]));
        let upsert = schema("batch_upsert");
        assert_eq!(
            upsert["properties"]["entities"]["items"]["required"],
            serde_json::json!(["id", "type"])
        );
        let subscribe = schema("create_subscription");
        assert_eq!(
            subscribe["properties"]["subscription"]["required"],
            serde_json::json!(["type", "notification"])
        );
        for tool in TOOLS {
            assert!(
                jsonschema::draft7::new(&tool.input_schema()).is_ok(),
                "{} publishes a schema that does not compile",
                tool.name
            );
        }
    }

    /// A tool that stands for no operation describes the endpoint rather than its data, and
    /// describing is reading (EP-55).
    #[test]
    fn a_tool_with_no_operation_of_its_own_reads() {
        for tool in TOOLS.iter().filter(|tool| tool.operations.is_empty()) {
            assert!(tool.read_only(), "{}", tool.name);
        }
    }

    /// The rule the task proposed, read-only when *any* operation reads, would announce a mixed
    /// tool as a read. This is the case that tells the two rules apart.
    #[test]
    fn a_tool_that_both_reads_and_writes_is_not_read_only() {
        let mixed = Tool {
            name: "read_and_write",
            operations: &[Operation::QueryEntity, Operation::UpsertBatch],
            description: "",
            shape: Shape::Own(|| serde_json::json!({})),
            result_key: "entities",
        };
        assert!(!mixed.read_only());
    }
}
