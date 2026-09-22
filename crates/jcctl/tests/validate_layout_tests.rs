//! `jcctl validate` over layout 2: an assembled organization, and one project repository
//! alone, the gate its own CI runs (T-2640, CC-86, CC-88, CC-90).

mod common;

use common::{temp_dir, write, ENDPOINT, ORG, PROJECT, SPACE};
use jcctl::assemble::Directories;
use jcctl::commands::validate::{run, run_assembled, run_project};
use serde_json::json;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

const ENTRY: &str = "apiVersion: joinedcontext.com/v1alpha1\nkind: Project\nmetadata:\n  \
                     name: ovzdusie\n  namespace: org\nspec:\n  organizationRef: banskabystrica\n  \
                     repository: { name: ovzdusie }\n  ref: main\n";

fn project_repository(test: &str) -> PathBuf {
    let dir = temp_dir(&format!("{test}-project"));
    write(&dir, ".jc/layout", "2\n");
    write(&dir, "project.yaml", PROJECT);
    write(&dir, "spaces/ovzdusie/space.yaml", SPACE);
    write(&dir, "spaces/ovzdusie/endpoints/public-air.yaml", ENDPOINT);
    dir
}

fn organization(test: &str) -> PathBuf {
    let dir = temp_dir(&format!("{test}-org"));
    write(&dir, "org.yaml", ORG);
    write(&dir, ".jc/layout", "2\n");
    write(&dir, "projects/ovzdusie.yaml", ENTRY);
    dir
}

fn with(slug: &str, dir: &Path) -> Directories {
    Directories(BTreeMap::from([(slug.to_owned(), dir.to_path_buf())]))
}

fn messages(report: &jcctl::commands::validate::Report) -> Vec<String> {
    report.findings.iter().map(ToString::to_string).collect()
}

/// CC-86: an assembled organization validates like the one repository it replaces.
#[test]
fn an_assembled_organization_validates_as_its_layout_1_twin() {
    let single = run(&common::demo_repo("validate-single"));
    let project = project_repository("validate-assembled");
    let assembled = run_assembled(
        &organization("validate-assembled"),
        &with("ovzdusie", &project),
    );
    assert!(assembled.is_valid(), "{:?}", messages(&assembled));
    assert_eq!(assembled.checked, single.checked);
}

/// CC-86: a project whose checkout is missing is a finding on its registry entry.
#[test]
fn a_project_without_its_checkout_is_a_finding() {
    let report = run_assembled(&organization("validate-missing"), &Directories::default());
    let found = messages(&report);
    assert!(
        found
            .iter()
            .any(|m| m.starts_with("projects/ovzdusie.yaml")
                && m.contains("could not be checked out")),
        "{found:?}"
    );
}

/// CC-90: a project repository validates alone, without its organization, and its findings
/// name the paths of the project repository.
#[test]
fn a_project_repository_validates_alone_with_its_own_paths() {
    let project = project_repository("validate-alone");
    let report = run_project(&project, None, "example.org", &BTreeMap::new());
    assert!(report.is_valid(), "{:?}", messages(&report));
    assert!(report.checked >= 3, "{}", report.checked);

    write(
        &project,
        "spaces/ovzdusie/endpoints/public-air.yaml",
        &ENDPOINT.replace("audience: public", "audience: everyone"),
    );
    let report = run_project(&project, None, "example.org", &BTreeMap::new());
    let found = messages(&report);
    assert!(
        found
            .iter()
            .any(|m| m.starts_with("spaces/ovzdusie/endpoints/public-air.yaml")),
        "{found:?}"
    );
}

/// CC-88, CC-90: a parameter without a default needs a value to check the project with, and
/// `--param` gives it; a project named `{project}` needs the slug to check it under.
#[test]
fn a_project_is_checked_with_its_parameters_and_its_slug() {
    let project = project_repository("validate-parameters");
    write(
        &project,
        "project.yaml",
        &(PROJECT.to_owned() + "  parameters:\n    audience: { type: string }\n"),
    );
    write(
        &project,
        "spaces/ovzdusie/endpoints/public-air.yaml",
        &ENDPOINT.replace("audience: public", "audience: \"{param:audience}\""),
    );
    let without = run_project(&project, None, "example.org", &BTreeMap::new());
    assert!(
        messages(&without).iter().any(|m| m.contains("audience")),
        "{:?}",
        messages(&without)
    );
    let values = BTreeMap::from([("audience".to_owned(), json!("public"))]);
    let given = run_project(&project, None, "example.org", &values);
    assert!(given.is_valid(), "{:?}", messages(&given));

    write(
        &project,
        "project.yaml",
        &(PROJECT.replace("name: ovzdusie", "name: \"{project}\"")
            + "  parameters:\n    audience: { type: string, default: public }\n"),
    );
    let unnamed = run_project(&project, None, "example.org", &BTreeMap::new());
    assert!(
        messages(&unnamed).iter().any(|m| m.contains("--slug")),
        "{:?}",
        messages(&unnamed)
    );
    let named = run_project(&project, Some("ovzdusie"), "example.org", &BTreeMap::new());
    assert!(named.is_valid(), "{:?}", messages(&named));
}
