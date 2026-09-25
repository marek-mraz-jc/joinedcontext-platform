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
//! It also refuses a one-to-one target another source already stores (`target-taken`), and runs
//! the delete rule of every stored end that points at an entity being deleted (DM-71). The
//! broker holds no relationship rules (the owner's decision of 2026-09-25, T-2858), so both are
//! a read of the space followed by the write, with the race window Architecture/11 §1.2 names:
//! [`claims_of`] and [`unlink`] decide, the enforcement point reads and writes. Reads are not
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
    /// Whether a target may be stored by one source only: a one-to-one (`target-taken`).
    pub unique: bool,
    /// What a delete of a target does to the entities that store it (DM-66).
    pub on_delete: OnDelete,
}

/// The delete rule of a relationship (DM-66): `restrict` when the model states none.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OnDelete {
    /// The delete is refused while any entity stores the target.
    #[default]
    Restrict,
    /// A single end's entity is deleted with the target; a many end loses the one link.
    Cascade,
    /// The reference is removed: the whole attribute of a single end, the one link of a many end.
    SetNull,
}

impl OnDelete {
    fn read(value: Option<&str>) -> Self {
        match value {
            Some("cascade") => Self::Cascade,
            Some("set-null") => Self::SetNull,
            // Model Tools refuses any other rule (DM-68), so an unknown one is held strictly.
            _ => Self::Restrict,
        }
    }
}

/// One stored end that points at a class: where to look for the entities a delete of that class
/// reaches (DM-71).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Referencing<'a> {
    /// The class that stores the end.
    pub class: &'a str,
    /// The stored end.
    pub slot: &'a str,
    /// The end's rules.
    pub end: &'a StoredEnd,
}

/// A one-to-one target a write gives an entity; no other source may store it (`target-taken`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Claim {
    /// The class of the entity written.
    pub class: String,
    /// The stored end.
    pub slot: String,
    /// The target URN.
    pub object: String,
    /// The class the end points at.
    pub target: String,
    /// The entity that claims it, when the write names it; its own current link is no conflict.
    pub holder: Option<String>,
}

impl Claim {
    /// The refusal for this claim when another source already stores the target.
    pub fn taken(&self) -> Violation {
        Violation {
            rule: "target-taken",
            class: self.class.clone(),
            slot: self.slot.clone(),
            object: Some(self.object.clone()),
            target: self.target.clone(),
        }
    }
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
    /// `required-end-missing`, `target-taken`.
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
            "target-taken" => format!(
                "`{slot}` of `{class}` is one-to-one, and another `{class}` already points at \
                 `{named}`; unlink it there first (DM-70)"
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
                            unique: relationship.get("unique").and_then(Value::as_bool)
                                == Some(true),
                            on_delete: OnDelete::read(
                                relationship.get("onDelete").and_then(Value::as_str),
                            ),
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

    /// Every stored end that points at `entity_type`, the entities a delete of one reaches.
    pub fn referencing(&self, entity_type: &str) -> Vec<Referencing<'_>> {
        let wanted = local(entity_type);
        self.by_class
            .iter()
            .flat_map(|(class, ends)| {
                ends.iter()
                    .filter(|(_, end)| local(&end.target) == wanted)
                    .map(move |(slot, end)| Referencing {
                        class: class.as_str(),
                        slot: slot.as_str(),
                        end,
                    })
            })
            .collect()
    }

    /// Every stored end that points at the type the id `urn:ngsi-ld:{Type}:…` names.
    pub fn referencing_id(&self, id: &str) -> Vec<Referencing<'_>> {
        type_of_id(id)
            .map(|kind| self.referencing(kind))
            .unwrap_or_default()
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

/// The one-to-one targets a write gives its entities, for the `target-taken` read. Run after
/// [`targets_of`] accepted the payload; the payload is not changed.
pub fn claims_of(payload: &Value, shape: Shape<'_>, rules: &RelationshipRules) -> Vec<Claim> {
    let mut claims = Vec::new();
    if rules.is_empty() {
        return claims;
    }
    let mut claim = |entity: &Value| {
        let Some(object) = entity.as_object() else {
            return;
        };
        let holder = object
            .get("id")
            .or_else(|| object.get("@id"))
            .and_then(Value::as_str)
            .or(shape.addressed)
            .map(str::to_owned);
        for entity_type in types_of(object, shape.addressed) {
            let Some((class, ends)) = rules.of_class(entity_type) else {
                continue;
            };
            for (slot, end) in ends.iter().filter(|(_, end)| end.unique) {
                let named = object.get(slot).and_then(objects).unwrap_or_default();
                claims.extend(named.into_iter().map(|urn| Claim {
                    class: class.to_owned(),
                    slot: slot.clone(),
                    object: urn,
                    target: end.target.clone(),
                    holder: holder.clone(),
                }));
            }
        }
    };
    if let Some(attribute) = shape.targeted {
        claim(&serde_json::json!({ attribute: payload }));
        return claims;
    }
    match payload {
        Value::Array(entities) => entities.iter().for_each(&mut claim),
        entity => claim(entity),
    }
    claims
}

/// The claim of `claims` that another holder in the same write already made: two entities of one
/// batch cannot both take a one-to-one target.
pub fn claimed_twice(claims: &[Claim]) -> Option<&Claim> {
    let mut seen: BTreeMap<(&str, &str, &str), Option<&str>> = BTreeMap::new();
    claims.iter().find(|claim| {
        let key = (
            claim.class.as_str(),
            claim.slot.as_str(),
            claim.object.as_str(),
        );
        match seen.get(&key) {
            Some(first) => *first != claim.holder.as_deref() || claim.holder.is_none(),
            None => {
                seen.insert(key, claim.holder.as_deref());
                false
            }
        }
    })
}

/// What a delete of `deleted` does to one entity that stores it on `slot` (DM-66, DM-71).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Unlink {
    /// The delete is refused: `restrict`, or a rule that would leave a required end empty.
    Refused,
    /// The entity goes with its target: `cascade` on a single end.
    DeleteEntity,
    /// The whole attribute goes: `set-null` on a single end.
    DeleteAttribute,
    /// A many end loses the links to the target and keeps the others: the instances to delete
    /// (by `datasetId`, `None` for the default instance) and the ones whose `object` list keeps
    /// other targets, with what they keep.
    Instances {
        /// Instances whose only target is the deleted entity.
        removed: Vec<Option<String>>,
        /// Instances that name the deleted entity beside others, with the others.
        rewritten: Vec<(Option<String>, Vec<String>)>,
    },
}

/// Decides what a delete of `deleted` does to `entity`, which stores it on the end `slot` (read
/// with at least that attribute). Pure: the enforcement point reads the entity and applies it.
pub fn unlink(entity: &Value, slot: &str, end: &StoredEnd, deleted: &str) -> Unlink {
    if end.on_delete == OnDelete::Restrict {
        return Unlink::Refused;
    }
    if !end.many {
        return match end.on_delete {
            OnDelete::Cascade => Unlink::DeleteEntity,
            OnDelete::SetNull if end.required => Unlink::Refused,
            _ => Unlink::DeleteAttribute,
        };
    }
    let instances: Vec<&Value> = match entity.get(slot) {
        Some(Value::Array(instances)) => instances.iter().collect(),
        Some(instance) => vec![instance],
        None => Vec::new(),
    };
    let mut removed = Vec::new();
    let mut rewritten = Vec::new();
    let mut left = 0;
    for instance in instances {
        let dataset = instance
            .get("datasetId")
            .and_then(Value::as_str)
            .map(str::to_owned);
        let named = objects(instance).unwrap_or_default();
        let others: Vec<String> = named
            .iter()
            .filter(|urn| *urn != deleted)
            .cloned()
            .collect();
        left += others.len();
        if others.len() == named.len() {
            continue;
        }
        if others.is_empty() {
            removed.push(dataset);
        } else {
            rewritten.push((dataset, others));
        }
    }
    if end.required && left == 0 && !(removed.is_empty() && rewritten.is_empty()) {
        return Unlink::Refused;
    }
    Unlink::Instances { removed, rewritten }
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

    // --- target-taken and the delete rules (T-2858) ------------------------------------------

    const DESK: &str = "urn:ngsi-ld:Desk:hel.fi:schools:d1";
    const TEACHER: &str = "urn:ngsi-ld:Teacher:hel.fi:schools:t1";
    const OTHER_TEACHER: &str = "urn:ngsi-ld:Teacher:hel.fi:schools:t2";

    /// A Teacher has one Desk (one-to-one, set-null), belongs to a School (required, cascade),
    /// and teaches Courses (many-to-many, cascade, required).
    fn staff() -> RelationshipRules {
        let end = |target: &str, cardinality: &str, on_delete: &str, unique: bool| {
            json!({ "target": target, "inverse": "x", "cardinality": cardinality,
                    "onDelete": on_delete, "unique": unique })
        };
        RelationshipRules::from_schema(&json!({ "definitions": {
            "Teacher": {
                "required": ["school", "courses"],
                "properties": {
                    "desk": { "type": "string", "x-ngsi-ld-relationship":
                        end("Desk", "one-to-one", "set-null", true) },
                    "school": { "type": "string", "x-ngsi-ld-relationship":
                        end("School", "many-to-one", "cascade", false) },
                    "courses": { "type": "array", "items": { "type": "string" },
                        "x-ngsi-ld-relationship": end("Course", "many-to-many", "cascade", false) }
                }
            },
            "Room": {
                "properties": {
                    "desks": { "type": "array", "items": { "type": "string" },
                        "x-ngsi-ld-relationship": end("Desk", "many-to-many", "bogus", false) }
                }
            }
        } }))
    }

    fn end_of<'a>(rules: &'a RelationshipRules, class: &str, slot: &str) -> &'a StoredEnd {
        &rules.of_class(class).expect("the class relates").1[slot]
    }

    #[test]
    fn unique_and_the_delete_rule_are_read_and_an_unknown_rule_restricts() {
        // The T-2740 rules state no delete rule: restrict, the model's default (DM-66).
        assert_eq!(
            end_of(&rules(), "User", "school").on_delete,
            OnDelete::Restrict
        );
        let rules = staff();
        assert!(end_of(&rules, "Teacher", "desk").unique);
        assert_eq!(
            end_of(&rules, "Teacher", "desk").on_delete,
            OnDelete::SetNull
        );
        assert_eq!(
            end_of(&rules, "Teacher", "school").on_delete,
            OnDelete::Cascade
        );
        assert!(!end_of(&rules, "Teacher", "school").unique);
        assert_eq!(
            end_of(&rules, "Room", "desks").on_delete,
            OnDelete::Restrict
        );
    }

    #[test]
    fn the_ends_that_point_at_a_type_are_every_class_that_stores_one() {
        let rules = staff();
        let desk: Vec<(&str, &str)> = rules
            .referencing("Desk")
            .iter()
            .map(|at| (at.class, at.slot))
            .collect();
        assert_eq!(desk, vec![("Room", "desks"), ("Teacher", "desk")]);
        assert!(
            rules.referencing("https://example.org/Desk").len() == 2,
            "an expanded type"
        );
        assert!(rules.referencing("Teacher").is_empty());
    }

    #[test]
    fn a_write_claims_only_its_one_to_one_targets_with_its_own_id() {
        let rules = staff();
        let teacher = json!({ "id": TEACHER, "type": "Teacher",
            "desk": { "type": "Relationship", "object": DESK },
            "school": { "type": "Relationship", "object": SCHOOL } });
        let claims = claims_of(&teacher, whole(), &rules);
        assert_eq!(claims.len(), 1, "{claims:?}");
        assert_eq!(claims[0].slot, "desk");
        assert_eq!(claims[0].object, DESK);
        assert_eq!(claims[0].holder.as_deref(), Some(TEACHER));
        assert_eq!(claims[0].taken().rule, "target-taken");

        // A fragment for one attribute is claimed by the entity of the path.
        let shape = Shape {
            addressed: Some(TEACHER),
            targeted: Some("desk"),
            extent: Extent::Partial,
        };
        let claims = claims_of(
            &json!({ "type": "Relationship", "object": DESK }),
            shape,
            &rules,
        );
        assert_eq!(claims[0].holder.as_deref(), Some(TEACHER));
        // Unlinking with NGSI-LD's null claims nothing.
        assert!(claims_of(
            &json!({ "id": TEACHER, "type": "Teacher", "desk": "urn:ngsi-ld:null" }),
            partial(TEACHER),
            &rules
        )
        .is_empty());
    }

    #[test]
    fn two_entities_of_one_batch_cannot_both_take_a_target() {
        let rules = staff();
        let teacher = |id: &str| {
            json!({ "id": id, "type": "Teacher",
            "desk": { "type": "Relationship", "object": DESK } })
        };
        let batch = json!([teacher(TEACHER), teacher(OTHER_TEACHER)]);
        let claims = claims_of(&batch, whole(), &rules);
        let twice = claimed_twice(&claims).expect("the second takes it again");
        assert_eq!(twice.holder.as_deref(), Some(OTHER_TEACHER));
        // One entity naming its own target twice is no conflict.
        let same = json!([teacher(TEACHER), teacher(TEACHER)]);
        assert!(claimed_twice(&claims_of(&same, whole(), &rules)).is_none());
    }

    #[test]
    fn a_delete_rule_decides_what_happens_to_each_referencing_entity() {
        let rules = staff();
        let teacher = json!({ "id": TEACHER, "type": "Teacher",
            "desk": { "type": "Relationship", "object": DESK } });
        // set-null on an optional single end removes the attribute.
        assert_eq!(
            unlink(&teacher, "desk", end_of(&rules, "Teacher", "desk"), DESK),
            Unlink::DeleteAttribute
        );
        // cascade on a single end takes the entity.
        assert_eq!(
            unlink(
                &teacher,
                "school",
                end_of(&rules, "Teacher", "school"),
                SCHOOL
            ),
            Unlink::DeleteEntity
        );
        // restrict refuses whatever the entity holds.
        assert_eq!(
            unlink(&json!({}), "desks", end_of(&rules, "Room", "desks"), DESK),
            Unlink::Refused
        );
        // set-null on a required single end would empty it: refused as restrict is.
        let mut required = end_of(&rules, "Teacher", "desk").clone();
        required.required = true;
        assert_eq!(unlink(&teacher, "desk", &required, DESK), Unlink::Refused);
    }

    #[test]
    fn a_many_end_loses_only_the_link_to_the_deleted_target() {
        let rules = staff();
        let courses = end_of(&rules, "Teacher", "courses");
        let teacher = json!({ "id": TEACHER, "type": "Teacher", "courses": [
            { "type": "Relationship", "object": COURSE, "datasetId": "urn:ngsi-ld:Dataset:c1" },
            { "type": "Relationship", "object": [COURSE, "urn:ngsi-ld:Course:hel.fi:schools:c3"] },
            { "type": "Relationship", "object": "urn:ngsi-ld:Course:hel.fi:schools:c4",
              "datasetId": "urn:ngsi-ld:Dataset:c4" }
        ] });
        assert_eq!(
            unlink(&teacher, "courses", courses, COURSE),
            Unlink::Instances {
                removed: vec![Some("urn:ngsi-ld:Dataset:c1".into())],
                rewritten: vec![(None, vec!["urn:ngsi-ld:Course:hel.fi:schools:c3".into()])],
            }
        );
        // The last course of a required many end cannot go.
        let last = json!({ "courses": { "type": "Relationship", "object": COURSE } });
        assert_eq!(unlink(&last, "courses", courses, COURSE), Unlink::Refused);
        // An entity that no longer names the target (it moved meanwhile) needs nothing.
        let moved = json!({ "courses": { "type": "Relationship", "object": "urn:ngsi-ld:Course:hel.fi:schools:c9" } });
        assert_eq!(
            unlink(&moved, "courses", courses, COURSE),
            Unlink::Instances {
                removed: vec![],
                rewritten: vec![]
            }
        );
    }
}
