//! `jcctl artifacts rebuild`: the artifact store re-rendered from Git (DM-44, PF-29, T-0827).

mod common;

use common::*;
use jcctl::commands::artifacts::{self, Options};
use std::path::{Path, PathBuf};

const MODEL: &str = r#"apiVersion: joinedcontext.com/v1alpha1
kind: DataModel
metadata:
  name: air-quality
  namespace: ovzdusie
spec:
  contextSpaceRef: ovzdusie
  linkml: ./air-quality.linkml.yaml
  version: 1.2.0
  lifecycle: published
  classes: [AirQualityObserved]
  artifacts:
    jsonSchema: ./json-schema/air-quality.v1.json
    context: ./context/air-quality.jsonld
"#;

const MAPPING: &str = r#"apiVersion: joinedcontext.com/v1alpha1
kind: Mapping
metadata:
  name: sensors-to-air
  namespace: ovzdusie
spec:
  contextSpaceRef: ovzdusie
  source: { name: sensors, version: 1.0.0 }
  target: { name: air-quality, version: 1.0.0 }
  transformation: {}
  artifacts:
    bloblang: ./generated/sensors-to-air.blobl
"#;

const LINKML: &str =
    "id: https://example.org/air-quality\nclasses:\n  AirQualityObserved:\n    slots: [pm10]\n";
const SCHEMA: &str = "{\"$schema\":\"http://json-schema.org/draft-07/schema#\"}";
const BLOBL: &str = "root = this\n";

fn repo(name: &str) -> PathBuf {
    let dir = demo_repo(name);
    let models = "projects/ovzdusie/spaces/ovzdusie/datamodels";
    common::write(&dir, &format!("{models}/air-quality.yaml"), MODEL);
    common::write(&dir, &format!("{models}/air-quality.linkml.yaml"), LINKML);
    common::write(
        &dir,
        &format!("{models}/json-schema/air-quality.v1.json"),
        SCHEMA,
    );
    common::write(
        &dir,
        &format!("{models}/mappings/sensors-to-air.yaml"),
        MAPPING,
    );
    common::write(
        &dir,
        &format!("{models}/mappings/generated/sensors-to-air.blobl"),
        BLOBL,
    );
    dir
}

fn options(out: &Path) -> Options {
    Options {
        out_dir: out.to_path_buf(),
        space: None,
        revision: Some("3f9c2e1".to_owned()),
    }
}

#[test]
fn every_declared_artifact_is_written_under_its_store_prefix_with_an_index() {
    let dir = repo("rebuild-repo");
    let out = temp_dir("rebuild-out");

    let report = artifacts::rebuild(&dir, &options(&out)).expect("the rebuild runs");

    let schema = out.join("schemas/banskabystrica/ovzdusie/ovzdusie/air-quality/v1");
    assert_eq!(
        std::fs::read_to_string(schema.join("air-quality.v1.json")).expect("the schema"),
        SCHEMA
    );
    assert_eq!(
        std::fs::read_to_string(schema.join("air-quality.linkml.yaml")).expect("the source"),
        LINKML,
        "the LinkML source is an object of the store too (DM-44)"
    );
    let index: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(schema.join("index.json")).expect("index"))
            .expect("the index is json");
    assert_eq!(index["sourceRevision"], "3f9c2e1");
    assert_eq!(
        index["objects"]["air-quality.v1.json"]["bytes"],
        SCHEMA.len()
    );
    assert!(
        index["objects"]["air-quality.v1.json"]["sha256"]
            .as_str()
            .is_some_and(|hash| hash.len() == 64),
        "{index}"
    );

    assert_eq!(
        std::fs::read_to_string(
            out.join(
                "mappings/banskabystrica/ovzdusie/ovzdusie/sensors-to-air/sensors-to-air.blobl"
            )
        )
        .expect("the compiled mapping"),
        BLOBL
    );

    // The `@context` the manifest declares is not in the repository, so it is named, not
    // invented: an operator sees what `jcctl model generate` still has to produce.
    assert_eq!(report.missing, vec!["air-quality/artifacts.context"]);

    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_dir_all(&out);
}

#[test]
fn a_second_rebuild_of_an_unchanged_repository_writes_the_same_bytes() {
    // DM-44: byte-identical for the same source commit, so a restore can be compared.
    let dir = repo("rebuild-twice");
    let (first, second) = (temp_dir("rebuild-twice-a"), temp_dir("rebuild-twice-b"));

    let one = artifacts::rebuild(&dir, &options(&first)).expect("the first rebuild");
    let two = artifacts::rebuild(&dir, &options(&second)).expect("the second rebuild");
    assert_eq!(one.written, two.written);

    for key in &one.written {
        assert_eq!(
            std::fs::read(first.join(key)).expect("first"),
            std::fs::read(second.join(key)).expect("second"),
            "{key} differs between two rebuilds of the same commit"
        );
    }

    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_dir_all(&first);
    let _ = std::fs::remove_dir_all(&second);
}

#[test]
fn a_space_filter_rebuilds_only_that_space() {
    let dir = repo("rebuild-space");
    let out = temp_dir("rebuild-space-out");
    let mut options = options(&out);
    options.space = Some("doprava".to_owned());

    let report = artifacts::rebuild(&dir, &options).expect("the rebuild runs");

    assert!(report.written.is_empty(), "{:?}", report.written);
    assert!(!out.join("schemas").exists());

    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_dir_all(&out);
}

/// DM-44: a repository that declares no Organization still rebuilds, under the project's name
/// where the organization would stand, rather than failing or writing to an empty segment.
#[test]
fn a_repository_without_an_organization_writes_under_the_project_name() {
    let dir = repo("rebuild-no-org-strict");
    std::fs::remove_file(dir.join("org.yaml")).expect("the organization manifest");
    let out = temp_dir("rebuild-no-org-strict-out");

    artifacts::rebuild(&dir, &options(&out)).expect("the rebuild runs");

    assert!(out
        .join("schemas/ovzdusie/ovzdusie/ovzdusie/air-quality/v1/air-quality.v1.json")
        .is_file());
    assert!(!out.join("schemas/banskabystrica").exists());

    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_dir_all(&out);
}

// --- T-2525: the edge cases of `rebuild` (DM-44, PF-29) ---

const MODELS: &str = "projects/ovzdusie/spaces/ovzdusie/datamodels";

/// `repo(name)` with the model's manifest replaced by `MODEL` edited with `edit`.
fn repo_with_model(name: &str, edit: impl Fn(&str) -> String) -> PathBuf {
    let dir = repo(name);
    common::write(&dir, &format!("{MODELS}/air-quality.yaml"), &edit(MODEL));
    dir
}

/// A file outside the repository, the one a climbing path would reach.
fn outside_secret(name: &str) -> PathBuf {
    let dir = temp_dir(name);
    std::fs::write(dir.join("outside.txt"), "outside-the-repository").expect("the file");
    dir
}

/// No object of the store holds the bytes of the file outside.
fn store_holds_nothing_from_outside(out: &Path) -> bool {
    fn walk(dir: &Path) -> Vec<PathBuf> {
        std::fs::read_dir(dir)
            .map(|entries| {
                entries
                    .flatten()
                    .flat_map(|e| {
                        if e.path().is_dir() {
                            walk(&e.path())
                        } else {
                            vec![e.path()]
                        }
                    })
                    .collect()
            })
            .unwrap_or_default()
    }
    walk(out).iter().all(|file| {
        !std::fs::read_to_string(file)
            .unwrap_or_default()
            .contains("outside-the-repository")
    })
}

/// PF-29: a declared file that is a link out of the repository is not read into the store.
#[test]
fn a_declared_file_that_links_outside_the_repository_is_not_read() {
    let outside = outside_secret("rebuild-link");
    let dir = repo("rebuild-link-repo");
    let schema = dir.join(MODELS).join("json-schema/air-quality.v1.json");
    std::fs::remove_file(&schema).expect("the schema file");
    std::os::unix::fs::symlink(outside.join("outside.txt"), &schema).expect("the link");
    let out = temp_dir("rebuild-link-out");
    let _ = artifacts::rebuild(&dir, &options(&out));
    assert!(store_holds_nothing_from_outside(&out));
}

/// DM-22: whatever the version says, the store prefix is `v` and a number, and nothing panics.
#[test]
fn a_version_without_a_numeric_major_never_panics_and_prefixes_a_number() {
    for (n, (version, major)) in [
        ("1", "v1"),
        ("one.two.three", "v0"),
        ("", "v0"),
        ("99999999999999999999.0.0", "v0"),
    ]
    .iter()
    .enumerate()
    {
        let dir = repo_with_model(&format!("rebuild-version-{n}"), |m| {
            m.replace("version: 1.2.0", &format!("version: \"{version}\""))
        });
        let out = temp_dir(&format!("rebuild-version-out-{n}"));
        if let Ok(report) = artifacts::rebuild(&dir, &options(&out)) {
            let prefix = format!("schemas/banskabystrica/ovzdusie/ovzdusie/air-quality/{major}/");
            assert!(
                report.written.iter().any(|k| k.starts_with(&prefix)),
                "{version}: {:?}",
                report.written
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}

/// PF-29: two projects with a model of one name keep two prefixes.
#[test]
fn two_models_of_one_name_in_two_projects_do_not_collide_in_the_store() {
    let dir = repo("rebuild-two-projects");
    common::write(
        &dir,
        "projects/doprava/project.yaml",
        &PROJECT.replace("ovzdusie", "doprava"),
    );
    common::write(
        &dir,
        "projects/doprava/spaces/doprava/space.yaml",
        &SPACE.replace("ovzdusie", "doprava"),
    );
    let other = "projects/doprava/spaces/doprava/datamodels";
    common::write(
        &dir,
        &format!("{other}/air-quality.yaml"),
        &MODEL.replace("ovzdusie", "doprava"),
    );
    common::write(
        &dir,
        &format!("{other}/air-quality.linkml.yaml"),
        "id: https://example.org/other\n",
    );
    let out = temp_dir("rebuild-two-projects-out");
    let report = artifacts::rebuild(&dir, &options(&out)).expect("the rebuild runs");
    let ours =
        out.join("schemas/banskabystrica/ovzdusie/ovzdusie/air-quality/v1/air-quality.linkml.yaml");
    let theirs =
        out.join("schemas/banskabystrica/doprava/doprava/air-quality/v1/air-quality.linkml.yaml");
    assert_eq!(std::fs::read_to_string(ours).expect("ours"), LINKML);
    assert_eq!(
        std::fs::read_to_string(theirs).expect("theirs"),
        "id: https://example.org/other\n"
    );
    assert!(report
        .written
        .iter()
        .any(|k| k.starts_with("schemas/banskabystrica/doprava/")));
}

/// PF-29: a repository without an Organization names the store by the project.
#[test]
fn a_missing_organization_manifest_falls_back_to_the_project_name() {
    let dir = repo("rebuild-no-org");
    for entry in std::fs::read_dir(&dir).expect("the checkout").flatten() {
        let text = std::fs::read_to_string(entry.path()).unwrap_or_default();
        if text.contains("kind: Organization") {
            std::fs::remove_file(entry.path()).expect("remove the organization");
        }
    }
    let out = temp_dir("rebuild-no-org-out");
    if let Ok(report) = artifacts::rebuild(&dir, &options(&out)) {
        assert!(
            report
                .written
                .iter()
                .all(|k| k.split('/').nth(1) == Some("ovzdusie")),
            "{:?}",
            report.written
        );
    }
}

/// DM-44: a filter naming no space writes nothing and says nothing is missing.
#[test]
fn a_space_filter_that_matches_nothing_writes_an_empty_report() {
    let dir = repo("rebuild-no-match");
    let out = temp_dir("rebuild-no-match-out");
    let mut options = options(&out);
    options.space = Some("no-such-space".to_owned());
    let report = artifacts::rebuild(&dir, &options).expect("the rebuild runs");
    assert_eq!(report, artifacts::Report::default());
}

/// DM-44: a store that cannot be written is an error naming where.
#[test]
fn an_out_dir_that_cannot_be_written_reports_a_file_error_naming_the_path() {
    let dir = repo("rebuild-readonly");
    let blocker = temp_dir("rebuild-readonly-out").join("store");
    std::fs::write(&blocker, "a file where the store's folder should be").expect("the blocker");
    let error = artifacts::rebuild(&dir, &options(&blocker)).expect_err("not a folder");
    match error {
        artifacts::Error::File { path, .. } => assert!(path.contains("store"), "{path}"),
        other => panic!("expected a file error, got {other}"),
    }
}

/// DM-44: a field neither declared on top nor under `artifacts` is neither written nor missing.
#[test]
fn a_field_the_manifest_does_not_declare_is_neither_written_nor_missing() {
    let dir = repo_with_model("rebuild-undeclared", |m| {
        m.replace("    context: ./context/air-quality.jsonld\n", "")
    });
    let out = temp_dir("rebuild-undeclared-out");
    let report = artifacts::rebuild(&dir, &options(&out)).expect("the rebuild runs");
    assert!(report.missing.is_empty(), "{:?}", report.missing);
    assert!(
        report.written.iter().all(|k| !k.contains("jsonld")),
        "{:?}",
        report.written
    );
}

/// PF-29: a declared path that climbs out of the manifest's folder never brings a file of the
/// host into the store, for every field `rebuild` reads.
#[test]
fn an_artifact_path_climbing_out_is_refused_for_every_declared_field() {
    let outside = outside_secret("rebuild-outside");
    let climb = format!(
        "../../../../../../../../..{}/outside.txt",
        outside.display()
    );
    for (n, (from, to)) in [
        (
            "linkml: ./air-quality.linkml.yaml",
            format!("linkml: {climb}"),
        ),
        (
            "jsonSchema: ./json-schema/air-quality.v1.json",
            format!("jsonSchema: {climb}"),
        ),
        (
            "context: ./context/air-quality.jsonld",
            format!("context: {climb}"),
        ),
    ]
    .iter()
    .enumerate()
    {
        let dir = repo_with_model(&format!("rebuild-climb-{n}"), |m| m.replace(from, to));
        let out = temp_dir(&format!("rebuild-climb-out-{n}"));
        let result = artifacts::rebuild(&dir, &options(&out));
        assert!(result.is_err(), "{from}: {result:?}");
        assert!(store_holds_nothing_from_outside(&out), "{from}");
        let _ = std::fs::remove_dir_all(&dir);
    }
    let dir = repo("rebuild-climb-mapping");
    common::write(
        &dir,
        &format!("{MODELS}/mappings/sensors-to-air.yaml"),
        &MAPPING.replace(
            "bloblang: ./generated/sensors-to-air.blobl",
            &format!("bloblang: {climb}"),
        ),
    );
    let out = temp_dir("rebuild-climb-mapping-out");
    assert!(artifacts::rebuild(&dir, &options(&out)).is_err());
    assert!(store_holds_nothing_from_outside(&out));
}

/// PF-29: an absolute path does not replace the manifest's folder.
#[test]
fn an_absolute_artifact_path_is_refused_at_load() {
    let outside = outside_secret("rebuild-absolute");
    let absolute = outside.join("outside.txt");
    let dir = repo_with_model("rebuild-absolute-repo", |m| {
        m.replace(
            "linkml: ./air-quality.linkml.yaml",
            &format!("linkml: {}", absolute.display()),
        )
    });
    let out = temp_dir("rebuild-absolute-out");
    assert!(artifacts::rebuild(&dir, &options(&out)).is_err());
    assert!(store_holds_nothing_from_outside(&out));
}
