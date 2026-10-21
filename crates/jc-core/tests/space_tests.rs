use jc_core::kinds::{
    duration_seconds, retention_seconds, ContextSpace, ContextSpaceSpec, MissingUnitCode, Project,
    QualityKind, QualityOutcome, Quotas,
};
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
        mqtt: None,
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
        retention: Vec::new(),
        quality: Vec::new(),
        broker_ref: None,
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

/// PF-67, ADR-N-055 §2.5: `d` and `me` address an assistant's and a person's MCP servers, so no
/// project takes them; a name that only starts with them is fine.
#[test]
fn a_project_never_takes_an_mcp_address_segment() {
    for reserved in ["d", "me"] {
        let manifest = GOLDEN_PROJECT.replace("name: ovzdusie", &format!("name: {reserved}"));
        let project = Project::from_yaml(&manifest).expect("the manifest parses");
        let refused = project.validate().expect_err(reserved).to_string();
        assert!(refused.contains("MCP addresses"), "{refused}");
    }
    let manifest = GOLDEN_PROJECT.replace("name: ovzdusie", "name: doprava");
    let project = Project::from_yaml(&manifest).expect("the manifest parses");
    assert!(project.validate().is_ok());
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

/// DM-82: a space's retention round-trips, holds one rule per type, and refuses a duration
/// the broker cannot run (zero, negative, under a minute, over ten years, an unknown unit).
#[test]
fn retention_rules_round_trip_and_refuse_what_the_broker_cannot_run_dm_81() {
    let yaml = GOLDEN_SPACE.replace(
        "isSandbox: false",
        "isSandbox: false\n  retention:\n    - { entityType: Vehicle, current: PT5M, history: PT8H }\n    - { entityType: Stop }",
    );
    let space = ContextSpace::from_yaml(&yaml).expect("parses");
    assert!(space.spec.validate_with_meta(&space.metadata).is_ok());
    assert_eq!(space.spec.retention.len(), 2);
    let back: ContextSpaceSpec =
        serde_json::from_value(serde_json::to_value(&space.spec).expect("serialize"))
            .expect("round trip");
    assert_eq!(back, space.spec);
    let empty = serde_json::to_value(ContextSpaceSpec::default()).expect("serialize");
    assert!(empty.get("retention").is_none(), "no rule, no field");

    // T-4275: history off records nothing; forever keeps all of it and says why.
    let words = GOLDEN_SPACE.replace(
        "isSandbox: false",
        "isSandbox: false\n  retention:\n    - { entityType: Vehicle, history: off }\n    - { entityType: Stop, history: forever, reason: \"the timetable archive is the dataset\" }",
    );
    let words = ContextSpace::from_yaml(&words).expect("parses");
    assert!(words.spec.validate_with_meta(&words.metadata).is_ok());
    assert_eq!(words.spec.retention[0].history.as_deref(), Some("off"));

    // DM-97: the space's default rule, "*", sets history only, and a type rule still beats it.
    let star = GOLDEN_SPACE.replace(
        "isSandbox: false",
        "isSandbox: false\n  retention:\n    - { entityType: \"*\", history: P90D }\n    - { entityType: Vehicle, history: PT8H }",
    );
    let star = ContextSpace::from_yaml(&star).expect("parses");
    assert!(star.spec.validate_with_meta(&star.metadata).is_ok());

    for (rule, field) in [
        (
            "{ entityType: Vehicle, current: PT0M }",
            "spec.retention.current",
        ),
        (
            "{ entityType: Vehicle, current: -PT5M }",
            "spec.retention.current",
        ),
        (
            "{ entityType: Vehicle, history: PT30S }",
            "spec.retention.history",
        ),
        (
            "{ entityType: Vehicle, history: P3651D }",
            "spec.retention.history",
        ),
        (
            "{ entityType: Vehicle, history: P1Y }",
            "spec.retention.history",
        ),
        (
            "{ entityType: Vehicle, history: 5m }",
            "spec.retention.history",
        ),
        ("{ entityType: vehicle }", "entityType"),
        (
            "{ entityType: Vehicle, history: Off }",
            "spec.retention.history",
        ),
        (
            "{ entityType: Vehicle, current: off }",
            "spec.retention.current",
        ),
        (
            "{ entityType: Vehicle, history: forever }",
            "spec.retention.reason",
        ),
        (
            "{ entityType: Vehicle, history: P30D, reason: \"kept\" }",
            "spec.retention.reason",
        ),
        (
            "{ entityType: Vehicle, history: forever, reason: \"  \" }",
            "spec.retention.reason",
        ),
        (
            "{ entityType: \"*\", current: PT5M, history: P7D }",
            "spec.retention.current",
        ),
        ("{ entityType: \"*\" }", "spec.retention.history"),
        (
            "{ entityType: \"*\", history: 7d }",
            "spec.retention.history",
        ),
        ("{ entityType: \"**\", history: P7D }", "entityType"),
    ] {
        let yaml = GOLDEN_SPACE.replace(
            "isSandbox: false",
            &format!("isSandbox: false\n  retention:\n    - {rule}"),
        );
        let space = ContextSpace::from_yaml(&yaml).expect("parses");
        let err = space
            .spec
            .validate_with_meta(&space.metadata)
            .expect_err(rule);
        assert!(
            matches!(&err, jc_core::Error::Name { field: f, .. } if *f == field),
            "{rule}: {err}"
        );
    }
    let twice = GOLDEN_SPACE.replace(
        "isSandbox: false",
        "isSandbox: false\n  retention:\n    - { entityType: Vehicle }\n    - { entityType: Vehicle, history: PT5M }",
    );
    let space = ContextSpace::from_yaml(&twice).expect("parses");
    assert!(space.spec.validate_with_meta(&space.metadata).is_err());
    let unknown = GOLDEN_SPACE.replace(
        "isSandbox: false",
        "isSandbox: false\n  retention:\n    - { entityType: Vehicle, keep: PT5M }",
    );
    assert!(
        ContextSpace::from_yaml(&unknown).is_err(),
        "deny_unknown_fields"
    );
}

#[test]
fn retention_durations_are_the_brokers_form() {
    for (raw, secs) in [
        ("PT1M", 60),
        ("PT5M", 300),
        ("PT8H", 28_800),
        ("P1DT12H", 129_600),
        ("PT1H30M15S", 5_415),
        ("P3650D", 315_360_000),
    ] {
        assert_eq!(retention_seconds(raw), Some(secs), "{raw}");
    }
    for raw in [
        "", "P", "PT", "P1DT", "PT59S", "P0D", "P1H", "PT5X", "PT5M3", "P1W", "5m",
    ] {
        assert_eq!(retention_seconds(raw), None, "{raw}");
    }
}

fn with_quality(rules: &str) -> String {
    GOLDEN_SPACE.replace(
        "isSandbox: false",
        &format!("isSandbox: false\n  quality:\n{rules}"),
    )
}

/// DM-87: the ADR's three rules round-trip, each kind takes its default outcome, and an
/// explicit outcome overrides it.
#[test]
fn quality_rules_round_trip_with_their_default_outcomes_dm_86() {
    let yaml = with_quality(
        "    - { name: speed-plausible, entityType: Vehicle, kind: range, attribute: speed, max: 33.4 }\n    - { name: inside-hsl-area, entityType: Vehicle, kind: area, attribute: location, bbox: [24.5, 60.0, 25.3, 60.45] }\n    - { name: position-current, entityType: Vehicle, kind: freshness, attribute: location, maxAhead: PT30S, maxAge: PT5M }\n    - { name: speed-sure, entityType: Vehicle, kind: range, attribute: speed, min: 0, outcome: reject }",
    );
    let space = ContextSpace::from_yaml(&yaml).expect("parses");
    space
        .spec
        .validate_with_meta(&space.metadata)
        .expect("valid");
    let rules = &space.spec.quality;
    assert_eq!(rules[0].kind, QualityKind::Range);
    assert_eq!(rules[0].write_outcome(), QualityOutcome::Flag);
    assert_eq!(rules[1].write_outcome(), QualityOutcome::Flag);
    assert_eq!(rules[2].write_outcome(), QualityOutcome::Reject);
    assert_eq!(rules[2].sweep_outcome(), QualityOutcome::Quarantine);
    assert_eq!(rules[3].write_outcome(), QualityOutcome::Reject);
    let value = serde_json::to_value(&space.spec).expect("serialize");
    assert_eq!(value["quality"][0]["max"], serde_json::json!(33.4));
    assert_eq!(value["quality"][2]["maxAhead"], "PT30S");
    let back: ContextSpaceSpec = serde_json::from_value(value).expect("round trip");
    assert_eq!(back, space.spec);
    let empty = serde_json::to_value(ContextSpaceSpec::default()).expect("serialize");
    assert!(empty.get("quality").is_none(), "no rule, no field");
}

/// DM-87, DM-92: a rule that mixes kinds, a bad bound, an unknown field or outcome, a duplicate
/// name and a 65th rule are refused.
#[test]
fn quality_rules_refuse_what_cannot_be_checked_dm_86_dm_91() {
    for (rule, field) in [
        ("{ name: Speed, entityType: Vehicle, kind: range, attribute: speed, max: 1 }", "spec.quality.name"),
        ("{ name: s, entityType: vehicle, kind: range, attribute: speed, max: 1 }", "entityType"),
        ("{ name: s, entityType: Vehicle, kind: range, attribute: '', max: 1 }", "spec.quality.attribute"),
        ("{ name: s, entityType: Vehicle, kind: range, attribute: speed }", "spec.quality"),
        ("{ name: s, entityType: Vehicle, kind: range, attribute: speed, max: 1, maxAge: PT5M }", "spec.quality"),
        ("{ name: s, entityType: Vehicle, kind: area, attribute: location, max: 1 }", "spec.quality"),
        ("{ name: s, entityType: Vehicle, kind: freshness, attribute: location }", "spec.quality"),
        ("{ name: s, entityType: Vehicle, kind: range, attribute: speed, min: 5, max: 1 }", "spec.quality.min"),
        ("{ name: s, entityType: Vehicle, kind: area, attribute: location, bbox: [25.3, 60.0, 24.5, 60.45] }", "spec.quality.bbox"),
        ("{ name: s, entityType: Vehicle, kind: area, attribute: location, bbox: [24.5, 60.0, 25.3, 91] }", "spec.quality.bbox"),
        ("{ name: s, entityType: Vehicle, kind: freshness, attribute: location, maxAge: PT0S }", "spec.quality.maxAge"),
        ("{ name: s, entityType: Vehicle, kind: freshness, attribute: location, maxAhead: P1Y }", "spec.quality.maxAhead"),
        ("{ name: s, entityType: Vehicle, kind: freshness, attribute: location, maxAge: PT5M, outcome: reject }", "spec.quality.outcome"),
        ("{ name: s, entityType: Vehicle, kind: freshness, attribute: location, maxAhead: PT30S, maxAge: PT5M, outcome: flag }", "spec.quality.outcome"),
    ] {
        let space = ContextSpace::from_yaml(&with_quality(&format!("    - {rule}"))).expect(rule);
        let err = space
            .spec
            .validate_with_meta(&space.metadata)
            .expect_err(rule);
        assert!(
            matches!(&err, jc_core::Error::Name { field: f, .. } if *f == field),
            "{rule}: {err}"
        );
    }
    for rule in [
        "{ name: s, entityType: Vehicle, kind: range, attribute: speed, max: 1, unit: kmh }",
        "{ name: s, entityType: Vehicle, kind: range, attribute: speed, max: 1, outcome: drop }",
        "{ name: s, entityType: Vehicle, kind: plausible, attribute: speed, max: 1 }",
        "{ name: s, entityType: Vehicle, kind: area, attribute: location, bbox: [1, 2, 3] }",
    ] {
        assert!(
            ContextSpace::from_yaml(&with_quality(&format!("    - {rule}"))).is_err(),
            "{rule}"
        );
    }
    let twice = with_quality(
        "    - { name: s, entityType: Vehicle, kind: range, attribute: speed, max: 1 }\n    - { name: s, entityType: Stop, kind: range, attribute: speed, max: 1 }",
    );
    let space = ContextSpace::from_yaml(&twice).expect("parses");
    assert!(space.spec.validate_with_meta(&space.metadata).is_err());

    let rules = |n: usize| {
        (0..n)
            .map(|i| format!("    - {{ name: r{i}, entityType: Vehicle, kind: range, attribute: speed, max: 1 }}"))
            .collect::<Vec<_>>()
            .join("\n")
    };
    let space = ContextSpace::from_yaml(&with_quality(&rules(64))).expect("parses");
    assert!(space.spec.validate_with_meta(&space.metadata).is_ok());
    let space = ContextSpace::from_yaml(&with_quality(&rules(65))).expect("parses");
    let err = space
        .spec
        .validate_with_meta(&space.metadata)
        .expect_err("65");
    assert!(err.to_string().contains("64"), "{err}");
}

#[test]
fn freshness_durations_start_at_one_second() {
    assert_eq!(duration_seconds("PT1S"), Some(1));
    assert_eq!(duration_seconds("PT30S"), Some(30));
    assert_eq!(
        retention_seconds("PT30S"),
        None,
        "retention starts at a minute"
    );
    assert_eq!(duration_seconds("PT0S"), None);
    assert_eq!(duration_seconds("P3651D"), None);
}

/// DM-108, DM-109 (ADR-N-054 2.2, T-4669): a type records `every` or `onChange` with a heartbeat,
/// an attribute may override it, and nothing records on the `*` rule or under `history: off`.
#[test]
fn a_retention_rule_records_on_change_with_a_heartbeat_dm_108() {
    let rule = |rule: &str| {
        let yaml = GOLDEN_SPACE.replace(
            "isSandbox: false",
            &format!("isSandbox: false\n  retention:\n    - {rule}"),
        );
        let space = ContextSpace::from_yaml(&yaml).expect("parses");
        space
            .spec
            .validate_with_meta(&space.metadata)
            .map(|()| space.spec)
    };
    let bikes = rule(
        "{ entityType: BikeHireDockingStation, history: P30D, record: onChange, heartbeat: PT1H }",
    )
    .expect("valid");
    let back: ContextSpaceSpec =
        serde_json::from_value(serde_json::to_value(&bikes).expect("serialize"))
            .expect("round trip");
    assert_eq!(back, bikes);
    assert_eq!(bikes.retention[0].heartbeat.as_deref(), Some("PT1H"));
    for valid in [
        "{ entityType: Spot, history: P90D, record: every, attributes: { status: { record: onChange, heartbeat: PT1H }, location: { record: onChange, minDistance: 5, heartbeat: PT1H } } }",
        "{ entityType: Spot, history: P90D, record: onChange, heartbeat: PT1H, deadband: 0.5, attributes: { count: { record: every } } }",
        "{ entityType: Spot, record: onChange, heartbeat: PT15M }",
    ] {
        assert!(rule(valid).is_ok(), "{valid}");
    }
    for (bad, field) in [
        (
            "{ entityType: Spot, record: onchange, heartbeat: PT1H }",
            "spec.retention.record",
        ),
        (
            "{ entityType: Spot, record: onChange }",
            "spec.retention.record",
        ),
        (
            "{ entityType: Spot, record: onChange, heartbeat: PT30S }",
            "spec.retention.record",
        ),
        (
            "{ entityType: Spot, heartbeat: PT1H }",
            "spec.retention.record",
        ),
        (
            "{ entityType: Spot, record: onChange, heartbeat: PT1H, deadband: -1 }",
            "spec.retention.record",
        ),
        (
            "{ entityType: Spot, history: off, record: onChange, heartbeat: PT1H }",
            "spec.retention.record",
        ),
        (
            "{ entityType: \"*\", history: P7D, record: onChange, heartbeat: PT1H }",
            "spec.retention.record",
        ),
        (
            "{ entityType: Spot, attributes: { status: { record: onChange } } }",
            "spec.retention.attributes",
        ),
        (
            "{ entityType: Spot, attributes: { count: { deadband: 1 } } }",
            "spec.retention.attributes",
        ),
        (
            "{ entityType: Spot, attributes: { \" \": { record: every } } }",
            "spec.retention.attributes",
        ),
    ] {
        match rule(bad) {
            Err(jc_core::Error::Name { field: got, .. }) => assert_eq!(got, field, "{bad}"),
            other => panic!("{bad}: {other:?}"),
        }
    }
    let unknown = GOLDEN_SPACE.replace(
        "isSandbox: false",
        "isSandbox: false\n  retention:\n    - { entityType: Spot, attributes: { status: { mode: onChange } } }",
    );
    assert!(
        ContextSpace::from_yaml(&unknown).is_err(),
        "an unknown field is refused"
    );
}
