//! A Project names the domain of the body that publishes its data (T-4375, PF-108).
//!
//! One installation hosts several publishers under one Organization; `spec.domain` names the
//! project's own, and an absent one is the Organization's. It is checked as the Organization's
//! domain is.

use jc_core::kinds::Project;

fn project(domain: Option<&str>) -> Project {
    let line = domain.map_or(String::new(), |d| format!("  domain: {d}\n"));
    Project::from_yaml(&format!(
        "apiVersion: joinedcontext.com/v1alpha1\nkind: Project\nmetadata: {{ name: zilina, namespace: org }}\n\
         spec:\n  organizationRef: {{ kind: Organization, name: helsinki }}\n{line}"
    ))
    .expect("the manifest parses")
}

/// PF-108: the field is optional; with it the project carries its publisher's domain.
#[test]
fn a_project_may_name_its_publishers_domain() {
    let without = project(None);
    without
        .validate()
        .expect("a project without a domain is valid");
    assert_eq!(without.spec.domain, None);

    let with = project(Some("zilina.sk"));
    with.validate().expect("a project with a domain is valid");
    assert_eq!(with.spec.domain.as_deref(), Some("zilina.sk"));
}

/// PF-108, PF-41: what is not a lowercase DNS name of two labels or more is refused, naming the
/// field.
#[test]
fn a_domain_that_is_not_one_is_refused() {
    for bad in [
        "zilina",
        "Zilina.SK",
        "zilina.sk.",
        ".zilina.sk",
        "https://zilina.sk",
        "zilina.sk:443",
        "\"\"",
    ] {
        let error = project(Some(bad)).validate().expect_err(bad).to_string();
        assert!(error.contains("spec.domain"), "{bad}: {error}");
    }
}

/// PF-61, T-5607: a project may keep itself to its members; absent, the Organization decides.
#[test]
fn a_project_may_keep_itself_to_its_members() {
    use jc_core::kinds::organization::ProjectVisibility;

    assert_eq!(project(None).spec.visibility, None);
    let members = Project::from_yaml(
        "apiVersion: joinedcontext.com/v1alpha1\nkind: Project\nmetadata: { name: zilina, namespace: org }\n\
         spec:\n  organizationRef: { kind: Organization, name: helsinki }\n  visibility: members\n",
    )
    .expect("the manifest parses");
    members.validate().expect("valid");
    assert_eq!(members.spec.visibility, Some(ProjectVisibility::Members));
    assert!(Project::from_yaml(
        "apiVersion: joinedcontext.com/v1alpha1\nkind: Project\nmetadata: { name: zilina, namespace: org }\n\
         spec:\n  organizationRef: { kind: Organization, name: helsinki }\n  visibility: granted\n",
    )
    .is_err());
}
