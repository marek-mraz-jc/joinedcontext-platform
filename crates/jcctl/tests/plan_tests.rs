mod common;

use common::*;
use jcctl::commands::plan::{compute, Action};
use jcctl::platform::InMemory;
use serde_json::json;

#[test]
fn a_fresh_platform_makes_every_manifest_a_create_in_wave_order() {
    let dir = demo_repo("plan-fresh");
    let changes = compute(&load(&dir), &InMemory::new()).expect("plan computes");

    assert_eq!(changes.count(Action::Create), 4);
    assert_eq!(changes.count(Action::Update), 0);
    assert_eq!(changes.count(Action::Delete), 0);
    assert!(!changes.is_clean());

    let waves: Vec<u8> = changes.waves.iter().map(|(wave, _)| *wave).collect();
    assert_eq!(waves, vec![0, 1, 3]);
    assert_eq!(
        changes
            .iter()
            .map(|c| c.id.kind.as_str())
            .collect::<Vec<_>>(),
        vec!["Organization", "Project", "ContextSpace", "Endpoint"]
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// The property the whole reconciler rests on: a platform that already matches the
/// repository has nothing pending (CC-17, CC-18).
#[test]
fn a_platform_that_matches_the_repository_is_clean() {
    let dir = demo_repo("plan-clean");
    let platform = InMemory::new()
        .with(manifest(ORG))
        .with(manifest(PROJECT))
        .with(manifest(SPACE))
        .with(manifest(ENDPOINT));

    let changes = compute(&load(&dir), &platform).expect("plan computes");

    assert!(changes.is_clean());
    assert_eq!(changes.count(Action::Unchanged), 4);
    assert_eq!(changes.to_json()["changes"], json!([]));

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_changed_member_is_an_update_carrying_its_field_diff() {
    let dir = demo_repo("plan-update");
    let live = ENDPOINT.replace(
        r#"enabledRepresentations: ["ngsi-ld", "geojson"]"#,
        r#"enabledRepresentations: ["ngsi-ld"]"#,
    );
    let platform = InMemory::new()
        .with(manifest(ORG))
        .with(manifest(PROJECT))
        .with(manifest(SPACE))
        .with(manifest(&live));

    let changes = compute(&load(&dir), &platform).expect("plan computes");

    assert_eq!(changes.count(Action::Update), 1);
    let update = changes
        .iter()
        .find(|c| c.action == Action::Update)
        .expect("the endpoint changed");
    assert_eq!(update.id.name, "public-air");
    assert_eq!(update.diff.len(), 1);
    assert_eq!(update.diff[0].path, "spec.enabledRepresentations");

    let _ = std::fs::remove_dir_all(&dir);
}

/// A resource the platform has and the repository no longer declares is reported, never
/// removed by `plan` itself (CC-19).
#[test]
fn a_resource_missing_from_the_repository_is_reported_for_deletion() {
    let dir = demo_repo("plan-delete");
    std::fs::remove_file(dir.join(ENDPOINT_PATH)).expect("drop the endpoint from Git");

    let platform = InMemory::new()
        .with(manifest(ORG))
        .with(manifest(PROJECT))
        .with(manifest(SPACE))
        .with(manifest(ENDPOINT));

    let changes = compute(&load(&dir), &platform).expect("plan computes");

    assert_eq!(changes.count(Action::Delete), 1);
    assert_eq!(platform.len(), 4, "plan writes nothing");

    let deletion = changes
        .iter()
        .find(|c| c.action == Action::Delete)
        .expect("the endpoint is gone from Git");
    assert_eq!(deletion.id.name, "public-air");
    assert!(deletion.declared.is_none());

    let _ = std::fs::remove_dir_all(&dir);
}

/// The `--json` contract of API/03 section 2.
#[test]
fn the_json_contract_carries_the_summary_and_the_nested_diff() {
    let dir = demo_repo("plan-json");
    let live = ENDPOINT.replace("audience: public", "audience: organization");
    let platform = InMemory::new()
        .with(manifest(ORG))
        .with(manifest(PROJECT))
        .with(manifest(SPACE))
        .with(manifest(&live));

    let json = compute(&load(&dir), &platform)
        .expect("plan computes")
        .to_json();

    assert_eq!(
        json["summary"],
        json!({"to_add": 0, "to_change": 1, "to_delete": 0})
    );
    assert_eq!(
        json["changes"],
        json!([{
            "kind": "Endpoint",
            "id": "public-air",
            "action": "UPDATE",
            "diff": { "spec": { "audience": "public" } },
        }])
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_rendered_diff_names_the_wave_the_resource_converges_in() {
    let dir = demo_repo("plan-render");
    let rendered = compute(&load(&dir), &InMemory::new())
        .expect("plan computes")
        .render();

    assert!(rendered.contains("wave 0"), "{rendered}");
    assert!(
        rendered.contains("create    Endpoint/ovzdusie/public-air"),
        "{rendered}"
    );
    assert!(
        rendered.contains("4 to add, 0 to change, 0 to delete"),
        "{rendered}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}
