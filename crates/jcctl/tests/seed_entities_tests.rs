//! T-0421: the seed entities of a checkout, and what the broker has to be told about them
//! (CC-72, CC-07).

mod common;

use jcctl::entities::{action, seed_entities, Action, SeedError};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

/// A checkout holding whatever files the caller names.
fn checkout(name: &str, files: &[(&str, &str)]) -> PathBuf {
    let root = common::temp_dir(name);
    for (path, contents) in files {
        common::write(&root, path, contents);
    }
    root
}

fn air(local: &str) -> String {
    json!({
        "id": format!("urn:ngsi-ld:AirQualityObserved:bb.sk:ovzdusie:{local}"),
        "type": "AirQualityObserved",
        "airQualityIndex": { "type": "Property", "value": 42 }
    })
    .to_string()
}

#[test]
fn every_seed_folder_of_every_space_is_read_in_path_order() {
    let root = checkout(
        "seed-order",
        &[
            (
                "projects/doprava/spaces/parkovanie/entities/seed/one.json",
                &air("1"),
            ),
            (
                "projects/ovzdusie/spaces/mestske/entities/seed/many.json",
                &format!("[{},{}]", air("2"), air("3")),
            ),
            // Not a seed entity: a manifest beside the space, which the loader reads instead.
            (
                "projects/ovzdusie/spaces/mestske/space.yaml",
                "kind: ContextSpace\n",
            ),
        ],
    );

    let seeds = seed_entities(&root).expect("the checkout reads");
    let ids: Vec<&str> = seeds.iter().map(|seed| seed.id.as_str()).collect();
    assert_eq!(
        ids,
        vec![
            "urn:ngsi-ld:AirQualityObserved:bb.sk:ovzdusie:1",
            "urn:ngsi-ld:AirQualityObserved:bb.sk:ovzdusie:2",
            "urn:ngsi-ld:AirQualityObserved:bb.sk:ovzdusie:3",
        ],
        "one file per entity and an array file both read, projects in path order"
    );
    assert_eq!(seeds[0].project, "doprava");
    assert_eq!(seeds[0].space, "parkovanie");
    assert_eq!(
        seeds[2].space, "mestske",
        "the space is the folder, not the URN"
    );
}

#[test]
fn a_repository_that_seeds_nothing_is_no_entities_and_no_error() {
    let root = checkout(
        "seed-none",
        &[(
            "projects/ovzdusie/spaces/mestske/space.yaml",
            "kind: ContextSpace\n",
        )],
    );
    assert!(seed_entities(&root)
        .expect("an unseeded checkout reads")
        .is_empty());

    let bare = checkout("seed-bare", &[]);
    assert!(seed_entities(&bare)
        .expect("a checkout with no projects reads")
        .is_empty());
}

#[test]
fn a_file_that_is_not_an_entity_is_refused_by_name() {
    for (file, contents, expected) in [
        ("broken.json", "{ not json", "is not JSON"),
        ("scalar.json", "7", "holds neither an entity nor an array"),
        (
            "manifest.json",
            r#"{"apiVersion":"joinedcontext.com/v1alpha1","kind":"Entity"}"#,
            "is a manifest",
        ),
        (
            "nameless.json",
            r#"{"type":"AirQualityObserved"}"#,
            "declares no id",
        ),
        (
            "typeless.json",
            r#"{"id":"urn:ngsi-ld:X:bb.sk:s:1"}"#,
            "declares no type",
        ),
    ] {
        let root = checkout(
            &format!("seed-malformed-{}", file.trim_end_matches(".json")),
            &[(
                &format!("projects/p/spaces/s/entities/seed/{file}"),
                contents,
            )],
        );
        let error = seed_entities(&root).expect_err(&format!("{file} is not an entity"));
        assert!(
            matches!(error, SeedError::Malformed { .. }),
            "{file}: {error:?}"
        );
        let said = error.to_string();
        assert!(
            said.contains(file),
            "{file}: the message names no file: {said}"
        );
        assert!(said.contains(expected), "{file}: {said}");
    }
}

#[test]
fn two_files_seeding_one_id_into_one_space_are_refused_rather_than_ordered() {
    let root = checkout(
        "seed-duplicate",
        &[
            ("projects/p/spaces/ovzdusie/entities/seed/a.json", &air("1")),
            ("projects/p/spaces/ovzdusie/entities/seed/b.json", &air("1")),
        ],
    );
    let error = seed_entities(&root).expect_err("the same id twice is ambiguous");
    let said = error.to_string();
    assert!(said.contains("a.json") && said.contains("b.json"), "{said}");

    // The same id in another space is another entity, and fine.
    let root = checkout(
        "seed-two-spaces",
        &[
            ("projects/p/spaces/ovzdusie/entities/seed/a.json", &air("1")),
            ("projects/p/spaces/doprava/entities/seed/a.json", &air("1")),
        ],
    );
    assert_eq!(seed_entities(&root).expect("two spaces").len(), 2);
}

fn declared() -> Value {
    serde_json::from_str(&air("1")).expect("an entity")
}

#[test]
fn an_entity_the_broker_does_not_hold_is_a_create() {
    assert_eq!(action(&declared(), None), Action::Create);
}

#[test]
fn an_entity_the_broker_holds_as_declared_is_unchanged() {
    assert_eq!(action(&declared(), Some(&declared())), Action::Unchanged);
}

#[test]
fn the_timestamps_and_the_telemetry_the_broker_owns_are_not_drift() {
    // What a broker answers for a seeded entity a pipeline has since written to: its own
    // timestamps, an attribute nobody declared, and `observedAt` beside the declared value
    // (CC-07, CC-69).
    let live = json!({
        "id": "urn:ngsi-ld:AirQualityObserved:bb.sk:ovzdusie:1",
        "type": "AirQualityObserved",
        "createdAt": "2026-09-01T10:00:00Z",
        "modifiedAt": "2026-09-17T09:12:00Z",
        "airQualityIndex": {
            "type": "Property",
            "value": 42,
            "observedAt": "2026-09-17T09:12:00Z",
            "createdAt": "2026-09-01T10:00:00Z"
        },
        "temperature": { "type": "Property", "value": 17.4 }
    });
    assert_eq!(action(&declared(), Some(&live)), Action::Unchanged);
}

#[test]
fn a_declared_value_the_broker_answers_differently_is_an_update() {
    let mut live = declared();
    live["airQualityIndex"]["value"] = json!(7);
    assert_eq!(action(&declared(), Some(&live)), Action::Update);

    let mut missing = declared();
    missing
        .as_object_mut()
        .expect("an object")
        .remove("airQualityIndex");
    assert_eq!(
        action(&declared(), Some(&missing)),
        Action::Update,
        "an attribute the broker lost is an update, not unchanged"
    );
}

#[test]
fn an_answer_that_is_not_an_entity_is_an_update_rather_than_a_panic() {
    assert_eq!(action(&declared(), Some(&json!("gone"))), Action::Update);
}

#[test]
fn the_source_file_of_every_entity_is_kept_for_the_message() {
    let root = checkout(
        "seed-source",
        &[("projects/p/spaces/s/entities/seed/one.json", &air("1"))],
    );
    let seeds = seed_entities(&root).expect("reads");
    assert_eq!(
        seeds[0].source,
        Path::new(&root).join("projects/p/spaces/s/entities/seed/one.json")
    );
}

/// CC-72: one file seeding the same id twice is refused like two files doing it, and names the
/// file, not an order the reader has to guess.
#[test]
fn one_file_seeding_one_id_twice_is_refused_naming_that_file() {
    let twice = format!("[{},{}]", air("1"), air("1"));
    let root = checkout(
        "seed-duplicate-one-file",
        &[("projects/p/spaces/ovzdusie/entities/seed/a.json", &twice)],
    );
    let error = seed_entities(&root).expect_err("the same id twice is ambiguous");
    let SeedError::Duplicate {
        id, first, second, ..
    } = &error
    else {
        panic!("a duplicate, not {error}");
    };
    assert!(id.ends_with(":ovzdusie:1"), "{id}");
    assert!(first.ends_with("a.json") && first == second, "{error}");
}
