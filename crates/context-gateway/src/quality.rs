//! A space's quality rules on every write through an Endpoint (DM-87..DM-93, ADR-N-046 2.5),
//! and its model's slot constraints (DM-86).
//!
//! The constraints are `jc_manifest::model_schema`'s, the compilation the Portal's pipeline stage
//! checks a record against: a write breaking one is refused as `quality-rule` naming the SHACL
//! component and the attribute, never the value.
//!
//! The verdict is `jc_core::quality`'s, the one the Portal's sweep and rejected route reach on
//! the same entity: a rejecting rule refuses the write naming the rule and the attribute, never
//! the value; a flag or a quarantine is written into the payload before it is forwarded. Every
//! check is counted in `jc_quality_checks_total`. Nothing here reads the network or the store.

use crate::units::Shape;
use chrono::{DateTime, Utc};
use jc_core::error::ProblemDetails;
use jc_core::kinds::{QualityOutcome, QualityRule};
use jc_core::quality::{mark, on_write, rejection, QUARANTINED_AT, QUARANTINE_RULES};
use jc_core::system_members::{is_entity_system_member, INSTANT_MEMBERS};
use jc_core::types_of;
use jc_manifest::model_schema::{ModelSchema, Problem};
use serde_json::{json, Map, Value};

/// Every quality check, by space, type, rule and outcome (DM-93).
pub const CHECKS: &str = "jc_quality_checks_total";

fn outcome(broken: Option<QualityOutcome>) -> &'static str {
    match broken {
        None => "pass",
        Some(QualityOutcome::Reject) => "reject",
        Some(QualityOutcome::Flag) => "flag",
        Some(QualityOutcome::Quarantine) => "quarantine",
    }
}

/// The refusal of a write `rule` rejects (DM-88): the slug `quality-rule`, the rule's and the
/// attribute's names in the detail and as members, the value nowhere.
pub fn refusal(rule: &QualityRule) -> ProblemDetails {
    let mut problem =
        ProblemDetails::new(400, "quality-rule", "Quality Rule Broken").with_detail(format!(
            "attribute `{}` of `{}` breaks the space's quality rule `{}` (DM-88)",
            rule.attribute, rule.entity_type, rule.name
        ));
    problem.extensions.insert("rule".into(), json!(rule.name));
    problem
        .extensions
        .insert("field".into(), json!(rule.attribute));
    problem
}

/// The attributes only the gateway writes (DM-90): the quarantine mark. A writer who could set
/// them would hide good data; one who could delete them would make quarantined data public.
pub const OWNED: [&str; 2] = [QUARANTINED_AT, QUARANTINE_RULES];

/// Whether `name` is an owned attribute: its term, or an IRI or compact IRI ending in it, so
/// the expanded `…/default-context/quarantinedAt` is the same attribute.
pub fn is_owned(name: &str) -> bool {
    OWNED.iter().any(|owned| {
        name.strip_suffix(owned)
            .is_some_and(|head| head.is_empty() || head.ends_with(['/', '#', ':']))
    })
}

/// The refusal of a write whose path addresses an attribute the gateway owns, a `DELETE`
/// included, the attribute written as a term or a percent-encoded IRI; `None` otherwise.
pub fn owned_path(targeted: Option<&str>) -> Option<ProblemDetails> {
    let attribute = targeted
        .map(crate::query::decode)
        .filter(|attribute| is_owned(attribute))?;
    Some(ProblemDetails::bad_request().with_detail(format!(
        "`{attribute}` is written by the space's quality rules alone; a write may not set or \
         delete it (DM-90)"
    )))
}

/// Takes every attribute the gateway owns off the entities of a write body, a batch's
/// included; `true` when one was there. A body addressed to one attribute is that attribute's
/// value, not an entity, and is left as it is.
pub fn strip_owned(payload: &mut Value) -> bool {
    let mut stripped = false;
    let mut strip = |entity: &mut Value| {
        if let Some(object) = entity.as_object_mut() {
            let before = object.len();
            object.retain(|name, _| !is_owned(name));
            stripped |= object.len() != before;
        }
    };
    match payload {
        Value::Array(entities) => entities.iter_mut().for_each(&mut strip),
        entity => strip(entity),
    }
    stripped
}

/// Judges every entity of a write payload against `rules` and marks what they flag or
/// quarantine. A rejected entity refuses the whole write, a batch included: nothing of it is
/// forwarded, and the refusal names the first rule broken.
pub fn enforce(
    payload: &mut Value,
    shape: Shape<'_>,
    rules: &[QualityRule],
    space: &str,
    now: DateTime<Utc>,
) -> Result<(), Box<ProblemDetails>> {
    if rules.is_empty() {
        return Ok(());
    }
    if let Some(attribute) = shape.targeted {
        let mut wrapped = json!({ attribute: std::mem::take(payload) });
        let judged = judge(&mut wrapped, shape, rules, space, now);
        *payload = wrapped
            .as_object_mut()
            .and_then(|object| object.remove(attribute))
            .unwrap_or(Value::Null);
        return judged;
    }
    match payload {
        Value::Array(entities) => {
            // A refused batch is forwarded not at all, so the members marked before the one
            // that refuses it change nothing.
            entities
                .iter_mut()
                .try_for_each(|entity| judge(entity, shape, rules, space, now))
        }
        entity => judge(entity, shape, rules, space, now),
    }
}

fn judge(
    entity: &mut Value,
    shape: Shape<'_>,
    rules: &[QualityRule],
    space: &str,
    now: DateTime<Utc>,
) -> Result<(), Box<ProblemDetails>> {
    let Some(object) = entity.as_object() else {
        return Ok(());
    };
    let types: Vec<String> = types_of(object, shape.addressed)
        .into_iter()
        .map(str::to_owned)
        .collect();
    // A fragment names its type only through the path's id: judge it as that type.
    let mut typed = entity.clone();
    if let Some(object) = typed.as_object_mut() {
        object.insert("type".into(), json!(types));
    }
    let verdicts = on_write(&typed, rules, now);
    for verdict in &verdicts {
        metrics::counter!(
            CHECKS,
            "space" => space.to_owned(),
            "type" => verdict.rule.entity_type.clone(),
            "rule" => verdict.rule.name.clone(),
            "outcome" => outcome(verdict.broken),
        )
        .increment(1);
    }
    if let Some(rule) = rejection(&verdicts) {
        tracing::info!(%space, rule = %rule.name, "write refused: it breaks a quality rule");
        return Err(Box::new(refusal(rule)));
    }
    // A write to one attribute's path carries that attribute alone, with nowhere to put the
    // entity's quarantine mark: it is refused, and the fragment route carries the mark.
    if shape.targeted.is_some() {
        if let Some(verdict) = verdicts
            .iter()
            .find(|verdict| verdict.broken == Some(QualityOutcome::Quarantine))
        {
            let mut problem = refusal(verdict.rule);
            problem.detail = Some(format!(
                "attribute `{}` of `{}` breaks the space's quality rule `{}`, which quarantines \
                 the entity; write it through PATCH /entities/{{id}}/attrs, which carries the \
                 mark (DM-90)",
                verdict.rule.attribute, verdict.rule.entity_type, verdict.rule.name
            ));
            return Err(Box::new(problem));
        }
    }
    mark(entity, &verdicts, now);
    Ok(())
}

/// The SHACL components of the slot constraints a write is refused for (DM-86): `required`,
/// `minimum_value` and `maximum_value`, `pattern`, an enum and `range`. The model's other words
/// are other checks' (a unit DM-06, a relationship DM-70, the type DM-61), or not the gateway's
/// to refuse (an attribute the model does not declare).
const SLOT_CONSTRAINTS: [&str; 6] = [
    "sh:minCount",
    "sh:minInclusive",
    "sh:maxInclusive",
    "sh:pattern",
    "sh:in",
    "sh:datatype",
];

/// The member that carries a value, and the kind of an attribute written with it but no `type`.
const MEMBERS: [(&str, &str); 4] = [
    ("value", "Property"),
    ("object", "Relationship"),
    ("languageMap", "LanguageProperty"),
    ("vocab", "VocabProperty"),
];

/// The refusal of a write that breaks a slot constraint of the space's model (DM-86).
pub fn model_refusal(model: &str, problem: &Problem) -> ProblemDetails {
    let mut refused =
        ProblemDetails::new(400, "quality-rule", "Quality Rule Broken").with_detail(format!(
            "{} ({} of the space's model `{model}`, DM-86)",
            problem.message, problem.rule
        ));
    refused
        .extensions
        .insert("rule".into(), json!(problem.rule));
    refused
        .extensions
        .insert("field".into(), json!(problem.path));
    refused
}

/// Checks every entity of a write payload against the slot constraints of the space's model.
/// A `whole` write (a create, a replace) carries the entity, so a required attribute must be
/// there; any other write is checked only for the attributes it carries. A batch is refused
/// whole by its first broken entity.
pub fn check_model(
    payload: &Value,
    shape: Shape<'_>,
    whole: bool,
    schema: &ModelSchema,
    space: &str,
) -> Result<(), Box<ProblemDetails>> {
    if let Some(attribute) = shape.targeted {
        let fragment = json!({ attribute: payload });
        return checked(&fragment, shape, false, schema, space);
    }
    match payload {
        Value::Array(entities) => entities
            .iter()
            .try_for_each(|entity| checked(entity, shape, whole, schema, space)),
        entity => checked(entity, shape, whole, schema, space),
    }
}

fn checked(
    entity: &Value,
    shape: Shape<'_>,
    whole: bool,
    schema: &ModelSchema,
    space: &str,
) -> Result<(), Box<ProblemDetails>> {
    let Some(object) = entity.as_object() else {
        return Ok(());
    };
    // A type written as an IRI or a CURIE is the class its local name names; a type the model
    // does not declare is DM-61's to refuse.
    let Some(class) = types_of(object, shape.addressed)
        .into_iter()
        .map(|t| t.rsplit(['/', '#', ':']).next().unwrap_or(t))
        .find(|t| schema.classes.contains_key(*t))
    else {
        return Ok(());
    };
    let id = object
        .get("id")
        .or_else(|| object.get("@id"))
        .and_then(Value::as_str)
        .or(shape.addressed)
        .unwrap_or_default();
    // The first instance of each attribute is checked with the entity, the others alone.
    let mut first = Map::new();
    let mut others: Vec<(&str, Value)> = Vec::new();
    // A system member is no attribute (DM-131); an instant among them is still held to its form.
    for (name, value) in object.iter().filter(|(name, _)| own_form_checked(name)) {
        if is_entity_system_member(name) {
            first.insert(name.clone(), value.clone());
            continue;
        }
        let mut instances = match value {
            Value::Array(many) if !many.is_empty() && many.iter().all(is_instance) => {
                many.iter().map(prepared).collect()
            }
            one => vec![prepared(one)],
        }
        .into_iter();
        first.extend(instances.next().map(|one| (name.clone(), one)));
        others.extend(instances.map(|one| (name.as_str(), one)));
    }
    let carried: Vec<String> = first.keys().cloned().collect();
    let mut problems = broken(schema, class, id, first, |path| {
        whole || carried.iter().any(|c| c == path)
    });
    for (name, instance) in others {
        let alone = Map::from_iter([(name.to_owned(), instance)]);
        problems.extend(broken(schema, class, id, alone, |path| path == name));
    }
    let refused = problems.first();
    metrics::counter!(
        CHECKS,
        "space" => space.to_owned(),
        "type" => class.to_owned(),
        "rule" => refused.map_or_else(|| "model".to_owned(), |p| p.rule.clone()),
        "outcome" => if refused.is_some() { "reject" } else { "pass" },
    )
    .increment(1);
    match refused {
        Some(problem) => {
            tracing::info!(%space, rule = %problem.rule, field = %problem.path, "write refused: it breaks a slot constraint of the space's model");
            Err(Box::new(model_refusal(&schema.model, problem)))
        }
        None => Ok(()),
    }
}

/// The slot constraints `attributes` of an entity of `class` break, at the paths `kept` keeps.
fn broken(
    schema: &ModelSchema,
    class: &str,
    id: &str,
    attributes: Map<String, Value>,
    kept: impl Fn(&str) -> bool,
) -> Vec<Problem> {
    let mut record = attributes;
    record.insert("id".into(), json!(id));
    record.insert("type".into(), json!(class));
    let normalized = schema.normalize(class, &Value::Object(record));
    schema
        .check(&normalized)
        .into_iter()
        .filter(|p| SLOT_CONSTRAINTS.contains(&p.rule.as_str()))
        .filter(|p| own_form_checked(&p.path) && kept(&p.path))
        .collect()
}

/// Whether a member of an entity is checked: an attribute, or a system member holding an instant
/// (DM-131). The other system members are no attribute and never refused as one.
fn own_form_checked(name: &str) -> bool {
    !is_entity_system_member(name) || INSTANT_MEMBERS.contains(&name)
}

/// Whether an array member is an instance of a normalized attribute (one per `datasetId`)
/// rather than an element of a list value.
fn is_instance(value: &Value) -> bool {
    value.as_object().is_some_and(|object| {
        object.contains_key("type")
            || MEMBERS
                .iter()
                .any(|(member, _)| object.contains_key(*member))
    })
}

/// An attribute as the check reads it: one written with its value member but no `type` gets the
/// kind that member says, and a typed literal (`{"@type": …, "@value": …}`), bare or as the
/// `value`, is its value.
fn prepared(value: &Value) -> Value {
    // A concise attribute written as a typed literal is that literal's value.
    let mut value = match value.get("@value") {
        Some(inner) if value.get("type").is_none() => inner.clone(),
        _ => value.clone(),
    };
    if let Some(object) = value.as_object_mut() {
        if !object.contains_key("type") {
            if let Some((_, kind)) = MEMBERS
                .iter()
                .find(|(member, _)| object.contains_key(*member))
            {
                object.insert("type".into(), json!(kind));
            }
        }
        if let Some(literal) = object.get_mut("value") {
            if let Some(inner) = literal.get("@value").cloned() {
                *literal = inner;
            }
        }
    }
    value
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rules() -> Vec<QualityRule> {
        serde_json::from_value(json!([
            {"name": "speed-plausible", "entityType": "Vehicle", "kind": "range", "attribute": "speed", "max": 33.4},
            {"name": "speed-sure", "entityType": "Vehicle", "kind": "range", "attribute": "speed", "min": 0, "outcome": "reject"}
        ]))
        .expect("rules")
    }

    const WHOLE: Shape<'static> = Shape {
        addressed: None,
        targeted: None,
    };

    // DM-90: the quarantine mark is the gateway's, however a writer spells it.
    #[test]
    fn a_writer_can_neither_set_nor_delete_the_quarantine_mark() {
        for owned in [
            "quarantinedAt",
            "quarantineRules",
            "https%3A%2F%2Furi.etsi.org%2Fngsi-ld%2Fdefault-context%2FquarantinedAt",
            "jc:quarantineRules",
        ] {
            assert!(owned_path(Some(owned)).is_some(), "{owned}");
        }
        for free in ["speed", "notquarantinedAt", "quarantinedAtX"] {
            assert!(owned_path(Some(free)).is_none(), "{free}");
        }
        assert!(owned_path(None).is_none());

        let mut batch = json!([
            {"id": "urn:ngsi-ld:Vehicle:1", "type": "Vehicle", "quarantinedAt": {"type": "Property", "value": "x"}},
            {"id": "urn:ngsi-ld:Vehicle:2", "type": "Vehicle",
             "https://uri.etsi.org/ngsi-ld/default-context/quarantineRules": {"type": "Property", "value": []}}
        ]);
        assert!(strip_owned(&mut batch));
        assert_eq!(batch[0].as_object().map(|o| o.len()), Some(2));
        assert_eq!(batch[1].as_object().map(|o| o.len()), Some(2));
        let mut clean = json!({"speed": {"type": "Property", "value": 3}});
        assert!(!strip_owned(&mut clean));
    }

    // DM-90: a one-attribute write that a rule quarantines is refused, never stored unmarked.
    #[test]
    fn a_one_attribute_write_that_quarantines_is_refused() {
        let rules: Vec<QualityRule> = serde_json::from_value(json!([
            {"name": "speed-hide", "entityType": "Vehicle", "kind": "range", "attribute": "speed", "max": 33.4, "outcome": "quarantine"}
        ]))
        .expect("rules");
        let targeted = Shape {
            addressed: Some("urn:ngsi-ld:Vehicle:1"),
            targeted: Some("speed"),
        };
        let mut value = json!({"type": "Property", "value": 90});
        let problem = enforce(&mut value, targeted, &rules, "bikes", Utc::now())
            .expect_err("quarantined on the attribute path");
        assert!(problem
            .detail
            .unwrap_or_default()
            .contains("PATCH /entities/{id}/attrs"));
        let mut fine = json!({"type": "Property", "value": 9});
        enforce(&mut fine, targeted, &rules, "bikes", Utc::now()).expect("within the rule");
        assert_eq!(fine, json!({"type": "Property", "value": 9}));
    }

    #[test]
    fn a_rejected_value_is_refused_by_rule_and_attribute_never_by_value() {
        let mut entity = json!({"id": "urn:ngsi-ld:Vehicle:1", "type": "Vehicle", "speed": {"type": "Property", "value": -7.25}});
        let problem =
            enforce(&mut entity, WHOLE, &rules(), "bikes", Utc::now()).expect_err("min 0");
        assert_eq!(problem.status, 400);
        assert!(
            problem.type_uri.ends_with("/quality-rule"),
            "{}",
            problem.type_uri
        );
        assert_eq!(problem.extensions["rule"], "speed-sure");
        assert_eq!(problem.extensions["field"], "speed");
        let shown = serde_json::to_string(&problem).expect("json");
        assert!(
            !shown.contains("-7.25"),
            "the value is never echoed: {shown}"
        );
    }

    #[test]
    fn a_flag_is_written_into_a_fragment_and_a_batch_refuses_whole() {
        let mut fragment = json!({"speed": {"type": "Property", "value": 40}});
        let addressed = Shape {
            addressed: Some("urn:ngsi-ld:Vehicle:1"),
            targeted: None,
        };
        enforce(&mut fragment, addressed, &rules(), "bikes", Utc::now()).expect("flags");
        assert_eq!(
            fragment["speed"]["qualityFlags"]["value"],
            json!(["speed-plausible"])
        );
        assert!(
            fragment.get("type").is_none(),
            "the fragment keeps its shape"
        );

        let mut bare = json!(50);
        let targeted = Shape {
            addressed: Some("urn:ngsi-ld:Vehicle:1"),
            targeted: Some("speed"),
        };
        enforce(&mut bare, targeted, &rules(), "bikes", Utc::now()).expect("flags");
        assert_eq!(bare["value"], json!(50));
        assert_eq!(bare["qualityFlags"]["value"], json!(["speed-plausible"]));

        let mut batch = json!([
            {"id": "urn:ngsi-ld:Vehicle:1", "type": "Vehicle", "speed": 3},
            {"id": "urn:ngsi-ld:Vehicle:2", "type": "Vehicle", "speed": -1}
        ]);
        assert!(enforce(&mut batch, WHOLE, &rules(), "bikes", Utc::now()).is_err());
    }

    #[test]
    fn a_space_without_rules_and_another_type_pass_untouched() {
        let mut entity = json!({"id": "urn:ngsi-ld:Stop:1", "type": "Stop", "speed": -1});
        let before = entity.clone();
        enforce(&mut entity, WHOLE, &rules(), "bikes", Utc::now()).expect("not a Vehicle");
        enforce(&mut entity, WHOLE, &[], "bikes", Utc::now()).expect("no rules");
        assert_eq!(entity, before);
    }
}
