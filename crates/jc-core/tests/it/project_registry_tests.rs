//! The project registry and the project file of layout 2 (T-2639, PF-85, PF-86, CC-85, CC-86).
//!
//! A `Project` with `spec.repository` is a registry entry in the organization repository; one
//! without it is the project file at the root of a project repository. The two carry disjoint
//! fields, and the loader reads them as one project under the registry slug.

use jc_core::kinds::Project;
use jc_core::project::{self, RepositoryRole};
use serde_json::json;

const ENTRY: &str = r#"apiVersion: joinedcontext.com/v1alpha1
kind: Project
metadata: { name: air, namespace: org }
spec:
  organizationRef: { kind: Organization, name: helsinki }
  repository: { name: air }
  ref: v1.4.0
  parameters: { stationCount: 12, ingestToken: air-ingest-token }
"#;

const FILE: &str = r#"apiVersion: joinedcontext.com/v1alpha1
kind: Project
metadata: { name: air, namespace: org }
spec:
  organizationRef: { kind: Organization, name: helsinki }
  version: 1.4.0
  quotas: { contextSpaces: 5 }
  parameters:
    stationCount: { type: integer, default: 8, description: "Stations the feed reads" }
    ingestToken: { type: secret }
"#;

fn refused(yaml: &str) -> String {
    let project = Project::from_yaml(yaml).expect("the manifest parses");
    project
        .validate()
        .expect_err("the manifest is refused")
        .to_string()
}

/// PF-86: a registry entry names a repository and a ref and carries values; the project file
/// carries the version and the declarations. Both validate, and each knows which it is.
#[test]
fn a_registry_entry_and_a_project_file_both_validate() {
    let entry = Project::from_yaml(ENTRY).expect("the entry parses");
    entry.validate().expect("the entry is valid");
    assert!(entry.spec.is_registry_entry());

    let file = Project::from_yaml(FILE).expect("the project file parses");
    file.validate().expect("the project file is valid");
    assert!(!file.spec.is_registry_entry());
    assert_eq!(
        file.spec.version.as_ref().map(|v| v.to_string()),
        Some("1.4.0".into())
    );
}

/// PF-86: an external repository is a URL with a secretRef; a local one is a name. A URL that
/// carries a credential, a plain-HTTP URL and a repository naming both are refused.
#[test]
fn the_repository_is_a_local_name_or_an_https_url() {
    let external = ENTRY.replace(
        "repository: { name: air }",
        "repository: { url: \"https://git.region.sk/udp/air.git\", secretRef: { name: region-git-ro } }",
    );
    Project::from_yaml(&external)
        .expect("parses")
        .validate()
        .expect("an https url with a secretRef is valid");

    let with_userinfo = ENTRY.replace(
        "repository: { name: air }",
        "repository: { url: \"https://robot:hunter2@git.region.sk/air.git\" }",
    );
    let message = refused(&with_userinfo);
    assert!(message.contains("spec.repository.url"), "{message}");
    assert!(!message.contains("hunter2"), "{message}");

    let plain = ENTRY.replace(
        "repository: { name: air }",
        "repository: { url: \"http://git.region.sk/air.git\" }",
    );
    assert!(refused(&plain).contains("https"));

    let both = ENTRY.replace(
        "repository: { name: air }",
        "repository: { name: air, url: \"https://git.region.sk/air.git\" }",
    );
    assert!(refused(&both).contains("spec.repository"));

    let secret_on_local = ENTRY.replace(
        "repository: { name: air }",
        "repository: { name: air, secretRef: { name: region-git-ro } }",
    );
    assert!(refused(&secret_on_local).contains("secretRef"));
}

/// PF-86: an entry without a ref, or one naming an option, is refused; a ref belongs to an entry.
#[test]
fn a_registry_entry_pins_a_ref_and_only_an_entry_does() {
    assert!(refused(&ENTRY.replace("  ref: v1.4.0\n", "")).contains("spec.ref"));
    for bad in [
        "\"--upload-pack=x\"",
        "\"main..dev\"",
        "\"has space\"",
        "\"\"",
    ] {
        let message = refused(&ENTRY.replace("ref: v1.4.0", &format!("ref: {bad}")));
        assert!(message.contains("spec.ref"), "{bad}: {message}");
    }
    let file_with_ref = FILE.replace("  version: 1.4.0\n", "  version: 1.4.0\n  ref: main\n");
    assert!(refused(&file_with_ref).contains("spec.ref"));
}

/// CC-88: values belong to the entry and declarations to the project file; a version and
/// quotas belong to the project file too, so no field is set in both.
#[test]
fn the_entry_and_the_file_carry_disjoint_fields() {
    let entry_declaring = ENTRY.replace(
        "parameters: { stationCount: 12, ingestToken: air-ingest-token }",
        "parameters: { stationCount: { type: integer } }",
    );
    assert!(refused(&entry_declaring).contains("stationCount"));

    let file_with_value = FILE.replace(
        "stationCount: { type: integer, default: 8, description: \"Stations the feed reads\" }",
        "stationCount: 8",
    );
    assert!(refused(&file_with_value).contains("stationCount"));

    let entry_with_version = ENTRY.replace("  ref: v1.4.0\n", "  ref: v1.4.0\n  version: 1.4.0\n");
    assert!(refused(&entry_with_version).contains("spec.version"));

    let entry_with_quotas =
        ENTRY.replace("  ref: v1.4.0\n", "  ref: v1.4.0\n  quotas: { apps: 1 }\n");
    assert!(refused(&entry_with_quotas).contains("spec.quotas"));
}

/// PF-86: the registry path is `projects/{slug}.yaml`, and a slug that is not a DNS-1123 label
/// is refused, in the path and in the entry's name alike.
#[test]
fn a_registry_slug_is_a_dns_label() {
    assert_eq!(
        project::registry_path("air").expect("a label"),
        "projects/air.yaml"
    );
    for bad in ["Air", "air_quality", "", "-air", "a".repeat(64).as_str()] {
        assert!(project::registry_path(bad).is_err(), "{bad:?}");
    }
    let bad_name = ENTRY.replace(
        "name: air, namespace: org",
        "name: Air_Quality, namespace: org",
    );
    let _ = refused(&bad_name);
}

/// CC-85: an organization repository without `.jc/layout` is layout 1; a project repository
/// always says `2`; a layout no loader knows is refused, naming the ones it knows.
#[test]
fn the_layout_file_is_read_and_an_unknown_layout_refused() {
    assert_eq!(
        project::layout_of(None, RepositoryRole::Organization).expect("1"),
        1
    );
    assert_eq!(
        project::layout_of(Some("2\n"), RepositoryRole::Organization).expect("2"),
        2
    );
    assert_eq!(
        project::layout_of(Some(" 2 "), RepositoryRole::Project).expect("2"),
        2
    );

    let missing = project::layout_of(None, RepositoryRole::Project).expect_err("refused");
    assert!(missing.to_string().contains(".jc/layout"), "{missing}");
    let one = project::layout_of(Some("1"), RepositoryRole::Project).expect_err("refused");
    assert!(one.to_string().contains("2"), "{one}");
    for bad in ["3", "two", "", "-1", "2.0"] {
        let message = project::layout_of(Some(bad), RepositoryRole::Organization)
            .expect_err("refused")
            .to_string();
        assert!(message.contains("1, 2"), "{bad:?}: {message}");
    }
}

/// CC-86, PF-86: a manifest of a project repository is mounted under the registry slug, so two
/// entries on one repository are two projects; a namespace naming another project is refused.
#[test]
fn the_registry_slug_renders_the_project_for_two_entries_on_one_repository() {
    let endpoint = json!({
        "apiVersion": "joinedcontext.com/v1alpha1",
        "kind": "Endpoint",
        "metadata": { "name": "public-air" },
        "spec": { "contextSpaceRef": "air" }
    });
    let project_file = json!({
        "apiVersion": "joinedcontext.com/v1alpha1",
        "kind": "Project",
        "metadata": { "name": "{project}", "namespace": "org" },
        "spec": {}
    });
    for slug in ["air", "air-staging"] {
        let mut mounted = endpoint.clone();
        project::mount(&mut mounted, slug).expect("mounted");
        assert_eq!(mounted["metadata"]["namespace"], slug);

        let mut project = project_file.clone();
        project::mount(&mut project, slug).expect("mounted");
        assert_eq!(project["metadata"]["name"], slug);
        assert_eq!(project["metadata"]["namespace"], "org");
    }

    let mut placeholder = endpoint.clone();
    placeholder["metadata"]["namespace"] = json!("{project}");
    project::mount(&mut placeholder, "air-staging").expect("the placeholder renders");
    assert_eq!(placeholder["metadata"]["namespace"], "air-staging");

    // A project file migrated from layout 1 keeps its name, which runs under that slug only.
    let mut migrated = project_file.clone();
    migrated["metadata"]["name"] = json!("air");
    project::mount(&mut migrated.clone(), "air").expect("its own slug");
    assert!(project::mount(&mut migrated, "air-staging").is_err());

    let mut foreign = endpoint.clone();
    foreign["metadata"]["namespace"] = json!("transport");
    let message = project::mount(&mut foreign, "air")
        .expect_err("refused")
        .to_string();
    assert!(
        message.contains("transport") && message.contains("air"),
        "{message}"
    );

    let mut organization_kind = json!({ "kind": "Group", "metadata": { "name": "stewards" } });
    assert!(project::mount(&mut organization_kind, "air").is_err());
}
