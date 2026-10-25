//! Tests for `kind: AgentProfile` (AG-26, AG-47..AG-50).

use jc_core::kinds::agent_profile::{
    AgentProfileRole, AgentTool, EndpointVerb, KindVerb, ModelProvider, ReasoningEffort,
};
use jc_core::kinds::AgentProfile;

const VALID_YAML: &str = r#"
apiVersion: joinedcontext.com/v1alpha1
kind: AgentProfile
metadata:
  name: app-builder
  namespace: org
  title:
    en: Application builder
spec:
  role: builder
  runtime:
    image: ghcr.io/marek-mraz/jc-agent-runner
    digest: sha256:414ba5b2b1163480e9ed4213a989cd798579cfa88582a2359303273009b2b852
  model:
    provider: anthropic
    name: claude-3-7-sonnet
    maxTokensPerRun: 2000000
  limits:
    stepsPerRun: 120
    wallClock: PT20M
    concurrentRunsPerOrganization: 2
    requestsPerMinute: 240
    maxResponseBytes: 8388608
  egress:
    allowedHosts:
      - static.crates.io
      - index.crates.io
      - registry.npmjs.org
  tools:
    - shell
    - cargo
    - pnpm
    - git
    - playwright
  workspace:
    cpu: "2"
    memory: 4Gi
    ephemeralStorage: 8Gi
"#;

#[test]
fn valid_agent_profile_parses_validates_and_roundtrips() {
    let profile = AgentProfile::from_yaml(VALID_YAML).expect("valid yaml");
    profile.validate().expect("valid profile");

    assert_eq!(profile.spec.role, AgentProfileRole::Builder);
    assert_eq!(profile.spec.model.provider, ModelProvider::Anthropic);
    assert_eq!(profile.spec.tools.len(), 5);
    assert!(profile.spec.tools.contains(&AgentTool::Cargo));
    assert_eq!(profile.spec.egress.allowed_hosts.len(), 3);

    let serialized = profile.to_yaml().expect("serialize");
    assert_eq!(
        profile,
        AgentProfile::from_yaml(&serialized).expect("re-import")
    );
}

#[test]
fn reject_secret_refs_via_deny_unknown_fields() {
    let bad_yaml = format!("{VALID_YAML}\n  secretRef:\n    name: stolen-key\n");
    assert!(AgentProfile::from_yaml(&bad_yaml).is_err());
}

#[test]
fn unpinned_digest_is_rejected() {
    let bad_yaml = VALID_YAML.replace(
        "sha256:414ba5b2b1163480e9ed4213a989cd798579cfa88582a2359303273009b2b852",
        "latest",
    );
    let profile = AgentProfile::from_yaml(&bad_yaml).expect("parses");
    assert!(profile.validate().is_err());
}

#[test]
fn wall_clock_over_one_hour_is_rejected() {
    let bad_yaml = VALID_YAML.replace("PT20M", "PT2H");
    let profile = AgentProfile::from_yaml(&bad_yaml).expect("parses");
    assert!(profile.validate().is_err());
}

#[test]
fn invalid_egress_hosts_rejected() {
    for bad_host in [
        "https://crates.io",
        "crates.io/path",
        "*.npmjs.org",
        "crates.io:443",
    ] {
        // Quoted: a bare `*` or `:` in a YAML scalar is an alias or a mapping, not a host.
        let bad_yaml = VALID_YAML.replace("static.crates.io", &format!("\"{bad_host}\""));
        let profile = AgentProfile::from_yaml(&bad_yaml).expect("parses");
        assert!(
            profile.validate().is_err(),
            "host {bad_host} must be rejected"
        );
    }
}

#[test]
fn steward_profile_rules() {
    // Steward cannot have coding tools
    let steward_with_tools = VALID_YAML
        .replace("role: builder", "role: steward")
        .replace("allowedHosts:\n      - static.crates.io\n      - index.crates.io\n      - registry.npmjs.org", "allowedHosts: []");
    let profile = AgentProfile::from_yaml(&steward_with_tools).expect("parses");
    assert!(profile.validate().is_err(), "steward with tools must fail");

    // Steward cannot have internet egress
    let steward_with_egress = VALID_YAML
        .replace("role: builder", "role: steward")
        .replace(
            "tools:\n    - shell\n    - cargo\n    - pnpm\n    - git\n    - playwright",
            "tools: []",
        );
    let profile2 = AgentProfile::from_yaml(&steward_with_egress).expect("parses");
    assert!(
        profile2.validate().is_err(),
        "steward with egress must fail"
    );

    // Pure steward succeeds
    let valid_steward = VALID_YAML
        .replace("role: builder", "role: steward")
        .replace("allowedHosts:\n      - static.crates.io\n      - index.crates.io\n      - registry.npmjs.org", "allowedHosts: []")
        .replace("tools:\n    - shell\n    - cargo\n    - pnpm\n    - git\n    - playwright", "tools: []");
    let profile3 = AgentProfile::from_yaml(&valid_steward).expect("parses");
    profile3.validate().expect("valid steward profile");
}

/// The access block of AG-70 appended to the valid profile.
fn with_access(block: &str) -> String {
    format!("{VALID_YAML}  access:\n{block}")
}

#[test]
fn an_access_block_parses_validates_and_roundtrips() {
    let yaml = with_access(
        "    operations: [jc_catalog_search, jc_endpoint_propose]\n    kinds:\n      - kind: ContextSpace\n        verbs: [read]\n      - kind: Endpoint\n        verbs: [read, propose]\n    endpoints:\n      - name: helsinki-bikes\n        verbs: [read]\n",
    );
    let profile = AgentProfile::from_yaml(&yaml).expect("parses");
    profile.validate().expect("valid access block");
    let access = profile.spec.access.as_ref().expect("access present");
    assert_eq!(
        access.operations,
        ["jc_catalog_search", "jc_endpoint_propose"]
    );
    assert_eq!(access.kinds[1].verbs, [KindVerb::Read, KindVerb::Propose]);
    assert_eq!(access.endpoints[0].verbs, [EndpointVerb::Read]);
    let serialized = profile.to_yaml().expect("serialize");
    assert_eq!(
        profile,
        AgentProfile::from_yaml(&serialized).expect("re-import")
    );
}

#[test]
fn a_profile_without_access_has_none() {
    let profile = AgentProfile::from_yaml(VALID_YAML).expect("parses");
    assert!(
        profile.spec.access.is_none(),
        "absent means the read-only default (AG-70)"
    );
}

#[test]
fn an_access_block_outside_mf_40_is_refused() {
    for (block, why) in [
        (
            "    operations: [catalog_search]\n",
            "an operation without the jc_ prefix",
        ),
        ("    operations: [\"jc_*\"]\n", "a wildcard operation"),
        ("    operations: [jc_a, jc_a]\n", "a repeated operation"),
        (
            "    kinds:\n      - kind: Spaceship\n        verbs: [read]\n",
            "an unknown kind",
        ),
        (
            "    kinds:\n      - kind: \"*\"\n        verbs: [read]\n",
            "a wildcard kind",
        ),
        (
            "    kinds:\n      - kind: Endpoint\n        verbs: []\n",
            "a kind grant without verbs",
        ),
        (
            "    endpoints:\n      - name: Helsinki_Bikes\n        verbs: [read]\n",
            "an endpoint name that is not DNS-1123",
        ),
        (
            "    endpoints:\n      - name: \"*\"\n        verbs: [write]\n",
            "a wildcard endpoint",
        ),
    ] {
        let profile = AgentProfile::from_yaml(&with_access(block)).expect("parses");
        assert!(profile.validate().is_err(), "{why} must be refused");
    }
    for (block, why) in [
        (
            "    kinds:\n      - kind: Endpoint\n        verbs: [write]\n",
            "a kind verb other than read or propose",
        ),
        (
            "    endpoints:\n      - name: bikes\n        verbs: [propose]\n",
            "an endpoint verb other than read or write",
        ),
        ("    hosts: [example.org]\n", "an unknown member"),
    ] {
        assert!(
            AgentProfile::from_yaml(&with_access(block)).is_err(),
            "{why} must not parse"
        );
    }
}

#[test]
fn reasoning_effort_is_optional_and_one_of_three() {
    let profile = AgentProfile::from_yaml(VALID_YAML).expect("valid yaml");
    assert_eq!(profile.spec.model.reasoning_effort, None);
    assert!(!profile.to_yaml().unwrap().contains("reasoningEffort"));

    for (value, effort) in [
        ("low", ReasoningEffort::Low),
        ("medium", ReasoningEffort::Medium),
        ("high", ReasoningEffort::High),
    ] {
        let yaml = VALID_YAML.replace(
            "maxTokensPerRun: 2000000",
            &format!("maxTokensPerRun: 2000000\n    reasoningEffort: {value}"),
        );
        let profile = AgentProfile::from_yaml(&yaml).expect("valid effort");
        profile.validate().expect("valid profile");
        assert_eq!(profile.spec.model.reasoning_effort, Some(effort));
        let again = AgentProfile::from_yaml(&profile.to_yaml().unwrap()).unwrap();
        assert_eq!(again, profile);
    }

    let yaml = VALID_YAML.replace(
        "maxTokensPerRun: 2000000",
        "maxTokensPerRun: 2000000\n    reasoningEffort: extreme",
    );
    let err = AgentProfile::from_yaml(&yaml).unwrap_err().to_string();
    assert!(err.contains("reasoningEffort"), "{err}");
}

/// The per-run byte budget of T-0557 (AG-65): what a profile has to say to get egress, and what
/// it must not say.
mod egress_budget {
    use super::*;
    use jc_core::DEFAULT_EGRESS_BYTES_PER_RUN;

    #[test]
    fn a_builder_that_names_hosts_and_no_budget_gets_the_default() {
        let profile = AgentProfile::from_yaml(VALID_YAML).expect("valid yaml");
        profile.validate().expect("valid profile");
        assert_eq!(profile.spec.egress.max_bytes_per_run, None);
        assert_eq!(
            profile.spec.egress.max_bytes_per_run(),
            DEFAULT_EGRESS_BYTES_PER_RUN
        );
    }

    #[test]
    fn a_profile_that_names_a_budget_keeps_it_through_a_round_trip() {
        let yaml = VALID_YAML.replace(
            "    allowedHosts:",
            "    maxBytesPerRun: 1048576\n    allowedHosts:",
        );
        let profile = AgentProfile::from_yaml(&yaml).expect("valid yaml");
        profile.validate().expect("valid profile");
        assert_eq!(profile.spec.egress.max_bytes_per_run(), 1_048_576);

        let again = AgentProfile::from_yaml(&profile.to_yaml().expect("serialize"))
            .expect("the serialized profile parses");
        assert_eq!(again.spec.egress.max_bytes_per_run, Some(1_048_576));
    }

    #[test]
    fn a_profile_with_no_host_has_no_budget_whatever_it_writes() {
        // The rule the proxy leans on: the budget is zero when there is nowhere to spend it,
        // so "reaches nothing" needs no second check at the door.
        let mut profile = AgentProfile::from_yaml(VALID_YAML).expect("valid yaml");
        profile.spec.egress.allowed_hosts.clear();
        profile.spec.egress.max_bytes_per_run = Some(999_999);
        assert_eq!(profile.spec.egress.max_bytes_per_run(), 0);
    }

    #[test]
    fn a_budget_of_zero_is_refused_rather_than_read_as_no_egress() {
        // Zero would be an allow-list that cannot be used: two ways to say "no egress", one of
        // which leaves reviewed hosts sitting in the manifest looking live.
        let yaml = VALID_YAML.replace(
            "    allowedHosts:",
            "    maxBytesPerRun: 0\n    allowedHosts:",
        );
        let profile = AgentProfile::from_yaml(&yaml).expect("valid yaml");
        let refused = profile.validate().expect_err("zero is refused");
        assert!(refused.to_string().contains("maxBytesPerRun"), "{refused}");
    }

    #[test]
    fn a_steward_may_not_carry_a_budget() {
        let yaml = VALID_YAML
            .replace("role: builder", "role: steward")
            .replace(
                "    allowedHosts:\n      - static.crates.io\n      - index.crates.io\n      - registry.npmjs.org",
                "    maxBytesPerRun: 1048576",
            )
            .replace(
                "  tools:\n    - shell\n    - cargo\n    - pnpm\n    - git\n    - playwright\n",
                "",
            );
        let profile = AgentProfile::from_yaml(&yaml).expect("valid yaml");
        let refused = profile.validate().expect_err("a steward reaches nothing");
        assert!(refused.to_string().contains("maxBytesPerRun"), "{refused}");
    }
}
