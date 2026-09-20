//! T-0563: `ModelProjection`, the named subset of a model an Endpoint reads (MP-01, MP-02).
//!
//! What is worth a test is what the manifest refuses and what two projections make together:
//! an unknown class and an unknown slot both named at once, a class with no slots kept apart
//! from a class that is absent, and an intersection that is never wider than either side.

use jc_core::envelope::ResourceEnvelope;
use jc_core::kinds::endpoint::Audience;
use jc_core::kinds::{Endpoint, ModelProjectionSpec};
use jc_core::registry;
use std::collections::{BTreeMap, BTreeSet};

type Projection = ResourceEnvelope<ModelProjectionSpec>;

const PARTNER_VIEW: &str = r#"apiVersion: joinedcontext.com/v1alpha1
kind: ModelProjection
metadata:
  name: partner-view
  namespace: helsinki
spec:
  contextSpaceRef: fleet
  dataModelRef: { kind: DataModel, name: fleet, version: "3" }
  classes:
    - name: Vehicle
      slots: [id, name, type]
    - name: User
      slots: [id, name, type, age]
  filter:
    q: category=="public"
"#;

fn fleet() -> BTreeMap<String, BTreeSet<String>> {
    let slots = |names: &[&str]| names.iter().map(|s| s.to_string()).collect();
    BTreeMap::from([
        (
            "Vehicle".to_owned(),
            slots(&["id", "name", "type", "location", "speed", "maintenanceNote"]),
        ),
        (
            "User".to_owned(),
            slots(&["id", "name", "type", "age", "email", "badgeNumber"]),
        ),
    ])
}

#[test]
fn a_projection_round_trips_and_is_catalogued() {
    let parsed = Projection::from_yaml(PARTNER_VIEW).expect("parses");
    parsed.validate().expect("valid");
    assert_eq!(parsed.spec.data_model_ref.name, "fleet");
    assert_eq!(parsed.spec.data_model_ref.version, "3");
    assert_eq!(parsed.spec.classes.len(), 2);
    let yaml = parsed.to_yaml().expect("serializes");
    let again = Projection::from_yaml(&yaml).expect("parses again");
    assert_eq!(again, parsed);

    let info = registry::by_kind("ModelProjection").expect("catalogued");
    assert_eq!(info.plural, "projections");
    assert_eq!(
        info.repo_path("helsinki", "fleet", "partner-view"),
        "projects/helsinki/spaces/fleet/projections/partner-view.yaml"
    );
    assert!(registry::validate_yaml("ModelProjection", PARTNER_VIEW)
        .expect("known kind")
        .is_ok());
    assert!(registry::schema_of("ModelProjection").is_some());
}

#[test]
fn an_unknown_class_and_an_unknown_slot_are_both_refused_and_both_named() {
    let yaml = PARTNER_VIEW
        .replace("name: User", "name: Bicycle")
        .replace("slots: [id, name, type]", "slots: [id, name, type, colour]");
    let parsed = Projection::from_yaml(&yaml).expect("well formed");
    parsed.validate().expect("the shape is fine");
    let err = parsed.spec.check_against(&fleet()).expect_err("refused");
    let text = err.to_string();
    assert!(text.contains("Bicycle"), "{text}");
    assert!(text.contains("Vehicle.colour"), "{text}");
    assert!(text.contains("MP-01"), "{text}");
    Projection::from_yaml(PARTNER_VIEW)
        .expect("parses")
        .spec
        .check_against(&fleet())
        .expect("every name is in the model");
}

#[test]
fn an_unknown_field_an_empty_class_list_and_an_empty_filter_are_refused() {
    let unknown = PARTNER_VIEW.replace("  filter:\n", "  hidden: true\n  filter:\n");
    assert!(
        Projection::from_yaml(&unknown).is_err(),
        "deny_unknown_fields"
    );

    let empty = PARTNER_VIEW.replace(
        "  classes:\n    - name: Vehicle\n      slots: [id, name, type]\n    - name: User\n      slots: [id, name, type, age]\n",
        "  classes: []\n",
    );
    let err = Projection::from_yaml(&empty)
        .expect("parses")
        .validate()
        .expect_err("a projection of nothing");
    assert!(err.to_string().contains("at least one class"), "{err}");

    let blank = PARTNER_VIEW.replace("    q: category==\"public\"\n", "    q: \"  \"\n");
    let err = Projection::from_yaml(&blank)
        .expect("parses")
        .validate()
        .expect_err("an empty query");
    assert!(err.to_string().contains("filters nothing"), "{err}");
}

#[test]
fn two_projections_compose_as_an_intersection() {
    let partner = Projection::from_yaml(PARTNER_VIEW).expect("parses").spec;
    let other = Projection::from_yaml(
        &PARTNER_VIEW
            .replace("slots: [id, name, type]\n", "slots: [id, type, speed]\n")
            .replace("    - name: User\n      slots: [id, name, type, age]\n", "")
            .replace("q: category==\"public\"", "scopeQ: /helsinki/#"),
    )
    .expect("parses")
    .spec;
    let both = partner.intersect(&other);
    assert_eq!(
        both.classes.len(),
        1,
        "User is only on one side, so it is gone"
    );
    assert_eq!(both.classes[0].name, "Vehicle");
    assert_eq!(
        both.classes[0].slots,
        vec!["id", "type"],
        "name and speed are on one side each"
    );
    let filter = both.filter.expect("both filters kept");
    assert_eq!(filter.q.as_deref(), Some("category==\"public\""));
    assert_eq!(filter.scope_q.as_deref(), Some("/helsinki/#"));
    assert_eq!(
        other.intersect(&partner).classes,
        both.classes,
        "commutative"
    );
    assert_eq!(partner.intersect(&partner), partner, "idempotent");
}

#[test]
fn a_class_with_no_slots_and_an_absent_class_are_different() {
    let yaml = PARTNER_VIEW.replace("      slots: [id, name, type, age]\n", "      slots: []\n");
    let spec = Projection::from_yaml(&yaml).expect("parses").spec;
    spec.validate().expect("identity-only is a valid exposure");
    let user = spec.attributes_of("User").expect("exposed");
    assert_eq!(user, BTreeSet::from(["id".to_owned(), "type".to_owned()]));
    assert!(
        spec.attributes_of("Bicycle").is_none(),
        "absent means not exposed at all"
    );
    let vehicle = spec.attributes_of("Vehicle").expect("exposed");
    assert!(vehicle.contains("name") && vehicle.contains("id"));
}

#[test]
fn an_endpoint_references_a_projection_and_nothing_else_by_that_name() {
    let yaml = r#"apiVersion: joinedcontext.com/v1alpha1
kind: Endpoint
metadata:
  name: fleet-partner
  namespace: helsinki
spec:
  contextSpaceRef: fleet
  slug: 3ecozggnnhjlp5miouhia53mr2
  audience: project-list
  allowedProjects: [regional-transport]
  projectionRef: { kind: ModelProjection, name: partner-view }
  enabledRepresentations: [ngsi-ld, geojson]
"#;
    let endpoint = Endpoint::from_yaml(yaml).expect("parses");
    endpoint.validate().expect("valid");
    assert_eq!(endpoint.spec.audience, Audience::ProjectList);
    assert_eq!(
        endpoint.spec.projection_ref,
        Some(ModelProjectionSpec::reference("partner-view"))
    );
    let wrong = Endpoint::from_yaml(&yaml.replace("kind: ModelProjection", "kind: Mapping"))
        .expect("parses");
    assert!(
        wrong.validate().is_err(),
        "projectionRef names a ModelProjection"
    );
}
