//! The charts of an Endpoint's dataset: `spec.catalog.visualizations` (ADR-N-053, EP-130).
//!
//! A visualization is a Vega-Lite 5 or Vega 5 specification, JSON and never code, over one of
//! the Endpoint's own sources. It is accepted only when it validates against the pinned JSON
//! Schema and holds nothing on the refusal list: no data from anywhere but `source`, no
//! `usermeta` or loader options, no link or image that can leave the platform's hosts, at most
//! 200 KB. Each refusal names the JSON pointer of the member from the manifest root and the rule.
//!
//! The schemas are vendored, minified, from the npm packages the renderer pins (T-4641):
//! `data/vega-lite-v5.23.0.schema.json` from vega-lite 5.23.0 `build/vega-lite-schema.json`
//! (sha256 1aeeda8aa44dcce60f6b6fb7d8b650487e3e389a8284a2040e590be9a4c7610e) and
//! `data/vega-v5.33.1.schema.json` from vega 5.33.1 `build/vega-schema.json`
//! (sha256 173a2d512cf0d8bc841bc90ce574f0526f710e4762820e6ca68bf86e35595920).

use crate::error::{Error, Result};
use crate::kinds::catalog::Texts;
use crate::kinds::endpoint::Representation;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::OnceLock;

/// At most this many visualizations on one Endpoint (EP-130).
pub const MAX_VISUALIZATIONS: usize = 20;
/// At most this many bytes of one spec, as compact JSON (EP-130).
pub const MAX_SPEC_BYTES: usize = 200 * 1024;
/// The longest `last` a `temporal` source may read: 31 days (EP-130).
const MAX_LAST_SECONDS: u64 = 31 * 86_400;

const VEGA_LITE_SCHEMA: &str = include_str!("../../data/vega-lite-v5.23.0.schema.json");
const VEGA_SCHEMA: &str = include_str!("../../data/vega-v5.33.1.schema.json");
const TEMPLATES: &str = include_str!("../../data/visualization-templates.json");

/// One chart on the dataset (EP-130).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Visualization {
    /// Unique on the Endpoint, a DNS label: the CKAN view is found again by it.
    pub name: String,
    /// The view's title per language.
    pub title: Texts,
    /// The view's description per language.
    pub description: Texts,
    /// Which of the Endpoint's own sources fills the spec's dataset `source`.
    pub data: VisualizationData,
    /// The Vega-Lite 5 or Vega 5 specification; it reads `{"data": {"name": "source"}}`.
    #[schemars(with = "serde_json::Map<String, Value>")]
    pub spec: Value,
}

/// The Endpoint source a visualization reads, never a URL (ADR-N-053 §2.2).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "source", rename_all = "lowercase", deny_unknown_fields)]
pub enum VisualizationData {
    /// The Endpoint's `file.csv` of one entity type.
    Csv {
        /// The entity type.
        #[serde(rename = "type")]
        entity_type: String,
    },
    /// The items of one OGC API Features collection.
    Ogc {
        /// The collection, the entity type.
        collection: String,
        /// A CQL2 text filter.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        filter: Option<String>,
        /// The properties to return, comma-separated.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        properties: Option<String>,
    },
    /// One OData entity set.
    Odata {
        /// The entity set, the entity type.
        #[serde(rename = "entitySet")]
        entity_set: String,
        /// `$filter`.
        #[serde(rename = "$filter", default, skip_serializing_if = "Option::is_none")]
        filter: Option<String>,
        /// `$select`.
        #[serde(rename = "$select", default, skip_serializing_if = "Option::is_none")]
        select: Option<String>,
        /// `$apply`.
        #[serde(rename = "$apply", default, skip_serializing_if = "Option::is_none")]
        apply: Option<String>,
        /// `$orderby`.
        #[serde(rename = "$orderby", default, skip_serializing_if = "Option::is_none")]
        orderby: Option<String>,
    },
    /// NGSI-LD Query Temporal Evolution of Entities over the last `last`.
    Temporal {
        /// The entity type.
        #[serde(rename = "type")]
        entity_type: String,
        /// The attributes whose history is read.
        attrs: Vec<String>,
        /// An ISO 8601 duration of at most `P31D`.
        last: String,
        /// The NGSI-LD aggregation method.
        #[serde(
            rename = "aggrMethod",
            default,
            skip_serializing_if = "Option::is_none"
        )]
        aggr_method: Option<AggrMethod>,
        /// The NGSI-LD aggregation period, an ISO 8601 duration.
        #[serde(
            rename = "aggrPeriod",
            default,
            skip_serializing_if = "Option::is_none"
        )]
        aggr_period: Option<String>,
    },
}

/// NGSI-LD's aggregation methods (ETSI GS CIM 009 §4.5.19).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum AggrMethod {
    /// `totalCount`.
    TotalCount,
    /// `distinctCount`.
    DistinctCount,
    /// `sum`.
    Sum,
    /// `avg`.
    Avg,
    /// `min`.
    Min,
    /// `max`.
    Max,
    /// `stddev`.
    Stddev,
    /// `sumsq`.
    Sumsq,
}

impl VisualizationData {
    /// The representation of the Endpoint this source reads; `temporal` is NGSI-LD's own.
    pub fn representation(&self) -> Representation {
        match self {
            Self::Csv { .. } => Representation::Csv,
            Self::Ogc { .. } => Representation::OgcFeatures,
            Self::Odata { .. } => Representation::Odata,
            Self::Temporal { .. } => Representation::NgsiLd,
        }
    }
}

/// Why a spec is refused: the rule and the member it names (ADR-N-053 §2.3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refusal {
    /// The JSON pointer of the member inside the spec.
    pub pointer: String,
    /// The rule: `schema`, `too-large`, `data-source`, `usermeta`, `loader`, `href` or `url`.
    pub rule: &'static str,
    /// What is wrong, one English sentence.
    pub reason: String,
}

/// Validates the list against EP-130: the count, unique names, texts, each source against what
/// the Endpoint serves, and each spec.
pub fn validate_list(items: &[Visualization], served: &[Representation]) -> Result<()> {
    const AT: &str = "/spec/catalog/visualizations";
    if items.len() > MAX_VISUALIZATIONS {
        return Err(invalid(
            AT,
            "too-many",
            format!("at most {MAX_VISUALIZATIONS} visualizations on one Endpoint"),
        ));
    }
    let mut names = BTreeSet::new();
    for (i, item) in items.iter().enumerate() {
        let at = format!("{AT}/{i}");
        if !is_label(&item.name) {
            return Err(invalid(
                &format!("{at}/name"),
                "name",
                format!(
                    "`{}` is not a DNS label of at most 63 characters",
                    item.name
                ),
            ));
        }
        if !names.insert(item.name.as_str()) {
            return Err(invalid(
                &format!("{at}/name"),
                "name",
                format!("`{}` is listed twice", item.name),
            ));
        }
        texts(&format!("{at}/title"), &item.title)?;
        texts(&format!("{at}/description"), &item.description)?;
        source(&format!("{at}/data"), &item.data, served)?;
        check_spec(&item.spec)
            .map_err(|r| invalid(&format!("{at}/spec{}", r.pointer), r.rule, r.reason))?;
    }
    Ok(())
}

/// Checks one spec: its size, the refusal list, then the pinned JSON Schema its `$schema` names.
pub fn check_spec(spec: &Value) -> std::result::Result<(), Refusal> {
    let refuse = |pointer: &str, rule, reason: String| Refusal {
        pointer: pointer.to_owned(),
        rule,
        reason,
    };
    let Value::Object(top) = spec else {
        return Err(refuse("", "schema", "a spec is a JSON object".into()));
    };
    let size = serde_json::to_vec(spec).map_or(usize::MAX, |b| b.len());
    if size > MAX_SPEC_BYTES {
        return Err(refuse(
            "",
            "too-large",
            format!("{size} bytes, at most {MAX_SPEC_BYTES}"),
        ));
    }
    walk(spec, "")?;
    let grammar = top.get("$schema").and_then(Value::as_str).and_then(grammar);
    let Some(validator) = grammar.map(validator) else {
        return Err(refuse(
            "/$schema",
            "schema",
            "name https://vega.github.io/schema/vega-lite/v5.json or …/vega/v5.json".into(),
        ));
    };
    if let Some(error) = validator.iter_errors(spec).next() {
        let error = deepest(&error);
        return Err(refuse(
            &error.instance_path().to_string(),
            "schema",
            error.to_string(),
        ));
    }
    Ok(())
}

/// The error under an `anyOf` or `oneOf` that reaches deepest into the spec: Vega-Lite's top
/// level is an `anyOf`, which alone would name the whole spec for a wrong `mark`.
fn deepest<'a>(error: &'a jsonschema::ValidationError<'a>) -> &'a jsonschema::ValidationError<'a> {
    use jsonschema::error::ValidationErrorKind::{AnyOf, OneOfNotValid};
    let (AnyOf { context } | OneOfNotValid { context }) = error.kind() else {
        return error;
    };
    context
        .iter()
        .flatten()
        .map(deepest)
        .max_by_key(|e| e.instance_path().to_string().matches('/').count())
        .filter(|e| e.instance_path().to_string().len() > error.instance_path().to_string().len())
        .unwrap_or(error)
}

/// The template `name` with each role placeholder (`{x}`, `{y}`, `{color}`, `{facet}`) set to the
/// column given for it; an encoding or facet whose optional role has no column is dropped. `None`
/// for an unknown template or a required role left out.
pub fn template(name: &str, columns: &BTreeMap<String, String>) -> Option<Value> {
    let all: Value = serde_json::from_str(TEMPLATES).ok()?;
    let entry = all.get(name)?;
    for role in entry.get("required")?.as_array()? {
        if !columns.contains_key(role.as_str()?) {
            return None;
        }
    }
    let mut spec = entry.get("spec")?.clone();
    fill(&mut spec, columns);
    Some(spec)
}

fn fill(value: &mut Value, columns: &BTreeMap<String, String>) {
    match value {
        Value::Object(map) => {
            map.retain(|_, v| unfilled(v, columns).is_none());
            map.values_mut().for_each(|v| fill(v, columns));
        }
        Value::Array(items) => {
            items.retain(|v| unfilled(v, columns).is_none());
            items.iter_mut().for_each(|v| fill(v, columns));
        }
        Value::String(s) => {
            if let Some(column) = placeholder(s).and_then(|role| columns.get(role)) {
                *s = column.clone();
            }
        }
        _ => {}
    }
}

/// The role of an object whose `field` is a placeholder no column fills: it is dropped.
fn unfilled<'a>(value: &'a Value, columns: &BTreeMap<String, String>) -> Option<&'a str> {
    let role = value.get("field")?.as_str().and_then(placeholder)?;
    (!columns.contains_key(role)).then_some(role)
}

fn placeholder(s: &str) -> Option<&str> {
    s.strip_prefix('{')?.strip_suffix('}')
}

#[derive(Clone, Copy)]
enum Grammar {
    VegaLite,
    Vega,
}

fn grammar(schema: &str) -> Option<Grammar> {
    let rest = schema
        .strip_prefix("https://vega.github.io/schema/")
        .and_then(|rest| rest.strip_suffix(".json"))?;
    let (name, version) = rest.split_once('/')?;
    let major = version.strip_prefix('v')?.split('.').next()?;
    match (name, major) {
        ("vega-lite", "5") => Some(Grammar::VegaLite),
        ("vega", "5") => Some(Grammar::Vega),
        _ => None,
    }
}

fn validator(grammar: Grammar) -> &'static jsonschema::Validator {
    static VEGA_LITE: OnceLock<jsonschema::Validator> = OnceLock::new();
    static VEGA: OnceLock<jsonschema::Validator> = OnceLock::new();
    let (cell, text) = match grammar {
        Grammar::VegaLite => (&VEGA_LITE, VEGA_LITE_SCHEMA),
        Grammar::Vega => (&VEGA, VEGA_SCHEMA),
    };
    cell.get_or_init(|| {
        let mut schema: Value = serde_json::from_str(text).expect("the vendored schema is JSON");
        encode_refs(&mut schema);
        jsonschema::draft7::new(&schema).expect("the vendored schema compiles")
    })
}

/// Vega-Lite names definitions like `MarkPropDef<(Gradient|string|null)>`, and its `$ref`s carry
/// those names raw, which is no URI reference; percent-encoded, they resolve to the same member.
fn encode_refs(value: &mut Value) {
    match value {
        Value::Object(map) => {
            for (key, member) in map.iter_mut() {
                match member {
                    Value::String(r) if key == "$ref" => {
                        let mut out = String::with_capacity(r.len());
                        for c in r.chars() {
                            if c.is_ascii_alphanumeric() || "#-._~!$&'()*+,;=:@/?%".contains(c) {
                                out.push(c);
                            } else {
                                let mut buf = [0u8; 4];
                                for b in c.encode_utf8(&mut buf).bytes() {
                                    out.push_str(&format!("%{b:02X}"));
                                }
                            }
                        }
                        *r = out;
                    }
                    other => encode_refs(other),
                }
            }
        }
        Value::Array(items) => items.iter_mut().for_each(encode_refs),
        _ => {}
    }
}

/// The refusal list over every member (ADR-N-053 §2.3), depth first.
fn walk(value: &Value, at: &str) -> std::result::Result<(), Refusal> {
    match value {
        Value::Object(map) => {
            for (key, member) in map {
                let here = format!("{at}/{}", escape(key));
                match key.as_str() {
                    "usermeta" => return Err(refusal(&here, "usermeta", "no usermeta")),
                    "loader" => return Err(refusal(&here, "loader", "no loader options")),
                    "data" => data(member, &here)?,
                    "href" => {
                        return Err(refusal(&here, "href", "no links out of the chart"));
                    }
                    "url" => image(member, &here)?,
                    // Inline rows are data, not spec: a column named `url` is no fetch.
                    "values" => continue,
                    _ => {}
                }
                walk(member, &here)?;
            }
        }
        Value::Array(items) => {
            for (i, item) in items.iter().enumerate() {
                walk(item, &format!("{at}/{i}"))?;
            }
        }
        _ => {}
    }
    Ok(())
}

/// A `data` member: a dataset's name (Vega's `from`), Vega's list of datasets, or one dataset
/// that is `source`, inline `values`, a generator or a dataset derived from another, never a URL.
fn data(member: &Value, at: &str) -> std::result::Result<(), Refusal> {
    let one = |dataset: &Value, at: &str| match dataset {
        Value::Object(map) if map.contains_key("url") => Err(refusal(
            &format!("{at}/url"),
            "data-source",
            "the rows come from the Endpoint's declared source, never a URL",
        )),
        Value::Object(map) => {
            let named = map.get("name").and_then(Value::as_str);
            let fed = ["values", "source", "sequence", "sphere", "graticule"]
                .iter()
                .any(|k| map.contains_key(*k));
            if named.is_some_and(|name| name != "source") && !fed {
                Err(refusal(
                    &format!("{at}/name"),
                    "data-source",
                    "the one named dataset is `source`; others hold inline values or derive from it",
                ))
            } else {
                Ok(())
            }
        }
        _ => Ok(()),
    };
    match member {
        Value::Array(sets) => sets
            .iter()
            .enumerate()
            .try_for_each(|(i, set)| one(set, &format!("{at}/{i}"))),
        other => one(other, at),
    }
}

/// A `url` outside `data`, an image mark's: only a literal `data:image/…` URI, as a string or
/// `{"value": …}`. Anything computed (`field`, `signal`, an expression) could take any value.
// ponytail: links (`href`) and image URLs on the platform's own hosts, which ADR-N-053 §2.3 would
// allow, are refused too, since a manifest is validated without the deployment's domain; pass the
// domain in here when a city asks for a linked chart.
fn image(member: &Value, at: &str) -> std::result::Result<(), Refusal> {
    let text = match member {
        Value::String(s) => Some(s.as_str()),
        Value::Object(map) if map.len() == 1 => map.get("value").and_then(Value::as_str),
        _ => None,
    };
    match text {
        Some(uri) if uri.starts_with("data:image/") => Ok(()),
        _ => Err(refusal(at, "url", "an image is a literal data:image/ URI")),
    }
}

fn source(at: &str, data: &VisualizationData, served: &[Representation]) -> Result<()> {
    let wanted = data.representation();
    if !served.contains(&wanted) {
        return Err(invalid(
            &format!("{at}/source"),
            "source",
            format!("the Endpoint does not serve {wanted:?}"),
        ));
    }
    let name = |field: &str, value: &str| {
        if crate::names::validate_entity_type(value).is_ok() {
            Ok(())
        } else {
            Err(invalid(
                &format!("{at}/{field}"),
                "source",
                format!("`{value}` is not an entity type"),
            ))
        }
    };
    match data {
        VisualizationData::Csv { entity_type } => name("type", entity_type),
        VisualizationData::Ogc { collection, .. } => name("collection", collection),
        VisualizationData::Odata { entity_set, .. } => name("entitySet", entity_set),
        VisualizationData::Temporal {
            entity_type,
            attrs,
            last,
            aggr_method,
            aggr_period,
        } => {
            name("type", entity_type)?;
            if attrs.is_empty() || attrs.iter().any(|a| !is_attribute(a)) {
                return Err(invalid(
                    &format!("{at}/attrs"),
                    "source",
                    "name one attribute or more".into(),
                ));
            }
            match seconds(last) {
                Some(s) if s > 0 && s <= MAX_LAST_SECONDS => {}
                _ => {
                    return Err(invalid(
                        &format!("{at}/last"),
                        "source",
                        format!("`{last}` is not an ISO 8601 duration up to P31D"),
                    ))
                }
            }
            match (aggr_method, aggr_period) {
                (None, Some(_)) => Err(invalid(
                    &format!("{at}/aggrPeriod"),
                    "source",
                    "an aggrPeriod needs an aggrMethod".into(),
                )),
                (_, Some(period)) if seconds(period).is_none_or(|s| s == 0) => Err(invalid(
                    &format!("{at}/aggrPeriod"),
                    "source",
                    format!("`{period}` is not an ISO 8601 duration"),
                )),
                _ => Ok(()),
            }
        }
    }
}

/// Seconds of an ISO 8601 duration of weeks, days, hours, minutes and seconds (`P1W`, `P1DT12H`,
/// `PT90M`); years and months have no fixed length and are not taken.
fn seconds(duration: &str) -> Option<u64> {
    let mut rest = duration.strip_prefix('P')?;
    let mut total = 0u64;
    let mut time = false;
    let mut any = false;
    while !rest.is_empty() {
        if let Some(r) = rest.strip_prefix('T') {
            if time || r.is_empty() {
                return None;
            }
            time = true;
            rest = r;
            continue;
        }
        let digits = rest.find(|c: char| !c.is_ascii_digit())?;
        let n: u64 = rest[..digits].parse().ok()?;
        let unit = match (time, rest[digits..].chars().next()?) {
            (false, 'W') => 604_800,
            (false, 'D') => 86_400,
            (true, 'H') => 3_600,
            (true, 'M') => 60,
            (true, 'S') => 1,
            _ => return None,
        };
        total = total.checked_add(n.checked_mul(unit)?)?;
        any = true;
        rest = &rest[digits + 1..];
    }
    any.then_some(total)
}

fn is_label(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 63
        && name.starts_with(|c: char| c.is_ascii_lowercase())
        && !name.ends_with('-')
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

fn is_attribute(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 256
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "_-.:".contains(c))
}

fn texts(at: &str, map: &Texts) -> Result<()> {
    if map.is_empty() || map.values().any(|t| t.trim().is_empty()) {
        return Err(invalid(
            at,
            "text",
            "give the text in each language listed, at least one".into(),
        ));
    }
    map.keys()
        .try_for_each(|l| crate::names::validate_locale(l))
}

fn escape(key: &str) -> String {
    key.replace('~', "~0").replace('/', "~1")
}

fn refusal(at: &str, rule: &'static str, reason: &str) -> Refusal {
    Refusal {
        pointer: at.to_owned(),
        rule,
        reason: reason.to_owned(),
    }
}

fn invalid(at: &str, rule: &str, reason: String) -> Error {
    Error::Invalid {
        field: at.to_owned(),
        reason: format!("{rule}: {reason}"),
    }
}
