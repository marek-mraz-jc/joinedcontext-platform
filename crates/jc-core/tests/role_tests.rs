//! T-0525: `kind: Role` and `kind: RoleBinding` (PF-49, PF-52).

use chrono::{TimeZone, Utc};
use jc_core::envelope::ResourceEnvelope;
use jc_core::error::Error;
use jc_core::kinds::{Role, RoleBindingSpec, SubjectSource, Verb};

const ROLE: &str = include_str!("golden/023-Role-12-identity-and-access.yaml");
const BINDING: &str = include_str!("golden/024-RoleBinding-12-identity-and-access.yaml");

type RoleBinding = ResourceEnvelope<RoleBindingSpec>;

fn role(replace: &str, with: &str) -> Result<Role, Error> {
    let manifest =
        Role::from_yaml(&ROLE.replace(replace, with)).map_err(|e| Error::Parse(e.to_string()))?;
    manifest.validate()?;
    Ok(manifest)
}

fn binding(replace: &str, with: &str) -> Result<RoleBinding, Error> {
    let manifest = RoleBinding::from_yaml(&BINDING.replace(replace, with))
        .map_err(|e| Error::Parse(e.to_string()))?;
    manifest.validate()?;
    Ok(manifest)
}

#[test]
fn the_documented_role_round_trips() {
    let role = role("", "").expect("the golden role validates");
    assert_eq!(role.spec.rules.len(), 2);
    assert_eq!(role.spec.rules[0].verbs, vec![Verb::Propose]);
    assert_eq!(role.spec.rules[1].constraints[0].not_in, vec!["public"]);
    let yaml = serde_norway::to_string(&role).expect("serializes");
    let again = Role::from_yaml(&yaml).expect("parses back");
    assert_eq!(again, role);
    assert!(!yaml.contains("one_of"), "serde name is `in`: {yaml}");
}

#[test]
fn the_documented_binding_round_trips() {
    let binding = binding("", "").expect("the golden binding validates");
    assert_eq!(binding.spec.subjects.len(), 2);
    assert_eq!(
        binding.spec.subjects[0].group.as_deref(),
        Some("air-quality-team")
    );
    assert_eq!(binding.spec.scope.project.as_deref(), Some("ovzdusie"));
    let yaml = serde_norway::to_string(&binding).expect("serializes");
    assert_eq!(RoleBinding::from_yaml(&yaml).expect("parses back"), binding);
}

#[test]
fn a_role_without_rules_kinds_or_verbs_is_refused() {
    let err = role("  rules:\n    - kinds: [Pipeline, DataSource, Mapping]\n      verbs: [propose]                       # propose | approve | delete\n    - kinds: [Endpoint]\n      verbs: [propose]\n      constraints:\n        - { field: spec.audience, notIn: [public] }   # a public endpoint is another role's\n", "  rules: []\n")
        .expect_err("no rules");
    assert!(err.to_string().contains("spec.rules"), "{err}");
    let err = role("kinds: [Endpoint]", "kinds: []").expect_err("no kinds");
    assert!(err.to_string().contains("kinds"), "{err}");
    let err = role("kinds: [Endpoint]", "kinds: [endpoint]").expect_err("lowercase kind");
    assert!(err.to_string().contains("Pipeline"), "{err}");
    let err = role(
        "      verbs: [propose]\n      constraints",
        "      verbs: []\n      constraints",
    )
    .expect_err("no verbs");
    assert!(err.to_string().contains("verbs"), "{err}");
    let err = role(
        "verbs: [propose]                       #",
        "verbs: [publish] #",
    )
    .expect_err("unknown verb");
    assert!(matches!(err, Error::Parse(_)), "{err}");
}

#[test]
fn a_constraint_has_exactly_one_operator_on_a_spec_field() {
    let err = role("notIn: [public] }", "notIn: [public], equals: internal }")
        .expect_err("two operators");
    assert!(err.to_string().contains("exactly one"), "{err}");
    let err = role("notIn: [public] }", "}").expect_err("no operator");
    assert!(err.to_string().contains("exactly one"), "{err}");
    let err =
        role("field: spec.audience", "field: metadata.namespace").expect_err("not a spec field");
    assert!(err.to_string().contains("spec field"), "{err}");
    let err = role("notIn: [public] }", "matches: [public] }").expect_err("unknown operator");
    assert!(matches!(err, Error::Parse(_)), "{err}");
    role("notIn: [public] }", "in: [internal, private] }").expect("`in` is an operator");
}

/// The janitor's rule: a name confines it (T-2627, PF-49).
fn janitor(pattern: &str) -> Result<Role, Error> {
    role(
        "{ field: spec.audience, notIn: [public] }",
        &format!("{{ field: metadata.name, pattern: '{pattern}' }}"),
    )
}

#[test]
fn a_pattern_on_the_name_matches_the_whole_value() {
    let role = janitor("t1[0-9]{3}[a-z]?-.+|.+-[0-9]{4}").expect("a name pattern validates");
    let constraint = &role.spec.rules[1].constraints[0];
    assert_eq!(constraint.field, "metadata.name");
    for name in ["t1588-space", "t1589r-bikes", "citybikes-0915"] {
        assert!(constraint.holds(Some(name)), "{name} is a journey's");
    }
    // Anchored on both ends: a pattern never matches a part of a name.
    for name in ["helsinki-t1588-space", "citybikes-09150", "air-quality", ""] {
        assert!(!constraint.holds(Some(name)), "{name} is nobody's residue");
    }
    assert!(!constraint.holds(None), "an absent name matches no pattern");
    let yaml = serde_norway::to_string(&role).expect("serializes");
    assert_eq!(Role::from_yaml(&yaml).expect("parses back"), role);
}

#[test]
fn a_pattern_compiles_is_short_and_is_the_only_operator() {
    let err = janitor("t1[0-9").expect_err("unclosed class");
    assert!(err.to_string().contains("regular expression"), "{err}");
    let err = janitor(&"a".repeat(257)).expect_err("too long");
    assert!(err.to_string().contains("256"), "{err}");
    janitor(&"a".repeat(256)).expect("256 characters are allowed");
    let err = role(
        "{ field: spec.audience, notIn: [public] }",
        "{ field: metadata.name, pattern: 't1.+', equals: t1 }",
    )
    .expect_err("two operators");
    assert!(err.to_string().contains("exactly one"), "{err}");
    let err = role(
        "{ field: spec.audience, notIn: [public] }",
        "{ field: metadata.name, pattern: '' }",
    )
    .expect_err("an empty pattern");
    assert!(err.to_string().contains("exactly one"), "{err}");
}

#[test]
fn every_operator_holds_the_way_the_portal_and_conftest_read_it() {
    let parse = |yaml: &str| -> jc_core::kinds::Constraint {
        serde_norway::from_str(yaml).expect("a constraint parses")
    };
    let equals = parse("{ field: spec.audience, equals: public }");
    assert!(equals.holds(Some("public")) && !equals.holds(Some("x")) && !equals.holds(None));
    let one_of = parse("{ field: spec.audience, in: [public] }");
    assert!(one_of.holds(Some("public")) && !one_of.holds(Some("x")) && !one_of.holds(None));
    // `notIn` admits an absent value: a manifest without the field is not one of the refused.
    let not_in = parse("{ field: spec.audience, notIn: [public] }");
    assert!(!not_in.holds(Some("public")) && not_in.holds(Some("x")) && not_in.holds(None));
}

#[test]
fn a_binding_needs_subjects_a_role_and_one_scope() {
    let err = binding(
        "subjects: [{ group: air-quality-team }, { user: jana.kovacova@banskabystrica.sk }]",
        "subjects: []",
    )
    .expect_err("no subjects");
    assert!(err.to_string().contains("subjects"), "{err}");
    let err = binding(
        "{ group: air-quality-team }",
        "{ group: air-quality-team, user: x }",
    )
    .expect_err("two names");
    assert!(err.to_string().contains("exactly one"), "{err}");
    let err = binding("{ group: air-quality-team }", "{ group: \"\" }").expect_err("empty name");
    assert!(err.to_string().contains("exactly one"), "{err}");
    let err = binding("{ group: air-quality-team }", "{ token: abc }")
        .expect_err("no secret field exists");
    assert!(matches!(err, Error::Parse(_)), "{err}");
    let err = binding("role: pipeline-developer", "role: Pipeline Developer")
        .expect_err("role is a label");
    assert!(
        err.to_string().contains("DNS-1123") || err.to_string().contains("label"),
        "{err}"
    );
    let err = binding(
        "scope: { project: ovzdusie }",
        "scope: { project: ovzdusie, organization: bb }",
    )
    .expect_err("two scopes");
    assert!(err.to_string().contains("exactly one"), "{err}");
    let err = binding("scope: { project: ovzdusie }", "scope: {}").expect_err("no scope");
    assert!(err.to_string().contains("exactly one"), "{err}");
}

#[test]
fn validity_is_ordered_and_bounds_are_inclusive() {
    let err = binding(
        "validity: { notAfter: \"2026-12-31T23:59:59Z\" }",
        "validity: { notBefore: \"2027-01-01T00:00:00Z\", notAfter: \"2026-12-31T23:59:59Z\" }",
    )
    .expect_err("ends before it starts");
    assert!(err.to_string().contains("notAfter"), "{err}");

    let binding = binding("", "").expect("valid");
    let validity = binding.spec.validity.as_ref().expect("has validity");
    assert!(validity.contains(Utc.with_ymd_and_hms(2026, 12, 31, 23, 59, 59).unwrap()));
    assert!(!validity.contains(Utc.with_ymd_and_hms(2027, 1, 1, 0, 0, 0).unwrap()));
    assert!(jc_core::kinds::BindingValidity::default()
        .contains(Utc.with_ymd_and_hms(2030, 6, 1, 0, 0, 0).unwrap()));
}

#[test]
fn both_kinds_live_in_users_of_the_organization_repository() {
    let role_info = jc_core::registry::by_kind("Role").expect("registered");
    // No namespace is the organization's, never a project's (PF-68).
    assert_eq!(
        role_info.repo_path("", "", "pipeline-developer"),
        "users/roles/pipeline-developer.yaml"
    );
    let binding_info = jc_core::registry::by_kind("RoleBinding").expect("registered");
    assert_eq!(
        binding_info.repo_path("", "", "ovzdusie-developers"),
        "users/assignments/ovzdusie-developers.yaml"
    );
    let err = binding("namespace: org", "namespace: ovzdusie").expect_err("organization scope");
    assert!(err.to_string().contains("org"), "{err}");
}

/// PF-59: `read` is a verb of its own, and `propose` implies it, because nobody proposes a
/// change to what they may not see. `approve` and `delete` imply nothing: a role that only
/// approves reads nothing by that rule.
#[test]
fn propose_implies_read_and_approve_does_not() {
    let rule = jc_core::kinds::role::Rule {
        kinds: vec!["Pipeline".to_owned()],
        verbs: vec![Verb::Propose],
        constraints: vec![],
    };
    assert!(rule.grants("Pipeline", Verb::Read));
    assert!(rule.grants("Pipeline", Verb::Propose));
    assert!(!rule.grants("Pipeline", Verb::Approve));
    assert!(
        !rule.grants("Endpoint", Verb::Read),
        "never outside its kinds"
    );

    let approving = jc_core::kinds::role::Rule {
        kinds: vec!["Endpoint".to_owned()],
        verbs: vec![Verb::Approve, Verb::Delete],
        constraints: vec![],
    };
    assert!(!approving.grants("Endpoint", Verb::Read));
    assert!(approving.grants("Endpoint", Verb::Approve));

    let reading = jc_core::kinds::role::Rule {
        kinds: vec!["Endpoint".to_owned()],
        verbs: vec![Verb::Read],
        constraints: vec![],
    };
    assert!(reading.grants("Endpoint", Verb::Read));
    assert!(
        !reading.grants("Endpoint", Verb::Propose),
        "read grants nothing else"
    );
}

/// The verb is written `read` in a manifest, beside the three that were there before.
#[test]
fn the_read_verb_parses_from_a_manifest() {
    let viewer = role("verbs: [propose]", "verbs: [read]").expect("a viewer role validates");
    assert_eq!(viewer.spec.rules[0].verbs, vec![Verb::Read]);
    let yaml = serde_norway::to_string(&viewer).expect("serializes");
    assert!(yaml.contains("- read"), "serde name is `read`: {yaml}");
}

// ---------------------------------------------------------------------------
// A role of a project (PF-68, T-0871)
// ---------------------------------------------------------------------------

#[test]
fn a_role_in_a_project_lands_in_that_project_and_names_project_kinds_only() {
    use jc_core::envelope::Kind;

    // The same rules, in a project: the path is the project's, not the organization's.
    let in_project = role("namespace: org", "namespace: ovzdusie").expect("a project role");
    assert_eq!(
        in_project.spec.repo_path(&in_project.metadata),
        "projects/ovzdusie/roles/pipeline-developer.yaml"
    );
    let in_org = role("", "").expect("the organization's role");
    assert_eq!(
        in_org.spec.repo_path(&in_org.metadata),
        "users/roles/pipeline-developer.yaml"
    );

    // Naming an organization kind from a project would let a project write the rules.
    for forbidden in ["Role", "RoleBinding", "Group", "Organization", "Project"] {
        let yaml = ROLE
            .replace("namespace: org", "namespace: ovzdusie")
            .replace("[Pipeline, DataSource, Mapping]", &format!("[{forbidden}]"));
        let manifest = Role::from_yaml(&yaml).expect("parses");
        let err = manifest
            .validate()
            .expect_err("{forbidden} is out of reach");
        assert!(
            err.to_string().contains(forbidden) && err.to_string().contains("PF-68"),
            "{err}"
        );
    }

    // The same kinds are a role of the organization's own, unchanged.
    let yaml = ROLE.replace("[Pipeline, DataSource, Mapping]", "[Role, RoleBinding]");
    Role::from_yaml(&yaml)
        .expect("parses")
        .validate()
        .expect("the organization's role names the organization's kinds");
}

#[test]
fn the_registry_knows_where_a_role_lives_in_either_place() {
    use jc_core::envelope::Scope;

    let info = jc_core::registry::by_kind("Role").expect("Role is catalogued");
    assert_eq!(info.scope, Scope::OrganizationOrProject);
    assert!(info.scope.allows_organization() && info.scope.allows_project());
    assert_eq!(
        info.repo_path("org", "", "pipeline-developer"),
        "users/roles/pipeline-developer.yaml"
    );
    assert_eq!(
        info.repo_path("ovzdusie", "", "air-analyst"),
        "projects/ovzdusie/roles/air-analyst.yaml"
    );

    // Every other kind lives in one place and is unchanged by the second template.
    let binding = jc_core::registry::by_kind("RoleBinding").expect("RoleBinding is catalogued");
    assert_eq!(binding.project_path_template, None);
    assert_eq!(
        binding.repo_path("ovzdusie", "", "analysts"),
        "users/assignments/analysts.yaml"
    );
}

const BUILD_LANE: &str = "\
apiVersion: joinedcontext.com/v1alpha1
kind: Role
metadata: { name: build-lane, namespace: org }
spec:
  rules:
    - kinds: [App]
      verbs: [propose]
      constraints:
        - { field: status.build }
";

fn build_lane(replace: &str, with: &str) -> Result<Role, Error> {
    let manifest = Role::from_yaml(&BUILD_LANE.replace(replace, with))
        .map_err(|e| Error::Parse(e.to_string()))?;
    manifest.validate()?;
    Ok(manifest)
}

/// AP-73, T-2636: the build lane's role names `status.build` with no operator, on `propose` of
/// `App` alone; any other status field, an operator, another kind, verb or constraint is refused.
#[test]
fn the_build_lanes_role_writes_status_build_and_nothing_beside_it() {
    let lane = build_lane("", "").expect("the build lane's role validates");
    assert!(lane.spec.rules[0].writes_status_only());

    for (replace, with, says) in [
        ("status.build }", "status.phase }", "status.build"),
        ("status.build }", "status.build, equals: x }", "carries no"),
        ("kinds: [App]", "kinds: [App, Pipeline]", "alone"),
        ("kinds: [App]", "kinds: [Pipeline]", "alone"),
        ("verbs: [propose]", "verbs: [propose, approve]", "alone"),
        (
            "- { field: status.build }",
            "- { field: status.build }\n        - { field: spec.kind, equals: static }",
            "alone",
        ),
    ] {
        let err = build_lane(replace, with).expect_err(with);
        assert!(err.to_string().contains(says), "{with}: {err}");
    }

    let editor = role("", "").expect("the golden role");
    assert!(
        editor
            .spec
            .rules
            .iter()
            .all(|rule| !rule.writes_status_only()),
        "a spec constraint is no status writer"
    );
}

/// ADR-N-031, PF-91: a person is not a manifest. A rule on `Person` names that kind alone and
/// takes `read`, `create`, `update`, `disable` and `delete`; the three person verbs mean nothing on
/// any other kind, and `propose`/`approve` mean nothing on a person.
#[test]
fn person_verbs_belong_to_person_alone() {
    let people =
        "  rules:\n    - kinds: [Person]\n      verbs: [read, create, update, disable, delete]\n";
    let golden_rules = ROLE
        .split_once("  rules:\n")
        .map(|(_, rules)| format!("  rules:\n{rules}"))
        .expect("the golden role has rules");
    let admin = role(&golden_rules, people).expect("people-admin validates");
    assert_eq!(
        admin.spec.rules[0].verbs,
        vec![
            Verb::Read,
            Verb::Create,
            Verb::Update,
            Verb::Disable,
            Verb::Delete
        ]
    );
    assert!(admin.spec.rules[0].grants("Person", Verb::Disable));
    assert!(!admin.spec.rules[0].grants("Pipeline", Verb::Disable));

    for (rules, says) in [
        ("    - kinds: [Pipeline]\n      verbs: [create]\n", "Person alone"),
        ("    - kinds: [Endpoint]\n      verbs: [disable]\n", "Person alone"),
        ("    - kinds: [Person]\n      verbs: [propose]\n", "no Change"),
        ("    - kinds: [Person]\n      verbs: [approve]\n", "no Change"),
        ("    - kinds: [Person, Pipeline]\n      verbs: [read]\n", "Person alone"),
        (
            "    - kinds: [Person]\n      verbs: [read]\n      constraints:\n        - { field: spec.email, equals: a }\n",
            "no manifest fields",
        ),
    ] {
        let err = role(&golden_rules, &format!("  rules:\n{rules}"))
            .expect_err("a person verb out of place");
        assert!(err.to_string().contains(says), "{rules}: {err}");
    }
}

/// A project role reaches project kinds only (PF-68), and a person is the organization's.
#[test]
fn a_project_role_names_no_person() {
    let spec: jc_core::kinds::RoleSpec = serde_json::from_value(serde_json::json!({
        "rules": [{ "kinds": ["Person"], "verbs": ["read"] }]
    }))
    .expect("a role spec");
    spec.validate().expect("valid in the organization");
    assert!(spec.validate_in_project().is_err());
}

#[test]
fn every_verb_is_written_as_a_manifest_writes_it() {
    for verb in [
        Verb::Read,
        Verb::Propose,
        Verb::Approve,
        Verb::Delete,
        Verb::Create,
        Verb::Update,
        Verb::Disable,
    ] {
        assert_eq!(
            serde_json::to_value(verb).expect("serialises"),
            serde_json::Value::from(verb.as_str())
        );
    }
}

/// T-1471, PF-64: a group the identity provider owns is bound with `source: provider`; the mark
/// is a group's alone and names one owner, `provider`.
#[test]
fn a_provider_group_is_marked_on_the_group_and_nowhere_else() {
    let marked = binding(
        "{ group: air-quality-team }",
        "{ group: platform-readers, source: provider }",
    )
    .expect("a marked group is a subject");
    assert_eq!(
        marked.spec.subjects[0].source,
        Some(SubjectSource::Provider)
    );
    assert_eq!(
        binding("", "").expect("unmarked").spec.subjects[0].source,
        None
    );

    let err = binding(
        "{ user: jana.kovacova@banskabystrica.sk }",
        "{ user: jana.kovacova@banskabystrica.sk, source: provider }",
    )
    .expect_err("a user has no source");
    assert!(err.to_string().contains("a user has no source"), "{err}");
    let err = binding(
        "{ group: air-quality-team }",
        "{ group: air-quality-team, source: keycloak }",
    )
    .expect_err("provider is the one source");
    assert!(matches!(err, Error::Parse(_)), "{err}");
    let err = binding("{ group: air-quality-team }", "{ source: provider }")
        .expect_err("a mark names no one");
    assert!(err.to_string().contains("exactly one"), "{err}");
}
