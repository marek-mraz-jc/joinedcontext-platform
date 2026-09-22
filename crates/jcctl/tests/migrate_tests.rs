//! `jcctl migrate`: a layout 1 repository becomes the organization repository and one
//! repository per project, history kept (T-2640, CC-85, CC-86, MF-47).

mod common;

use common::{temp_dir, write, ENDPOINT, ENDPOINT_PATH, ORG, PROJECT, SPACE};
use jcctl::assemble::{assemble, Directories};
use jcctl::commands::migrate;
use std::collections::BTreeMap;
use std::path::Path;
use std::process::Command;

const DOPRAVA: &str = "apiVersion: joinedcontext.com/v1alpha1\nkind: Project\nmetadata:\n  \
                       name: doprava\n  namespace: org\nspec:\n  organizationRef: banskabystrica\n";

fn git(dir: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .args(["-c", "user.name=t", "-c", "user.email=t@example.org", "-C"])
        .arg(dir)
        .args(args)
        .output()
        .expect("git runs");
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

fn commit(dir: &Path, message: &str) {
    git(dir, &["add", "-A"]);
    git(dir, &["commit", "-q", "-m", message]);
}

/// A layout 1 repository with four commits: the organization, two projects, and an endpoint.
fn layout_1(test: &str) -> std::path::PathBuf {
    let dir = temp_dir(test);
    git(&dir, &["init", "-q", "-b", "main"]);
    write(&dir, "org.yaml", ORG);
    commit(&dir, "org: the organization");
    write(&dir, "projects/ovzdusie/project.yaml", PROJECT);
    write(&dir, "projects/ovzdusie/spaces/ovzdusie/space.yaml", SPACE);
    commit(&dir, "ovzdusie: the project and its space");
    write(&dir, "projects/doprava/project.yaml", DOPRAVA);
    commit(&dir, "doprava: another project");
    write(&dir, ENDPOINT_PATH, ENDPOINT);
    commit(&dir, "ovzdusie: the public endpoint");
    dir
}

fn log(dir: &Path) -> Vec<String> {
    git(dir, &["log", "--format=%s"])
        .lines()
        .map(str::to_owned)
        .collect()
}

/// CC-85: each project repository holds, byte for byte, what `projects/{slug}/` held, and the
/// commits that touched it, with their messages; the organization repository keeps its whole
/// history and holds the registry entry where the project was.
#[test]
fn migrate_moves_each_project_with_its_history() {
    let source = layout_1("migrate-history");
    let out = temp_dir("migrate-history-out");
    let migration = migrate::run(&source, &out).expect("the migration runs");

    let slugs: Vec<&str> = migration
        .projects
        .iter()
        .map(|(s, _, _)| s.as_str())
        .collect();
    assert_eq!(slugs, ["doprava", "ovzdusie"]);

    let air = out.join("projects/ovzdusie");
    assert_eq!(
        log(&air),
        [
            "jcctl migrate: the project repository of layout 2 (CC-85)",
            "ovzdusie: the public endpoint",
            "ovzdusie: the project and its space",
        ]
    );
    for rel in [
        "project.yaml",
        "spaces/ovzdusie/space.yaml",
        "spaces/ovzdusie/endpoints/public-air.yaml",
    ] {
        assert_eq!(
            std::fs::read(air.join(rel)).expect("moved"),
            std::fs::read(source.join("projects/ovzdusie").join(rel)).expect("original"),
            "{rel}"
        );
    }
    assert_eq!(
        std::fs::read_to_string(air.join(".jc/layout")).expect("layout"),
        "2\n"
    );
    assert_eq!(
        migration.projects[1].2,
        git(&air, &["rev-parse", "HEAD"]),
        "the migration reports the head it wrote"
    );

    let org = &migration.organization;
    assert_eq!(log(org).len(), 5);
    assert!(!org.join("projects/ovzdusie").exists());
    let entry = std::fs::read_to_string(org.join("projects/ovzdusie.yaml")).expect("the entry");
    let entry = jc_core::kinds::Project::from_yaml(&entry).expect("the entry parses");
    entry.validate().expect("the entry is valid");
    assert_eq!(entry.spec.git_ref.as_deref(), Some("main"));
    assert_eq!(
        std::fs::read_to_string(org.join(".jc/layout")).expect("layout"),
        "2\n"
    );
    assert!(
        log(&source).len() == 4 && git(&source, &["branch", "--format=%(refname:short)"]) == "main"
    );
}

/// CC-86: the migrated organization assembles to the render the layout 1 repository loads.
#[test]
fn the_migrated_organization_renders_what_it_rendered_before() {
    let source = layout_1("migrate-render");
    let out = temp_dir("migrate-render-out");
    let migration = migrate::run(&source, &out).expect("the migration runs");

    let before = assemble(
        &source,
        &Directories::default(),
        &temp_dir("unused-render"),
        None,
    )
    .expect("layout 1 loads");
    let checkouts = Directories(
        migration
            .projects
            .iter()
            .map(|(slug, path, _)| (slug.clone(), path.clone()))
            .collect::<BTreeMap<_, _>>(),
    );
    let after = assemble(
        &migration.organization,
        &checkouts,
        &temp_dir("migrate-render-into").join("render"),
        None,
    )
    .expect("layout 2 assembles");

    let render = |repository: &jcctl::loader::Repository| {
        repository
            .iter()
            .map(|(id, loaded)| {
                (
                    id.to_string(),
                    loaded.path.clone(),
                    serde_json::to_value(&loaded.manifest).expect("serialises"),
                )
            })
            .collect::<Vec<_>>()
    };
    assert_eq!(render(&after.repository), render(&before.repository));
}

/// CC-85: a migration runs once, into an empty directory, from what is committed.
#[test]
fn migrate_refuses_to_run_twice_or_over_anything() {
    let source = layout_1("migrate-twice");
    let out = temp_dir("migrate-twice-out");
    let migration = migrate::run(&source, &out).expect("the first migration");

    let again = migrate::run(&migration.organization, &temp_dir("migrate-twice-again"))
        .expect_err("layout 2 is not migrated again");
    assert!(again.to_string().contains("already layout 2"), "{again}");

    let over = migrate::run(&source, &out).expect_err("the output holds the first migration");
    assert!(over.to_string().contains("not empty"), "{over}");

    write(
        &source,
        "projects/ovzdusie/project.yaml",
        "kind: half-typed\n",
    );
    let dirty = migrate::run(&source, &temp_dir("migrate-dirty")).expect_err("uncommitted");
    assert!(dirty.to_string().contains("uncommitted"), "{dirty}");

    git(&source, &["checkout", "-q", "--", "."]);
    write(&source, "projects/stray/notes.md", "not a project\n");
    commit(&source, "a directory that is no project");
    let stray = migrate::run(&source, &temp_dir("migrate-stray")).expect_err("no project.yaml");
    assert!(stray.to_string().contains("projects/stray/"), "{stray}");
}
