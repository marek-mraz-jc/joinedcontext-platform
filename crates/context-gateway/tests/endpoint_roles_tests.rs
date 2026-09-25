//! The roles an Endpoint gives the callers it admits, on requests through it alone (T-2591,
//! AP-96, AP-97, ADR-N-027).
//!
//! **The contract.** A `Policy` names a space, not an endpoint, so the App reconciler assigns an
//! application's grants to roles only its own Endpoint hands out: `endpoint:{project}/{name}`
//! to every caller it admits when it sets `callerRole`, and `endpoint:{project}/{name}/{role}`
//! to a caller matching a subject of that role. Two endpoints over one space carry the same
//! Policies here, as they do on a cluster; what each caller's access document lists says which
//! roles reached them.

mod common;

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use context_gateway::app::{router, Gateway};
use context_gateway::auth::accounts::ServiceAccounts;
use context_gateway::pdp::PolicyPdp;
use context_gateway::proxy::Broker;
use context_gateway::resolver::{Endpoint, EndpointRoles};
use jc_core::kinds::{endpoint_role, Audience, EndpointSpec, PolicySpec, Representation};
use serde_json::{json, Value};
use std::sync::Arc;
use tower::ServiceExt;

/// The endpoint the reconciler generates for the App `alerts`.
const APP_SLUG: &str = "k4y7pq2mztsvhx3nbwrs5cjdef";
/// Another endpoint over the same space, with no roles of its own.
const PLAIN_SLUG: &str = "q7w6e5r4t3y2uaiopazsxdcfgh";
const PROJECT: &str = "helsinki";

fn policy(role: &str, marker: &str) -> PolicySpec {
    serde_norway::from_str(&format!(
        "contextSpaceRef: helsinki\nassigner: did:web:hel.fi\n\
         assignee: {{ kind: role, id: \"{role}\" }}\noperations: [queryEntity]\n\
         information:\n  - entities:\n      - type: {marker}\n"
    ))
    .expect("the policy spec parses")
}

/// The Policies of the space: the app's base grant, its editors' grant, and a realm role's.
fn policies() -> Vec<PolicySpec> {
    vec![
        policy(&endpoint_role(PROJECT, "app-alerts", None), "AppRead"),
        policy(
            &endpoint_role(PROJECT, "app-alerts", Some("editor")),
            "EditorWrite",
        ),
        policy("data-steward", "StewardView"),
    ]
}

/// The App's Endpoint as the reconciler renders it: a caller role and one named role.
fn app_roles() -> EndpointRoles {
    let spec: EndpointSpec = serde_norway::from_str(&format!(
        "contextSpaceRef: helsinki\nslug: {APP_SLUG}\naudience: organization\n\
         enabledRepresentations: [ngsi-ld]\ncallerRole: true\nroles:\n\
         \x20 - name: editor\n\
         \x20   subjects: [{{ user: jana.kovacova@hel.fi }}, {{ group: alert-editors }}]\n"
    ))
    .expect("the endpoint spec parses");
    spec.validate().expect("the endpoint spec is valid");
    EndpointRoles::of(PROJECT, "app-alerts", &spec)
}

fn endpoint(slug: &str, audience: Audience, roles: EndpointRoles) -> Endpoint {
    Endpoint {
        declared_types: None,
        roles,
        slug: slug.to_owned(),
        title: Default::default(),
        description: Default::default(),
        space: "helsinki".to_owned(),
        project: PROJECT.to_owned(),
        audience,
        allowed_projects: Vec::new(),
        representations: vec![Representation::NgsiLd],
        rate_limit: None,
        file_limits: None,
        hidden_attributes: Default::default(),
        projection: None,
        base_path: format!("/api/endpoint/{slug}"),
        models: Vec::new(),
        view_mapping: None,
        catalog: None,
        policies: policies(),
    }
}

struct Fixture {
    realm: common::Realm,
    app: axum::Router,
}

fn fixture(audience: Audience) -> Fixture {
    let realm = common::Realm::new();
    let app = router(Arc::new(
        Gateway::new(
            Broker::new("http://127.0.0.1:1"),
            Box::new(PolicyPdp),
            "hel.fi",
        )
        .serve([
            endpoint(APP_SLUG, audience, app_roles()),
            endpoint(PLAIN_SLUG, audience, EndpointRoles::default()),
        ])
        .authenticate(Arc::new(realm.verifier()), ServiceAccounts::new(), None),
    ));
    Fixture { realm, app }
}

/// `GET …/access` on `slug`, with a token of `claims` or none: the status and what was granted.
async fn granted(
    fixture: &Fixture,
    slug: &str,
    claims: Option<Value>,
) -> (StatusCode, Vec<String>) {
    let mut request = Request::builder().uri(format!("/api/endpoint/{slug}/access"));
    if let Some(extra) = claims {
        let mut all = json!({
            "iss": common::ISSUER,
            "sub": "f:1:someone",
            "aud": "context-gateway",
            "exp": common::in_seconds(300),
            "iat": common::in_seconds(-10),
        });
        for (key, value) in extra.as_object().expect("an object") {
            all[key] = value.clone();
        }
        request = request.header(
            header::AUTHORIZATION,
            format!("Bearer {}", fixture.realm.mint(&all)),
        );
    }
    let response = fixture
        .app
        .clone()
        .oneshot(request.body(Body::empty()).expect("a request"))
        .await
        .expect("the gateway answers");
    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .expect("a readable body");
    let document: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    let mut markers: Vec<String> = document["permissions"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|entry| Some(entry["resource"]["type"].as_str()?.to_owned()))
        .collect();
    markers.sort();
    markers.dedup();
    (status, markers)
}

/// AP-96, AP-97: every person the app's endpoint admits holds its caller role there, and the
/// same person on another endpoint of the space holds nothing of the app's.
#[tokio::test]
async fn the_caller_role_is_held_on_the_apps_endpoint_and_not_on_another_of_the_space() {
    let fixture = fixture(Audience::Organization);
    let person = json!({ "preferred_username": "peter.novak@hel.fi" });

    let (status, on_app) = granted(&fixture, APP_SLUG, Some(person.clone())).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(on_app, ["AppRead"]);

    let (status, elsewhere) = granted(&fixture, PLAIN_SLUG, Some(person)).await;
    assert_eq!(status, StatusCode::OK);
    assert!(elsewhere.is_empty(), "{elsewhere:?}");
}

/// AP-97: a role's user matches the token's `preferred_username` whatever its case, its group
/// matches the `groups` claim with or without the leading slash, and nobody else holds it.
#[tokio::test]
async fn a_named_role_goes_to_its_user_and_its_group_and_to_nobody_else() {
    let fixture = fixture(Audience::Organization);
    for editor in [
        json!({ "preferred_username": "jana.kovacova@hel.fi" }),
        json!({ "preferred_username": "Jana.Kovacova@HEL.fi" }),
        json!({ "preferred_username": "someone@hel.fi", "groups": ["alert-editors"] }),
        json!({ "preferred_username": "someone@hel.fi", "groups": ["/alert-editors"] }),
    ] {
        let (_, on_app) = granted(&fixture, APP_SLUG, Some(editor.clone())).await;
        assert_eq!(on_app, ["AppRead", "EditorWrite"], "{editor}");
    }
    for viewer in [
        json!({ "preferred_username": "someone@hel.fi" }),
        json!({ "preferred_username": "jana.kovacova@hel.fi.evil.org" }),
        json!({ "preferred_username": "someone@hel.fi", "groups": ["alert-editors-2", "editor"] }),
    ] {
        let (_, on_app) = granted(&fixture, APP_SLUG, Some(viewer.clone())).await;
        assert_eq!(on_app, ["AppRead"], "{viewer}");
    }
}

/// AP-97: a role of the reserved prefix that a token asserts is dropped on every endpoint, so a
/// realm role named like an application's grant is worth nothing; the realm's other roles stay.
#[tokio::test]
async fn a_token_asserting_an_endpoint_role_gains_nothing_by_it() {
    let fixture = fixture(Audience::Organization);
    let forged = json!({
        "preferred_username": "someone@hel.fi",
        "realm_access": { "roles": [
            endpoint_role(PROJECT, "app-alerts", Some("editor")),
            endpoint_role(PROJECT, "app-alerts", None),
            "data-steward",
        ] },
    });

    let (_, elsewhere) = granted(&fixture, PLAIN_SLUG, Some(forged.clone())).await;
    assert_eq!(elsewhere, ["StewardView"]);

    let (_, on_app) = granted(&fixture, APP_SLUG, Some(forged)).await;
    assert_eq!(on_app, ["AppRead", "StewardView"]);
}

/// AP-97: an anonymous caller a public endpoint admits holds its caller role too, and an
/// endpoint that refuses the caller hands out no role at all.
#[tokio::test]
async fn the_caller_role_follows_admission_for_anonymous_and_refused_callers() {
    let public = fixture(Audience::Public);
    let (status, on_app) = granted(&public, APP_SLUG, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(on_app, ["AppRead"]);

    let organization = fixture(Audience::Organization);
    let (status, on_app) = granted(&organization, APP_SLUG, None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert!(on_app.is_empty(), "{on_app:?}");
}
