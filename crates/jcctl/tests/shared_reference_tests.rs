//! The loader resolves an `endpointRef` to the slug this environment minted, and never to a
//! slug the referring project may not use (EP-77, EP-15, T-1448).

use jcctl::commands::validate;
use jcctl::loader::{Repository, ResourceId};
use std::path::{Path, PathBuf};

const SLUG: &str = "scsd2eehkx42n53z2zyd6vshfh7s7irf";

fn write(dir: &Path, rel: &str, body: &str) {
    let path = dir.join(rel);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, body).unwrap();
}

/// Project `source` publishes `bikes` with `audience`; project `user` refers to it by name.
fn repo(test: &str, source: &str, user: &str, audience: &str, reference: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("jcctl-shared-ref-{test}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    write(&dir, "org.yaml", "apiVersion: joinedcontext.com/v1alpha1\nkind: Organization\nmetadata:\n  name: helsinki\n  namespace: org\nspec:\n  domain: hel.fi\n  locales: [\"en\"]\n  defaultLocale: en\n");
    for project in [source, user] {
        write(&dir, &format!("projects/{project}/project.yaml"), &format!("apiVersion: joinedcontext.com/v1alpha1\nkind: Project\nmetadata:\n  name: {project}\n  namespace: org\nspec:\n  organizationRef: helsinki\n"));
    }
    write(&dir, &format!("projects/{source}/spaces/hub/space.yaml"), &format!("apiVersion: joinedcontext.com/v1alpha1\nkind: ContextSpace\nmetadata:\n  name: hub\n  namespace: {source}\nspec:\n  isSandbox: false\n"));
    write(&dir, &format!("projects/{source}/spaces/hub/endpoints/bikes.yaml"), &format!("apiVersion: joinedcontext.com/v1alpha1\nkind: Endpoint\nmetadata:\n  name: bikes\n  namespace: {source}\nspec:\n  slug: {SLUG}\n  contextSpaceRef: hub\n  audience: {audience}\n  enabledRepresentations: [ngsi-ld]\n"));
    write(&dir, &format!("projects/{user}/shared/city-bikes.yaml"), &format!("apiVersion: joinedcontext.com/v1alpha1\nkind: SharedSpaceReference\nmetadata:\n  name: city-bikes\n  namespace: {user}\nspec:\n  endpointRef: {reference}\n  alias: city-bikes\n"));
    dir
}

fn resolved(dir: &Path, user: &str) -> serde_json::Value {
    let repo = Repository::load(dir).expect("the repository loads");
    let id = ResourceId {
        group: "joinedcontext.com".to_owned(),
        kind: "SharedSpaceReference".to_owned(),
        namespace: Some(user.to_owned()),
        name: "city-bikes".to_owned(),
    };
    repo.get(&id)
        .expect("the reference loaded")
        .manifest
        .spec
        .clone()
}

#[test]
fn a_ref_resolves_to_the_slug_of_this_environment() {
    let dir = repo(
        "resolves",
        "helsinki",
        "helsinki-mobility",
        "organization",
        "{ project: helsinki, name: bikes }",
    );
    let spec = resolved(&dir, "helsinki-mobility");
    assert_eq!(spec["endpointSlug"], SLUG);
    assert!(spec.get("endpointRef").is_none());
    assert!(validate::run(&dir)
        .findings
        .iter()
        .all(|f| !f.message.contains("SharedSpaceReference")));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_pair_copied_under_two_new_names_resolves_with_no_edit_but_the_ref_project() {
    // A bundle imported under new names carries its reference as a name, so only the
    // project the import renamed changes; the slug is whatever this environment minted.
    let dir = repo(
        "renamed",
        "espoo",
        "espoo-mobility",
        "project-list\n  allowedProjects: [espoo-mobility]",
        "{ project: espoo, name: bikes }",
    );
    assert_eq!(resolved(&dir, "espoo-mobility")["endpointSlug"], SLUG);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_ref_to_a_missing_endpoint_is_a_finding_naming_it() {
    let dir = repo(
        "missing",
        "helsinki",
        "helsinki-mobility",
        "organization",
        "{ project: helsinki, name: trams }",
    );
    let report = validate::run(&dir);
    let finding = report
        .findings
        .iter()
        .find(|f| f.message.contains("SharedSpaceReference city-bikes"))
        .unwrap_or_else(|| panic!("{:?}", report.findings));
    assert!(
        finding.message.contains("helsinki/trams"),
        "{}",
        finding.message
    );
    assert_eq!(
        finding.path,
        PathBuf::from("projects/helsinki-mobility/shared/city-bikes.yaml")
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn an_endpoint_that_does_not_admit_the_project_is_refused_and_its_slug_never_leaks() {
    let dir = repo(
        "refused",
        "helsinki",
        "helsinki-mobility",
        "project-list\n  allowedProjects: [helsinki-kpi]",
        "{ project: helsinki, name: bikes }",
    );
    let spec = resolved(&dir, "helsinki-mobility");
    assert!(spec.get("endpointSlug").is_none(), "{spec}");
    assert!(!spec.to_string().contains(SLUG));
    let report = validate::run(&dir);
    assert!(
        report.findings.iter().any(|f| f
            .message
            .contains("not shared with project helsinki-mobility")),
        "{:?}",
        report.findings
    );
    let _ = std::fs::remove_dir_all(&dir);
}
