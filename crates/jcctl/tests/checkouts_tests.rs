//! `jcctl checkouts`: the registry's projects linked at their refs for the gateway (T-2646,
//! CC-86, CC-89).

mod common;

use common::{temp_dir, write, ENDPOINT, ORG, PROJECT, SPACE};
use jcctl::commands::checkouts::{mounted_secret, sync, Options};
use std::path::{Path, PathBuf};
use std::process::Command;

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

fn entry(git_ref: &str) -> String {
    format!(
        "apiVersion: joinedcontext.com/v1alpha1\nkind: Project\nmetadata: {{ name: ovzdusie, namespace: org }}\n\
         spec:\n  organizationRef: banskabystrica\n  repository: {{ name: ovzdusie }}\n  ref: {git_ref}\n"
    )
}

/// A forge with the project repository `ovzdusie`, the working clone that pushes to it, and an
/// organization checkout registering it at `main`.
fn setup(test: &str) -> (PathBuf, PathBuf, PathBuf, Options) {
    let forge = temp_dir(&format!("{test}-forge"));
    git(
        &forge,
        &["init", "-q", "--bare", "-b", "main", "ovzdusie.git"],
    );
    let work = temp_dir(&format!("{test}-work"));
    git(&work, &["init", "-q", "-b", "main"]);
    write(&work, ".jc/layout", "2\n");
    write(&work, "project.yaml", PROJECT);
    write(&work, "spaces/ovzdusie/space.yaml", SPACE);
    git(&work, &["add", "-A"]);
    git(&work, &["commit", "-q", "-m", "the project"]);
    push(&forge, &work);
    let org = temp_dir(&format!("{test}-org"));
    write(&org, "org.yaml", ORG);
    write(&org, ".jc/layout", "2\n");
    write(&org, "projects/ovzdusie.yaml", &entry("main"));
    let options = Options {
        forge: format!("file://{}", forge.display()),
        token_file: None,
        secrets_dir: None,
    };
    (forge, work, org, options)
}

fn push(forge: &Path, work: &Path) {
    let remote = forge.join("ovzdusie.git").to_string_lossy().into_owned();
    git(work, &["push", "-q", &remote, "main"]);
}

/// CC-86: the link follows the ref to each new commit, stays where it was when a fetch fails,
/// and goes with the registry entry; the gateway reads nothing but the linked tree.
#[test]
fn the_link_follows_the_ref_and_the_registry() {
    let (forge, work, org, options) = setup("checkouts-follow");
    let projects = temp_dir("checkouts-follow-projects");

    let first = sync(&org, &projects, &options).expect("a pass");
    assert_eq!(first.len(), 1);
    assert!(first[0].moved);
    let link = projects.join("ovzdusie");
    assert!(link.is_symlink() && link.join("project.yaml").is_file());
    assert!(
        !link.join(".git").exists(),
        "the gateway reads a tree, not a clone"
    );
    assert!(
        !sync(&org, &projects, &options).expect("again")[0].moved,
        "nothing moved"
    );

    write(&work, "spaces/ovzdusie/endpoints/public-air.yaml", ENDPOINT);
    git(&work, &["add", "-A"]);
    git(&work, &["commit", "-q", "-m", "the endpoint"]);
    push(&forge, &work);
    assert!(sync(&org, &projects, &options).expect("a pass")[0].moved);
    assert!(link
        .join("spaces/ovzdusie/endpoints/public-air.yaml")
        .is_file());

    write(&org, "projects/ovzdusie.yaml", &entry("v9"));
    let failed = sync(&org, &projects, &options).expect("a pass");
    assert!(failed[0]
        .result
        .as_ref()
        .is_err_and(|err| err.contains("v9")));
    assert!(
        link.join("spaces/ovzdusie/endpoints/public-air.yaml")
            .is_file(),
        "a failed fetch keeps the last checkout"
    );

    std::fs::remove_file(org.join("projects/ovzdusie.yaml")).expect("unregister");
    assert!(sync(&org, &projects, &options).expect("a pass").is_empty());
    assert!(
        !link.exists() && !link.is_symlink(),
        "the link goes with the entry"
    );
}

/// CC-89: an external repository's Secret is read from its mounted directory, and neither a
/// missing mount nor a name that walks out of it reads anything; no error carries a value.
#[test]
fn a_mounted_secret_is_read_by_name_only() {
    let secret = |name: &str| jc_core::SecretRef {
        name: name.to_owned(),
        key: None,
        env_var: None,
    };
    let dir = temp_dir("checkouts-secrets");
    write(&dir, "region-git-ro/username", "reader\n");
    write(&dir, "region-git-ro/password", "s3cr3t-value\n");
    let (user, password) = mounted_secret(Some(&dir), &secret("region-git-ro")).expect("read");
    assert_eq!(
        (user.as_str(), password.expose()),
        ("reader", "s3cr3t-value")
    );

    let unmounted = mounted_secret(None, &secret("region-git-ro")).expect_err("no mount");
    assert!(unmounted.contains("--secrets-dir"), "{unmounted}");
    let outside = mounted_secret(Some(&dir), &secret("../etc")).expect_err("out of the mount");
    assert!(outside.contains("not a Secret name"), "{outside}");
    let empty = mounted_secret(Some(&dir), &secret("absent")).expect_err("no such Secret");
    assert!(!empty.contains("s3cr3t"), "{empty}");
}
