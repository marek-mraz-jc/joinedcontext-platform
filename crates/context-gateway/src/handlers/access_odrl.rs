//! The caller's grants as a W3C ODRL 2.2 policy (T-0164, EP-57, R52, R26, MIM3-R10).
//!
//! The same grants the AuthZEN document reports ([`super::access`]), written in the language a
//! data-space connector negotiates in. Nothing new is computed here: the PDP has already said
//! which policies name this caller and are in force, and this module only changes the words.
//! Two implementations of "what may this caller do" would be the bug (EP-60).
//!
//! The dialect is the `ngsi-ld:` profile of R52: ODRL's own vocabulary for the frame, and
//! profile terms for what ODRL has no word for — CIM 009 operations as actions, entity types
//! as targets, and `q`, `scopeQ`, `geoQ`, `temporalQ` as constraint left operands. A partner
//! that reads plain ODRL still sees the shape of the agreement; one that loads the profile
//! context reads the residual too.
//!
//! **Round-trip (R26).** [`read`] takes the document back apart, and the tests assert that the
//! operations, entity selectors, attribute whitelists, `q` and `scopeQ` survive the journey
//! unchanged — those are the six R26 names. `geoQ` and `temporalQ` are written decomposed, as
//! the geometry and the interval a plain ODRL reader can act on, and are read back from the
//! same pieces.

use crate::pdp::evaluator::{effective, granted_attrs, granted_operations, Subject};
use crate::resolver::Endpoint;
use chrono::{DateTime, Utc};
use jc_core::kinds::PolicySpec;
use serde_json::{json, Map, Value};

/// The media type of the JSON-LD serialization (EP-57).
pub const ODRL_JSON: &str = "application/odrl+json";

/// The media type of the RDF serialization (EP-57).
pub const TURTLE: &str = "text/turtle";

/// The JSON-LD contexts the document is read with: ODRL's own, then the `ngsi-ld:` profile.
pub const CONTEXTS: [&str; 2] = [
    "http://www.w3.org/ns/odrl.jsonld",
    "https://joinedcontext.com/odrl/ngsi-ld/v1/context.jsonld",
];

/// The namespace the profile's terms live in, as Turtle writes it.
pub const PROFILE_NAMESPACE: &str = "https://joinedcontext.com/odrl/ngsi-ld/v1#";

/// The grants of one caller on one endpoint, as an ODRL 2.2 `Set` (EP-57).
///
/// `base` is the gateway's public URL when the deployment names one; the `uid` is that URL of
/// the access document with the digest of the grants after it, so two identical grants have
/// one identifier and a changed policy set has a new one (EP-59).
pub fn policy(
    subject: &Subject,
    endpoint: &Endpoint,
    now: DateTime<Utc>,
    base: &str,
    digest: impl Fn(&[u8]) -> String,
) -> Value {
    let (prohibitions, permissions): (Vec<_>, Vec<_>) = effective(subject, &endpoint.policies, now)
        .into_iter()
        .partition(|policy| policy.effect.is_prohibition());

    let permission = rules(&permissions);
    let prohibition = rules(&prohibitions);

    let mut document = Map::new();
    document.insert("@context".to_owned(), json!(CONTEXTS));
    document.insert("@type".to_owned(), json!("Set"));
    // The identity of the grants, not of the request: the digest is over the rules alone, so
    // the same grants read twice carry the same uid.
    let body = serde_json::to_vec(&json!([&permission, &prohibition])).unwrap_or_default();
    document.insert(
        "uid".to_owned(),
        json!(format!(
            "{base}/api/endpoint/{}/access#sha256:{}",
            endpoint.slug,
            digest(&body)
        )),
    );
    // One assigner for the document when every rule agrees, which is the usual case: a space's
    // policies are authored by the organization that owns it. When they do not agree the
    // assigner stays on each rule, where ODRL also allows it, rather than being averaged away.
    if let Some(assigner) = single_assigner(&permissions, &prohibitions) {
        document.insert("assigner".to_owned(), json!(assigner));
    }
    document.insert("assignee".to_owned(), json!(assignee_of(subject)));
    document.insert("permission".to_owned(), Value::Array(permission));
    if !prohibition.is_empty() {
        document.insert("prohibition".to_owned(), Value::Array(prohibition));
    }
    Value::Object(document)
}

/// The same document as Turtle (EP-57).
///
/// Written from the JSON-LD rather than from the policies, so the two serializations cannot
/// drift apart: whatever `policy` decides to say, this says the same thing in RDF.
pub fn turtle(document: &Value) -> String {
    let mut out = String::from(
        "@prefix odrl: <http://www.w3.org/ns/odrl/2/> .\n\
         @prefix ngsi-ld: <https://joinedcontext.com/odrl/ngsi-ld/v1#> .\n\
         @prefix geojson: <https://purl.org/geojson/vocab#> .\n\n",
    );
    let uid = document.get("uid").and_then(Value::as_str).unwrap_or("");
    out.push_str(&format!("<{uid}> a odrl:Set"));
    for party in ["assigner", "assignee"] {
        if let Some(id) = document.get(party).and_then(Value::as_str) {
            out.push_str(&format!(" ;\n  odrl:{party} {}", party_term(id)));
        }
    }
    for (key, predicate) in [
        ("permission", "odrl:permission"),
        ("prohibition", "odrl:prohibition"),
    ] {
        for rule in document
            .get(key)
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            out.push_str(&format!(" ;\n  {predicate} {}", rule_node(rule)));
        }
    }
    out.push_str(" .\n");
    out
}

/// What a document says, read back into the grant it was written from (R26).
///
/// The inverse of [`policy`] over the six names R26 requires to survive: operations, entity
/// selectors, property and relationship whitelists, `q` and `scopeQ`. It is the mapper the
/// round-trip tests use, and the reader a `DataAgreement` compiler starts from (DS-10).
pub fn read(document: &Value) -> Vec<Grant> {
    let mut grants = Vec::new();
    for (key, prohibition) in [("permission", false), ("prohibition", true)] {
        for rule in document
            .get(key)
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            grants.push(grant_of(rule, prohibition));
        }
    }
    grants
}

/// One rule of an ODRL document, in the terms a `Policy` is written in (R26).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Grant {
    /// Whether the rule takes access away rather than giving it (GW8).
    pub prohibition: bool,
    /// The party that granted it.
    pub assigner: Option<String>,
    /// The CIM 009 operations, without the profile prefix, sorted.
    pub operations: Vec<String>,
    /// The entity type the rule targets.
    pub entity_type: String,
    /// The explicit entity URN the rule targets, when it names one.
    pub id: Option<String>,
    /// The anchored pattern the rule targets, when it names one.
    pub id_pattern: Option<String>,
    /// The readable and writable attributes, sorted; empty means every attribute.
    pub attributes: Vec<String>,
    /// The residual NGSI-LD query filter.
    pub q: Option<String>,
    /// The residual scope query.
    pub scope_q: Option<String>,
    /// The residual geographic query, rebuilt from the geometry the constraint carries.
    pub geo_q: Option<String>,
    /// The residual temporal query, rebuilt from the interval the constraints carry.
    pub temporal_q: Option<String>,
}

/// One rule per entity type a policy names: ODRL targets one thing, a `Policy` may name several.
fn rules(policies: &[&PolicySpec]) -> Vec<Value> {
    let mut rules = Vec::new();
    for policy in policies {
        let actions: Vec<Value> = granted_operations(policy)
            .into_iter()
            .map(|operation| json!(format!("ngsi-ld:{operation}")))
            .collect();
        let attributes = granted_attrs(&policy.information);

        for selector in policy
            .information
            .iter()
            .flat_map(|info| info.entities.iter())
        {
            let mut refinement = Vec::new();
            if !attributes.is_empty() {
                refinement.push(constraint(
                    "ngsi-ld:attrs",
                    "isAnyOf",
                    json!(attributes.iter().collect::<Vec<_>>()),
                ));
            }
            if let Some(id) = &selector.id {
                refinement.push(constraint("ngsi-ld:id", "eq", json!(id.to_string())));
            }
            if let Some(pattern) = &selector.id_pattern {
                // ODRL has no regular-expression operator, so the profile defines the left
                // operand and `eq` compares the pattern itself. A reader that does not load
                // the profile sees a refinement it cannot evaluate and, per ODRL, must not
                // act on the rule — which is the safe direction.
                refinement.push(constraint("ngsi-ld:idPattern", "eq", json!(pattern)));
            }

            let mut target = Map::new();
            target.insert("@type".to_owned(), json!("ngsi-ld:EntityType"));
            target.insert("uid".to_owned(), json!(selector.entity_type));
            if !refinement.is_empty() {
                target.insert("refinement".to_owned(), Value::Array(refinement));
            }

            let mut rule = Map::new();
            rule.insert("action".to_owned(), json!(actions));
            rule.insert("target".to_owned(), Value::Object(target));
            rule.insert("assigner".to_owned(), json!(policy.assigner));
            let constraints = constraints_of(policy);
            if !constraints.is_empty() {
                rule.insert("constraint".to_owned(), Value::Array(constraints));
            }
            rules.push(Value::Object(rule));
        }
    }
    rules
}

/// The residual a rule carries: the two query strings verbatim, geography and time decomposed.
fn constraints_of(policy: &PolicySpec) -> Vec<Value> {
    let mut out = Vec::new();
    // `q` and `scopeQ` travel as themselves. R26 asks for these two to survive the round trip
    // exactly, and a string that is never taken apart cannot lose anything.
    if let Some(q) = &policy.q {
        out.push(constraint("ngsi-ld:q", "eq", json!(q)));
    }
    if let Some(scope) = &policy.scope_q {
        out.push(constraint("ngsi-ld:scopeQ", "isPartOf", json!(scope)));
    }
    if let Some(geo) = &policy.geo_q {
        out.push(geo_constraint(geo));
    }
    if let Some(temporal) = &policy.temporal_q {
        out.extend(temporal_constraints(temporal));
    }
    out
}

/// `georel=within;geometry=Polygon;coordinates=[[…]]` as an ODRL constraint over a geometry.
///
/// The relation becomes the operator and the geometry the right operand, so a reader that
/// speaks GeoJSON can act on it. The words are split rather than parsed: `pdp::geo` already
/// parses the same string into a polygon, and it does that because enforcement needs the
/// shape, while a serialization needs only the terms.
fn geo_constraint(geo_q: &str) -> Value {
    let mut relation = None;
    let mut geometry = None;
    let mut coordinates = None;
    for part in geo_q.split(';') {
        match part.split_once('=') {
            Some(("georel", value)) => relation = Some(value.trim()),
            Some(("geometry", value)) => geometry = Some(value.trim()),
            Some(("coordinates", value)) => coordinates = Some(value.trim()),
            _ => {}
        }
    }
    match (relation, geometry, coordinates) {
        (Some(relation), Some(geometry), Some(coordinates)) => {
            let mut right = Map::new();
            right.insert("@type".to_owned(), json!(format!("geojson:{geometry}")));
            right.insert(
                "coordinates".to_owned(),
                serde_json::from_str(coordinates).unwrap_or_else(|_| json!(coordinates)),
            );
            constraint(
                "ngsi-ld:geoQ",
                &format!("ngsi-ld:{}", relation.split(';').next().unwrap_or(relation)),
                Value::Object(right),
            )
        }
        // A geometry this does not recognise is still a constraint, and dropping it would
        // describe a wider grant than the one in force.
        _ => constraint("ngsi-ld:geoQ", "eq", json!(geo_q)),
    }
}

/// `timerel=after;timeAt=P-1D` as one or two `dateTime` constraints.
fn temporal_constraints(temporal_q: &str) -> Vec<Value> {
    let mut relation = None;
    let mut at = None;
    let mut end = None;
    for part in temporal_q.split([';', '&']) {
        match part.split_once('=') {
            Some(("timerel", value)) => relation = Some(value.trim()),
            Some(("timeAt", value)) => at = Some(value.trim()),
            Some(("endTimeAt", value)) => end = Some(value.trim()),
            _ => {}
        }
    }
    match (relation, at, end) {
        (Some("after"), Some(at), _) => vec![constraint("dateTime", "gteq", json!(at))],
        (Some("before"), Some(at), _) => vec![constraint("dateTime", "lteq", json!(at))],
        (Some("between"), Some(at), Some(end)) => vec![
            constraint("dateTime", "gteq", json!(at)),
            constraint("dateTime", "lteq", json!(end)),
        ],
        _ => vec![constraint("ngsi-ld:temporalQ", "eq", json!(temporal_q))],
    }
}

fn constraint(left: &str, operator: &str, right: Value) -> Value {
    json!({ "leftOperand": left, "operator": operator, "rightOperand": right })
}

/// The assigner every rule agrees on, or `None` when they do not.
fn single_assigner(permissions: &[&PolicySpec], prohibitions: &[&PolicySpec]) -> Option<String> {
    let mut assigners = permissions
        .iter()
        .chain(prohibitions.iter())
        .map(|policy| policy.assigner.as_str());
    let first = assigners.next()?;
    assigners
        .all(|assigner| assigner == first)
        .then(|| first.to_owned())
}

/// The caller as an ODRL party. Never more than the caller already knows about itself.
fn assignee_of(subject: &Subject) -> String {
    if let Some(did) = &subject.did {
        return did.clone();
    }
    if let Some(user) = &subject.user {
        return user.clone();
    }
    if let Some(account) = &subject.service_account {
        return account.clone();
    }
    "public".to_owned()
}

fn grant_of(rule: &Value, prohibition: bool) -> Grant {
    let target = rule.get("target");
    let refinements = target
        .and_then(|target| target.get("refinement"))
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default();

    let mut grant = Grant {
        prohibition,
        assigner: rule
            .get("assigner")
            .and_then(Value::as_str)
            .map(str::to_owned),
        operations: rule
            .get("action")
            .and_then(Value::as_array)
            .map(|actions| {
                actions
                    .iter()
                    .filter_map(Value::as_str)
                    .map(|action| action.trim_start_matches("ngsi-ld:").to_owned())
                    .collect()
            })
            .unwrap_or_default(),
        entity_type: target
            .and_then(|target| target.get("uid"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        ..Grant::default()
    };

    for refinement in refinements {
        match left_of(refinement) {
            "ngsi-ld:attrs" => {
                grant.attributes = refinement
                    .get("rightOperand")
                    .and_then(Value::as_array)
                    .map(|values| {
                        values
                            .iter()
                            .filter_map(Value::as_str)
                            .map(str::to_owned)
                            .collect()
                    })
                    .unwrap_or_default();
            }
            "ngsi-ld:id" => grant.id = right_string(refinement),
            "ngsi-ld:idPattern" => grant.id_pattern = right_string(refinement),
            _ => {}
        }
    }

    let constraints = rule
        .get("constraint")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default();
    let mut after = None;
    let mut before = None;
    for item in constraints {
        match left_of(item) {
            "ngsi-ld:q" => grant.q = right_string(item),
            "ngsi-ld:scopeQ" => grant.scope_q = right_string(item),
            "ngsi-ld:geoQ" => grant.geo_q = geo_q_of(item),
            "ngsi-ld:temporalQ" => grant.temporal_q = right_string(item),
            "dateTime" => match item.get("operator").and_then(Value::as_str) {
                Some("gteq") => after = right_string(item),
                Some("lteq") => before = right_string(item),
                _ => {}
            },
            _ => {}
        }
    }
    grant.temporal_q = grant.temporal_q.or(match (after, before) {
        (Some(from), Some(to)) => Some(format!("timerel=between;timeAt={from};endTimeAt={to}")),
        (Some(from), None) => Some(format!("timerel=after;timeAt={from}")),
        (None, Some(to)) => Some(format!("timerel=before;timeAt={to}")),
        (None, None) => None,
    });
    grant
}

fn geo_q_of(item: &Value) -> Option<String> {
    let right = item.get("rightOperand")?;
    let Some(object) = right.as_object() else {
        return right.as_str().map(str::to_owned);
    };
    let relation = item
        .get("operator")
        .and_then(Value::as_str)?
        .trim_start_matches("ngsi-ld:");
    let geometry = object
        .get("@type")
        .and_then(Value::as_str)?
        .trim_start_matches("geojson:");
    let coordinates = match object.get("coordinates") {
        Some(Value::String(text)) => text.clone(),
        Some(value) => serde_json::to_string(value).ok()?,
        None => return None,
    };
    Some(format!(
        "georel={relation};geometry={geometry};coordinates={coordinates}"
    ))
}

fn left_of(node: &Value) -> &str {
    node.get("leftOperand")
        .and_then(Value::as_str)
        .unwrap_or_default()
}

fn right_string(node: &Value) -> Option<String> {
    node.get("rightOperand")
        .and_then(Value::as_str)
        .map(str::to_owned)
}

/// A party as Turtle writes it: an IRI when the identifier is one, a literal otherwise.
///
/// A DID and a URL are IRIs; a role name like `public` is not, and inventing an IRI scheme for
/// it here would put a term in the graph that nothing else in the platform uses.
fn party_term(id: &str) -> String {
    let iri = id.contains(':') && !id.contains(char::is_whitespace) && !id.contains('"');
    match iri {
        true => format!("<{id}>"),
        false => literal(id),
    }
}

/// One permission or prohibition as a Turtle blank node.
fn rule_node(rule: &Value) -> String {
    let mut parts = Vec::new();
    if let Some(actions) = rule.get("action").and_then(Value::as_array) {
        let names: Vec<String> = actions
            .iter()
            .filter_map(Value::as_str)
            .map(term_or_literal)
            .collect();
        if !names.is_empty() {
            parts.push(format!("odrl:action {}", names.join(", ")));
        }
    }
    if let Some(target) = rule.get("target") {
        parts.push(format!("odrl:target {}", target_node(target)));
    }
    if let Some(assigner) = rule.get("assigner").and_then(Value::as_str) {
        parts.push(format!("odrl:assigner {}", party_term(assigner)));
    }
    for item in rule
        .get("constraint")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        parts.push(format!("odrl:constraint {}", constraint_node(item)));
    }
    format!("[\n    {}\n  ]", parts.join(" ;\n    "))
}

fn target_node(target: &Value) -> String {
    let mut parts = Vec::new();
    if let Some(kind) = target.get("@type").and_then(Value::as_str) {
        parts.push(format!("a {}", term_or_literal(kind)));
    }
    if let Some(uid) = target.get("uid").and_then(Value::as_str) {
        parts.push(format!("odrl:uid {}", literal(uid)));
    }
    for item in target
        .get("refinement")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        parts.push(format!("odrl:refinement {}", constraint_node(item)));
    }
    format!("[ {} ]", parts.join(" ; "))
}

fn constraint_node(item: &Value) -> String {
    let left = item
        .get("leftOperand")
        .and_then(Value::as_str)
        .map(term_or_literal)
        .unwrap_or_default();
    let operator = item
        .get("operator")
        .and_then(Value::as_str)
        .map(|operator| match operator.contains(':') {
            true => term_or_literal(operator),
            false => format!("odrl:{operator}"),
        })
        .unwrap_or_default();
    let right = item
        .get("rightOperand")
        .map(right_node)
        .unwrap_or_else(|| literal(""));
    format!("[ odrl:leftOperand {left} ; odrl:operator {operator} ; odrl:rightOperand {right} ]")
}

/// A right operand as Turtle: a list for a set, a blank node for a geometry, a literal
/// otherwise. Numbers and booleans keep their own types rather than becoming strings.
fn right_node(value: &Value) -> String {
    match value {
        Value::String(text) => literal(text),
        Value::Array(values) => values.iter().map(right_node).collect::<Vec<_>>().join(", "),
        Value::Object(object) => {
            let mut parts = Vec::new();
            if let Some(kind) = object.get("@type").and_then(Value::as_str) {
                parts.push(format!("a {}", term_or_literal(kind)));
            }
            for (key, member) in object {
                if key == "@type" {
                    continue;
                }
                parts.push(format!("geojson:{key} {}", right_node(member)));
            }
            format!("[ {} ]", parts.join(" ; "))
        }
        Value::Number(number) => number.to_string(),
        Value::Bool(flag) => flag.to_string(),
        Value::Null => literal(""),
    }
}

/// A prefixed name when the value carries a known prefix, a literal otherwise.
fn term_or_literal(value: &str) -> String {
    match value.split_once(':') {
        Some((prefix, local)) if matches!(prefix, "ngsi-ld" | "odrl" | "geojson") => {
            format!("{prefix}:{local}")
        }
        _ => literal(value),
    }
}

/// A Turtle string literal with the four escapes the grammar requires.
fn literal(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for character in text.chars() {
        match character {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            other => out.push(other),
        }
    }
    out.push('"');
    out
}
