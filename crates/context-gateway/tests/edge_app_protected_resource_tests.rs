//! Edge cases of `app::protected_resource` (T-1878, EP-26, MP-02).
//!
//! Contract, in one sentence: `GET /api/endpoint/{slug}/.well-known/oauth-protected-resource`
//! is the RFC 9728 metadata an MCP client reads to find out where to authenticate — answered
//! for **every** slug, whether one resolves or not, because an answer that depended on the
//! endpoint table would be the enumeration oracle the 401 challenge was built to avoid
//! (AG-32, R20, EP-23).
//!
//! It is the one surface here that authenticates nobody and resolves nothing: the only inputs
//! are the slug in the path and the deployment's own configuration. What must not leak is the
//! existence of an endpoint; what must not happen is a caller's slug reaching the document as
//! anything but a JSON string — the `resource` and `resource_documentation` members are built
//! by interpolating it into a URL.
//!
//! The one case that does depend on configuration is the realm: a deployment with no verifier
//! serves public endpoints only and has no authorization server to name, which is 404.

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use context_gateway::app::{router, Gateway};
use context_gateway::auth::accounts::ServiceAccounts;
use context_gateway::pdp::PolicyPdp;
use context_gateway::proxy::Broker;
use context_gateway::resolver::Endpoint;
use jc_core::kinds::{Audience, PolicySpec, Representation};
use serde_json::{json, Value};
use std::sync::Arc;
use tower::ServiceExt;

const OPEN: &str = "k4y7pq2mzt6vhx3nbwrs5cjd8f";
const HOST: &str = "https://bb.example.sk";

fn policy(yaml: &str) -> PolicySpec {
    serde_norway::from_str(yaml).expect("the policy spec parses")
}

fn endpoint() -> Endpoint {
    Endpoint {
        declared_types: None,
        roles: Default::default(),
        slug: OPEN.to_owned(),
        title: Default::default(),
        description: Default::default(),
        space: "ovzdusie".to_owned(),
        project: "ovzdusie".to_owned(),
        audience: Audience::Organization,
        allowed_projects: Vec::new(),
        representations: vec![Representation::Mcp],
        rate_limit: None,
        file_limits: None,
        hidden_attributes: Default::default(),
        projection: None,
        base_path: format!("/api/endpoint/{OPEN}"),
        models: Vec::new(),
        view_mapping: None,
        catalog: None,
        policies: vec![policy(
            r#"contextSpaceRef: ovzdusie
assigner: did:web:banskabystrica.sk
assignee: { kind: role, id: data-steward }
operations: [queryEntity]
information:
  - entities:
      - type: AirQualityObserved
"#,
        )],
    }
}

/// The deployment as it runs: a realm and a public URL.
fn app() -> axum::Router {
    let realm = common::Realm::new();
    router(Arc::new(
        Gateway::new(
            Broker::new("http://127.0.0.1:1"),
            Box::new(PolicyPdp),
            "banskabystrica.sk",
        )
        .serve([endpoint()])
        .authenticate(
            Arc::new(realm.verifier()),
            ServiceAccounts::new(),
            Some(HOST.to_owned()),
        ),
    ))
}

/// A deployment that serves public endpoints only: no realm, therefore no authorization
/// server to point anybody at.
fn app_without_a_realm() -> axum::Router {
    router(Arc::new(
        Gateway::new(
            Broker::new("http://127.0.0.1:1"),
            Box::new(PolicyPdp),
            "banskabystrica.sk",
        )
        .serve([endpoint()]),
    ))
}

async fn call_on(app: axum::Router, path: &str) -> (StatusCode, String, String) {
    let response = app
        .oneshot(
            Request::builder()
                .uri(path)
                .body(Body::empty())
                .expect("a request"),
        )
        .await
        .expect("the gateway answers");
    let status = response.status();
    let media = response
        .headers()
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    let body = axum::body::to_bytes(response.into_body(), 256 * 1024)
        .await
        .expect("a readable body");
    (status, media, String::from_utf8_lossy(&body).into_owned())
}

fn metadata(slug: &str) -> String {
    format!("/api/endpoint/{slug}/.well-known/oauth-protected-resource")
}

async fn call(slug: &str) -> (StatusCode, String, String) {
    call_on(app(), &metadata(slug)).await
}

/// AG-32, R20: the first case, and the one the surface exists for. A slug that resolves and
/// one that never will answer the same document but for the name the caller typed, so the
/// metadata route says nothing about what this deployment serves.
#[tokio::test]
async fn a_slug_that_exists_and_one_that_does_not_answer_the_same_document() {
    let (real_status, real_media, real_body) = call(OPEN).await;
    let (invented_status, invented_media, invented_body) = call("zzzzzzzzzzzzzzzzzzzzzzzzzz").await;

    assert_eq!(real_status, StatusCode::OK);
    assert_eq!(invented_status, StatusCode::OK);
    assert_eq!(real_media, invented_media);
    assert_eq!(
        real_body.replace(OPEN, "SLUG"),
        invented_body.replace("zzzzzzzzzzzzzzzzzzzzzzzzzz", "SLUG"),
        "the two differ only in the name that was typed",
    );
}

/// RFC 9728 section 2: the document names the resource, its authorization server and how a
/// token is presented. A client that cannot read all three has nothing to do next.
#[tokio::test]
async fn the_document_carries_what_rfc_9728_needs() {
    let (status, media, body) = call(OPEN).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(media, "application/json");
    let document: Value = serde_json::from_str(&body).expect("a JSON document");
    assert_eq!(
        document["resource"],
        json!(format!("{HOST}/api/endpoint/{OPEN}/mcp")),
    );
    assert_eq!(document["authorization_servers"], json!([common::ISSUER]));
    assert_eq!(document["bearer_methods_supported"], json!(["header"]));
    assert_eq!(
        document["resource_documentation"],
        json!(format!("{HOST}/api/endpoint/{OPEN}/")),
    );
}

/// The slug is interpolated into two URLs, so every shape of it is looked at here. It reaches
/// the document as a JSON string and nothing else: no member is split, no second member
/// appears, and the document still parses whatever was typed.
#[tokio::test]
async fn a_hostile_slug_reaches_the_document_as_a_string_and_nothing_else() {
    let quote = "%22%2C%22authorization_servers%22%3A%5B%22https%3A%2F%2Fevil.example%22%5D%2C%22x%22%3A%22";
    let backslash = "a%5C%22b";
    let newline = "a%0D%0Ab";
    let unicode = "%C5%BEiar-nad-hronom";
    for slug in [quote, backslash, newline, unicode, "%00", "%2e%2e%2f"] {
        let (status, media, body) = call(slug).await;
        assert_eq!(status, StatusCode::OK, "{slug} answered {body}");
        assert_eq!(media, "application/json", "{slug}");

        let document: Value = serde_json::from_str(&body).expect("a JSON document");
        assert_eq!(
            document["authorization_servers"],
            json!([common::ISSUER]),
            "{slug} moved the authorization server: {body}",
        );
        let members = document.as_object().expect("an object");
        assert_eq!(
            members.keys().collect::<Vec<_>>(),
            vec![
                "authorization_servers",
                "bearer_methods_supported",
                "resource",
                "resource_documentation",
            ],
            "{slug} changed the shape of the document: {body}",
        );
        // Whatever was typed comes back inside the two URL members, escaped — so it is a
        // string and never a member of its own. The client checks that `resource` is the URL
        // it asked for (RFC 9728 section 3.3), and it asked for this one.
        assert!(
            !document["bearer_methods_supported"]
                .to_string()
                .contains("evil.example"),
            "{slug} reached a member it has no business in: {body}",
        );
    }
}

/// A slug is opaque and this surface resolves nothing, so its length is not checked either.
/// The case is here to show what that costs: a very long name is echoed back, and the answer
/// is bounded by the URL the client could send in the first place.
#[tokio::test]
async fn a_very_long_slug_is_echoed_and_nothing_more() {
    let long = "a".repeat(2000);
    let (status, _, body) = call(&long).await;

    assert_eq!(status, StatusCode::OK);
    let document: Value = serde_json::from_str(&body).expect("a JSON document");
    assert_eq!(
        document["resource"],
        json!(format!("{HOST}/api/endpoint/{long}/mcp")),
    );
}

/// PF-46: the metadata is unauthenticated by design, and presenting a token — any token —
/// changes nothing about it. A document that varied with the caller would be a way of testing
/// tokens against this deployment.
#[tokio::test]
async fn no_token_changes_the_document() {
    let realm = common::Realm::new();
    let elsewhere = realm.workload_token("other-workload", json!("another-slug"));
    let plain = call(OPEN).await;
    for header in [
        format!("Bearer {elsewhere}"),
        "Bearer not.a.token".to_owned(),
        "Basic YWRtaW46YWRtaW4=".to_owned(),
    ] {
        let response = app()
            .oneshot(
                Request::builder()
                    .uri(metadata(OPEN))
                    .header(axum::http::header::AUTHORIZATION, &header)
                    .body(Body::empty())
                    .expect("a request"),
            )
            .await
            .expect("the gateway answers");
        let status = response.status();
        let body = axum::body::to_bytes(response.into_body(), 256 * 1024)
            .await
            .expect("a readable body");
        assert_eq!(status, plain.0, "{header:?}");
        assert_eq!(
            String::from_utf8_lossy(&body),
            plain.2,
            "{header:?} changed the document",
        );
    }
}

/// A deployment with no realm has no authorization server to name, and says so with a 404
/// rather than a document whose `authorization_servers` is empty — which a client would read
/// as "this resource takes no token" and then fail to authenticate against for ever.
#[tokio::test]
async fn a_deployment_without_a_realm_answers_no_metadata() {
    let (status, _, body) = call_on(app_without_a_realm(), &metadata(OPEN)).await;

    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(!body.contains("authorization_servers"), "{body}");
}

/// The document is built from the deployment's own public URL, never from a header the client
/// wrote. A caller who could move `resource` to a host of their own would be handing the next
/// client a token endpoint they control.
#[tokio::test]
async fn a_client_supplied_host_does_not_become_the_resource() {
    let response = app()
        .oneshot(
            Request::builder()
                .uri(metadata(OPEN))
                .header("Host", "evil.example")
                .header("X-Forwarded-Host", "evil.example")
                .header("X-Forwarded-Proto", "http")
                .header("Forwarded", "host=evil.example;proto=http")
                .body(Body::empty())
                .expect("a request"),
        )
        .await
        .expect("the gateway answers");
    let body = axum::body::to_bytes(response.into_body(), 256 * 1024)
        .await
        .expect("a readable body");
    let body = String::from_utf8_lossy(&body);

    assert!(!body.contains("evil.example"), "{body}");
    assert!(body.contains(HOST), "{body}");
}

/// R20: the document says nothing about the endpoint behind the slug — not its space, not its
/// project, not its audience, not whether it exists.
#[tokio::test]
async fn the_document_says_nothing_about_the_endpoint() {
    let (_, _, body) = call(OPEN).await;

    for secret in [
        "ovzdusie",
        "organization",
        "data-steward",
        "127.0.0.1",
        "AirQualityObserved",
    ] {
        assert!(!body.contains(secret), "{secret:?} leaked into {body}");
    }
}

/// RFC 9110 section 9.1: the metadata is a `GET` surface. Nothing else reaches it, so it
/// cannot be used as a place to post a body at an unauthenticated route.
#[tokio::test]
async fn nothing_but_get_reaches_the_metadata() {
    for method in ["POST", "PUT", "PATCH", "DELETE"] {
        let response = app()
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri(metadata(OPEN))
                    .body(Body::from("{}"))
                    .expect("a request"),
            )
            .await
            .expect("the gateway answers");
        assert_eq!(
            response.status(),
            StatusCode::METHOD_NOT_ALLOWED,
            "{method} reached the metadata",
        );
    }
}

/// EP-23: the route is exact. A near-miss of the well-known path is the ordinary 404 of
/// anything off the surface, so nothing else of the tree is reachable through it.
#[tokio::test]
async fn only_the_exact_well_known_path_answers() {
    for path in [
        format!("/api/endpoint/{OPEN}/.well-known/oauth-protected-resource/"),
        format!("/api/endpoint/{OPEN}/.well-known/oauth-authorization-server"),
        format!("/api/endpoint/{OPEN}/.well-known/"),
        format!("/api/endpoint/{OPEN}/.well-known"),
        "/.well-known/oauth-protected-resource".to_owned(),
    ] {
        let (status, _, _) = call_on(app(), &path).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{path} answered");
    }
}

/// The same request twice at once is the same document twice: the surface holds no state, so
/// nothing of one caller's request can appear in another's.
#[tokio::test]
async fn the_same_request_twice_at_once_is_the_same_document() {
    let (first, second) = tokio::join!(call(OPEN), call("another-slug-entirely"));

    assert_eq!(first.0, StatusCode::OK);
    assert_eq!(second.0, StatusCode::OK);
    assert!(!first.2.contains("another-slug-entirely"), "{}", first.2);
    assert!(!second.2.contains(OPEN), "{}", second.2);
}
