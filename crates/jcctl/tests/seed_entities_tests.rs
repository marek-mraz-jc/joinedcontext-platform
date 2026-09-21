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

// --- T-2529: the edge cases of `seed_entities` (CC-72, CC-04, CC-07, CC-69) ---

const SEED: &str = "entities/seed";

/// CC-72: a space is written through `/cs/{space}`, which names no project, so one id in two
/// projects' spaces of the same name is one entity twice: refused, never last-one-wins.
#[test]
fn one_id_in_same_named_spaces_of_two_projects_is_refused_as_the_duplicate_it_is() {
    let root = checkout(
        "seed-two-projects",
        &[
            (
                &format!("projects/a/spaces/ovzdusie/{SEED}/x.json"),
                &air("1"),
            ),
            (
                &format!("projects/b/spaces/ovzdusie/{SEED}/x.json"),
                &air("1"),
            ),
        ],
    );
    let error = seed_entities(&root).expect_err("one entity seeded twice");
    assert!(matches!(error, SeedError::Duplicate { .. }), "{error:?}");
}

/// CC-72: an id or a type that is present but not a string is no id or no type.
#[test]
fn an_id_or_type_that_is_not_a_string_is_refused_by_name() {
    for (file, contents, expected) in [
        (
            "numeric-id.json",
            r#"{"id":7,"type":"AirQualityObserved"}"#,
            "declares no id",
        ),
        (
            "empty-id.json",
            r#"{"id":"","type":"AirQualityObserved"}"#,
            "declares an empty id",
        ),
        (
            "object-type.json",
            r#"{"id":"urn:ngsi-ld:X:bb.sk:s:1","type":{"a":1}}"#,
            "declares no type",
        ),
    ] {
        let root = checkout(
            &format!("seed-typed-{}", file.trim_end_matches(".json")),
            &[(&format!("projects/p/spaces/s/{SEED}/{file}"), contents)],
        );
        let said = seed_entities(&root).expect_err(file).to_string();
        assert!(
            said.contains(file) && said.contains(expected),
            "{file}: {said}"
        );
    }
}

/// CC-72: a file that is not UTF-8 is an error naming it, not a panic.
#[test]
fn a_binary_json_file_is_reported_by_name_not_a_panic() {
    let root = checkout("seed-binary", &[("projects/p/spaces/s/entities/.keep", "")]);
    let file = root
        .join("projects/p/spaces/s")
        .join(SEED)
        .join("blob.json");
    std::fs::create_dir_all(file.parent().expect("a parent")).expect("the folder");
    std::fs::write(&file, [0xff_u8, 0xfe, 0x00, 0x7b]).expect("the blob");
    let said = seed_entities(&root).expect_err("not text").to_string();
    assert!(said.contains("blob.json"), "{said}");
}

/// CC-72: an empty list seeds nothing and is not an error.
#[test]
fn a_seed_file_with_an_empty_json_array_yields_no_entities_and_no_error() {
    let root = checkout(
        "seed-empty-array",
        &[(&format!("projects/p/spaces/s/{SEED}/none.json"), "[]")],
    );
    assert!(seed_entities(&root).expect("no entities").is_empty());
}

/// CC-72: only `.json` files are seed files; a README or an editor's backup beside them is not.
#[test]
fn a_non_json_extension_file_in_the_seed_folder_is_ignored() {
    let root = checkout(
        "seed-other-files",
        &[
            (
                &format!("projects/p/spaces/s/{SEED}/README.md"),
                "# not an entity",
            ),
            (&format!("projects/p/spaces/s/{SEED}/x.json~"), "{ broken"),
            (&format!("projects/p/spaces/s/{SEED}/x.JSON"), &air("upper")),
        ],
    );
    let found = seed_entities(&root).expect("the json file only");
    assert_eq!(found.len(), 1);
    assert!(found[0].id.ends_with(":upper"));
}

/// CC-72: a folder inside the seed folder is not part of it.
#[test]
fn a_nested_directory_under_seed_is_not_recursed_into() {
    let root = checkout(
        "seed-nested",
        &[
            (&format!("projects/p/spaces/s/{SEED}/top.json"), &air("top")),
            (
                &format!("projects/p/spaces/s/{SEED}/old/stale.json"),
                &air("stale"),
            ),
        ],
    );
    let found = seed_entities(&root).expect("the top file only");
    assert_eq!(
        found
            .iter()
            .map(|e| e.id.rsplit(':').next().unwrap_or_default())
            .collect::<Vec<_>>(),
        vec!["top"]
    );
}

/// CC-72: a seed folder the process may not read is an error naming the folder.
#[test]
fn an_unreadable_seed_directory_is_reported_by_path_not_a_panic() {
    use std::os::unix::fs::PermissionsExt as _;
    let root = checkout(
        "seed-locked",
        &[(&format!("projects/p/spaces/s/{SEED}/x.json"), &air("1"))],
    );
    let folder = root.join("projects/p/spaces/s").join(SEED);
    std::fs::set_permissions(&folder, std::fs::Permissions::from_mode(0o000)).expect("lock");
    let read = seed_entities(&root);
    std::fs::set_permissions(&folder, std::fs::Permissions::from_mode(0o755)).expect("unlock");
    // Root reads through any mode; everyone else gets the error, and it names the folder.
    if let Err(error) = read {
        assert!(matches!(error, SeedError::Unreadable { .. }), "{error:?}");
        assert!(error.to_string().contains("entities/seed"), "{error}");
    }
}

/// CC-04: an id is the entity's name, never a path: slashes and dots in it stay as written.
#[test]
fn an_entity_id_containing_path_segments_is_kept_as_an_opaque_string() {
    let id = "urn:ngsi-ld:AirQualityObserved:bb.sk:ovzdusie:../../etc/passwd";
    let body = json!({ "id": id, "type": "AirQualityObserved" }).to_string();
    let root = checkout(
        "seed-path-id",
        &[(&format!("projects/p/spaces/s/{SEED}/x.json"), &body)],
    );
    let found = seed_entities(&root).expect("an opaque id");
    assert_eq!(found[0].id, id);
    assert_eq!(found[0].body["id"], Value::String(id.to_owned()));
}

/// CC-72: an empty checkout directory for a space seeds nothing.
#[test]
fn a_space_without_a_seed_folder_and_a_project_without_spaces_seed_nothing() {
    let root = checkout(
        "seed-bare",
        &[
            ("projects/p/project.yaml", "kind: Project"),
            ("projects/q/spaces/s/space.yaml", "kind: ContextSpace"),
        ],
    );
    assert!(seed_entities(&root).expect("nothing").is_empty());
}

/// CC-72: a seed folder that is a link out of the checkout is not read: the repository is the
/// whole of what `apply` writes.
#[test]
fn a_symlinked_seed_directory_pointing_outside_the_repo_is_not_followed() {
    let outside = checkout("seed-outside-target", &[("x.json", &air("outside"))]);
    let root = checkout(
        "seed-outside-link",
        &[("projects/p/spaces/s/entities/.keep", "")],
    );
    std::os::unix::fs::symlink(&outside, root.join("projects/p/spaces/s").join(SEED))
        .expect("the link");
    let read = seed_entities(&root);
    assert!(
        !matches!(&read, Ok(found) if found.iter().any(|e| e.id.ends_with(":outside"))),
        "an entity from outside the checkout was seeded: {read:?}"
    );
}

/// CC-72: a seed file that is a link out of the checkout is refused by name, not read.
#[test]
fn a_symlinked_seed_file_pointing_outside_the_repo_is_refused() {
    let outside = checkout("seed-outside-file", &[("x.json", &air("outside"))]);
    let root = checkout(
        "seed-outside-file-link",
        &[(&format!("projects/p/spaces/s/{SEED}/own.json"), &air("own"))],
    );
    std::os::unix::fs::symlink(
        outside.join("x.json"),
        root.join("projects/p/spaces/s")
            .join(SEED)
            .join("linked.json"),
    )
    .expect("the link");
    match seed_entities(&root) {
        Err(SeedError::Outside { path }) => assert!(path.ends_with("linked.json"), "{path}"),
        other => panic!("a linked seed file was not refused: {other:?}"),
    }
}
