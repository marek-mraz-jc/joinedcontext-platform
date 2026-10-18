//! A Project names the body that publishes its data for its own catalogue (T-4586, EP-127).
//!
//! The publisher carries its registry IRI, because a catalogue's publisher is an IRI in every
//! national profile; the block is checked as an Endpoint's is, and it belongs to the project
//! file, never to a registry entry.

use jc_core::kinds::Project;

fn project(spec: &str) -> Result<Project, String> {
    let project = Project::from_yaml(&format!(
        "apiVersion: joinedcontext.com/v1alpha1\nkind: Project\nmetadata: {{ name: banskabystrica, namespace: org }}\n\
         spec:\n  organizationRef: {{ kind: Organization, name: hel }}\n{spec}"
    ))
    .map_err(|e| e.to_string())?;
    project.validate().map_err(|e| e.to_string())?;
    Ok(project)
}

const PUBLISHER: &str = "  catalog:\n    publisher:\n      name: { sk: Mesto Banská Bystrica }\n      uri: https://data.gov.sk/id/legal-subject/00313271\n";

#[test]
fn a_project_names_its_publisher_and_desk() {
    let named = project(&format!(
        "{PUBLISHER}    contactPoint: {{ name: Oddelenie informatiky, email: opendata@example.org }}\n"
    ))
    .expect("valid");
    let catalog = named.spec.catalog.expect("the block");
    assert_eq!(
        catalog.publisher.uri.as_deref(),
        Some("https://data.gov.sk/id/legal-subject/00313271")
    );
    assert_eq!(
        catalog.contact_point.map(|c| c.email).as_deref(),
        Some("opendata@example.org")
    );
    assert!(project("").expect("valid").spec.catalog.is_none());
}

#[test]
fn a_publisher_without_its_iri_or_with_a_bad_one_is_refused_naming_the_field() {
    let error =
        project("  catalog:\n    publisher: { name: { sk: Mesto } }\n").expect_err("no uri");
    assert!(error.contains("spec.catalog.publisher.uri"), "{error}");
    let error = project(
        "  catalog:\n    publisher: { name: { sk: Mesto }, uri: \"data.gov.sk/id/legal-subject/1\" }\n",
    )
    .expect_err("not absolute");
    assert!(error.contains("spec.catalog.publisher.uri"), "{error}");
    let error = project("  catalog:\n    publisher: { name: {}, uri: https://example.org/ }\n")
        .expect_err("no name");
    assert!(error.contains("spec.catalog.publisher.name"), "{error}");
}

#[test]
fn a_person_or_a_broken_address_is_no_desk() {
    let error = project(&format!(
        "{PUBLISHER}    contactPoint: {{ name: Desk, email: not-an-address }}\n"
    ))
    .expect_err("bad email");
    assert!(error.contains("spec.catalog.contactPoint.email"), "{error}");
    let error = project(&format!(
        "{PUBLISHER}    contactPoint: {{ name: \"\", email: opendata@example.org }}\n"
    ))
    .expect_err("no name");
    assert!(error.contains("spec.catalog.contactPoint.name"), "{error}");
}

#[test]
fn an_unknown_member_and_a_registry_entry_carrying_the_block_are_refused() {
    let error = project(&format!("{PUBLISHER}    homepage: https://example.org/\n"))
        .expect_err("unknown member");
    assert!(error.contains("homepage"), "{error}");
    let error = project(&format!(
        "  repository: {{ url: https://git.example.org/bb.git }}\n  ref: v1.0.0\n{PUBLISHER}"
    ))
    .expect_err("registry entry");
    assert!(error.contains("spec.catalog"), "{error}");
}
