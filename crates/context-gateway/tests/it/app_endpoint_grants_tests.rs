//! An App's generated Endpoint evaluates the grants of its own roles and every prohibition of
//! the space, nothing else (AP-139, T-2938).
//!
//! A Policy names a space, not an endpoint, and an Endpoint without `policyRef` evaluates every
//! Policy of its space (EP-14). For a hand-written endpoint that is the point; for the Endpoint
//! the Portal generates for an App it made every sibling grant, the space's open-data `public`
//! grant and a steward's personal write grant included, reach the App's surface, its `/access`
//! document and its schema. Here one space carries both kinds of endpoint. These tests hold the
//! gateway's table, which still decides for an Endpoint no view serves; through a view the
//! Portal's rule document binds the same Policies (`views`, T-3702), and the engine decides.

use context_gateway::resolver::Endpoint;
use context_gateway::store;
use std::path::{Path, PathBuf};

const APP_SLUG: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaa";
const ALL_SLUG: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbb";
/// Named like an App's endpoint, but no App of that name exists: a hand-written endpoint.
const LOOKALIKE_SLUG: &str = "cccccccccccccccccccccccccc";
const STEWARD: &str = "demo.steward@hel.fi";

const SPACE: &str = r#"apiVersion: joinedcontext.com/v1alpha1
kind: ContextSpace
metadata:
  name: helsinki
  namespace: helsinki
spec:
  isSandbox: false
  urnSegment: helsinki
"#;

const APP: &str = r#"apiVersion: joinedcontext.com/v1alpha1
kind: App
metadata:
  name: events
  namespace: helsinki
spec:
  kind: static
  source: { path: ./src }
  build: {}
  visibility: public
  dataNeeds:
    - contextSpaceRef: { kind: ContextSpace, name: helsinki }
      types: [Event]
      operations: [queryEntity]
"#;

fn endpoint(name: &str, slug: &str, caller_role: bool) -> String {
    format!(
        r#"apiVersion: joinedcontext.com/v1alpha1
kind: Endpoint
metadata:
  name: {name}
  namespace: helsinki
spec:
  contextSpaceRef: helsinki
  slug: {slug}
  audience: public
  callerRole: {caller_role}
  enabledRepresentations: ["ngsi-ld"]
"#
    )
}

fn policy(name: &str, assignee: &str, entity_type: &str, effect: &str) -> String {
    format!(
        r#"apiVersion: joinedcontext.com/v1alpha1
kind: Policy
metadata:
  name: {name}
  namespace: helsinki
spec:
  contextSpaceRef: {{ kind: ContextSpace, name: helsinki }}
  assigner: did:web:hel.fi
  assignee: {assignee}
  effect: {effect}
  operations: [queryEntity]
  information:
    - entities:
        - type: {entity_type}
"#
    )
}

fn repo(test_name: &str) -> PathBuf {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock after the epoch")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("gw-app-grants-{test_name}-{now}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create the repository");
    let files = [
        ("space.yaml", SPACE.to_owned()),
        ("app.yaml", APP.to_owned()),
        ("app-events.yaml", endpoint("app-events", APP_SLUG, true)),
        (
            "helsinki-all.yaml",
            endpoint("helsinki-all", ALL_SLUG, false),
        ),
        (
            "app-lookalike.yaml",
            endpoint("app-lookalike", LOOKALIKE_SLUG, false),
        ),
        // What the App reconciler renders: the App's base grant and one role's grant.
        (
            "p-app.yaml",
            policy(
                "app-events-1",
                "{ kind: role, id: \"endpoint:helsinki/app-events\" }",
                "Event",
                "permission",
            ),
        ),
        (
            "p-app-editor.yaml",
            policy(
                "app-events-2-editor",
                "{ kind: role, id: \"endpoint:helsinki/app-events/editor\" }",
                "EventDraft",
                "permission",
            ),
        ),
        // The space's own grants, written for other endpoints.
        (
            "p-public.yaml",
            policy(
                "public-all",
                "{ kind: role, id: public }",
                "Alert",
                "permission",
            ),
        ),
        (
            "p-steward.yaml",
            policy(
                "bikes-ops-steward",
                &format!("{{ kind: user, id: \"{STEWARD}\" }}"),
                "BikeHireDockingStation",
                "permission",
            ),
        ),
        // Another App's grant of the same space.
        (
            "p-other-app.yaml",
            policy(
                "app-bikes-1",
                "{ kind: role, id: \"endpoint:helsinki/app-bikes\" }",
                "Vehicle",
                "permission",
            ),
        ),
        // A prohibition narrows every surface, the App's included.
        (
            "p-no-events.yaml",
            policy(
                "no-events-for-the-steward",
                &format!("{{ kind: user, id: \"{STEWARD}\" }}"),
                "Event",
                "prohibition",
            ),
        ),
    ];
    for (name, body) in files {
        write(&dir, name, &body);
    }
    dir
}

fn write(dir: &Path, name: &str, body: &str) {
    std::fs::write(dir.join(name), body).expect("write a manifest");
}

fn loaded(test_name: &str) -> Vec<Endpoint> {
    let (endpoints, ..) = store::load(&repo(test_name)).expect("the repository loads");
    endpoints
}

fn by_slug<'a>(endpoints: &'a [Endpoint], slug: &str) -> &'a Endpoint {
    endpoints
        .iter()
        .find(|endpoint| endpoint.slug == slug)
        .expect("the endpoint is in the table")
}

/// The types each of an endpoint's Policies names, with `!` in front of a prohibition's.
fn marked(endpoint: &Endpoint) -> Vec<String> {
    let mut types: Vec<String> = endpoint
        .policies
        .iter()
        .flat_map(|policy| {
            let bang = if policy.effect.is_prohibition() {
                "!"
            } else {
                ""
            };
            policy
                .information
                .iter()
                .flat_map(|info| info.entities.iter())
                .map(move |entity| format!("{bang}{}", entity.entity_type))
        })
        .collect();
    types.sort();
    types
}

#[test]
fn the_apps_endpoint_keeps_its_own_grants_and_every_prohibition_and_nothing_else() {
    let endpoints = loaded("app");
    assert_eq!(
        marked(by_slug(&endpoints, APP_SLUG)),
        ["!Event", "Event", "EventDraft"]
    );
}

#[test]
fn a_hand_written_endpoint_still_evaluates_every_policy_of_its_space() {
    let endpoints = loaded("hand");
    let everything = [
        "!Event",
        "Alert",
        "BikeHireDockingStation",
        "Event",
        "EventDraft",
        "Vehicle",
    ];
    assert_eq!(marked(by_slug(&endpoints, ALL_SLUG)), everything);
    // Its name is not what makes an endpoint an App's: no App `lookalike` exists.
    assert_eq!(marked(by_slug(&endpoints, LOOKALIKE_SLUG)), everything);
}
