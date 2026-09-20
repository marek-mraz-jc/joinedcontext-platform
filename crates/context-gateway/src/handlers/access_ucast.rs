//! The caller's residual as a UCAST condition tree (T-0165, EP-58, EP-59).
//!
//! A client that knows what it may see can filter before it asks, instead of sending a wide
//! query and being narrowed. This is that grant in a shape a client can compile: one entry per
//! entity type, the operations it may call, the attributes it may project, and the conditions
//! the gateway would add to any request it makes (GW11).
//!
//! It says nothing about what it does not grant. A type no policy names is absent rather than
//! listed as denied, and a prohibition that takes back every operation on a type removes the
//! type instead of appearing as a rule of its own — the caller learns what it may do and never
//! what exists (EP-59, R20).
//!
//! **`q` is not decomposed.** `scopeQ`, `geoQ` and `temporalQ` have a shape this module can
//! read, so they become branches. A residual `q` is an NGSI-LD query filter, and R56 allows the
//! gateway exactly one grammar for those — the broker's own parser compiled to Wasm, which the
//! gateway does not host. Writing a second one here to split `pm10>=0` into a branch is the one
//! thing that requirement forbids, so `q` travels verbatim beside `where` and the client applies
//! it as it applies any NGSI-LD filter (API/02). Dropping it instead would let a client compile
//! a filter wider than its own grant.

use crate::pdp::evaluator::{effective, granted_attrs, granted_operations, Subject};
use crate::resolver::Endpoint;
use chrono::{DateTime, Utc};
use jc_core::kinds::PolicySpec;
use serde_json::{json, Map, Value};
use std::collections::{BTreeMap, BTreeSet};

/// The media type of the condition-tree representation (EP-58).
pub const GRANT_AST_JSON: &str = "application/vnd.joinedcontext.grant-ast+json";

/// The caller's grants as one entry per entity type (EP-58).
pub fn grant_ast(subject: &Subject, endpoint: &Endpoint, now: DateTime<Utc>) -> Value {
    let (prohibitions, permissions): (Vec<_>, Vec<_>) = effective(subject, &endpoint.policies, now)
        .into_iter()
        .partition(|policy| policy.effect.is_prohibition());

    // What each type is denied, so it can be taken off what it is granted (GW8).
    let mut denied: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for policy in &prohibitions {
        let operations: BTreeSet<String> = granted_operations(policy)
            .into_iter()
            .map(str::to_owned)
            .collect();
        for entity_type in types_of(policy) {
            denied
                .entry(entity_type)
                .or_default()
                .extend(operations.iter().cloned());
        }
    }

    let mut granted: BTreeMap<String, Type> = BTreeMap::new();
    for policy in &permissions {
        let operations: BTreeSet<String> = granted_operations(policy)
            .into_iter()
            .map(str::to_owned)
            .collect();
        let attributes = granted_attrs(&policy.information);
        for entity_type in types_of(policy) {
            let entry = granted.entry(entity_type).or_default();
            entry.operations.extend(operations.iter().cloned());
            // No whitelist reaches every attribute, and a grant that reaches everything is not
            // narrowed by one that reaches three columns.
            match attributes.is_empty() {
                true => entry.all_attributes = true,
                false => entry.attributes.extend(attributes.iter().cloned()),
            }
            entry.branches.push(branch(policy));
        }
    }

    let mut document = Map::new();
    for (entity_type, mut entry) in granted {
        if let Some(denied) = denied.get(&entity_type) {
            entry
                .operations
                .retain(|operation| !denied.contains(operation));
        }
        // Every operation taken back is a type the caller may no longer touch at all, and a
        // type it may not touch has no place in a document about what it may do.
        if entry.operations.is_empty() {
            continue;
        }
        document.insert(entity_type, entry.into_value());
    }
    Value::Object(document)
}

/// What the grants say about one entity type, before they are written out.
#[derive(Default)]
struct Type {
    operations: BTreeSet<String>,
    attributes: BTreeSet<String>,
    /// Whether some grant reaches every attribute, which subsumes any whitelist.
    all_attributes: bool,
    /// One entry per grant: its residual `q` and its condition tree, either of which may be
    /// absent when that grant does not narrow in that dimension.
    branches: Vec<(Option<String>, Option<Value>)>,
}

impl Type {
    fn into_value(self) -> Value {
        let mut out = Map::new();
        out.insert(
            "operations".to_owned(),
            json!(self.operations.iter().collect::<Vec<_>>()),
        );
        out.insert(
            "project".to_owned(),
            match self.all_attributes {
                true => json!("*"),
                false => json!(self.attributes.iter().collect::<Vec<_>>()),
            },
        );

        // A grant that narrows nothing subsumes every other: the caller may see the type
        // unconditionally, and repeating the narrower grants would describe a smaller right
        // than the one it holds.
        if self
            .branches
            .iter()
            .any(|(q, tree)| q.is_none() && tree.is_none())
        {
            return Value::Object(out);
        }

        let filters: Vec<&str> = self
            .branches
            .iter()
            .filter_map(|(q, _)| q.as_deref())
            .collect();
        if filters.len() == self.branches.len() && !filters.is_empty() {
            // Every grant carries one, so the union is expressible; NGSI-LD writes OR as `|`
            // and every operand is parenthesized so none can reach outside itself.
            out.insert("q".to_owned(), json!(union(&filters)));
        } else if let Some(single) = filters.first().filter(|_| filters.len() == 1) {
            out.insert("q".to_owned(), json!(*single));
        }

        let trees: Vec<Value> = self
            .branches
            .iter()
            .filter_map(|(_, tree)| tree.clone())
            .collect();
        match trees.len() {
            0 => {}
            1 => {
                out.insert(
                    "where".to_owned(),
                    trees.into_iter().next().unwrap_or(Value::Null),
                );
            }
            // Several grants are several ways in, so the caller may see what satisfies any of
            // them (R7): a union, never an intersection.
            _ => {
                out.insert("where".to_owned(), compound("or", trees));
            }
        }
        Value::Object(out)
    }
}

/// One grant's residual: its `q` verbatim, and the tree its other filters make.
fn branch(policy: &PolicySpec) -> (Option<String>, Option<Value>) {
    let mut conditions = Vec::new();
    if let Some(scope) = &policy.scope_q {
        conditions.push(field("scope_under", "scope", json!(scope)));
    }
    if let Some(geo) = &policy.geo_q {
        conditions.push(geo_condition(geo));
    }
    if let Some(temporal) = &policy.temporal_q {
        conditions.extend(temporal_conditions(temporal));
    }

    let tree = match conditions.len() {
        0 => None,
        1 => conditions.into_iter().next(),
        _ => Some(compound("and", conditions)),
    };
    (policy.q.clone(), tree)
}

/// `georel=within;geometry=Polygon;coordinates=[[…]]` as a `geo_within` leaf.
///
/// The words are split, not parsed into a shape: `pdp::geo` builds the polygon because
/// enforcement needs to test points against it, and a client compiling a filter needs the
/// GeoJSON rather than our idea of it.
fn geo_condition(geo_q: &str) -> Value {
    let mut geometry = None;
    let mut coordinates = None;
    let mut relation = None;
    for part in geo_q.split(';') {
        match part.split_once('=') {
            Some(("georel", value)) => relation = Some(value.trim()),
            Some(("geometry", value)) => geometry = Some(value.trim()),
            Some(("coordinates", value)) => coordinates = Some(value.trim()),
            _ => {}
        }
    }
    match (geometry, coordinates) {
        (Some(geometry), Some(coordinates)) => field(
            "geo_within",
            "location",
            json!({
                "type": geometry,
                "coordinates": serde_json::from_str::<Value>(coordinates)
                    .unwrap_or_else(|_| json!(coordinates)),
            }),
        ),
        // A geometry this cannot read is still a constraint the caller is under. It travels as
        // the filter string, because leaving it out would describe a wider grant.
        _ => field(
            "geo_within",
            "location",
            json!({ "geoQ": geo_q, "georel": relation.unwrap_or("within") }),
        ),
    }
}

/// `timerel=…` as one `gte`/`lte` leaf, or a `time_between` leaf for a closed window.
///
/// A bound written as an ISO 8601 duration stays one: it is a moving window, and resolving it
/// against this instant would hand the client a boundary that is already stale.
fn temporal_conditions(temporal_q: &str) -> Vec<Value> {
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
        (Some("after"), Some(at), _) => vec![field("gte", "observedAt", instant(at))],
        (Some("before"), Some(at), _) => vec![field("lte", "observedAt", instant(at))],
        (Some("between"), Some(at), Some(end)) => vec![field(
            "time_between",
            "observedAt",
            json!([instant(at), instant(end)]),
        )],
        _ => Vec::new(),
    }
}

/// An absolute instant as itself, a duration as the moving bound it is.
fn instant(value: &str) -> Value {
    match value.starts_with('P') || value.starts_with("-P") {
        true => json!({ "relative": value }),
        false => json!(value),
    }
}

fn field(operator: &str, name: &str, value: Value) -> Value {
    json!({ "type": "field", "operator": operator, "field": name, "value": value })
}

fn compound(operator: &str, value: Vec<Value>) -> Value {
    json!({ "type": "compound", "operator": operator, "value": value })
}

/// The union of several NGSI-LD filters, in NGSI-LD's own syntax: `|` is OR, and every operand
/// is parenthesized so no operand can change how its neighbours group.
fn union(filters: &[&str]) -> String {
    match filters.len() {
        1 => filters[0].to_owned(),
        _ => filters
            .iter()
            .map(|filter| format!("({filter})"))
            .collect::<Vec<_>>()
            .join("|"),
    }
}

/// The entity types one policy names.
fn types_of(policy: &PolicySpec) -> Vec<String> {
    policy
        .information
        .iter()
        .flat_map(|info| info.entities.iter())
        .map(|selector| selector.entity_type.clone())
        .collect()
}
