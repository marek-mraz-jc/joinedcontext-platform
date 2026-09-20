//! NGSI-LD as OGC SensorThings API v1.1, Sensing profile, read only (T-0160, EP-12, EP-13).
//!
//! An NGSI-LD entity carries its measurements as its own attributes. SensorThings splits the
//! same information across four linked entity sets, so one entity becomes one `Thing`, one
//! `Location`, and one `Datastream` and one `ObservedProperty` per measured attribute. Nothing
//! is stored twice and nothing is looked up twice: every set below is a view of the same
//! projected entity page, which is why a policy cannot be softer here than on the canonical
//! surface (EP-06, EP-07).
//!
//! Identifiers are the NGSI-LD identity itself rather than numbers the gateway would have to
//! keep a table for. A `Thing` is the entity URN, a `Datastream` is `{urn}/{attribute}` and an
//! `Observation` is `{urn}/{attribute}/{phenomenonTime}`. So a link a client saved a year ago
//! still resolves, across a restart and across a broker swap. v1.1 permits string ids.
//!
//! What cannot be expressed is absent rather than faked: an attribute with no numeric value is
//! not a datastream, and the sets the platform has no data for answer an empty `value` rather
//! than an invented one (EP-13).

use serde_json::{json, Map, Value};

/// SensorThings answers plain JSON; the OData annotations carry the typing.
pub const MEDIA_TYPE: &str = "application/json";

/// The entity sets of the Sensing profile, in the order the service document lists them.
pub const SETS: [&str; 8] = [
    "Things",
    "Locations",
    "HistoricalLocations",
    "Datastreams",
    "Sensors",
    "Observations",
    "ObservedProperties",
    "FeaturesOfInterest",
];

/// The sets the platform holds no data of its own for (EP-13).
///
/// A municipal context space records what was measured, not the hardware that measured it and
/// not a feature of interest separate from the thing itself. Answering an empty collection is
/// the honest shape: the set exists, a conformance suite can walk it, and it contains nothing.
pub const EMPTY_SETS: [&str; 3] = ["Sensors", "FeaturesOfInterest", "HistoricalLocations"];

/// The default and largest page an STA request may ask for.
pub const DEFAULT_TOP: usize = 100;

/// A `$filter` this translator cannot compile into an NGSI-LD query.
///
/// Named, because a filter that is silently ignored returns more rows than the caller asked
/// for, and a client that is told only "400" retries the same request.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("$filter: {0}")]
pub struct FilterError(pub String);

/// The STA root of one endpoint.
fn root(endpoint: &str) -> String {
    format!("{endpoint}/sta/v1.1")
}

/// One entity's self link, with the id in the parentheses OData puts it in.
fn self_link(endpoint: &str, set: &str, id: &str) -> String {
    format!("{}/{set}('{}')", root(endpoint), escape(id))
}

/// A single quote inside an OData key literal is written twice, which is the only escape the
/// grammar has. Our ids are URNs and attribute names, so this is a guard and not a hot path.
fn escape(id: &str) -> String {
    id.replace('\'', "''")
}

/// The service document: the sets this service has, whether or not they hold anything.
pub fn service_document(endpoint: &str) -> Value {
    let root = root(endpoint);
    let sets: Vec<Value> = SETS
        .iter()
        .map(|name| json!({ "name": name, "url": format!("{root}/{name}") }))
        .collect();
    json!({ "value": sets, "serverSettings": { "conformance": conformance() } })
}

/// The conformance classes of the Sensing profile this read surface satisfies.
fn conformance() -> Vec<&'static str> {
    vec![
        "http://www.opengis.net/spec/iot_sensing/1.1/req/datamodel",
        "http://www.opengis.net/spec/iot_sensing/1.1/req/resource-path",
        "http://www.opengis.net/spec/iot_sensing/1.1/req/request-data",
    ]
}

/// A collection answer: `value`, the count when asked for, and the link to the next page.
pub fn collection(items: Vec<Value>, count: Option<usize>, next: Option<String>) -> Value {
    let mut answer = Map::new();
    if let Some(count) = count {
        answer.insert("@iot.count".to_owned(), json!(count));
    }
    answer.insert("value".to_owned(), Value::Array(items));
    if let Some(next) = next {
        answer.insert("@iot.nextLink".to_owned(), json!(next));
    }
    Value::Object(answer)
}

/// One entity as a `Thing` (EP-12).
///
/// `name` and `description` come from the attributes a Smart Data Model uses for them, and
/// fall back to the URN, because a `Thing` without a name is not a valid `Thing` and an empty
/// string tells the reader less than the identifier does.
pub fn thing(endpoint: &str, entity: &Value, expand: &[&str]) -> Option<Value> {
    let id = entity.get("id").and_then(Value::as_str)?;
    let link = self_link(endpoint, "Things", id);
    let mut thing = Map::new();
    thing.insert("@iot.id".to_owned(), json!(id));
    thing.insert("@iot.selfLink".to_owned(), json!(link));
    thing.insert("name".to_owned(), json!(text(entity, "name").unwrap_or(id)));
    thing.insert(
        "description".to_owned(),
        json!(text(entity, "description").unwrap_or_default()),
    );
    // Everything that is not a measurement still describes the thing, so it goes where STA
    // puts what it has no slot for.
    thing.insert("properties".to_owned(), properties(entity));

    for (set, inline) in [
        ("Locations", expand.contains(&"Locations")),
        ("Datastreams", expand.contains(&"Datastreams")),
    ] {
        if inline {
            let items = match set {
                "Locations" => location(endpoint, entity).into_iter().collect(),
                _ => datastreams(endpoint, entity),
            };
            thing.insert(format!("{set}@iot.count"), json!(items.len()));
            thing.insert(set.to_owned(), Value::Array(items));
        } else {
            thing.insert(
                format!("{set}@iot.navigationLink"),
                json!(format!("{link}/{set}")),
            );
        }
    }
    Some(Value::Object(thing))
}

/// The entity's `location` as a `Location`, or nothing when it declares none (EP-13).
pub fn location(endpoint: &str, entity: &Value) -> Option<Value> {
    let id = entity.get("id").and_then(Value::as_str)?;
    let geometry = entity.get("location")?;
    let geometry = geometry.get("value").unwrap_or(geometry);
    geometry.get("coordinates").or(geometry.get("geometries"))?;
    Some(json!({
        "@iot.id": format!("{id}/location"),
        "@iot.selfLink": self_link(endpoint, "Locations", &format!("{id}/location")),
        "name": "location",
        "description": "The entity's primary GeoProperty",
        "encodingType": "application/geo+json",
        "location": geometry,
    }))
}

/// One `Datastream` per measured attribute of the entity (EP-12).
pub fn datastreams(endpoint: &str, entity: &Value) -> Vec<Value> {
    measurements(entity)
        .into_iter()
        .map(|(name, attribute)| {
            let id = format!("{}/{name}", entity["id"].as_str().unwrap_or_default());
            let unit = attribute.get("unitCode").and_then(Value::as_str);
            json!({
                "@iot.id": id,
                "@iot.selfLink": self_link(endpoint, "Datastreams", &id),
                "name": name,
                "description": format!("{name} of {}", entity["id"].as_str().unwrap_or_default()),
                "observationType": "http://www.opengis.net/def/observationType/OGC-OM/2.0/OM_Measurement",
                "unitOfMeasurement": {
                    "name": unit.unwrap_or("unknown"),
                    "symbol": unit.unwrap_or(""),
                    "definition": unit.map_or_else(String::new, |code|
                        format!("https://vocabulary.uncefact.org/UnitMeasureCode#{code}")),
                },
                "Observations@iot.navigationLink":
                    format!("{}/Observations", self_link(endpoint, "Datastreams", &id)),
                "Thing@iot.navigationLink":
                    self_link(endpoint, "Things", entity["id"].as_str().unwrap_or_default()),
            })
        })
        .collect()
}

/// One `Observation` per measured attribute: the instant of it this entity holds (EP-12).
///
/// An NGSI-LD entity is the current state, so one attribute is one observation here. The
/// history of it is [`temporal_observations`], which the `Observations` of one datastream come
/// from; this is what the sets that page over entities can offer without a second hop each.
pub fn observations(endpoint: &str, entity: &Value) -> Vec<Value> {
    let urn = entity["id"].as_str().unwrap_or_default();
    let fallback = entity.get("observedAt").and_then(Value::as_str);
    measurements(entity)
        .into_iter()
        .map(|(name, attribute)| observation(endpoint, urn, name, attribute, fallback))
        .collect()
}

/// The `Observations` of one datastream: the history the broker holds for that attribute
/// (T-0438, EP-12).
///
/// The temporal representation carries every attribute as an array of instances rather than
/// one value, so one attribute is a series here where it is a single point on the current
/// state. An entity that carries the attribute as a single instance is still a series of one:
/// a broker that answers the current state to a temporal request is not a reason to answer
/// nothing.
///
/// Only the named attribute is read. The rest of the entity is the caller's too — the
/// projection already ran — but a datastream is one attribute, and returning its siblings
/// under its own id would put observations in a series they do not belong to.
pub fn temporal_observations(endpoint: &str, entity: &Value, attribute: &str) -> Vec<Value> {
    let urn = entity.get("id").and_then(Value::as_str).unwrap_or_default();
    let fallback = entity.get("observedAt").and_then(Value::as_str);
    let Some(instances) = entity.get(attribute) else {
        return Vec::new();
    };
    match instances {
        Value::Array(series) => series
            .iter()
            .filter(|instance| instance.get("value").is_some())
            .map(|instance| observation(endpoint, urn, attribute, instance, fallback))
            .collect(),
        single if single.get("value").is_some() => {
            vec![observation(endpoint, urn, attribute, single, fallback)]
        }
        _ => Vec::new(),
    }
}

/// One instance of one attribute as an `Observation`.
///
/// `phenomenonTime` is the instance's own `observedAt` and falls back to the entity's, because
/// an observation without a time is not one a client can chart. The id is
/// `{urn}/{attribute}/{observedAt}`, which is the identity the docs promise and stays
/// resolvable across a restart.
fn observation(
    endpoint: &str,
    urn: &str,
    name: &str,
    attribute: &Value,
    fallback: Option<&str>,
) -> Value {
    let phenomenon = attribute
        .get("observedAt")
        .and_then(Value::as_str)
        .or(fallback);
    let stream = format!("{urn}/{name}");
    let id = phenomenon.map_or_else(|| stream.clone(), |at| format!("{stream}/{at}"));
    let mut observation = Map::new();
    observation.insert("@iot.id".to_owned(), json!(id));
    observation.insert(
        "@iot.selfLink".to_owned(),
        json!(self_link(endpoint, "Observations", &id)),
    );
    observation.insert("result".to_owned(), attribute["value"].clone());
    if let Some(at) = phenomenon {
        observation.insert("phenomenonTime".to_owned(), json!(at));
    }
    if let Some(at) = attribute.get("modifiedAt").and_then(Value::as_str) {
        observation.insert("resultTime".to_owned(), json!(at));
    }
    observation.insert(
        "Datastream@iot.navigationLink".to_owned(),
        json!(self_link(endpoint, "Datastreams", &stream)),
    );
    Value::Object(observation)
}

/// One `ObservedProperty` per measured attribute (EP-12).
pub fn observed_properties(endpoint: &str, entity: &Value) -> Vec<Value> {
    let urn = entity["id"].as_str().unwrap_or_default();
    measurements(entity)
        .into_iter()
        .map(|(name, _)| {
            let id = format!("{urn}/{name}");
            json!({
                "@iot.id": id,
                "@iot.selfLink": self_link(endpoint, "ObservedProperties", &id),
                "name": name,
                // The slot IRI of the attribute in the space's own model namespace; a client
                // resolves it against the endpoint's `@context`, which is the only place the
                // term is actually defined.
                "definition": format!("{endpoint}/schema/index.json#{name}"),
                "description": format!("{name} as measured by {urn}"),
            })
        })
        .collect()
}

/// The attributes that are measurements: a numeric `Property` value (EP-13).
///
/// A relationship, a string, a geometry and the JSON-LD keywords are not observations of
/// anything, and a `Datastream` built over one would answer `result: null` forever.
fn measurements(entity: &Value) -> Vec<(&str, &Value)> {
    let Some(members) = entity.as_object() else {
        return Vec::new();
    };
    members
        .iter()
        .filter(|(name, _)| !matches!(name.as_str(), "id" | "type" | "@id" | "@type" | "@context"))
        .filter(|(_, value)| value.get("value").is_some_and(Value::is_number))
        .map(|(name, value)| (name.as_str(), value))
        .collect()
}

/// The entity's non-measurement attributes, flattened, plus its NGSI-LD type.
fn properties(entity: &Value) -> Value {
    let mut properties = Map::new();
    if let Some(kind) = entity.get("type").and_then(Value::as_str) {
        properties.insert("type".to_owned(), json!(kind));
    }
    for (name, value) in entity.as_object().into_iter().flatten() {
        if matches!(
            name.as_str(),
            "id" | "type" | "@id" | "@type" | "@context" | "location"
        ) {
            continue;
        }
        if value.get("value").is_some_and(Value::is_number) {
            continue;
        }
        let flat = value
            .get("value")
            .or_else(|| value.get("object"))
            .unwrap_or(value);
        properties.insert(name.clone(), flat.clone());
    }
    Value::Object(properties)
}

/// One string attribute of an entity, for the fields STA insists every entity has.
fn text<'a>(entity: &'a Value, name: &str) -> Option<&'a str> {
    let attribute = entity.get(name)?;
    attribute
        .get("value")
        .unwrap_or(attribute)
        .as_str()
        .filter(|text| !text.is_empty())
}

/// The self link of one `Datastream`, which its `Observations` hang off.
pub fn datastream_link(endpoint: &str, id: &str) -> String {
    self_link(endpoint, "Datastreams", id)
}

/// The entity URN and the attribute of a `Datastream` id, which is `{urn}/{attribute}`.
///
/// A URN has no slash, so the split is the first one. An id that is not shaped this way names
/// no datastream, which is a 404 and never a query.
pub fn split_stream_id(id: &str) -> Option<(&str, &str)> {
    let (urn, attribute) = id.split_once('/')?;
    if !urn.starts_with("urn:") || attribute.is_empty() || attribute.contains('/') {
        return None;
    }
    Some((urn, attribute))
}

/// The entity URN of any STA id: a `Thing`'s is the whole id, the rest carry it in front.
pub fn urn_of(id: &str) -> Option<&str> {
    if !id.starts_with("urn:") {
        return None;
    }
    Some(id.split_once('/').map_or(id, |(urn, _)| urn))
}

/// A `$filter` on a series, split into the part NGSI-LD carries as a window and the rest
/// (T-0438).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Series {
    /// `timerel`, `timeAt` and possibly `endTimeAt`, from the `phenomenonTime` predicates.
    pub window: Vec<(String, String)>,
    /// Everything else, as the NGSI-LD `q`.
    pub q: Option<String>,
}

/// The `$filter` of an `Observations` request (T-0438).
///
/// `phenomenonTime` is not an attribute of the entity: it is the instant of an instance, and
/// NGSI-LD selects on it with a temporal window rather than with `q`. So the predicates naming
/// it are pulled out and become that window, and the rest compiles as any other filter does.
///
/// A `phenomenonTime` predicate is only expressible on the top-level `and` spine, because the
/// window applies to the whole request. One inside a parenthesis or an `or` is refused by name
/// rather than applied to both branches of a disjunction it was never in.
pub fn series_filter(filter: &str) -> Result<Series, FilterError> {
    const TIME: &str = "phenomenontime";
    let mut window: Vec<(String, String)> = Vec::new();
    let mut start: Option<String> = None;
    let mut end: Option<String> = None;
    let mut rest: Vec<&str> = Vec::new();

    for term in conjuncts(filter) {
        let lowered = term.to_ascii_lowercase();
        if !lowered.contains(TIME) {
            rest.push(term);
            continue;
        }
        if !lowered.trim_start().starts_with(TIME) {
            return Err(FilterError(
                "phenomenonTime selects the window of the whole request, so it cannot sit \
                 inside a parenthesis or an or"
                    .to_owned(),
            ));
        }
        let mut parts = term.split_whitespace();
        let (Some(_), Some(operator), Some(value)) = (parts.next(), parts.next(), parts.next())
        else {
            return Err(FilterError(
                "a phenomenonTime predicate is `phenomenonTime <op> <instant>`".to_owned(),
            ));
        };
        if parts.next().is_some() {
            return Err(FilterError(
                "a phenomenonTime predicate takes one instant".to_owned(),
            ));
        }
        let instant = instant_of(value)?;
        match operator.to_ascii_lowercase().as_str() {
            "gt" | "ge" => start = Some(instant),
            "lt" | "le" => end = Some(instant),
            "eq" => {
                start = Some(instant.clone());
                end = Some(instant);
            }
            other => {
                return Err(FilterError(format!(
                    "{other} is not supported on phenomenonTime"
                )))
            }
        }
    }

    match (start, end) {
        (Some(start), Some(end)) if start > end => {
            return Err(FilterError("the window ends before it starts".to_owned()))
        }
        (Some(start), Some(end)) => {
            window.push(("timerel".to_owned(), "between".to_owned()));
            window.push(("timeAt".to_owned(), start));
            window.push(("endTimeAt".to_owned(), end));
        }
        (Some(start), None) => {
            window.push(("timerel".to_owned(), "after".to_owned()));
            window.push(("timeAt".to_owned(), start));
        }
        (None, Some(end)) => {
            window.push(("timerel".to_owned(), "before".to_owned()));
            window.push(("timeAt".to_owned(), end));
        }
        (None, None) => {}
    }

    let q = match rest.is_empty() {
        true => None,
        false => Some(filter_to_q(&rest.join(" and "))?),
    };
    Ok(Series { window, q })
}

/// The top-level `and` terms of a filter, with everything inside parentheses left whole.
fn conjuncts(filter: &str) -> Vec<&str> {
    let mut terms = Vec::new();
    let mut depth = 0usize;
    let mut start = 0usize;
    let bytes = filter.as_bytes();
    let mut at = 0usize;
    while at < bytes.len() {
        match bytes[at] {
            b'(' => depth += 1,
            b')' => depth = depth.saturating_sub(1),
            _ if depth == 0 => {
                // ` and ` with its spaces, so an attribute named `brand` is not a connective.
                let tail = &filter[at..];
                if tail.len() >= 5 && tail[..5].eq_ignore_ascii_case(" and ") {
                    terms.push(filter[start..at].trim());
                    at += 5;
                    start = at;
                    continue;
                }
            }
            _ => {}
        }
        at += 1;
    }
    terms.push(filter[start..].trim());
    terms.into_iter().filter(|term| !term.is_empty()).collect()
}

/// One instant of an OData filter, quoted or bare, as RFC 3339.
fn instant_of(value: &str) -> Result<String, FilterError> {
    let text = value.trim().trim_matches('\'');
    chrono::DateTime::parse_from_rfc3339(text)
        .map(|_| text.to_owned())
        .map_err(|_| FilterError(format!("{text} is not an RFC 3339 instant")))
}

/// Whether `$orderby` asks for the newest observation first.
///
/// The one ordering a chart asks for. Any other `$orderby` leaves the series in the order the
/// broker returned it rather than being refused, because an ordering is a presentation detail
/// and refusing one would break a client over something that changes no row.
pub fn newest_first(order_by: Option<&str>) -> bool {
    order_by.is_some_and(|raw| {
        let lowered = raw.to_ascii_lowercase();
        lowered.contains("phenomenontime") && lowered.contains("desc")
    })
}

/// `$filter` compiled to the NGSI-LD `q` the other representations already use.
///
/// The subset is what a SensorThings client actually sends against a read surface: comparison
/// over an attribute name, `and`, `or`, `not` and `substringof`. Everything else is refused
/// with the operator named, because a filter that is dropped silently returns rows the caller
/// asked not to see.
pub fn filter_to_q(filter: &str) -> Result<String, FilterError> {
    let mut out = String::new();
    let mut rest = filter.trim();
    let mut expect_operand = true;

    while !rest.is_empty() {
        rest = rest.trim_start();
        if rest.is_empty() {
            break;
        }
        if let Some(tail) = rest.strip_prefix('(') {
            out.push('(');
            rest = tail;
            expect_operand = true;
            continue;
        }
        if let Some(tail) = rest.strip_prefix(')') {
            out.push(')');
            rest = tail;
            expect_operand = false;
            continue;
        }
        let (token, tail) = match rest.find([' ', '(', ')']) {
            Some(at) if at > 0 => rest.split_at(at),
            _ => (rest, ""),
        };
        rest = tail;

        if expect_operand {
            if let Some(rendered) = substring_call(token, &mut rest)? {
                out.push_str(&rendered);
                expect_operand = false;
                continue;
            }
            if token.eq_ignore_ascii_case("not") {
                return Err(FilterError(
                    "not is not supported; write the negated comparison instead".to_owned(),
                ));
            }
            out.push_str(&operand(token));
            expect_operand = false;
            continue;
        }

        let rendered = match token.to_ascii_lowercase().as_str() {
            "eq" => "==",
            "ne" => "!=",
            "gt" => ">",
            "ge" => ">=",
            "lt" => "<",
            "le" => "<=",
            "and" => ";",
            "or" => "|",
            other => {
                return Err(FilterError(format!(
                    "{other} is not supported by this endpoint"
                )))
            }
        };
        out.push_str(rendered);
        expect_operand = true;
    }

    if out.is_empty() {
        return Err(FilterError("the filter is empty".to_owned()));
    }
    Ok(out)
}

/// `substringof('text',attribute)` as an NGSI-LD pattern match, when the token is one.
fn substring_call(token: &str, rest: &mut &str) -> Result<Option<String>, FilterError> {
    if !token.eq_ignore_ascii_case("substringof") {
        return Ok(None);
    }
    let tail = rest.trim_start();
    let Some(inner) = tail.strip_prefix('(').and_then(|open| open.split_once(')')) else {
        return Err(FilterError(
            "substringof needs (text, attribute)".to_owned(),
        ));
    };
    let (arguments, after) = inner;
    *rest = after;
    let Some((needle, attribute)) = arguments.split_once(',') else {
        return Err(FilterError(
            "substringof needs two arguments: (text, attribute)".to_owned(),
        ));
    };
    let needle = needle.trim().trim_matches('\'');
    let attribute = attribute.trim();
    if needle.is_empty() || attribute.is_empty() {
        return Err(FilterError(
            "substringof needs a non-empty text and an attribute".to_owned(),
        ));
    }
    Ok(Some(format!("{attribute}~=\".*{needle}.*\"")))
}

/// One operand: a quoted literal keeps its quotes as NGSI-LD writes them, a name stays a name.
fn operand(token: &str) -> String {
    if let Some(text) = token.strip_prefix('\'').and_then(|t| t.strip_suffix('\'')) {
        return format!("\"{text}\"");
    }
    // `Datastreams/name` and the like address a navigation property; NGSI-LD has one flat
    // attribute namespace, so the last segment is the name that exists here.
    token.rsplit('/').next().unwrap_or(token).to_owned()
}
