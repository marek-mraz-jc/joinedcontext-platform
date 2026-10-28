//! `kind: DelegationPolicy` (AG-150, ADR-N-064): the ceiling on what a person may hand an AI
//! client through a connector. Every rule of the docs page refuses its field by path.
use jc_core::envelope::Kind;
use jc_core::kinds::{
    AboveCeiling, ConnectorVerb, DelegationPolicy, DelegationPolicySpec, Segment,
};
use jc_core::registry;

/// The example of Development/04 §DelegationPolicy, verbatim (T-5946).
const DOCS: &str = r#"kind: DelegationPolicy
apiVersion: joinedcontext.com/v1alpha1
metadata: { name: stewards-pipelines, namespace: org }
spec:
  subjects:                      # at least one; a Group by name, a user by Keycloak user id (ADR-N-061)
    - { kind: Group, name: air-stewards }
    - { kind: User, id: 3f1c9a52-6b1e-4d0a-9a4e-0c8d1b7e2f10 }
  projects: [air, mobility]      # the projects a connector may name; "*" is refused, list them
  segments: [data-read, context, pipeline]
  kinds:                         # verbs per kind, from the registry's verb set
    - { kind: Pipeline, verbs: [read, propose, run] }
    - { kind: DataSource, verbs: [read, propose] }
    - { kind: Endpoint, verbs: [read] }
  endpoints: [air-quality-sensors-2026-public]   # optional; absent = the projects' Endpoints the person may read
  writes: true                   # may a connector of theirs have writes on; default false
  personalData: false            # may a scope item tick personalData (AG-158); default false
  maxExpiry: P90D                # ISO 8601 duration, at most P365D
  allowedClients:                # optional; a connector of theirs must name a subset
    - https://claude.ai/oauth/mcp-oauth-client-metadata
    - https://claude.ai/oauth/claude-code-client-metadata
  aboveCeiling: queue            # refuse (default) or queue: a request above it waits for an administrator
"#;

const MINIMAL: &str = "apiVersion: joinedcontext.com/v1alpha1
kind: DelegationPolicy
metadata: { name: readers, namespace: org }
spec:
  subjects: [{ kind: Group, name: readers }]
  projects: [air]
";

/// The minimal policy with `line` appended to its spec (or replacing a key it already has).
fn with(line: &str) -> String {
    let key = line.split(':').next().unwrap_or_default().trim();
    let kept: Vec<&str> = MINIMAL
        .lines()
        .filter(|l| !l.trim_start().starts_with(&format!("{key}:")))
        .collect();
    format!("{}\n  {line}\n", kept.join("\n"))
}

/// The error text of a refused manifest; panics when it is accepted.
fn refused(yaml: &str) -> String {
    match registry::validate_yaml("DelegationPolicy", yaml).expect("a known kind") {
        Ok(()) => panic!("accepted:\n{yaml}"),
        Err(err) => err.to_string(),
    }
}

fn spec(yaml: &str) -> DelegationPolicySpec {
    DelegationPolicy::from_yaml(yaml).expect("parses").spec
}

#[test]
fn the_docs_example_validates_and_reads_as_written() {
    assert!(registry::validate_yaml("DelegationPolicy", DOCS)
        .expect("a known kind")
        .is_ok());
    let spec = spec(DOCS);
    assert_eq!(
        spec.segments(),
        &[Segment::DataRead, Segment::Context, Segment::Pipeline]
    );
    assert_eq!(
        spec.verbs_for("Pipeline"),
        &[
            ConnectorVerb::Read,
            ConnectorVerb::Propose,
            ConnectorVerb::Run
        ]
    );
    assert_eq!(spec.verbs_for("Policy"), &[] as &[ConnectorVerb]);
    assert_eq!(spec.max_expiry_days(), 90);
    assert_eq!(spec.above_ceiling, AboveCeiling::Queue);
    assert!(spec.allows_client("https://claude.ai/oauth/claude-code-client-metadata"));
    assert!(!spec.allows_client("mcp-local"));
    assert!(spec.binds("3f1c9a52-6b1e-4d0a-9a4e-0c8d1b7e2f10", &[]));
    assert!(spec.binds("8b1d4e2f-0c3a-4f5b-9e6d-7a2c1b0e9f38", &["air-stewards"]));
    assert!(!spec.binds("8b1d4e2f-0c3a-4f5b-9e6d-7a2c1b0e9f38", &["other"]));
    assert_eq!(
        DelegationPolicySpec::PATH_TEMPLATE,
        "users/delegation/{name}.yaml"
    );
}

#[test]
fn a_field_left_out_allows_the_default_and_never_everything() {
    let spec = spec(MINIMAL);
    assert_eq!(spec.segments(), &[Segment::DataRead]);
    assert_eq!(spec.verbs_for("Pipeline"), &[ConnectorVerb::Read]);
    assert!(!spec.writes);
    assert_eq!(spec.max_expiry_days(), 30);
    assert_eq!(spec.above_ceiling, AboveCeiling::Refuse);
    assert!(spec.allows_client("https://chatgpt.com/oauth/client"));
}

#[test]
fn empty_subjects_bind_nobody_and_are_refused() {
    assert!(refused(&with("subjects: []")).contains("spec.subjects"));
}

#[test]
fn a_user_is_named_by_keycloak_id_never_by_email() {
    let err = refused(&with("subjects: [{ kind: User, id: jana@example.sk }]"));
    assert!(
        err.contains("spec.subjects[].id") && err.contains("e-mail"),
        "{err}"
    );
    let err = refused(&with("subjects: [{ kind: User, id: jana }]"));
    assert!(err.contains("spec.subjects[].id"), "{err}");
}

#[test]
fn a_subject_is_a_group_or_a_user_and_nothing_else() {
    let err = refused(&with("subjects: [{ kind: Role, name: admin }]"));
    assert!(err.contains("Role") || err.contains("variant"), "{err}");
    let err = refused(&with("subjects: [{ kind: Group, name: air, id: x }]"));
    assert!(err.contains("id"), "{err}");
    let err = refused(&with("subjects: [{ kind: Group, name: Air_Team }]"));
    assert!(err.contains("spec.subjects[].name"), "{err}");
    let err = refused(&with(
        "subjects: [{ kind: Group, name: air }, { kind: Group, name: air }]",
    ));
    assert!(err.contains("twice"), "{err}");
}

#[test]
fn projects_are_listed_without_a_wildcard() {
    assert!(refused(&with("projects: []")).contains("spec.projects"));
    assert!(refused(&with("projects: ['*']")).contains("wildcard"));
    assert!(refused(&with("projects: [air, air]")).contains("twice"));
    assert!(refused(&with("projects: [Air]")).contains("spec.projects"));
    let without = MINIMAL.replace("  projects: [air]\n", "");
    assert!(refused(&without).contains("projects"));
}

#[test]
fn a_segment_outside_the_eight_is_refused() {
    let err = refused(&with("segments: [data-read, approve]"));
    assert!(err.contains("approve"), "{err}");
    assert!(refused(&with("segments: []")).contains("spec.segments"));
    assert!(refused(&with("segments: [context, context]")).contains("twice"));
    for segment in [
        "data-read",
        "context",
        "pipeline",
        "access",
        "publish",
        "apps",
        "operate",
        "admin",
    ] {
        assert!(
            registry::validate_yaml("DelegationPolicy", &with(&format!("segments: [{segment}]")))
                .expect("a known kind")
                .is_ok(),
            "{segment}"
        );
    }
}

#[test]
fn verbs_come_from_the_registry_set_and_no_verb_approves() {
    let err = refused(&with("kinds: [{ kind: Pipeline, verbs: [approve] }]"));
    assert!(err.contains("approve"), "{err}");
    let err = refused(&with("kinds: [{ kind: Pipeline, verbs: [] }]"));
    assert!(err.contains("spec.kinds[].verbs"), "{err}");
    let err = refused(&with("kinds: [{ kind: Endpoint, verbs: [read, run] }]"));
    assert!(err.contains("Pipeline only"), "{err}");
    let err = refused(&with("kinds: [{ kind: Pipeline, verbs: [read, read] }]"));
    assert!(err.contains("twice"), "{err}");
    assert!(refused(&with("kinds: []")).contains("spec.kinds"));
}

#[test]
fn a_kind_outside_the_registry_is_refused() {
    let err = refused(&with("kinds: [{ kind: Secret, verbs: [read] }]"));
    assert!(
        err.contains("spec.kinds[].kind") && err.contains("Secret"),
        "{err}"
    );
    let err = refused(&with(
        "kinds: [{ kind: Pipeline, verbs: [read] }, { kind: Pipeline, verbs: [propose] }]",
    ));
    assert!(err.contains("twice"), "{err}");
}

#[test]
fn endpoints_are_listed_by_name_without_a_wildcard() {
    assert!(refused(&with("endpoints: []")).contains("spec.endpoints"));
    assert!(refused(&with("endpoints: ['air-*']")).contains("wildcard"));
    assert!(refused(&with("endpoints: [Air Sensors]")).contains("spec.endpoints"));
}

#[test]
fn max_expiry_is_days_up_to_a_year() {
    for bad in ["P366D", "P0D", "PT24H", "P1Y", "90", "P-1D", "P１D"] {
        let err = refused(&with(&format!("maxExpiry: \"{bad}\"")));
        assert!(err.contains("spec.maxExpiry"), "{bad}: {err}");
    }
    assert_eq!(spec(&with("maxExpiry: P365D")).max_expiry_days(), 365);
    assert_eq!(spec(&with("maxExpiry: P1D")).max_expiry_days(), 1);
}

#[test]
fn allowed_clients_are_https_cimd_urls_or_realm_client_ids() {
    for bad in [
        "http://claude.ai/x",
        "*",
        "https://",
        "claude code",
        "javascript:alert(1)",
    ] {
        let err = refused(&with(&format!("allowedClients: [\"{bad}\"]")));
        assert!(err.contains("spec.allowedClients"), "{bad}: {err}");
    }
    assert!(refused(&with("allowedClients: []")).contains("spec.allowedClients"));
    assert!(refused(&with("allowedClients: [mcp-local, mcp-local]")).contains("twice"));
    assert!(
        registry::validate_yaml("DelegationPolicy", &with("allowedClients: [mcp-local]"))
            .expect("a known kind")
            .is_ok()
    );
}

#[test]
fn above_ceiling_is_refuse_or_queue_and_unknown_fields_are_refused() {
    assert!(refused(&with("aboveCeiling: grant")).contains("grant"));
    assert!(refused(&with("everything: true")).contains("everything"));
}

#[test]
fn the_policy_lives_in_the_organization_only() {
    let err = refused(&MINIMAL.replace("namespace: org", "namespace: air"));
    assert!(err.contains("namespace") || err.contains("org"), "{err}");
}

#[test]
fn the_schema_round_trips_and_names_the_rules() {
    let manifest = DelegationPolicy::from_yaml(DOCS).expect("parses");
    let json = serde_json::to_value(&manifest).expect("serializes");
    let back: DelegationPolicy = serde_json::from_value(json.clone()).expect("reads back");
    assert_eq!(back, manifest);
    let schema = registry::schema_of("DelegationPolicy").expect("a schema");
    let text = schema.to_string();
    for word in [
        "subjects",
        "maxExpiry",
        "allowedClients",
        "aboveCeiling",
        "data-read",
        "propose",
    ] {
        assert!(text.contains(word), "{word} missing from the schema");
    }
    let validator = jsonschema::validator_for(&schema).expect("a valid schema");
    assert!(validator.is_valid(&json));
}

#[test]
fn personal_data_is_off_unless_the_policy_says_true() {
    assert!(!spec(DOCS).personal_data);
    let allowing = DOCS.replace("personalData: false", "personalData: true");
    assert!(spec(&allowing).personal_data);
    let without = DOCS.replace(
        "  personalData: false            # may a scope item tick personalData (AG-158); default false\n",
        "",
    );
    assert!(!spec(&without).personal_data);
}
