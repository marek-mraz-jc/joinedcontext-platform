//! The command line over layout 2: `plan`, `validate` and `validate --project` take a layout 2
//! organization and its project checkouts (T-2640, CC-86, CC-90).

mod common;

use common::{temp_dir, write, ENDPOINT, ORG, PROJECT, SPACE};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const ENTRY: &str = "apiVersion: joinedcontext.com/v1alpha1\nkind: Project\nmetadata:\n  \
                     name: ovzdusie\n  namespace: org\nspec:\n  organizationRef: banskabystrica\n  \
                     repository: { name: ovzdusie }\n  ref: main\n";

fn layout_2(test: &str) -> (PathBuf, PathBuf) {
    let org = temp_dir(&format!("{test}-org"));
    write(&org, "org.yaml", ORG);
    write(&org, ".jc/layout", "2\n");
    write(&org, "projects/ovzdusie.yaml", ENTRY);
    let project = temp_dir(&format!("{test}-project"));
    write(&project, ".jc/layout", "2\n");
    write(&project, "project.yaml", PROJECT);
    write(&project, "spaces/ovzdusie/space.yaml", SPACE);
    write(
        &project,
        "spaces/ovzdusie/endpoints/public-air.yaml",
        ENDPOINT,
    );
    (org, project)
}

fn jcctl(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_jcctl"))
        .args(args)
        .env_remove("JC_GATEWAY_URL")
        .output()
        .expect("jcctl runs")
}

fn text(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

/// CC-86: `plan` of a layout 2 organization with its project checkout says what `plan` of the
/// layout 1 repository says.
#[test]
fn plan_of_layout_2_says_what_plan_of_layout_1_says() {
    let single = common::demo_repo("cli-plan-single");
    let (org, project) = layout_2("cli-plan");
    let before = jcctl(&["plan", "--repo-dir", &text(&single), "--json"]);
    let pair = format!("ovzdusie={}", text(&project));
    let after = jcctl(&[
        "plan",
        "--repo-dir",
        &text(&org),
        "--project-dir",
        &pair,
        "--json",
    ]);
    assert_eq!(
        after.status.code(),
        before.status.code(),
        "{}",
        String::from_utf8_lossy(&after.stderr)
    );
    assert_eq!(after.stdout, before.stdout);

    let refused = jcctl(&["plan", "--repo-dir", &text(&single), "--project-dir", &pair]);
    assert!(!refused.status.success());
    assert!(String::from_utf8_lossy(&refused.stderr).contains("layout 1"));
}

/// CC-86, CC-90: `validate` takes the organization with its checkouts, and `validate
/// --project` one project repository with a parameter value.
#[test]
fn validate_takes_an_organization_or_one_project() {
    let (org, project) = layout_2("cli-validate");
    let pair = format!("ovzdusie={}", text(&project));
    let organization = jcctl(&[
        "validate",
        "--repo-dir",
        &text(&org),
        "--project-dir",
        &pair,
    ]);
    assert!(
        organization.status.success(),
        "{}",
        String::from_utf8_lossy(&organization.stderr)
    );

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
    let alone = jcctl(&[
        "validate",
        "--project",
        &text(&project),
        "--param",
        "audience=public",
    ]);
    assert!(
        alone.status.success(),
        "{}",
        String::from_utf8_lossy(&alone.stderr)
    );
    let without = jcctl(&["validate", "--project", &text(&project)]);
    assert!(!without.status.success());
    assert!(String::from_utf8_lossy(&without.stderr).contains("audience"));
}
