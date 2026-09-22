//! Checkouts fetched with git at each registry entry's ref (T-2640, CC-86, CC-89).

mod common;

use common::{temp_dir, write, ENDPOINT, ORG, PROJECT, SPACE};
use jc_core::kinds::Project;
use jcctl::assemble::{assemble, Credential, GitCheckouts, Resolver};
use std::cell::Cell;
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

/// A forge holding the bare repository `ovzdusie`, pushed from a working clone with a tag.
fn forge(test: &str) -> (PathBuf, PathBuf) {
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
    git(&work, &["tag", "v1.0.0"]);
    let remote = forge.join("ovzdusie.git").to_string_lossy().into_owned();
    git(&work, &["push", "-q", &remote, "main", "--tags"]);
    (forge, work)
}

fn entry(git_ref: &str) -> Project {
    Project::from_yaml(&format!(
        "apiVersion: joinedcontext.com/v1alpha1\nkind: Project\nmetadata: {{ name: ovzdusie, namespace: org }}\n\
         spec:\n  organizationRef: banskabystrica\n  repository: {{ name: ovzdusie }}\n  ref: {git_ref}\n"
    ))
    .expect("the entry parses")
}

fn no_secrets(_: &jc_core::SecretRef) -> Result<jcctl::assemble::Basic, String> {
    Err("no secret store in this test".into())
}

fn checkouts<'a>(
    forge: &Path,
    cache: PathBuf,
    secrets: &'a dyn Fn(&jc_core::SecretRef) -> Result<jcctl::assemble::Basic, String>,
) -> GitCheckouts<'a> {
    GitCheckouts {
        cache,
        forge: format!("file://{}", forge.display()),
        forge_credential: Some(("reader".into(), jc_core::Secret::new("forge-read-token"))),
        secrets,
    }
}

/// CC-86: a tag and a branch are checked out at their commits; a moved branch is fetched again
/// and the checkout of the old commit goes; a ref that names nothing is refused by name.
#[test]
fn each_ref_is_checked_out_at_its_commit() {
    let (forge, work) = forge("git-refs");
    let cache = temp_dir("git-refs-cache");
    let resolver = checkouts(&forge, cache.clone(), &no_secrets);

    let tagged = resolver
        .checkout("ovzdusie", &entry("v1.0.0").spec)
        .expect("the tag");
    assert!(tagged.join("project.yaml").is_file());
    assert!(
        !tagged.join(".git").exists(),
        "a checkout is a tree, not a clone"
    );

    write(&work, "spaces/ovzdusie/endpoints/public-air.yaml", ENDPOINT);
    git(&work, &["add", "-A"]);
    git(&work, &["commit", "-q", "-m", "the endpoint"]);
    let remote = forge.join("ovzdusie.git").to_string_lossy().into_owned();
    git(&work, &["push", "-q", &remote, "main"]);

    let first = resolver
        .checkout("ovzdusie", &entry("main").spec)
        .expect("main");
    assert!(first
        .join("spaces/ovzdusie/endpoints/public-air.yaml")
        .is_file());
    assert!(
        !tagged.exists(),
        "the checkout of another commit is removed"
    );

    let missing = resolver
        .checkout("ovzdusie", &entry("v9.9.9").spec)
        .expect_err("no such ref");
    assert!(missing.contains("v9.9.9"), "{missing}");
}

/// CC-89: a repository of the forge is read with the forge's credential; one outside it with
/// its own secretRef or none, and the forge's credential never goes to it.
#[test]
fn an_external_repository_never_gets_the_forge_credential() {
    assert_eq!(
        GitCheckouts::credential(&entry("main").spec),
        Ok(Credential::Forge)
    );

    let external = |secret: &str| {
        Project::from_yaml(&format!(
            "apiVersion: joinedcontext.com/v1alpha1\nkind: Project\nmetadata: {{ name: air, namespace: org }}\n\
             spec:\n  organizationRef: banskabystrica\n  repository: {{ url: \"https://git.region.sk/air.git\"{secret} }}\n  ref: main\n"
        ))
        .expect("parses")
    };
    let with_secret = external(", secretRef: { name: region-git-ro }");
    match GitCheckouts::credential(&with_secret.spec) {
        Ok(Credential::SecretRef(secret)) => assert_eq!(secret.name, "region-git-ro"),
        other => panic!("expected the entry's secretRef, got {other:?}"),
    }
    assert_eq!(
        GitCheckouts::credential(&external("").spec),
        Ok(Credential::Anonymous)
    );

    // The secret store is asked for the entry's own secret, and a refusal stops the fetch.
    let asked = Cell::new(0);
    let refusing = |secret: &jc_core::SecretRef| -> Result<jcctl::assemble::Basic, String> {
        asked.set(asked.get() + 1);
        Err(format!("{} is not in the store", secret.name))
    };
    let resolver = checkouts(
        Path::new("/nonexistent"),
        temp_dir("git-external-cache"),
        &refusing,
    );
    let refused = resolver
        .checkout("air", &with_secret.spec)
        .expect_err("the secret is missing");
    assert!(refused.contains("region-git-ro"), "{refused}");
    assert_eq!(asked.get(), 1);
}

/// CC-86: an organization assembles from the forge through git, at the entry's ref.
#[test]
fn an_organization_assembles_from_the_forge() {
    let (forge, _work) = forge("git-assemble");
    let org = temp_dir("git-assemble-org");
    write(&org, "org.yaml", ORG);
    write(&org, ".jc/layout", "2\n");
    write(
        &org,
        "projects/ovzdusie.yaml",
        &serde_norway::to_string(&entry("v1.0.0")).expect("yaml"),
    );
    let resolver = checkouts(&forge, temp_dir("git-assemble-cache"), &no_secrets);
    let assembly = assemble(
        &org,
        &resolver,
        &temp_dir("git-assemble-into").join("r"),
        None,
    )
    .expect("assembles from git");
    assert!(assembly.entries[0].rendered && assembly.entries[0].error.is_none());
    assert!(assembly
        .repository
        .iter()
        .any(|(id, _)| id.kind == "ContextSpace" && id.name == "ovzdusie"));
}
