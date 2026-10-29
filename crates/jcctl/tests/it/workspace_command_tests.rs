//! `jcctl workspace render|diff` (CC-76, CC-78, T-1238).

use jcctl::commands::workspace::{diff_dirs, render, Operation};
use std::path::{Path, PathBuf};
use std::process::Command;

fn write(dir: &Path, rel: &str, body: &str) {
    let path = dir.join(rel);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, body).unwrap();
}

const ORG: &str = "apiVersion: joinedcontext.com/v1alpha1\nkind: Organization\nmetadata:\n  name: hel\n  namespace: org\nspec:\n  domain: hel.fi\n  locales: [\"en\"]\n  defaultLocale: en\n";
const PROJECT: &str = "apiVersion: joinedcontext.com/v1alpha1\nkind: Project\nmetadata:\n  name: helsinki\n  namespace: org\nspec:\n  organizationRef: hel\n";

fn space(name: &str, spec: &str) -> String {
    format!("apiVersion: joinedcontext.com/v1alpha1\nkind: ContextSpace\nmetadata:\n  name: {name}\n  namespace: helsinki\nspec:\n{spec}")
}

/// A repository with the organization, project `helsinki` and the given spaces.
fn repo(test: &str, spaces: &[(&str, &str)]) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("jcctl-ws-{test}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    write(&dir, "org.yaml", ORG);
    write(&dir, "projects/helsinki/project.yaml", PROJECT);
    for (name, spec) in spaces {
        write(
            &dir,
            &format!("projects/helsinki/spaces/{name}/space.yaml"),
            &space(name, spec),
        );
    }
    dir
}

#[test]
fn render_prefixes_every_project_namespace_and_space() {
    let dir = repo("render", &[("bikes", "  defaultLocale: en\n")]);
    let files = render(&dir, "ws-demo-").expect("rendered");
    let names: Vec<&str> = files.iter().map(|(name, _)| name.as_str()).collect();
    assert!(
        names.contains(&"ws-demo-helsinki/contextspace-bikes.yaml"),
        "{names:?}"
    );
    assert!(
        names.contains(&"org/project-ws-demo-helsinki.yaml"),
        "{names:?}"
    );
    let (_, bikes) = files
        .iter()
        .find(|(name, _)| name.ends_with("contextspace-bikes.yaml"))
        .unwrap();
    assert!(bikes.contains("namespace: ws-demo-helsinki"), "{bikes}");
}

#[test]
fn an_empty_prefix_renders_the_repository_as_it_is() {
    let dir = repo("plain", &[("bikes", "  defaultLocale: en\n")]);
    let files = render(&dir, "").expect("rendered");
    assert!(files
        .iter()
        .any(|(name, _)| name == "helsinki/contextspace-bikes.yaml"));
}

#[test]
fn render_of_a_repository_that_does_not_load_is_an_error() {
    let dir = repo("broken", &[]);
    write(
        &dir,
        "projects/helsinki/spaces/x/space.yaml",
        "kind: [not a manifest",
    );
    assert!(render(&dir, "ws-x-").is_err());
}

#[test]
fn diff_names_what_the_workspace_creates_changes_and_removes() {
    let base = repo(
        "base",
        &[
            (
                "bikes",
                "  defaultLocale: en\n  isSandbox: true\n  ttlDays: 3\n",
            ),
            ("gone", "  defaultLocale: en\n"),
        ],
    );
    let ours = repo(
        "ours",
        &[
            ("bikes", "  defaultLocale: fi\n  isSandbox: true\n"),
            ("air", "  defaultLocale: en\n"),
        ],
    );
    let entries = diff_dirs(&base, &ours).expect("compared");
    let summary: Vec<(String, Operation)> = entries
        .iter()
        .map(|e| (e.id.name.clone(), e.operation))
        .collect();
    assert_eq!(
        summary,
        [
            ("air".to_owned(), Operation::Create),
            ("bikes".to_owned(), Operation::Update),
            ("gone".to_owned(), Operation::Delete),
        ]
    );
    let bikes = &entries[1].fields;
    let paths: Vec<&str> = bikes.iter().map(|f| f.path.as_str()).collect();
    assert_eq!(paths, ["spec.defaultLocale", "spec.ttlDays"]);
    assert_eq!(bikes[1].declared, None, "removed in the workspace");
    assert_eq!(bikes[1].live, Some(serde_json::json!(3)));
}

#[test]
fn the_command_exits_2_on_a_change_and_0_on_none() {
    let base = repo("cli-base", &[("bikes", "  defaultLocale: en\n")]);
    let same = repo("cli-same", &[("bikes", "  defaultLocale: en\n")]);
    let changed = repo("cli-changed", &[("bikes", "  defaultLocale: fi\n")]);
    let run = |dir: &Path, json: bool| {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_jcctl"));
        cmd.args(["workspace", "diff", "--base-dir"])
            .arg(&base)
            .arg("--repo-dir")
            .arg(dir)
            .env_remove("JC_ENVIRONMENT");
        if json {
            cmd.arg("--json");
        }
        cmd.output().expect("ran")
    };
    let none = run(&same, false);
    assert_eq!(none.status.code(), Some(0));
    assert!(String::from_utf8_lossy(&none.stdout).contains("changes nothing"));
    let some = run(&changed, true);
    assert_eq!(some.status.code(), Some(2));
    let json: serde_json::Value = serde_json::from_slice(&some.stdout).unwrap();
    assert_eq!(json[0]["operation"], "Update");
    assert_eq!(json[0]["fields"][0]["from"], "en");
    assert_eq!(json[0]["fields"][0]["to"], "fi");

    let out = std::env::temp_dir().join(format!("jcctl-ws-out-{}", std::process::id()));
    let render = Command::new(env!("CARGO_BIN_EXE_jcctl"))
        .args(["workspace", "render", "--repo-dir"])
        .arg(&base)
        .args(["--prefix", "ws-a-", "--out-dir"])
        .arg(&out)
        .output()
        .unwrap();
    assert!(
        render.status.success(),
        "{}",
        String::from_utf8_lossy(&render.stderr)
    );
    assert!(out.join("ws-a-helsinki/contextspace-bikes.yaml").exists());
    let bad = Command::new(env!("CARGO_BIN_EXE_jcctl"))
        .args(["workspace", "render", "--repo-dir", "x", "--prefix"])
        .output()
        .unwrap();
    assert!(!bad.status.success(), "a missing prefix is a usage error");
}
