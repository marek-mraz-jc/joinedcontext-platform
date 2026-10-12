//! The broker's ODRL `Set` for one caller as an Endpoint publishes it (T-0164, EP-57, EP-79,
//! R52): the broker's engine writes the grants (plan 5.6, T-3649), the gateway adds the
//! Endpoint's licence duties and its own `uid`, and writes the Turtle from the result, so the two
//! serializations say one thing.
//!
//! The dialect is the `ngsi-ld:` profile of R52: ODRL's own vocabulary for the frame, and
//! profile terms for what ODRL has no word for (CIM 009 operations as actions, entity types as
//! targets, and `q`, `scopeQ`, `geoQ`, `temporalQ` as constraint left operands).

use crate::resolver::Endpoint;
use serde_json::{json, Value};

/// The media type of the JSON-LD serialization (EP-57).
pub const ODRL_JSON: &str = "application/odrl+json";

/// The media type of the RDF serialization (EP-57).
pub const TURTLE: &str = "text/turtle";

/// The broker's ODRL `Set` for one caller (plan 5.6, T-3649) as this Endpoint publishes it: the
/// licence's duties on every permission (EP-79), which the broker does not hold, and the `uid`
/// of the gateway's own access URL over the rules with those duties, so a changed licence is a
/// new identifier too (EP-59). `None` when the broker's answer is not a `Set` object.
pub fn published(
    document: Value,
    endpoint: &Endpoint,
    base: &str,
    digest: impl Fn(&[u8]) -> String,
) -> Option<Value> {
    let Value::Object(mut document) = document else {
        return None;
    };
    let duties = licence_duties(endpoint);
    if !duties.is_empty() {
        let rules = document.get_mut("permission").and_then(Value::as_array_mut);
        for rule in rules.into_iter().flatten().filter_map(Value::as_object_mut) {
            rule.insert("duty".to_owned(), Value::Array(duties.clone()));
        }
    }
    let rules = |name: &str| document.get(name).cloned().unwrap_or_else(|| json!([]));
    let body =
        serde_json::to_vec(&json!([rules("permission"), rules("prohibition")])).unwrap_or_default();
    document.insert(
        "uid".to_owned(),
        json!(format!(
            "{base}/api/endpoint/{}/access#sha256:{}",
            endpoint.slug,
            digest(&body)
        )),
    );
    Some(Value::Object(document))
}

/// The same document as Turtle (EP-57).
///
/// Written from the JSON-LD rather than from the policies, so the two serializations cannot
/// drift apart: whatever the published document says, this says the same thing in RDF.
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

/// The duties of the endpoint's licence in this document's dialect: `attribute`, and the
/// share-alike requirement by its full IRI, which the ODRL context does not name (EP-79).
fn licence_duties(endpoint: &Endpoint) -> Vec<Value> {
    let Some(licence) = endpoint.catalog.as_deref().and_then(|c| c.license) else {
        return Vec::new();
    };
    crate::handlers::endpoint_surface::duties(licence.duty())
        .iter()
        .filter_map(|duty| duty.get("odrl:action").and_then(Value::as_str))
        .map(|action| json!({ "action": action.strip_prefix("odrl:").unwrap_or(action) }))
        .collect()
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
    for duty in rule
        .get("duty")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        if let Some(action) = duty.get("action").and_then(Value::as_str) {
            let action = if action.contains("://") {
                format!("<{action}>")
            } else {
                format!("odrl:{action}")
            };
            parts.push(format!("odrl:duty [ odrl:action {action} ]"));
        }
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
