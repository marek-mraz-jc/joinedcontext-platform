//! T-0135: blueprint expansion is deterministic, sandboxed and refuses bad parameters
//! (CC-23, CC-24, CC-25, CC-27, ADR-N-005).

use jc_core::kinds::Blueprint;
use jcctl::blueprints::{expand, ExpandError, ANNOTATION_PARAMETERS};
use serde_json::json;

const BLUEPRINT: &str = r#"apiVersion: joinedcontext.com/v1alpha1
kind: Blueprint
metadata:
  name: threshold-alert
  namespace: org
spec:
  version: 1.2.0
  riskClass: green
  allowedRoles: [domain-editor]
  parameterSchema:
    type: object
    required: [entityType, thresholdValue]
    additionalProperties: false
    properties:
      entityType: { type: string, enum: [AirQualityObserved, NoiseLevelObserved] }
      thresholdValue: { type: number, minimum: 0 }
  templates:
    - name: subscription
      template: |
        apiVersion: joinedcontext.com/v1alpha1
        kind: Subscription
        metadata:
          name: "alert-{{ entityType | lower }}"
        spec:
          q: "airQualityIndex > {{ thresholdValue }}"
    - name: seed-entity
      template: |
        apiVersion: joinedcontext.com/v1alpha1
        kind: Entity
        metadata:
          name: "alert-target"
          annotations:
            joinedcontext.com/managed-attributes: "type"
        spec:
          type: "{{ entityType }}"
"#;

fn blueprint() -> Blueprint {
    Blueprint::from_yaml(BLUEPRINT).expect("the fixture blueprint parses")
}

fn parameters() -> serde_json::Value {
    json!({ "entityType": "AirQualityObserved", "thresholdValue": 50 })
}

#[test]
fn the_same_parameters_render_byte_identical_manifests() {
    let blueprint = blueprint();
    let first = expand(&blueprint, &parameters()).expect("expands");
    let second = expand(&blueprint, &parameters()).expect("expands");
    assert_eq!(first, second, "expansion is not deterministic (CC-25)");
    assert_eq!(first.len(), 2, "one manifest per template");
    assert_eq!(first[0].template, "subscription");
    assert!(first[0].manifest.contains("alert-airqualityobserved"));
    assert!(first[1].manifest.contains("type: AirQualityObserved"));
}

#[test]
fn parameters_written_in_another_order_still_render_the_same_bytes() {
    // The provenance annotation is canonical JSON, so a form that submits its fields in a
    // different order must not produce a diff (CC-25, CC-27).
    let blueprint = blueprint();
    let ordered = expand(&blueprint, &parameters()).expect("expands");
    let reordered = expand(
        &blueprint,
        &json!({ "thresholdValue": 50, "entityType": "AirQualityObserved" }),
    )
    .expect("expands");
    assert_eq!(ordered, reordered);
}

#[test]
fn every_manifest_records_the_blueprint_it_came_from() {
    let expanded = expand(&blueprint(), &parameters()).expect("expands");
    for manifest in &expanded {
        let parsed: serde_json::Value =
            serde_norway::from_str(&manifest.manifest).expect("rendered manifest parses");
        let annotations = &parsed["metadata"]["annotations"];
        assert_eq!(
            annotations["joinedcontext.com/blueprint"],
            "threshold-alert"
        );
        assert_eq!(annotations["joinedcontext.com/blueprint-version"], "1.2.0");
        assert_eq!(
            annotations[ANNOTATION_PARAMETERS],
            r#"{"entityType":"AirQualityObserved","thresholdValue":50}"#
        );
    }
}

#[test]
fn provenance_does_not_overwrite_the_templates_own_annotations() {
    let expanded = expand(&blueprint(), &parameters()).expect("expands");
    let seed: serde_json::Value = serde_norway::from_str(&expanded[1].manifest).expect("parses");
    assert_eq!(
        seed["metadata"]["annotations"]["joinedcontext.com/managed-attributes"],
        "type"
    );
}

/// A parameter may name a picker the Portal renders instead of a fixed enum (CC-24,
/// Development/05 section 2.1). The hint is a keyword draft-07 does not know, so expansion has
/// to ignore it: a blueprint whose form is friendlier must not become a blueprint that refuses
/// to expand.
#[test]
fn a_picker_hint_is_ignored_by_the_expansion_that_validates_the_value() {
    const HINTED: &str = r#"apiVersion: joinedcontext.com/v1alpha1
kind: Blueprint
metadata:
  name: new-dashboard
  namespace: org
spec:
  version: 0.4.0
  riskClass: green
  allowedRoles: [domain-editor]
  parameterSchema:
    type: object
    required: [space]
    additionalProperties: false
    properties:
      space:
        type: string
        x-jc-widget: resourcePicker
        x-jc-options: { plural: spaces }
  templates:
    - name: dashboard
      template: |
        apiVersion: joinedcontext.com/v1alpha1
        kind: Dashboard
        metadata:
          name: "over-{{ space }}"
        spec:
          space: "{{ space }}"
"#;
    let hinted = Blueprint::from_yaml(HINTED).expect("the hinted blueprint parses");
    let expanded = expand(&hinted, &json!({ "space": "ovzdusie" })).expect("expands");
    assert_eq!(expanded.len(), 1);
    assert!(expanded[0].manifest.contains("over-ovzdusie"));

    // The hint decides nothing: the value is still judged by the schema around it.
    let refused = expand(&hinted, &json!({ "space": 7 })).expect_err("a number is not a name");
    assert!(
        matches!(refused, ExpandError::Parameters(_)),
        "expected the parameters to be refused, got {refused:?}"
    );
}

#[test]
fn a_value_outside_the_schema_is_refused_with_every_violation_at_once() {
    let err = expand(
        &blueprint(),
        &json!({ "entityType": "TrafficFlowObserved", "thresholdValue": -1 }),
    )
    .expect_err("an enum miss and a minimum miss are both refused");
    let ExpandError::Parameters(violations) = err else {
        panic!("expected a parameter rejection, got {err}");
    };
    assert_eq!(violations.len(), 2, "{violations:?}");
    assert!(violations.iter().any(|v| v.contains("/entityType")));
    assert!(violations.iter().any(|v| v.contains("/thresholdValue")));
}

#[test]
fn a_missing_required_parameter_is_refused() {
    let err = expand(&blueprint(), &json!({ "entityType": "AirQualityObserved" }))
        .expect_err("a missing required parameter is refused");
    assert!(matches!(err, ExpandError::Parameters(_)), "{err}");
}

#[test]
fn an_unknown_parameter_is_refused_rather_than_ignored() {
    // additionalProperties: false in the blueprint's schema is what makes the parameter set
    // the complete surface (CC-24); a typo must not disappear silently.
    let err = expand(
        &blueprint(),
        &json!({ "entityType": "AirQualityObserved", "thresholdValue": 50, "treshold": 1 }),
    )
    .expect_err("an unknown parameter is refused");
    assert!(matches!(err, ExpandError::Parameters(_)), "{err}");
}

#[test]
fn a_template_that_reads_an_undefined_variable_fails_instead_of_rendering_a_hole() {
    let yaml = BLUEPRINT.replace("{{ entityType | lower }}", "{{ entitytype | lower }}");
    let blueprint = Blueprint::from_yaml(&yaml).expect("parses");
    let err = expand(&blueprint, &parameters()).expect_err("strict undefined (ADR-N-005)");
    let ExpandError::Render { template, .. } = err else {
        panic!("expected a render failure, got {err}");
    };
    assert_eq!(template, "subscription");
}

#[test]
fn a_template_may_not_read_the_filesystem() {
    // No loader is installed, so include/extends have nowhere to reach: a blueprint cannot
    // pull a file off the reconciler's host (ADR-N-005).
    let yaml = BLUEPRINT.replace(
        "        spec:\n          q: \"airQualityIndex > {{ thresholdValue }}\"\n",
        "        spec:\n          q: \"{% include '/etc/passwd' %}\"\n",
    );
    let blueprint = Blueprint::from_yaml(&yaml).expect("parses");
    let err = expand(&blueprint, &parameters()).expect_err("include is not available");
    assert!(matches!(err, ExpandError::Render { .. }), "{err}");
}

#[test]
fn a_template_that_renders_something_other_than_a_manifest_is_refused() {
    let yaml = BLUEPRINT.replace(
        "        apiVersion: joinedcontext.com/v1alpha1\n        kind: Subscription\n",
        "        just: a mapping\n",
    );
    let blueprint = Blueprint::from_yaml(&yaml).expect("parses");
    let err = expand(&blueprint, &parameters()).expect_err("not a manifest");
    assert!(matches!(err, ExpandError::NotAManifest { .. }), "{err}");
}
