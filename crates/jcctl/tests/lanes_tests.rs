//! T-0143: the lane a change has to go through, and the Change envelope it is proposed in
//! (CC-63, CC-64, CC-70, AG-10).

mod common;

use common::*;
use jcctl::commands::plan::{compute, Action, ChangeSet, ResourceChange};
use jcctl::lanes::{change_envelope, lane_of, lane_of_changeset, Lane};
use jcctl::loader::{RawManifest, ResourceId};
use jcctl::platform::InMemory;
use serde_json::json;

const SANDBOX_SPACE: &str = r#"apiVersion: joinedcontext.com/v1alpha1
kind: ContextSpace
metadata:
  name: pokus
  namespace: ovzdusie
spec:
  isSandbox: true
  ttlDays: 7
"#;

const PRIVATE_ENDPOINT: &str = r#"apiVersion: joinedcontext.com/v1alpha1
kind: Endpoint
metadata:
  name: internal-air
  namespace: ovzdusie
spec:
  contextSpaceRef: ovzdusie
  slug: mluyob4nz52lok3ssk7pgn5vwt
  audience: internal
  enabledRepresentations: ["ngsi-ld"]
"#;

const PIPELINE: &str = r#"apiVersion: joinedcontext.com/v1alpha1
kind: Pipeline
metadata:
  name: senzory
  namespace: ovzdusie
spec:
  contextSpaceRef: ovzdusie
  class: resident
"#;

const POLICY: &str = r#"apiVersion: joinedcontext.com/v1alpha1
kind: Policy
metadata:
  name: public-read
  namespace: ovzdusie
spec:
  contextSpaceRef: { kind: ContextSpace, name: ovzdusie }
  assignee: { kind: role, id: public }
  operations: [retrieveOps]
"#;

const ROLE: &str = r#"apiVersion: joinedcontext.com/v1alpha1
kind: Role
metadata:
  name: pipeline-developer
  namespace: org
spec:
  rules:
    - kinds: [Pipeline]
      verbs: [propose]
"#;

const ROLE_BINDING: &str = r#"apiVersion: joinedcontext.com/v1alpha1
kind: RoleBinding
metadata:
  name: ovzdusie-developers
  namespace: org
spec:
  subjects: [{ group: air-quality-team }]
  role: pipeline-developer
  scope: { project: ovzdusie }
"#;

fn change(action: Action, body: &str) -> ResourceChange {
    let declared: RawManifest = serde_norway::from_str(body).expect("fixture parses");
    ResourceChange {
        id: ResourceId::new(
            "joinedcontext.com",
            declared.kind.clone(),
            declared.metadata.namespace.clone(),
            declared.metadata.name.clone(),
        ),
        action,
        diff: Vec::new(),
        declared: match action {
            Action::Delete => None,
            _ => Some(declared.clone()),
        },
        // The lane is decided from what the repository declares; the live copy only
        // matters to `drift`, so a delete is the one case that has to carry it.
        live: match action {
            Action::Create => None,
            _ => Some(declared),
        },
    }
}

fn set(changes: Vec<ResourceChange>) -> ChangeSet {
    ChangeSet {
        waves: vec![(0, changes)],
        ..ChangeSet::default()
    }
}

#[test]
fn a_deletion_is_red_whatever_it_deletes() {
    let verdict = lane_of(&change(Action::Delete, PRIVATE_ENDPOINT));
    assert_eq!(verdict.lane, Lane::Red, "{}", verdict.reason);

    // Even the greenest kind there is: a sandbox space is self-service to create and a
    // governance decision to remove (CC-63, CC-19).
    let verdict = lane_of(&change(Action::Delete, SANDBOX_SPACE));
    assert_eq!(verdict.lane, Lane::Red, "{}", verdict.reason);
}

#[test]
fn publishing_an_endpoint_to_the_public_is_red() {
    let verdict = lane_of(&change(Action::Create, ENDPOINT));
    assert_eq!(verdict.lane, Lane::Red, "{}", verdict.reason);
    assert!(verdict.reason.contains("public"));
}

#[test]
fn moving_the_audience_of_a_published_endpoint_is_red_in_both_directions() {
    let mut update = change(Action::Update, PRIVATE_ENDPOINT);
    update.diff = vec![jcctl::FieldDiff {
        path: "spec.audience".to_owned(),
        declared: Some(json!("internal")),
        live: Some(json!("public")),
    }];

    let verdict = lane_of(&update);
    assert_eq!(
        verdict.lane,
        Lane::Red,
        "taking an endpoint off the public internet is a governance decision too: {}",
        verdict.reason
    );
}

#[test]
fn an_ephemeral_sandbox_space_is_green() {
    let verdict = lane_of(&change(Action::Create, SANDBOX_SPACE));
    assert_eq!(verdict.lane, Lane::Green, "{}", verdict.reason);
}

#[test]
fn a_managed_space_and_a_new_pipeline_are_yellow() {
    assert_eq!(lane_of(&change(Action::Create, SPACE)).lane, Lane::Yellow);
    assert_eq!(
        lane_of(&change(Action::Create, PIPELINE)).lane,
        Lane::Yellow
    );
    assert_eq!(
        lane_of(&change(Action::Create, PRIVATE_ENDPOINT)).lane,
        Lane::Yellow,
        "an endpoint inside an existing space, not published, is domain review"
    );
}

#[test]
fn identity_access_and_lane_policy_are_red_even_when_they_only_add() {
    assert_eq!(lane_of(&change(Action::Create, POLICY)).lane, Lane::Red);
    assert_eq!(lane_of(&change(Action::Update, ORG)).lane, Lane::Red);
    // PF-52: who may change configuration is itself red, even when a binding only adds a subject.
    assert_eq!(lane_of(&change(Action::Create, ROLE)).lane, Lane::Red);
    assert_eq!(
        lane_of(&change(Action::Update, ROLE_BINDING)).lane,
        Lane::Red
    );
}

#[test]
fn a_proposal_takes_the_strictest_lane_of_its_parts() {
    let proposal = set(vec![
        change(Action::Create, SANDBOX_SPACE),
        change(Action::Create, PIPELINE),
        change(Action::Delete, PRIVATE_ENDPOINT),
    ]);

    let verdict = lane_of_changeset(&proposal);
    assert_eq!(
        verdict.lane,
        Lane::Red,
        "one deletion among additions still needs the full chain: {}",
        verdict.reason
    );
}

#[test]
fn a_proposal_that_crosses_two_projects_is_red() {
    let mut elsewhere = change(Action::Create, PIPELINE);
    elsewhere.id.namespace = Some("doprava".to_owned());

    let verdict = lane_of_changeset(&set(vec![change(Action::Create, PIPELINE), elsewhere]));
    assert_eq!(verdict.lane, Lane::Red, "{}", verdict.reason);
    assert!(verdict.reason.contains("crosses domains"));
}

#[test]
fn an_unchanged_resource_never_raises_the_lane_of_a_green_proposal() {
    let unchanged = change(Action::Unchanged, POLICY);
    let verdict = lane_of_changeset(&set(vec![change(Action::Create, SANDBOX_SPACE), unchanged]));

    assert_eq!(
        verdict.lane,
        Lane::Green,
        "a policy the proposal does not touch is not a policy change: {}",
        verdict.reason
    );
}

#[test]
fn the_envelope_carries_the_lane_the_counts_and_the_phase() {
    let proposal = set(vec![
        change(Action::Create, SANDBOX_SPACE),
        change(Action::Update, PIPELINE),
        change(Action::Unchanged, PRIVATE_ENDPOINT),
    ]);

    let envelope = change_envelope(&proposal, "chg-7f3a", "ovzdusie", None);

    assert_eq!(envelope["kind"], "Change");
    assert_eq!(envelope["apiVersion"], jc_core::API_VERSION);
    assert_eq!(envelope["metadata"]["name"], "chg-7f3a");
    assert_eq!(envelope["metadata"]["namespace"], "ovzdusie");
    assert_eq!(envelope["status"]["lane"], "yellow");
    assert_eq!(envelope["status"]["phase"], "PendingApproval");
    assert_eq!(
        envelope["status"]["plan"],
        json!({ "create": 1, "update": 1, "delete": 0 }),
        "an unchanged resource is in no count"
    );
    assert!(envelope["status"].get("mergeRequest").is_none());
}

#[test]
fn a_green_proposal_is_auto_approved_and_already_deploying() {
    let envelope = change_envelope(
        &set(vec![change(Action::Create, SANDBOX_SPACE)]),
        "chg-0001",
        "ovzdusie",
        Some("https://git.example.sk/bb/org/pulls/412"),
    );

    assert_eq!(envelope["status"]["lane"], "green");
    assert_eq!(envelope["status"]["phase"], "Deploying");
    assert_eq!(
        envelope["status"]["mergeRequest"], "https://git.example.sk/bb/org/pulls/412",
        "green is auto-approved, never unrecorded (CC-64)"
    );
}

/// The lane comes from what the change does, never from what the manifest says about
/// itself: a red change carrying a green label stays red (CC-63, AG-11).
#[test]
fn a_manifest_cannot_talk_itself_into_a_lower_lane() {
    let labelled = r#"apiVersion: joinedcontext.com/v1alpha1
kind: Endpoint
metadata:
  name: public-air
  namespace: ovzdusie
  riskClass: green
  labels: { lane: green }
  annotations: { "joinedcontext.com/risk-class": "green" }
spec:
  contextSpaceRef: ovzdusie
  slug: zt4qm7ge2xdv6ksb3ncf5arw2y
  audience: public
"#;

    assert_eq!(lane_of(&change(Action::Create, labelled)).lane, Lane::Red);
}

/// The classifier runs on what `plan` produces, not on a hand-built structure only the
/// tests know how to make.
#[test]
fn the_demo_repository_on_a_fresh_platform_is_red_because_it_publishes() {
    let dir = demo_repo("lanes-demo");
    let changes = compute(&load(&dir), &InMemory::new()).expect("plan computes");

    let verdict = lane_of_changeset(&changes);
    assert_eq!(verdict.lane, Lane::Red, "{}", verdict.reason);
    assert_eq!(
        change_envelope(&changes, "chg-demo", "ovzdusie", None)["status"]["plan"],
        json!({ "create": 4, "update": 0, "delete": 0 })
    );

    let _ = std::fs::remove_dir_all(&dir);
}
