//! Attribute ownership, end to end (T-0131, CC-69, MF-15).
//!
//! `diff_tests.rs` proves the rule on one pair of manifests. This proves the consequence the
//! requirement is actually about: a seed entity whose live values move every few seconds
//! because a sensor is writing to it must not appear in a plan or in a drift report. An
//! ownership rule that holds in the differ and leaks anywhere above it is worth nothing —
//! an operator who sees telemetry in a drift report stops reading drift reports.

mod common;

use common::*;
use jcctl::commands::drift::detect;
use jcctl::commands::plan::{compute, Action};
use jcctl::platform::InMemory;
use jcctl::RawManifest;
use serde_json::{json, Value};

/// The endpoint as Git declares it, and as the platform holds it after telemetry wrote to it.
fn with_live_members(members: &[(&str, Value)]) -> RawManifest {
    let mut live = manifest(ENDPOINT);
    let spec = live.spec.as_object_mut().expect("a spec is an object");
    for (name, value) in members {
        spec.insert((*name).to_owned(), value.clone());
    }
    live
}

fn platform(live: RawManifest) -> InMemory {
    InMemory::new()
        .with(manifest(ORG))
        .with(manifest(PROJECT))
        .with(manifest(SPACE))
        .with(live)
}

#[test]
fn live_telemetry_on_members_the_manifest_never_declared_is_not_drift() {
    // CC-69's default: the manifest owns exactly what it declares. Everything a sensor
    // appended belongs to whoever wrote it.
    let dir = demo_repo("ownership-telemetry");
    let live = with_live_members(&[
        ("temperature", json!(21.4)),
        ("observedAt", json!("2026-01-01T00:00:00Z")),
        ("readings", json!([{"value": 1}, {"value": 2}])),
    ]);

    let report = detect(&load(&dir), &platform(live)).expect("drift runs");
    assert!(report.is_clean(), "{:?}", report.drifted);

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_same_telemetry_makes_no_pending_change_either() {
    // The plan is what CI publishes on every merge request (CC-20). Telemetry in it would
    // make every merge request's diff unreadable, and `apply` would write it back.
    let dir = demo_repo("ownership-plan");
    let live = with_live_members(&[("temperature", json!(21.4))]);

    let changes = compute(&load(&dir), &platform(live)).expect("plan computes");

    assert!(changes.is_clean());
    assert_eq!(changes.count(Action::Unchanged), 4);

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_declared_member_still_drifts_when_the_platform_disagrees() {
    // The rule narrows what is compared; it does not stop the comparison. A manifest that
    // owns a member and does not match it is drift, telemetry beside it or not.
    let dir = demo_repo("ownership-declared");
    let live = with_live_members(&[("audience", json!("shared")), ("temperature", json!(21.4))]);

    let report = detect(&load(&dir), &platform(live)).expect("drift runs");

    assert_eq!(report.drifted.len(), 1);
    let paths: Vec<&str> = report.drifted[0]
        .diff
        .iter()
        .map(|field| field.path.as_str())
        .collect();
    assert_eq!(paths, vec!["spec.audience"], "only the owned member");

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_claimed_attribute_is_watched_even_where_the_manifest_stopped_declaring_it() {
    // MF-15: `joinedcontext.com/managed-attributes` is field ownership made explicit. A
    // manifest that claims an attribute owns it, so a live value under it is drift even
    // after the member itself is dropped from the manifest — which is what makes the
    // annotation a claim rather than a comment.
    let dir = temp_dir("ownership-claimed");
    write(&dir, "org.yaml", ORG);
    write(&dir, "projects/ovzdusie/project.yaml", PROJECT);
    write(&dir, "projects/ovzdusie/spaces/ovzdusie/space.yaml", SPACE);
    write(
        &dir,
        ENDPOINT_PATH,
        &ENDPOINT.replace(
            "  namespace: ovzdusie\n",
            "  namespace: ovzdusie\n  annotations:\n    joinedcontext.com/managed-attributes: \"audience,rateLimit\"\n",
        ),
    );
    let live = with_live_members(&[("rateLimit", json!(1000)), ("temperature", json!(21.4))]);

    let report = detect(&load(&dir), &platform(live)).expect("drift runs");

    let paths: Vec<&str> = report.drifted[0]
        .diff
        .iter()
        .map(|field| field.path.as_str())
        .collect();
    assert_eq!(paths, vec!["spec.rateLimit"], "claimed, and only claimed");

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn an_annotation_narrows_ownership_below_what_the_manifest_declares() {
    // The other direction: the annotation is the whole claim, so a member the manifest
    // declares and the annotation leaves out is handed over to whoever writes it live.
    let dir = temp_dir("ownership-narrowed");
    write(&dir, "org.yaml", ORG);
    write(&dir, "projects/ovzdusie/project.yaml", PROJECT);
    write(&dir, "projects/ovzdusie/spaces/ovzdusie/space.yaml", SPACE);
    write(
        &dir,
        ENDPOINT_PATH,
        &ENDPOINT.replace(
            "  namespace: ovzdusie\n",
            "  namespace: ovzdusie\n  annotations:\n    joinedcontext.com/managed-attributes: \"contextSpaceRef,slug\"\n",
        ),
    );
    let live = with_live_members(&[("audience", json!("shared"))]);

    let report = detect(&load(&dir), &platform(live)).expect("drift runs");

    assert!(
        report.is_clean(),
        "audience is declared but not claimed: {:?}",
        report.drifted
    );

    let _ = std::fs::remove_dir_all(&dir);
}
