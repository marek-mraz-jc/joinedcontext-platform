//! T-0135: `kind: Blueprint` (CC-23, CC-24, CC-26, CC-27, CC-59).

use jc_core::error::Error;
use jc_core::kinds::blueprint::RiskClass;
use jc_core::kinds::Blueprint;

/// Verbatim from docs/Development/05-blueprints.md, trimmed to one template.
const GOLDEN: &str = r#"apiVersion: joinedcontext.com/v1alpha1
kind: Blueprint
metadata:
  name: threshold-alert
  namespace: org
  title: { en: "Threshold Alert Subscription", sk: "Notifikácia prekročenia limitu" }
spec:
  version: 1.2.0
  category: alerting
  riskClass: green
  allowedRoles: [domain-editor, approver, org-admin]
  parameterSchema:
    type: object
    required: [entityType, thresholdValue]
    properties:
      entityType: { type: string, title: "Target entity type" }
      thresholdValue: { type: number, title: "Alert threshold" }
  templates:
    - name: subscription
      template: |
        apiVersion: joinedcontext.com/v1alpha1
        kind: Subscription
        metadata:
          name: "alert-{{ entityType | lower }}"
"#;

fn with_spec(replace: &str, with: &str) -> Result<Blueprint, Error> {
    let yaml = GOLDEN.replace(replace, with);
    let manifest = Blueprint::from_yaml(&yaml).map_err(|e| Error::Parse(e.to_string()))?;
    manifest.validate()?;
    Ok(manifest)
}

#[test]
fn the_documented_blueprint_parses_and_validates() {
    let blueprint = with_spec("", "").expect("the golden blueprint validates");
    assert_eq!(blueprint.spec.version.as_str(), "1.2.0");
    assert_eq!(blueprint.spec.risk_class, RiskClass::Green);
    assert_eq!(blueprint.spec.category.as_deref(), Some("alerting"));
    assert_eq!(blueprint.spec.templates.len(), 1);
}

#[test]
fn a_blueprint_nobody_may_run_is_refused() {
    let err = with_spec(
        "  allowedRoles: [domain-editor, approver, org-admin]\n",
        "  allowedRoles: []\n",
    )
    .expect_err("empty allowedRoles is refused");
    assert!(err.to_string().contains("allowedRoles"), "{err}");
}

#[test]
fn a_parameter_schema_that_is_not_an_object_schema_is_refused() {
    // A form is built from the properties of one object; an array schema has none (CC-24).
    let err = with_spec("    type: object\n", "    type: array\n")
        .expect_err("a non-object parameter schema is refused");
    assert!(err.to_string().contains("parameterSchema"), "{err}");
}

#[test]
fn two_templates_may_not_share_a_name() {
    let err = with_spec(
        "  templates:\n",
        "  templates:\n    - { name: subscription, template: \"kind: Subscription\" }\n",
    )
    .expect_err("duplicate template names are refused");
    assert!(err.to_string().contains("unique"), "{err}");
}

#[test]
fn a_blueprint_that_expands_to_nothing_is_refused() {
    let yaml = GOLDEN
        .split("  templates:")
        .next()
        .expect("golden has a templates block")
        .to_string()
        + "  templates: []\n";
    let manifest = Blueprint::from_yaml(&yaml).expect("parses");
    let err = manifest.validate().expect_err("no template is refused");
    assert!(err.to_string().contains("templates"), "{err}");
}

#[test]
fn an_unknown_spec_member_is_refused_rather_than_ignored() {
    // A blueprint carries no secret and no host access; a typo must not slip through as one.
    let err = with_spec("  category: alerting\n", "  categorie: alerting\n")
        .expect_err("unknown members are refused");
    assert!(matches!(err, Error::Parse(_)), "{err}");
}
