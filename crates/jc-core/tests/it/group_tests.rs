//! `kind: Group` (PF-62, PF-64): membership as configuration.
use jc_core::envelope::Kind;
use jc_core::kinds::GroupSpec;
use jc_core::registry;

const GROUP: &str = r#"apiVersion: joinedcontext.com/v1alpha1
kind: Group
metadata: { name: air-quality-team, namespace: org }
spec:
  description: The air quality domain, measurement and modelling
  members:
    - { user: 3f2a9c1e-7b4d-4e8a-9c61-0d5e2b7f8a14 }
    - { user: 8b1d4e2f-0c3a-4f5b-9e6d-7a2c1b0e9f38 }
"#;

#[test]
fn a_group_parses_with_its_members_and_belongs_beside_the_roles() {
    let spec: GroupSpec = serde_norway::from_str(
        serde_norway::from_str::<serde_norway::Value>(GROUP)
            .expect("yaml")
            .get("spec")
            .map(|spec| serde_norway::to_string(spec).expect("spec"))
            .expect("a spec")
            .as_str(),
    )
    .expect("a group");
    assert_eq!(
        spec.members().collect::<Vec<_>>(),
        vec![
            "3f2a9c1e-7b4d-4e8a-9c61-0d5e2b7f8a14",
            "8b1d4e2f-0c3a-4f5b-9e6d-7a2c1b0e9f38"
        ]
    );
    assert_eq!(GroupSpec::PATH_TEMPLATE, "users/groups/{name}.yaml");
    assert_eq!(GroupSpec::PLURAL, "groups");
    assert!(registry::validate_yaml("Group", GROUP)
        .expect("a known kind")
        .is_ok());
}

#[test]
fn a_field_the_kind_does_not_have_is_refused() {
    let yaml = GROUP.replace("  description:", "  keycloakId: 42\n  description:");
    let err = registry::validate_yaml("Group", &yaml)
        .expect("a known kind")
        .expect_err("an unknown field is refused");
    assert!(err.to_string().contains("keycloakId"), "{err}");
}

/// PF-126, ADR-N-061: a member is a Keycloak user id; an e-mail, a username or an upper-case
/// id is refused with the field named, so no personal data reaches Git history.
#[test]
fn a_member_that_is_not_a_keycloak_id_is_refused_and_so_is_the_same_person_twice() {
    for named in [
        "jana.kovacova@banskabystrica.sk",
        "jana.kovacova",
        "3F2A9C1E-7B4D-4E8A-9C61-0D5E2B7F8A14",
        " 3f2a9c1e-7b4d-4e8a-9c61-0d5e2b7f8a14",
        "3f2a9c1e7b4d4e8a9c610d5e2b7f8a14",
    ] {
        let yaml = GROUP.replace(
            "{ user: 3f2a9c1e-7b4d-4e8a-9c61-0d5e2b7f8a14 }",
            &format!("{{ user: \"{named}\" }}"),
        );
        let err = registry::validate_yaml("Group", &yaml)
            .expect("a known kind")
            .expect_err("a member is a Keycloak user id");
        assert!(
            err.to_string().contains("spec.members[].user"),
            "{named}: {err}"
        );
        assert!(
            err.to_string().contains("Keycloak user id"),
            "{named}: {err}"
        );
    }

    let twice = GROUP.replace(
        "8b1d4e2f-0c3a-4f5b-9e6d-7a2c1b0e9f38",
        "3f2a9c1e-7b4d-4e8a-9c61-0d5e2b7f8a14",
    );
    let err = registry::validate_yaml("Group", &twice)
        .expect("a known kind")
        .expect_err("nobody is in the group twice");
    assert!(err.to_string().contains("twice"), "{err}");
}

#[test]
fn a_group_with_no_members_is_legal_and_matches_nobody() {
    let yaml = r#"apiVersion: joinedcontext.com/v1alpha1
kind: Group
metadata: { name: nobody-yet, namespace: org }
spec: {}
"#;
    assert!(registry::validate_yaml("Group", yaml)
        .expect("a known kind")
        .is_ok());
    let spec: GroupSpec = serde_norway::from_str("{}").expect("an empty spec");
    assert_eq!(spec.members().count(), 0);
}
