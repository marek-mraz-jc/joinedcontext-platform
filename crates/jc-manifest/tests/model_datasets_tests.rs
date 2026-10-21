//! T-5066: the write path agrees with SHACL on the generated test datasets.
//!
//! Model Tools' `gen_datasets` makes a valid and an invalid dataset of a model and proves its
//! SHACL shapes and its JSON Schema agree on every record. The gateway and the Portal hold a
//! write to the `ModelSchema` compiled from that JSON Schema, so this test holds the same records
//! to it: a valid record, normalized as a pipeline writes it, has no problem; an invalid one has
//! a problem at the slot it breaks.
//!
//! The committed fixtures are `gen_datasets -o` of the Model Tools fixtures; CI regenerates them
//! from every committed model into `MODEL_DATASETS_DIR` and runs this test on those.

use jc_manifest::model_schema::ModelSchema;
use serde_json::Value;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

fn read(path: &Path) -> Value {
    let text = std::fs::read_to_string(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    serde_json::from_str(&text).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

fn models(dir: &Path) -> Vec<PathBuf> {
    let mut found: Vec<PathBuf> = std::fs::read_dir(dir)
        .unwrap_or_else(|e| panic!("{}: {e}", dir.display()))
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| path.to_string_lossy().ends_with(".schema.json"))
        .collect();
    found.sort();
    found
}

/// Every disagreement of one model's datasets with its compiled schema.
fn disagreements(schema_path: &Path) -> Vec<String> {
    let stem = schema_path
        .to_string_lossy()
        .trim_end_matches(".schema.json")
        .to_owned();
    let valid = read(Path::new(&format!("{stem}.valid.json")));
    let invalid = read(Path::new(&format!("{stem}.invalid.json")));
    let records = valid.as_array().cloned().unwrap_or_default();
    let classes: Vec<String> = records
        .iter()
        .filter_map(|record| record.get("type").and_then(Value::as_str))
        .map(str::to_owned)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let schema = ModelSchema::compile("datasets", "1.0.0", &read(schema_path), &classes, false);
    let normalized = |record: &Value| {
        let class = record
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default();
        schema.normalize(class, record)
    };
    let mut out = Vec::new();
    for record in &records {
        let problems = schema.check(&normalized(record));
        if !problems.is_empty() {
            out.push(format!(
                "{stem}: valid {} refused: {problems:?}",
                record["type"]
            ));
        }
    }
    for case in invalid.as_array().cloned().unwrap_or_default() {
        let path = case["path"].as_str().unwrap_or_default();
        let problems = schema.check(&normalized(&case["record"]));
        if !problems.iter().any(|problem| problem.path == path) {
            out.push(format!(
                "{stem}: invalid {}.{path} ({}) not refused at {path}: {problems:?}",
                case["record"]["type"], case["constraint"]
            ));
        }
    }
    out
}

#[test]
fn the_write_path_agrees_with_shacl_on_every_generated_record() {
    let dir = std::env::var_os("MODEL_DATASETS_DIR").map_or_else(
        || Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/model-datasets"),
        PathBuf::from,
    );
    let found = models(&dir);
    assert!(
        !found.is_empty(),
        "{} holds no generated dataset",
        dir.display()
    );
    let problems: Vec<String> = found.iter().flat_map(|path| disagreements(path)).collect();
    assert!(problems.is_empty(), "{}", problems.join("\n"));
}
