use jc_core::kinds::{ContextSpace, ContextSpaceSpec, MissingUnitCode, Project, Quotas};
use jc_core::Urn;

const GOLDEN_PROJECT: &str = r#"apiVersion: joinedcontext.com/v1alpha1
kind: Project
metadata:
  name: ovzdusie
  namespace: org
  title: { sk: "Ovzdušie", en: "Air quality" }
spec:
  organizationRef: { kind: Organization, name: banskabystrica }
  quotas:                           # structural quotas per Project (PF-17), checked by Conftest (PF-18)
    contextSpaces: 5
    residentPipelines: 3
    publicEndpoints: 2
    ingestEventsPerSecond: 500
"#;

const GOLDEN_SPACE: &str = r#"apiVersion: joinedcontext.com/v1alpha1
kind: ContextSpace
metadata:
  name: ovzdusie                 # DNS-1123, unique per kind in the namespace
  namespace: bb-doprava          # = project slug ("org" for organization-level kinds)
  labels:
    joinedcontext.com/domain: environment
  annotations:
    joinedcontext.com/managed-attributes: "title,description,tags"
    joinedcontext.com/imported-from: "https://udp.example.sk/bb-doprava@3f9c2e1"
  title: { sk: "Ovzdušie", en: "Air quality", de: "Luftqualität", cs: "Ovzduší" }
  description: { sk: "Merania kvality ovzdušia", en: "Air-quality observations" }
spec:
  isSandbox: false
  defaultLocale: sk
"#;

#[test]
fn golden_project_manifest_roundtrips_and_validates() {
    let project = Project::from_yaml(GOLDEN_PROJECT).expect("parse golden project");

    assert_eq!(project.metadata.name, "ovzdusie");
    assert_eq!(project.metadata.namespace.as_deref(), Some("org"));
    assert_eq!(project.spec.organization_ref.name(), "banskabystrica");
    assert_eq!(project.spec.organization_ref.kind(), Some("Organization"));

    let quotas = project
        .spec
        .quotas
        .as_ref()
        .expect("quotas should be defined");
    assert_eq!(quotas.context_spaces, Some(5));
    assert_eq!(quotas.resident_pipelines, Some(3));
    assert_eq!(quotas.public_endpoints, Some(2));
    assert_eq!(quotas.ingest_events_per_second, Some(500));

    project.validate().expect("project envelope validates");
    project.spec.validate().expect("project spec validates");
    assert_eq!(
        project.resource_path().expect("resource_path succeeds"),
        "projects/ovzdusie/project.yaml"
    );

    let serialized = project.to_yaml().expect("serialize project to yaml");
    let reimported = Project::from_yaml(&serialized).expect("deserialize project");
    assert_eq!(project, reimported);
}

#[test]
fn project_quota_zero_fails_validation() {
    // Every dimension of PF-17 and PF-73: a zero forbids everything by accident, and an absent
    // field is what "unlimited" looks like.
    let full = Quotas {
        context_spaces: Some(1),
        resident_pipelines: Some(1),
        public_endpoints: Some(1),
        ingest_events_per_second: Some(1),
        apps: Some(1),
        agent_runs_per_day: Some(1),
        entities_per_space: Some(1),
        requests_per_minute: Some(1),
    };
    assert!(full.validate().is_ok());
    assert_eq!(full.dimensions().len(), 8);

    for (field, _) in full.dimensions() {
        let zeroed: Quotas = serde_norway::from_str(&format!("{field}: 0\n")).expect("quotas");
        let err = zeroed
            .validate()
            .expect_err("a quota of zero is not a quota");
        assert!(err.to_string().contains(field), "{err}");
    }

    assert!(Quotas::default().validate().is_ok(), "no quota is no limit");
}

#[test]
fn golden_context_space_roundtrips_and_validates() {
    let cs = ContextSpace::from_yaml(GOLDEN_SPACE).expect("parse golden context space");

    assert_eq!(cs.metadata.name, "ovzdusie");
    assert_eq!(cs.metadata.namespace.as_deref(), Some("bb-doprava"));
    assert_eq!(
        cs.metadata.labels.get("joinedcontext.com/domain"),
        Some(&"environment".to_string())
    );
    assert_eq!(
        cs.metadata
            .annotations
            .get("joinedcontext.com/managed-attributes"),
        Some(&"title,description,tags".to_string())
    );
    assert_eq!(
        cs.metadata
            .annotations
            .get("joinedcontext.com/imported-from"),
        Some(&"https://udp.example.sk/bb-doprava@3f9c2e1".to_string())
    );

    let title = cs.metadata.title.as_ref().expect("title exists");
    assert_eq!(title.get("sk"), Some("Ovzdušie"));
    assert_eq!(title.get("en"), Some("Air quality"));
    assert_eq!(title.get("de"), Some("Luftqualität"));
    assert_eq!(title.get("cs"), Some("Ovzduší"));

    let desc = cs
        .metadata
        .description
        .as_ref()
        .expect("description exists");
    assert_eq!(desc.get("sk"), Some("Merania kvality ovzdušia"));
    assert_eq!(desc.get("en"), Some("Air-quality observations"));

    assert!(!cs.spec.is_sandbox);
    assert_eq!(cs.spec.default_locale.as_deref(), Some("sk"));

    cs.validate().expect("envelope should validate");
    cs.spec
        .validate_with_meta(&cs.metadata)
        .expect("spec with meta validates");
    assert_eq!(
        cs.resource_path().expect("resource_path succeeds"),
        "projects/bb-doprava/spaces/ovzdusie/space.yaml"
    );

    let serialized = cs.to_yaml().expect("serialize space to yaml");
    let reimported = ContextSpace::from_yaml(&serialized).expect("deserialize space");
    assert_eq!(cs, reimported);
}

#[test]
fn context_space_name_validation_pf_09() {
    let cs = ContextSpace::from_yaml(GOLDEN_SPACE).expect("parse golden space");
    let spec = cs.spec;

    for bad_name in [
        "Ovzdusie",
        "ovz_dusie",
        "-ovzdusie",
        "ovzdusie.sk",
        "a".repeat(64).as_str(),
    ] {
        let mut meta = cs.metadata.clone();
        meta.name = bad_name.to_string();
        assert!(
            spec.validate_with_meta(&meta).is_err(),
            "expected `{bad_name}` to fail PF-09 validation"
        );
    }
}

#[test]
fn context_space_sandbox_ttl_rules_pf_19() {
    let mut spec = ContextSpaceSpec {
        is_sandbox: true,
        default_locale: None,
        data_model_ref: None,
        ttl_days: Some(7),
        urn_segment: None,
        missing_unit_code: MissingUnitCode::Fill,
    };
    assert!(spec.validate().is_ok());

    spec.is_sandbox = false;
    assert!(spec.validate().is_err());

    spec.is_sandbox = true;
    spec.ttl_days = Some(15);
    assert!(spec.validate().is_err());

    spec.ttl_days = Some(0);
    assert!(spec.validate().is_err());

    spec.ttl_days = None;
    assert!(spec.validate().is_ok());
}

#[test]
fn context_space_name_agrees_with_urn_space_segment_pf_42() {
    let cs = ContextSpace::from_yaml(GOLDEN_SPACE).expect("parse golden space");
    let urn = Urn::new(
        "AirQualityObserved",
        "banskabystrica.sk",
        &cs.metadata.name,
        "s1",
    );
    assert!(urn.is_ok());
    assert_eq!(
        urn.expect("valid urn").to_string(),
        "urn:ngsi-ld:AirQualityObserved:banskabystrica.sk:ovzdusie:s1"
    );
}

#[test]
fn space_rejects_unknown_fields() {
    let bad_project = GOLDEN_PROJECT.replace("quotas:", "quotas:\n  unknownField: bad");
    assert!(Project::from_yaml(&bad_project).is_err());

    let bad_space =
        GOLDEN_SPACE.replace("isSandbox: false", "isSandbox: false\n  unknownField: bad");
    assert!(ContextSpace::from_yaml(&bad_space).is_err());
}

#[test]
fn a_space_names_a_model_of_its_own_project_dm_61() {
    let own = GOLDEN_SPACE.replace(
        "isSandbox: false",
        "isSandbox: false\n  dataModelRef: { kind: DataModel, name: bb-air-quality }",
    );
    let space = ContextSpace::from_yaml(&own).expect("a space naming its own model");
    space
        .spec
        .validate_with_meta(&space.metadata)
        .expect("a model of the space's own project");

    let foreign = GOLDEN_SPACE.replace(
        "isSandbox: false",
        "isSandbox: false\n  dataModelRef: { kind: DataModel, name: fleet, namespace: helsinki }",
    );
    let space = ContextSpace::from_yaml(&foreign).expect("the manifest parses");
    let refused = space
        .spec
        .validate_with_meta(&space.metadata)
        .expect_err("another project's model is imported, never named");
    assert!(
        refused.to_string().contains("spec.dataModelRef.namespace")
            && refused.to_string().contains("imports"),
        "{refused}"
    );

    let wrong_kind = GOLDEN_SPACE.replace(
        "isSandbox: false",
        "isSandbox: false\n  dataModelRef: { kind: Endpoint, name: bb-air-quality }",
    );
    assert!(ContextSpace::from_yaml(&wrong_kind)
        .map(|space| space.spec.validate_with_meta(&space.metadata))
        .is_ok_and(|verdict| verdict.is_err()));
}

/// DM-06: a space fills a missing `unitCode` unless it says `refuse`; the default stays unwritten,
/// and a value outside the two is refused when the manifest is read.
#[test]
fn a_space_fills_a_missing_unit_code_unless_it_refuses_dm_06() {
    let default: ContextSpaceSpec =
        serde_json::from_value(serde_json::json!({})).expect("empty spec");
    assert_eq!(default.missing_unit_code, MissingUnitCode::Fill);
    assert!(serde_json::to_value(&default)
        .expect("serialize")
        .get("missingUnitCode")
        .is_none());

    let strict: ContextSpaceSpec =
        serde_json::from_value(serde_json::json!({ "missingUnitCode": "refuse" }))
            .expect("strict spec");
    assert_eq!(strict.missing_unit_code, MissingUnitCode::Refuse);
    assert_eq!(
        serde_json::to_value(&strict).expect("serialize")["missingUnitCode"],
        "refuse"
    );

    assert!(serde_json::from_value::<ContextSpaceSpec>(
        serde_json::json!({ "missingUnitCode": "guess" })
    )
    .is_err());
}
