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
    - { user: jana.kovacova@banskabystrica.sk }
    - { user: peter.novak@banskabystrica.sk }
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
            "jana.kovacova@banskabystrica.sk",
            "peter.novak@banskabystrica.sk"
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

#[test]
fn a_member_that_is_not_an_address_is_refused_and_so_is_the_same_person_twice() {
    let yaml = GROUP.replace("jana.kovacova@banskabystrica.sk", "jana.kovacova");
    let err = registry::validate_yaml("Group", &yaml)
        .expect("a known kind")
        .expect_err("a member is an address");
    assert!(err.to_string().contains("spec.members[].user"), "{err}");

    let twice = GROUP.replace(
        "peter.novak@banskabystrica.sk",
        "jana.kovacova@banskabystrica.sk",
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
