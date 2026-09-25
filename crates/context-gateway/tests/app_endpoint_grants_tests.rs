//! An App's generated Endpoint evaluates the grants of its own roles and every prohibition of
//! the space, nothing else (AP-139, T-2938).
//!
//! A Policy names a space, not an endpoint, and an Endpoint without `policyRef` evaluates every
//! Policy of its space (EP-14). For a hand-written endpoint that is the point; for the Endpoint
//! the Portal generates for an App it made every sibling grant, the space's open-data `public`
//! grant and a steward's personal write grant included, reach the App's surface, its `/access`
//! document and its schema. Here one space carries both kinds of endpoint.

mod common;

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use context_gateway::app::{router, Gateway};
use context_gateway::auth::accounts::ServiceAccounts;
use context_gateway::pdp::PolicyPdp;
use context_gateway::proxy::Broker;
use context_gateway::resolver::Endpoint;
use context_gateway::store;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tower::ServiceExt;

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

fn gateway(realm: &common::Realm, endpoints: Vec<Endpoint>) -> axum::Router {
    router(Arc::new(
        Gateway::new(
            Broker::new("http://127.0.0.1:1"),
            Box::new(PolicyPdp),
            "hel.fi",
        )
        .serve(endpoints)
        .authenticate(Arc::new(realm.verifier()), ServiceAccounts::new(), None),
    ))
}

/// `GET …/access` on `slug` as `user`, or anonymously: the permitted and the prohibited types.
async fn access(
    realm: &common::Realm,
    app: &axum::Router,
    slug: &str,
    user: Option<&str>,
) -> (Vec<String>, Vec<String>) {
    let mut request = Request::builder().uri(format!("/api/endpoint/{slug}/access"));
    if let Some(user) = user {
        let token = realm.mint(&json!({
            "iss": common::ISSUER,
            "sub": "f:1:steward",
            "aud": "context-gateway",
            "preferred_username": user,
            "exp": common::in_seconds(300),
            "iat": common::in_seconds(-10),
        }));
        request = request.header(header::AUTHORIZATION, format!("Bearer {token}"));
    }
    let response = app
        .clone()
        .oneshot(request.body(Body::empty()).expect("a request"))
        .await
        .expect("the gateway answers");
    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .expect("a readable body");
    let document: Value = serde_json::from_slice(&body).expect("a JSON document");
    let types = |key: &str| {
        let mut types: Vec<String> = document[key]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|entry| Some(entry["resource"]["type"].as_str()?.to_owned()))
            .collect();
        types.sort();
        types.dedup();
        types
    };
    (types("permissions"), types("prohibitions"))
}

/// EP-55 on both surfaces: the App's `/access` document lists what the App declares, for the
/// anonymous reader and for the steward alike, and the space's endpoint keeps its own grants.
#[tokio::test]
async fn the_apps_access_document_lists_only_its_own_grants() {
    let realm = common::Realm::new();
    let app = gateway(&realm, loaded("access"));

    let (permitted, prohibited) = access(&realm, &app, APP_SLUG, None).await;
    assert_eq!(permitted, ["Event"], "the public grant stays off the App");
    assert!(prohibited.is_empty(), "{prohibited:?}");

    let (permitted, prohibited) = access(&realm, &app, APP_SLUG, Some(STEWARD)).await;
    assert_eq!(
        permitted,
        ["Event"],
        "the steward's own grant stays off the App"
    );
    assert_eq!(prohibited, ["Event"], "a prohibition still reaches the App");

    let (permitted, _) = access(&realm, &app, ALL_SLUG, Some(STEWARD)).await;
    assert_eq!(permitted, ["Alert", "BikeHireDockingStation"]);
}
