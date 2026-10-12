//! A space's quality rules on every write through an Endpoint (DM-87..DM-93, ADR-N-046 2.5).
//!
//! The verdict is `jc_core::quality`'s, the one the Portal's sweep and rejected route reach on
//! the same entity: a rejecting rule refuses the write naming the rule and the attribute, never
//! the value; a flag or a quarantine is written into the payload before it is forwarded. Every
//! check is counted in `jc_quality_checks_total`. Nothing here reads the network or the store.

use crate::units::{types_of, Shape};
use chrono::{DateTime, Utc};
use jc_core::error::ProblemDetails;
use jc_core::kinds::{QualityOutcome, QualityRule};
use jc_core::quality::{mark, on_write, rejection};
use serde_json::{json, Value};

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
    mark(entity, &verdicts, now);
    Ok(())
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
