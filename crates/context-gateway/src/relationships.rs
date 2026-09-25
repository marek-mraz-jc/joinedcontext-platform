//! The relationships of a space's model, held at every write of an Endpoint (DM-70, T-2740).
//!
//! A space's generated JSON Schema carries each stored end of a relationship as a property with
//! `x-ngsi-ld-relationship {target, cardinality, unique, …}` (T-2739, DM-67). The gateway reads
//! those into [`RelationshipRules`] and refuses, before the broker is asked, a write that:
//!
//! - names a target of another type than the end's (`target-wrong-type`, read from the id
//!   scheme `urn:ngsi-ld:{Type}:…`, PF-10);
//! - puts more than one target on a single end (`single-end-many-targets`);
//! - leaves a `required` end absent on a whole-entity write, or empties it on a partial one
//!   (`required-end-missing`);
//! - names a target the space does not hold, or that the writer may not read
//!   (`target-missing`, checked by [`targets_of`] and one read through the endpoint).
//!
//! `target-taken` and the delete rules need the store's own constraint, so they are the
//! broker's (T-2858); the gateway never reads and then writes to decide them. Reads are not
//! touched: the computed end is never added to an answer (DM-67, CIM 009).

use std::collections::{BTreeMap, BTreeSet};

use jc_core::ProblemDetails;
use serde_json::Value;

/// NGSI-LD's null, which deletes an attribute in a merge.
const NGSI_LD_NULL: &str = "urn:ngsi-ld:null";

/// The problem type DM-70 names for a broken relationship.
const BAD_REQUEST_DATA: &str = "https://uri.etsi.org/ngsi-ld/errors/BadRequestData";

/// One stored end of a relationship, as the model's JSON Schema states it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredEnd {
    /// The class the end points at.
    pub target: String,
    /// Whether the end holds several targets (one `datasetId` each).
    pub many: bool,
    /// Whether an entity needs at least one target on it.
    pub required: bool,
}

/// A target a write names, and where: what a `target-missing` refusal says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Named {
    /// The class of the entity written.
    pub class: String,
    /// The stored end that names it.
    pub slot: String,
    /// The class it must be.
    pub target: String,
}

/// The targets of one write, by URN.
pub type Targets = BTreeMap<String, Named>;

/// Every stored end of a space's model, by class and attribute.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RelationshipRules {
    by_class: BTreeMap<String, BTreeMap<String, StoredEnd>>,
}

/// Why a write breaks a relationship (DM-70); the `rule` identifiers are the model's own
/// (Architecture/11 §1.2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Violation {
    /// The rule identifier: `target-missing`, `target-wrong-type`, `single-end-many-targets`,
    /// `required-end-missing`.
    pub rule: &'static str,
    /// The class of the entity written.
    pub class: String,
    /// The stored end.
    pub slot: String,
    /// The URN the rule is about, when there is one.
    pub object: Option<String>,
    /// The class the end points at.
    pub target: String,
}

impl From<Violation> for ProblemDetails {
    fn from(violation: Violation) -> Self {
        let Violation {
            rule,
            class,
            slot,
            object,
            target,
        } = violation;
        let named = object.as_deref().unwrap_or_default();
        let detail = match rule {
            "target-wrong-type" => format!(
                "`{slot}` of `{class}` points at a {target}, and `{named}` is not one (DM-70)"
            ),
            "single-end-many-targets" => format!(
                "`{slot}` of `{class}` holds one {target}, and the write gives it more than one (DM-70)"
            ),
            "required-end-missing" => format!(
                "`{slot}` of `{class}` is required: every `{class}` points at a {target} (DM-70)"
            ),
            _ => format!(
                "`{slot}` of `{class}` points at `{named}`, which is no {target} of this space you \
                 may read; create it first or name one that exists (DM-70)"
            ),
        };
        let mut problem = ProblemDetails::bad_request().with_detail(detail);
        problem.type_uri = BAD_REQUEST_DATA.to_owned();
        let problem = problem
            .with_extension("slot", Value::String(slot))
            .with_extension("rule", Value::String(rule.to_owned()));
        match object {
            Some(object) => problem.with_extension("object", Value::String(object)),
            None => problem,
        }
    }
}

/// The local name of a type written as an IRI, a CURIE or a plain name.
fn local(name: &str) -> &str {
    name.rsplit(['/', '#', ':']).next().unwrap_or(name)
}

/// The type an id names: `urn:ngsi-ld:{Type}:…` (PF-10).
fn type_of_id(id: &str) -> Option<&str> {
    let mut parts = id.splitn(4, ':');
    match (parts.next(), parts.next(), parts.next(), parts.next()) {
        (Some(urn), Some(ngsi), Some(kind), Some(_))
            if urn.eq_ignore_ascii_case("urn") && ngsi.eq_ignore_ascii_case("ngsi-ld") =>
        {
            Some(kind)
        }
        _ => None,
    }
}

impl RelationshipRules {
    /// The stored ends a model's generated JSON Schema states: every property that carries
    /// `x-ngsi-ld-relationship` with a `target`. An external reference (no target, DM-69) is
    /// not a relationship and is left alone.
    pub fn from_schema(schema: &Value) -> Self {
        let mut by_class = BTreeMap::new();
        let classes = schema
            .get("definitions")
            .or_else(|| schema.get("$defs"))
            .and_then(Value::as_object);
        for (class, definition) in classes.into_iter().flatten() {
            let Some(properties) = definition.get("properties").and_then(Value::as_object) else {
                continue;
            };
            let required: BTreeSet<&str> = definition
                .get("required")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .collect();
            let ends: BTreeMap<String, StoredEnd> = properties
                .iter()
                .filter_map(|(name, property)| {
                    let relationship = property.get("x-ngsi-ld-relationship")?;
                    let target = relationship.get("target")?.as_str()?;
                    if target.is_empty() {
                        return None;
                    }
                    Some((
                        name.clone(),
                        StoredEnd {
                            target: target.to_owned(),
                            many: property.get("type").and_then(Value::as_str) == Some("array"),
                            required: required.contains(name.as_str()),
                        },
                    ))
                })
                .collect();
            if !ends.is_empty() {
                by_class.insert(class.clone(), ends);
            }
        }
        Self { by_class }
    }

    /// Whether the model relates nothing, so a write needs no look.
    pub fn is_empty(&self) -> bool {
        self.by_class.is_empty()
    }

    fn of_class(&self, entity_type: &str) -> Option<(&str, &BTreeMap<String, StoredEnd>)> {
        let wanted = local(entity_type);
        self.by_class
            .get_key_value(wanted)
            .map(|(class, ends)| (class.as_str(), ends))
    }
}

/// How much of an entity a write states.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Extent {
    /// The whole entity: create, replace, a batch create or upsert. A required end must be there.
    Whole,
    /// Some attributes: append, update, merge. A required end may be left out, never emptied.
    Partial,
}

/// Where in a write payload the entities are.
#[derive(Debug, Clone, Copy)]
pub struct Shape<'a> {
    /// The entity id of the path, for a fragment that names no type (`/entities/{id}/attrs`).
    pub addressed: Option<&'a str>,
    /// The attribute of the path, when the body is that attribute's bare value.
    pub targeted: Option<&'a str>,
    /// Whole entities or fragments.
    pub extent: Extent,
}

/// The targets one attribute value names, in every form a write may use: normalized
/// (`{"type": "Relationship", "object": …}`), concise (`{"object": …}`), a multi-attribute
/// array of instances, and an `object` that is itself a list. `None` when the value is
/// NGSI-LD's null, which removes the attribute.
fn objects(value: &Value) -> Option<Vec<String>> {
    match value {
        Value::Null => None,
        Value::String(text) if text == NGSI_LD_NULL => None,
        Value::Array(instances) => {
            let mut all = Vec::new();
            for instance in instances {
                all.extend(objects(instance).unwrap_or_default());
            }
            Some(all)
        }
        Value::Object(member) => match member.get("object") {
            Some(Value::String(one)) if one == NGSI_LD_NULL => None,
            Some(Value::String(one)) => Some(vec![one.clone()]),
            Some(Value::Array(many)) => Some(
                many.iter()
                    .filter_map(|object| match object {
                        Value::String(one) => Some(one.clone()),
                        Value::Object(inner) => inner
                            .get("object")
                            .or_else(|| inner.get("@id"))
                            .and_then(Value::as_str)
                            .map(str::to_owned),
                        _ => None,
                    })
                    .collect(),
            ),
            _ => Some(Vec::new()),
        },
        // A bare string in the concise form is a Property's value, not a Relationship: the
        // write is the model's other checks' to judge.
        _ => Some(Vec::new()),
    }
}

fn types_of<'a>(
    entity: &'a serde_json::Map<String, Value>,
    addressed: Option<&'a str>,
) -> Vec<&'a str> {
    match entity.get("type").or_else(|| entity.get("@type")) {
        Some(Value::String(one)) => vec![one.as_str()],
        Some(Value::Array(many)) => many.iter().filter_map(Value::as_str).collect(),
        _ => entity
            .get("id")
            .or_else(|| entity.get("@id"))
            .and_then(Value::as_str)
            .or(addressed)
            .and_then(type_of_id)
            .into_iter()
            .collect(),
    }
}

/// Checks one entity of a write against the stored ends of its classes, answering the targets it
/// names by the class each must be, for the existence read.
fn check_entity(
    entity: &Value,
    shape: Shape<'_>,
    rules: &RelationshipRules,
    targets: &mut Targets,
) -> Result<(), Violation> {
    let Some(object) = entity.as_object() else {
        return Ok(());
    };
    for entity_type in types_of(object, shape.addressed) {
        let Some((class, ends)) = rules.of_class(entity_type) else {
            continue;
        };
        for (slot, end) in ends {
            let violation = |rule, named: Option<&str>| Violation {
                rule,
                class: class.to_owned(),
                slot: slot.clone(),
                object: named.map(str::to_owned),
                target: end.target.clone(),
            };
            let named = match object.get(slot) {
                None if end.required && shape.extent == Extent::Whole => {
                    return Err(violation("required-end-missing", None));
                }
                None => continue,
                Some(value) => objects(value),
            };
            let Some(named) = named else {
                if end.required {
                    return Err(violation("required-end-missing", None));
                }
                continue;
            };
            if named.is_empty() && end.required {
                return Err(violation("required-end-missing", None));
            }
            if !end.many && named.len() > 1 {
                return Err(violation(
                    "single-end-many-targets",
                    named.get(1).map(String::as_str),
                ));
            }
            for urn in &named {
                if type_of_id(urn).map(local) != Some(local(&end.target)) {
                    return Err(violation("target-wrong-type", Some(urn)));
                }
                targets.entry(urn.clone()).or_insert_with(|| Named {
                    class: class.to_owned(),
                    slot: slot.clone(),
                    target: end.target.clone(),
                });
            }
        }
    }
    Ok(())
}

/// Checks every entity of a write payload against the model's relationships, answering each
/// target it names, for [`first_missing`] to hold against what the endpoint reads. The payload
/// is not changed.
pub fn targets_of(
    payload: &Value,
    shape: Shape<'_>,
    rules: &RelationshipRules,
) -> Result<Targets, Violation> {
    let mut targets = Targets::new();
    if rules.is_empty() {
        return Ok(targets);
    }
    if let Some(attribute) = shape.targeted {
        let wrapped = serde_json::json!({ attribute: payload });
        check_entity(&wrapped, shape, rules, &mut targets)?;
        return Ok(targets);
    }
    match payload {
        Value::Array(entities) => {
            for entity in entities {
                check_entity(entity, shape, rules, &mut targets)?;
            }
        }
        entity => check_entity(entity, shape, rules, &mut targets)?,
    }
    Ok(targets)
}

/// The refusal for deleting a required stored end outright (`DELETE /entities/{id}/attrs/{attr}`).
pub fn deleted_attribute(
    entity_id: &str,
    attribute: &str,
    rules: &RelationshipRules,
) -> Result<(), Violation> {
    let Some((class, ends)) = type_of_id(entity_id).and_then(|kind| rules.of_class(kind)) else {
        return Ok(());
    };
    match ends.get(attribute) {
        Some(end) if end.required => Err(Violation {
            rule: "required-end-missing",
            class: class.to_owned(),
            slot: attribute.to_owned(),
            object: None,
            target: end.target.clone(),
        }),
        _ => Ok(()),
    }
}

/// The first named target the space does not hold for the writer: `found` is the ids the read
/// through the endpoint answered. A target the writer may not read is answered as absent, so the
/// refusal says nothing a read would not (DM-71).
pub fn first_missing(targets: &Targets, found: &BTreeSet<String>) -> Option<Violation> {
    targets
        .iter()
        .find(|(urn, _)| !found.contains(urn.as_str()))
        .map(|(urn, named)| Violation {
            rule: "target-missing",
            class: named.class.clone(),
            slot: named.slot.clone(),
            object: Some(urn.clone()),
            target: named.target.clone(),
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn rules() -> RelationshipRules {
        RelationshipRules::from_schema(&json!({ "definitions": {
            "User": {
                "required": ["school"],
                "properties": {
                    "school": { "type": "string", "pattern": "^urn:ngsi-ld:School:",
                        "x-ngsi-ld-kind": "Relationship",
                        "x-ngsi-ld-relationship": { "target": "School", "inverse": "users",
                            "cardinality": "many-to-one", "onDelete": "restrict", "unique": false } },
                    "courses": { "type": "array", "items": { "type": "string" },
                        "x-ngsi-ld-kind": "Relationship",
                        "x-ngsi-ld-relationship": { "target": "Course", "inverse": "users",
                            "cardinality": "many-to-many", "onDelete": "restrict", "unique": false } },
                    "refDevice": { "type": "string", "x-ngsi-ld-kind": "Relationship" },
                    "name": { "type": "string" }
                }
            }
        } }))
    }

    const SCHOOL: &str = "urn:ngsi-ld:School:hel.fi:schools:s1";
    const OTHER_SCHOOL: &str = "urn:ngsi-ld:School:hel.fi:schools:s2";
    const COURSE: &str = "urn:ngsi-ld:Course:hel.fi:schools:c1";

    fn whole() -> Shape<'static> {
        Shape {
            addressed: None,
            targeted: None,
            extent: Extent::Whole,
        }
    }

    fn partial(id: &'static str) -> Shape<'static> {
        Shape {
            addressed: Some(id),
            targeted: None,
            extent: Extent::Partial,
        }
    }

    fn user(school: Value) -> Value {
        json!({ "id": "urn:ngsi-ld:User:hel.fi:schools:u1", "type": "User", "school": school })
    }

    #[test]
    fn only_ends_with_a_target_are_rules_and_required_is_read_from_the_class() {
        let rules = rules();
        let (_, ends) = rules.of_class("User").expect("User relates");
        assert_eq!(
            ends.len(),
            2,
            "an external reference is no relationship: {ends:?}"
        );
        assert!(ends["school"].required && !ends["school"].many);
        assert!(ends["courses"].many && !ends["courses"].required);
        assert!(
            rules.of_class("https://example.org/User").is_some(),
            "an expanded type is the class"
        );
        assert!(RelationshipRules::from_schema(&json!({})).is_empty());
    }

    #[test]
    fn every_form_of_a_target_is_read_and_answered_for_the_existence_read() {
        let rules = rules();
        for school in [
            json!({ "type": "Relationship", "object": SCHOOL }),
            json!({ "object": SCHOOL }),
            json!([{ "type": "Relationship", "object": SCHOOL, "datasetId": "urn:ngsi-ld:Dataset:a" }]),
        ] {
            let targets = targets_of(&user(school.clone()), whole(), &rules).expect("one school");
            let named = targets.get(SCHOOL).expect("the school is a target");
            assert_eq!(
                (
                    named.class.as_str(),
                    named.slot.as_str(),
                    named.target.as_str()
                ),
                ("User", "school", "School"),
                "{school}"
            );
        }
        let many = json!({ "id": "urn:ngsi-ld:User:hel.fi:schools:u1", "type": "User",
            "school": { "object": SCHOOL },
            "courses": [{ "object": COURSE, "datasetId": "urn:ngsi-ld:Dataset:1" },
                        { "object": "urn:ngsi-ld:Course:hel.fi:schools:c2", "datasetId": "urn:ngsi-ld:Dataset:2" }] });
        assert_eq!(
            targets_of(&many, whole(), &rules)
                .expect("a many end")
                .len(),
            3
        );
    }

    #[test]
    fn a_target_of_another_type_is_refused_naming_the_slot_and_the_urn() {
        let wrong = "urn:ngsi-ld:Course:hel.fi:schools:c1";
        let violation = targets_of(&user(json!({ "object": wrong })), whole(), &rules())
            .expect_err("a course is no school");
        assert_eq!(violation.rule, "target-wrong-type");
        assert_eq!(violation.object.as_deref(), Some(wrong));
        let problem = ProblemDetails::from(violation);
        assert_eq!(problem.status, 400);
        assert_eq!(problem.type_uri, BAD_REQUEST_DATA);
        assert_eq!(problem.extensions["slot"], "school");
        assert_eq!(problem.extensions["rule"], "target-wrong-type");
        assert_eq!(problem.extensions["object"], wrong);
        for malformed in ["not-a-urn", "urn:ngsi-ld:School"] {
            let refused = targets_of(&user(json!({ "object": malformed })), whole(), &rules());
            assert_eq!(
                refused.map_err(|v| v.rule),
                Err("target-wrong-type"),
                "{malformed}"
            );
        }
    }

    #[test]
    fn a_single_end_takes_one_target_in_every_form() {
        for two in [
            json!({ "object": [SCHOOL, OTHER_SCHOOL] }),
            json!([{ "object": SCHOOL, "datasetId": "urn:ngsi-ld:Dataset:a" },
                   { "object": OTHER_SCHOOL, "datasetId": "urn:ngsi-ld:Dataset:b" }]),
        ] {
            let refused = targets_of(&user(two.clone()), whole(), &rules());
            assert_eq!(
                refused.map_err(|v| v.rule),
                Err("single-end-many-targets"),
                "{two}"
            );
        }
    }

    #[test]
    fn a_required_end_is_there_on_a_whole_write_and_never_emptied_by_a_partial_one() {
        let rules = rules();
        let bare =
            json!({ "id": "urn:ngsi-ld:User:hel.fi:schools:u1", "type": "User", "name": "Jana" });
        assert_eq!(
            targets_of(&bare, whole(), &rules).map_err(|v| v.rule),
            Err("required-end-missing")
        );
        let id = "urn:ngsi-ld:User:hel.fi:schools:u1";
        assert!(targets_of(&json!({ "name": "Jana" }), partial(id), &rules)
            .expect("left out")
            .is_empty());
        for emptied in [
            json!("urn:ngsi-ld:null"),
            json!({ "object": "urn:ngsi-ld:null" }),
            json!(null),
            json!([]),
        ] {
            let refused = targets_of(&json!({ "school": emptied.clone() }), partial(id), &rules);
            assert_eq!(
                refused.map_err(|v| v.rule),
                Err("required-end-missing"),
                "{emptied}"
            );
        }
        assert_eq!(
            deleted_attribute(id, "school", &rules).map_err(|v| v.rule),
            Err("required-end-missing")
        );
        assert!(deleted_attribute(id, "courses", &rules).is_ok());
        assert!(deleted_attribute("urn:ngsi-ld:Other:hel.fi:x:1", "school", &rules).is_ok());
    }

    #[test]
    fn a_fragment_for_one_attribute_and_a_batch_are_read_like_an_entity() {
        let rules = rules();
        let id = "urn:ngsi-ld:User:hel.fi:schools:u1";
        let one = Shape {
            addressed: Some(id),
            targeted: Some("school"),
            extent: Extent::Partial,
        };
        let refused = targets_of(&json!({ "object": COURSE }), one, &rules);
        assert_eq!(refused.map_err(|v| v.rule), Err("target-wrong-type"));
        let batch = json!([
            user(json!({ "object": SCHOOL })),
            user(json!({ "object": OTHER_SCHOOL }))
        ]);
        assert_eq!(
            targets_of(&batch, whole(), &rules)
                .expect("two users")
                .len(),
            2
        );
    }

    #[test]
    fn the_first_target_the_read_did_not_answer_is_the_missing_one() {
        let payload = json!({ "id": "urn:ngsi-ld:User:hel.fi:schools:u1", "type": "User",
            "school": { "object": SCHOOL }, "courses": [{ "object": COURSE, "datasetId": "urn:ngsi-ld:Dataset:1" }] });
        let targets = targets_of(&payload, whole(), &rules()).expect("well formed");
        let found = BTreeSet::from([SCHOOL.to_owned()]);
        let missing = first_missing(&targets, &found).expect("the course is not there");
        assert_eq!(
            (
                missing.rule,
                missing.slot.as_str(),
                missing.object.as_deref()
            ),
            ("target-missing", "courses", Some(COURSE))
        );
        let problem = ProblemDetails::from(missing);
        assert!(
            problem
                .detail
                .as_deref()
                .is_some_and(|detail| detail.contains("may read")),
            "{problem:?}"
        );
        let all = BTreeSet::from([SCHOOL.to_owned(), COURSE.to_owned()]);
        assert_eq!(first_missing(&targets, &all), None);
    }

    #[test]
    fn a_batch_of_a_hundred_is_checked_in_under_five_milliseconds_at_p95() {
        let batch = Value::Array(
            (0..100)
                .map(|n| {
                    json!({ "id": format!("urn:ngsi-ld:User:hel.fi:schools:u{n}"), "type": "User",
                        "school": { "type": "Relationship", "object": SCHOOL },
                        "courses": { "type": "Relationship", "object": [COURSE] },
                        "name": { "type": "Property", "value": n } })
                })
                .collect(),
        );
        let rules = rules();
        let mut took: Vec<std::time::Duration> = (0..50)
            .map(|_| {
                let started = std::time::Instant::now();
                let targets = targets_of(&batch, whole(), &rules).expect("well formed");
                assert_eq!(targets.len(), 2);
                started.elapsed()
            })
            .collect();
        took.sort();
        let p95 = took[took.len() * 95 / 100];
        assert!(p95 < std::time::Duration::from_millis(5), "p95 {p95:?}");
    }
}
