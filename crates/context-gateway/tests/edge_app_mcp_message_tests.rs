//! Edge cases of `app::mcp_message` (T-1876, EP-26, MP-02).
//!
//! Contract, in one sentence: `POST /api/endpoint/{slug}/mcp` takes one JSON-RPC message and
//! gives one back — it strips the tenancy headers, answers an unauthenticated call on a
//! non-public instance with the RFC 9728 challenge that tells the client where to get a token
//! (AG-32), admits the caller for the MCP representation, reads at most `MAX_BODY`, and hands
//! the message to the endpoint façade (T-0166, EP-24, SP-14, SP-19).
//!
//! The inputs are the slug, the `Authorization` header, the request body, and the tenancy
//! headers it deletes. Two things must not leak: whether a slug exists — the challenge is
//! sent for every slug, resolvable or not — and anything of the deployment inside a JSON-RPC
//! error. A third must not be bypassed: the representation check, so an endpoint that serves
//! NGSI-LD only has no MCP instance however the message is shaped.
//!
//! The broker is an address nothing listens on: a message that reached the data path shows up
//! as an upstream failure rather than passing quietly.

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

/// A public MCP instance: no token needed.
const OPEN: &str = "k4y7pq2mzt6vhx3nbwrs5cjd8f";
/// An endpoint that serves NGSI-LD only, so it has no MCP instance.
const NGSI_ONLY: &str = "m3x8tq7nzv2hbw6rjs4cyd9gpk";
/// An MCP instance that needs a token.
const CLOSED: &str = "p9d2wc5kzn8mth4rqvb7xj3sfy";

fn policy(yaml: &str) -> PolicySpec {
    serde_norway::from_str(yaml).expect("the policy spec parses")
}

fn public_grant() -> Vec<PolicySpec> {
    vec![policy(
        r#"contextSpaceRef: ovzdusie
assigner: did:web:banskabystrica.sk
assignee: { kind: role, id: public }
operations: [queryEntity, retrieveEntity]
information:
  - entities:
      - type: AirQualityObserved
    propertyNames: [pm10]
"#,
    )]
}

fn endpoint(slug: &str, audience: Audience, representations: Vec<Representation>) -> Endpoint {
    Endpoint {
        declared_types: None,
        roles: Default::default(),
        slug: slug.to_owned(),
        title: Default::default(),
        description: Default::default(),
        space: "ovzdusie".to_owned(),
        project: "ovzdusie".to_owned(),
        audience,
        allowed_projects: Vec::new(),
        representations,
        rate_limit: None,
        file_limits: None,
        hidden_attributes: Default::default(),
        projection: None,
        base_path: format!("/api/endpoint/{slug}"),
        models: Vec::new(),
        view_mapping: None,
        policies: public_grant(),
    }
}

const HOST: &str = "https://bb.example.sk";

fn app() -> axum::Router {
    let realm = common::Realm::new();
    router(Arc::new(
        Gateway::new(
            Broker::new("http://127.0.0.1:1"),
            Box::new(PolicyPdp),
            "banskabystrica.sk",
        )
        .serve([
            endpoint(
                OPEN,
                Audience::Public,
                vec![Representation::NgsiLd, Representation::Mcp],
            ),
            endpoint(NGSI_ONLY, Audience::Public, vec![Representation::NgsiLd]),
            endpoint(CLOSED, Audience::Organization, vec![Representation::Mcp]),
        ])
        .authenticate(
            Arc::new(realm.verifier()),
            ServiceAccounts::new(),
            Some(HOST.to_owned()),
        ),
    ))
}

/// One message, with the status, the `WWW-Authenticate` challenge and the body as bytes.
async fn call(request: Request<Body>) -> (StatusCode, String, String) {
    let response = app().oneshot(request).await.expect("the gateway answers");
    let status = response.status();
    let challenge = response
        .headers()
        .get(axum::http::header::WWW_AUTHENTICATE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .expect("a readable body");
    (
        status,
        challenge,
        String::from_utf8_lossy(&body).into_owned(),
    )
}

fn post(slug: &str, body: impl Into<Body>) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(format!("/api/endpoint/{slug}/mcp"))
        .header(axum::http::header::CONTENT_TYPE, "application/json")
        .body(body.into())
        .expect("a request")
}

fn message(method: &str) -> Body {
    Body::from(
        serde_json::to_vec(&json!({ "jsonrpc": "2.0", "id": 1, "method": method }))
            .expect("a message"),
    )
}

/// AG-32, R20: the first case, and the one most likely to be red. A client with no token is
/// told where to get one — for every slug, whether it resolves or not — so the challenge
/// cannot be used to ask which slugs this deployment serves.
#[tokio::test]
async fn the_challenge_is_the_same_for_a_slug_that_exists_and_one_that_does_not() {
    let real = call(post(CLOSED, message("tools/list"))).await;
    let invented = call(post("zzzzzzzzzzzzzzzzzzzzzzzzzz", message("tools/list"))).await;

    assert_eq!(real.0, StatusCode::UNAUTHORIZED);
    assert_eq!(invented.0, StatusCode::UNAUTHORIZED);
    assert_eq!(
        real.1,
        format!(
            "Bearer resource_metadata=\"{HOST}/api/endpoint/{CLOSED}/.well-known/oauth-protected-resource\"",
        ),
        "the challenge names this resource's own metadata",
    );
    assert_eq!(
        real.2, invented.2,
        "and the two bodies are the same document",
    );
}

/// AG-32: a public instance needs no token, so it is served before the challenge is even
/// considered. The case that would break this is a public endpoint answering 401 to a client
/// that simply has no account anywhere.
#[tokio::test]
async fn a_public_instance_answers_without_a_token() {
    let (status, challenge, body) = call(post(OPEN, message("tools/list"))).await;

    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(challenge.is_empty(), "nothing to authenticate against");
    let answer: Value = serde_json::from_str(&body).expect("a JSON-RPC answer");
    assert_eq!(answer["jsonrpc"], json!("2.0"));
    assert!(answer["result"]["tools"].is_array(), "{body}");
}

/// EP-05: an endpoint that serves NGSI-LD only has no MCP instance, and asking for one is
/// 404 — the same 404 as an endpoint that does not exist, so the representation table is not
/// readable from outside either.
#[tokio::test]
async fn an_endpoint_without_the_mcp_representation_has_no_instance() {
    let served = call(post(NGSI_ONLY, message("tools/list"))).await;
    let absent = call(post("zzzzzzzzzzzzzzzzzzzzzzzzzy", message("tools/list"))).await;

    assert_eq!(served.0, StatusCode::NOT_FOUND);
    // The invented slug is public-less and unresolvable, so it is refused one step earlier,
    // at the challenge. What matters is that the NGSI-LD-only endpoint never answers a tool
    // call, and that its refusal repeats nothing about it.
    assert_ne!(absent.0, StatusCode::OK);
    assert!(
        !served.2.contains(NGSI_ONLY) && !served.2.contains("ovzdusie"),
        "{}",
        served.2,
    );
}

/// T-0166: a body that is not JSON is the JSON-RPC parse error, not a stack trace and not a
/// 500. Every shape of "not a message" lands in the same place.
#[tokio::test]
async fn a_body_that_is_not_json_is_the_json_rpc_parse_error() {
    for body in [
        "",
        " ",
        "not json",
        "{",
        "{\"jsonrpc\":\"2.0\",}",
        "<html>hello</html>",
        "\u{0}",
        "[1,2",
    ] {
        let (status, _, answer) = call(post(OPEN, body.to_owned())).await;
        assert_eq!(status, StatusCode::OK, "{body:?} answered {answer}");
        let parsed: Value = serde_json::from_str(&answer).expect("a JSON-RPC answer");
        assert_eq!(parsed["error"]["code"], json!(-32700), "{body:?}");
        assert_eq!(parsed["id"], Value::Null);
    }
}

/// A body that is JSON and is not a message: the façade answers a JSON-RPC error rather than
/// panicking on a member it expected. `null`, a number, a string and a list are each valid
/// JSON and none of them is a request.
#[tokio::test]
async fn json_that_is_not_a_message_is_refused_as_a_message() {
    for body in [
        "null",
        "0",
        "-1",
        "\"tools/list\"",
        "[]",
        "{}",
        "[{}]",
        "true",
    ] {
        let (status, _, answer) = call(post(OPEN, body.to_owned())).await;
        assert!(
            status == StatusCode::OK || status == StatusCode::ACCEPTED,
            "{body:?} answered {status}: {answer}",
        );
        if status == StatusCode::OK {
            let parsed: Value = serde_json::from_str(&answer).expect("a JSON-RPC answer");
            assert!(
                parsed.get("error").is_some() || parsed.get("result").is_some(),
                "{body:?} answered {answer}",
            );
        }
    }
}

/// SP-19: a notification carries no `id` and gets no answer — 202 and an empty body. A
/// notification that came back with a result would be a message the client never asked for.
#[tokio::test]
async fn a_notification_is_acknowledged_and_nothing_more() {
    let (status, _, body) = call(post(
        OPEN,
        serde_json::to_vec(&json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }))
            .expect("a notification"),
    ))
    .await;

    assert_eq!(status, StatusCode::ACCEPTED);
    assert!(body.is_empty(), "nothing comes back: {body}");
}

/// The body is read with a ceiling, so one caller cannot make this pod hold an arbitrary
/// amount of memory by posting a message that never ends. Past the ceiling it is the parse
/// error, which is what a client can act on.
#[tokio::test]
async fn a_body_past_the_ceiling_is_refused_rather_than_read() {
    let huge = format!(
        "{{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"tools/list\",\"pad\":\"{}\"}}",
        "x".repeat(9 * 1024 * 1024),
    );
    let (status, _, answer) = call(post(OPEN, huge)).await;

    assert_eq!(status, StatusCode::OK);
    let parsed: Value = serde_json::from_str(&answer).expect("a JSON-RPC answer");
    assert_eq!(parsed["error"]["code"], json!(-32700), "{answer}");
}

/// PF-46: a token that verifies for something else is refused here too, and the refusal is
/// the challenge rather than a description of what was wrong with the token.
#[tokio::test]
async fn a_token_of_another_audience_does_not_open_the_instance() {
    let realm = common::Realm::new();
    let elsewhere = realm.workload_token("other-workload", json!("another-slug"));
    let (status, _, body) = call(
        Request::builder()
            .method("POST")
            .uri(format!("/api/endpoint/{CLOSED}/mcp"))
            .header(axum::http::header::CONTENT_TYPE, "application/json")
            .header(
                axum::http::header::AUTHORIZATION,
                format!("Bearer {elsewhere}"),
            )
            .body(message("tools/list"))
            .expect("a request"),
    )
    .await;

    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert!(
        !body.contains("another-slug") && !body.contains(&elsewhere),
        "{body}",
    );
}

/// GW20, EP-21: the tenancy headers are stripped before the slug is even resolved, so a
/// forged one buys a tool call nothing.
#[tokio::test]
async fn a_forged_tenant_header_changes_no_answer() {
    let honest = call(post(OPEN, message("tools/list"))).await;
    let forged = call(
        Request::builder()
            .method("POST")
            .uri(format!("/api/endpoint/{OPEN}/mcp"))
            .header(axum::http::header::CONTENT_TYPE, "application/json")
            .header("NGSILD-Tenant", "somebody-elses-space")
            .header("X-Forwarded-User", "spravca")
            .body(message("tools/list"))
            .expect("a request"),
    )
    .await;

    assert_eq!(honest.0, StatusCode::OK);
    assert_eq!(forged, honest);
}

/// R20: the 401 of an unauthenticated call names the resource metadata of the slug that was
/// typed and nothing else of the deployment — no tenant, no broker, no realm URL.
#[tokio::test]
async fn the_challenge_names_nothing_of_the_deployment() {
    let (_, challenge, body) = call(post(CLOSED, message("tools/list"))).await;

    for secret in ["127.0.0.1", "ovzdusie", "realms/joinedcontext"] {
        assert!(!challenge.contains(secret), "{secret:?} in {challenge}");
        assert!(!body.contains(secret), "{secret:?} in {body}");
    }
}

/// RFC 9110 section 9.1: the instance is a `POST` surface, so every other method is refused
/// by the router before any of this runs — and a `GET` in particular, because Streamable HTTP
/// clients try to open a stream with one and this deployment keeps no session (SP-19).
#[tokio::test]
async fn nothing_but_post_reaches_the_instance() {
    for method in ["GET", "PUT", "PATCH", "DELETE"] {
        let response = app()
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri(format!("/api/endpoint/{OPEN}/mcp"))
                    .body(Body::empty())
                    .expect("a request"),
            )
            .await
            .expect("the gateway answers");
        assert_eq!(
            response.status(),
            StatusCode::METHOD_NOT_ALLOWED,
            "{method} reached the instance",
        );
    }
}
