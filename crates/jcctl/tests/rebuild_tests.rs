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
