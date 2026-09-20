//! The discovery surface, narrowed like the data it describes (T-2134; EP-25, EP-26, MP-02).
//!
//! `GET /types`, `GET /types/{type}`, `GET /attributes` and `GET /attributes/{attr}` answer
//! documents about the space rather than entities of it, and CIM 009 gives them an `id` and a
//! `type` like an entity's. Sending them through the entity projection gets both directions
//! wrong: the allowed answer loses `attributeDetails` and `typeList` — the members the document
//! exists for — while a name no grant reaches is still forwarded and answered `200`, where a
//! name the space does not hold answers `404`. That difference is a directory of the withheld
//! names, read one request at a time.
//!
//! So discovery is enforced with the same constraint set, on its own members: a name out of
//! reach is not found before the broker is asked, and an allowed document keeps its shape with
//! every list of names narrowed to what this caller may read.

use super::evaluator::Constraints;
use jc_core::kinds::Operation;
use serde_json::Value;
use std::collections::BTreeSet;

/// Whether the operation answers a document about the vocabulary of the space.
pub fn describes(operation: Operation) -> bool {
    matches!(
        operation,
        Operation::RetrieveEntityTypes
            | Operation::RetrieveEntityTypeDetails
            | Operation::RetrieveEntityTypeInfo
            | Operation::RetrieveAttrTypes
            | Operation::RetrieveAttrTypeDetails
            | Operation::RetrieveAttrTypeInfo
    )
}

/// Whether the one name this request addresses is a name the caller may learn about at all.
///
/// The answer decides between the broker being asked and the gateway's own `404`, so it is the
/// whole of the existence guard: the name is judged against the grants and the projection, never
/// against what the space happens to hold.
pub fn reaches(operation: Operation, name: &str, constraints: &Constraints) -> bool {
    match operation {
        Operation::RetrieveEntityTypeInfo => reaches_type(name, constraints),
        Operation::RetrieveAttrTypeInfo => reaches_attribute(name, constraints, None),
        _ => true,
    }
}

/// Whether a type name is one the grants and the projection leave (EP-26).
///
/// An empty set is a grant over every type the endpoint serves, which is the same rule
/// [`super::projection::permitted`] applies to an entity's own type.
fn reaches_type(name: &str, constraints: &Constraints) -> bool {
    constraints.types.is_empty() || constraints.types.contains(term(name))
}

/// Whether an attribute name is one this endpoint serves, within one type or across all of them.
///
/// `hidden` is a denial whatever the whitelists say (EP-61). A whitelist that is empty is a
/// grant over the whole entity, so only the hidden names are withheld.
fn reaches_attribute(name: &str, constraints: &Constraints, of_type: Option<&str>) -> bool {
    let name = term(name);
    if constraints.hidden.contains(name) {
        return false;
    }
    match served(constraints, of_type) {
        Some(whitelist) => whitelist.contains(name),
        None => true,
    }
}

/// The attributes this endpoint serves, of one type when the projection says so.
///
/// `attrs` is not read here: it is what the *caller* asked for, and a caller asking for one
/// attribute does not narrow the vocabulary of the endpoint. `served` is the grants' own
/// whitelist and `attrs_by_type` the projection's, which are the two statements about what may
/// be read at all.
fn served(constraints: &Constraints, of_type: Option<&str>) -> Option<BTreeSet<String>> {
    if !constraints.attrs_by_type.is_empty() {
        return match of_type {
            Some(class) => Some(
                constraints
                    .attrs_by_type
                    .get(term(class))
                    .cloned()
                    .unwrap_or_default(),
            ),
            None => Some(
                constraints
                    .attrs_by_type
                    .values()
                    .flatten()
                    .cloned()
                    .collect(),
            ),
        };
    }
    if constraints.served.is_empty() {
        return None;
    }
    Some(constraints.served.clone())
}

/// Members that list entity type names.
const TYPE_LISTS: &[&str] = &["typeList", "typeNames"];
/// Members that list attribute names.
const ATTRIBUTE_LISTS: &[&str] = &["attributeList", "attributeNames"];
/// Members that count entities or attribute instances of the whole space.
const COUNTS: &[&str] = &["entityCount", "attributeCount"];

/// Narrows a vocabulary document, or an array of them, to what the caller may read (EP-26).
///
/// Returns whether the answer may be served at all. The name in the path was judged before the
/// broker was asked, and the broker is not the authority on which name it answered about: a
/// document that comes back about another type or another attribute is a document this caller
/// did not reach, whether the broker resolved an alias, followed a registration or has a defect.
#[must_use]
pub fn narrow(payload: &mut Value, constraints: &Constraints) -> bool {
    match payload {
        Value::Array(entries) => {
            entries.retain(|entry| about_a_reachable_name(entry, constraints));
            for entry in entries {
                narrow_document(entry, constraints);
            }
            true
        }
        document => {
            if !about_a_reachable_name(document, constraints) {
                return false;
            }
            narrow_document(document, constraints);
            true
        }
    }
}

/// Whether one entry of a listing is about a type or an attribute the caller may read.
fn about_a_reachable_name(entry: &Value, constraints: &Constraints) -> bool {
    if let Some(class) = entry.get("typeName").and_then(Value::as_str) {
        return reaches_type(class, constraints);
    }
    if let Some(attribute) = entry.get("attributeName").and_then(Value::as_str) {
        return reaches_attribute(attribute, constraints, None);
    }
    true
}

/// One document, with every list of names narrowed and every count of the whole space removed.
///
/// A document that is about one type narrows its attribute lists by that type's own slots, which
/// is what makes `GET /types/Vehicle` on a projection say `Vehicle: [name, location]` rather than
/// the union over every class the projection names (MP-02, T-1862).
fn narrow_document(document: &mut Value, constraints: &Constraints) {
    let of_type = document
        .get("typeName")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let Some(members) = document.as_object_mut() else {
        return;
    };
    for (name, value) in members.iter_mut() {
        if TYPE_LISTS.contains(&name.as_str()) {
            if let Some(listed) = value.as_array_mut() {
                listed.retain(|class| match class.as_str() {
                    Some(class) => reaches_type(class, constraints),
                    None => false,
                });
            }
        } else if ATTRIBUTE_LISTS.contains(&name.as_str()) {
            if let Some(listed) = value.as_array_mut() {
                listed.retain(|attribute| match attribute.as_str() {
                    Some(attribute) => {
                        reaches_attribute(attribute, constraints, of_type.as_deref())
                    }
                    None => false,
                });
            }
        } else if let Some(entries) = value.as_array_mut() {
            // `attributeDetails` and anything shaped like it: entries naming an attribute or a
            // type, each of which is narrowed as a document of its own.
            entries.retain(|entry| {
                entry
                    .get("attributeName")
                    .and_then(Value::as_str)
                    .is_none_or(|attribute| {
                        reaches_attribute(attribute, constraints, of_type.as_deref())
                    })
                    && about_a_reachable_name(entry, constraints)
            });
            for entry in entries {
                narrow_document(entry, constraints);
            }
        }
    }
    // A count is an aggregate over the whole space. Where anything at all was narrowed, the
    // caller cannot see every entity or instance it counts, and a number about what is withheld
    // is what EP-25 refuses. ponytail: dropped whenever the decision narrowed; counting only the
    // visible entities is a query per type, which is the upgrade if people ask for the number.
    if constraints.restricted {
        members.retain(|name, _| !COUNTS.contains(&name.as_str()));
    }
}

/// The term of a name, whether the document compacted it or left the IRI expanded.
fn term(name: &str) -> &str {
    name.rsplit(['#', '/']).next().unwrap_or(name)
}
