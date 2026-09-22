//! Edge cases of `app::space_mcp_message` (T-1877, EP-26, MP-02).
//!
//! Contract, in one sentence: `POST /cs/{space}/mcp` is the same façade as the endpoint's own
//! MCP instance, reached by a space name instead of a slug — and because a space name is a
//! guessable word, `admit_space` collapses absent, not-served, not-authenticated and
//! not-granted into one 404, so this surface never sends the RFC 9728 challenge the endpoint
//! surface sends (SP-14, SP-06, SP-11, R20, AG-32).
//!
//! The inputs are the space name, the `Authorization` header, the body and the tenancy
//! headers it deletes. What must not leak is which of the four reasons refused the caller;
//! what must not be bypassed is the representation check and the PDP's own view of who may
//! discover the space.
//!
//! The consequence of the collapse is written down in
//! `no_challenge_and_no_metadata_route_answers_for_a_space`: a client that needs a token for a
//! space instance is not told so and cannot discover where to get one on this surface. It is
//! deliberate for the name, and it leaves the MCP client with nothing to act on; recorded as a
//! defect rather than as a hole, because the endpoint surface serves the same space.

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use context_gateway::app::{router, Gateway};
use context_gateway::auth::accounts::ServiceAccounts;
use context_gateway::pdp::PolicyPdp;
use context_gateway::proxy::Broker;
use context_gateway::resolver::{Endpoint, Space};
use jc_core::kinds::{Audience, PolicySpec, Representation};
use serde_json::{json, Value};
use std::sync::Arc;
use tower::ServiceExt;

/// A space whose MCP instance anyone may use.
const OPEN: &str = "ovzdusie";
/// A space that serves NGSI-LD only.
const NGSI_ONLY: &str = "doprava";
/// A space whose instance needs a token.
const CLOSED: &str = "socialne-sluzby";
/// A space that admits the anonymous caller and grants them nothing.
const UNGRANTED: &str = "vodovody";

fn policy(yaml: &str) -> PolicySpec {
    serde_norway::from_str(yaml).expect("the policy spec parses")
}

fn public_grant(space: &str) -> Vec<PolicySpec> {
    vec![policy(&format!(
        r#"contextSpaceRef: {space}
assigner: did:web:banskabystrica.sk
assignee: {{ kind: role, id: public }}
operations: [queryEntity, retrieveEntity]
information:
  - entities:
      - type: AirQualityObserved
    propertyNames: [pm10]
"#
    ))]
}

fn somebody_elses_grant(space: &str) -> Vec<PolicySpec> {
    vec![policy(&format!(
        r#"contextSpaceRef: {space}
assigner: did:web:banskabystrica.sk
assignee: {{ kind: role, id: data-steward }}
operations: [queryEntity, retrieveEntity]
information:
  - entities:
      - type: AirQualityObserved
"#
    ))]
}

fn space(
    name: &str,
    audience: Audience,
    representations: Vec<Representation>,
    policies: Vec<PolicySpec>,
) -> Space {
    Space {
        endpoint: Arc::new(Endpoint {
            roles: Default::default(),
            slug: name.to_owned(),
            title: Default::default(),
            description: Default::default(),
            space: name.to_owned(),
            project: name.to_owned(),
            audience,
            allowed_projects: Vec::new(),
            representations,
            rate_limit: None,
            file_limits: None,
            hidden_attributes: Default::default(),
            projection: None,
            base_path: format!("/cs/{name}"),
            models: Vec::new(),
            view_mapping: None,
            policies,
        }),
        title: Default::default(),
        description: Default::default(),
        is_sandbox: false,
        default_locale: Some("sk".to_owned()),
    }
}

fn app() -> axum::Router {
    let realm = common::Realm::new();
    router(Arc::new(
        Gateway::new(
            Broker::new("http://127.0.0.1:1"),
            Box::new(PolicyPdp),
            "banskabystrica.sk",
        )
        .serve_spaces([
            space(
                OPEN,
                Audience::Public,
                vec![Representation::NgsiLd, Representation::Mcp],
                public_grant(OPEN),
            ),
            space(
                NGSI_ONLY,
                Audience::Public,
                vec![Representation::NgsiLd],
                public_grant(NGSI_ONLY),
            ),
            space(
                CLOSED,
                Audience::Organization,
                vec![Representation::Mcp],
                public_grant(CLOSED),
            ),
            space(
                UNGRANTED,
                Audience::Public,
                vec![Representation::Mcp],
                somebody_elses_grant(UNGRANTED),
            ),
        ])
        .authenticate(
            Arc::new(realm.verifier()),
            ServiceAccounts::new(),
            Some("https://bb.example.sk".to_owned()),
        ),
    ))
}

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

fn post(space: &str, body: impl Into<Body>) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(format!("/cs/{space}/mcp"))
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

/// SP-06, SP-11, R20: the first case, and the one the handler stands on. Four reasons for
/// refusing, one answer, so a client walking a dictionary of city words learns nothing.
#[tokio::test]
async fn every_reason_for_refusing_a_space_instance_is_the_same_refusal() {
    let absent = call(post("mesto-neexistuje", message("tools/list"))).await;
    let not_served = call(post(NGSI_ONLY, message("tools/list"))).await;
    let needs_a_token = call(post(CLOSED, message("tools/list"))).await;
    let no_grant = call(post(UNGRANTED, message("tools/list"))).await;

    assert_eq!(absent.0, StatusCode::NOT_FOUND);
    for (name, answer) in [
        ("a space that serves no MCP", &not_served),
        ("a space that needs a token", &needs_a_token),
        ("a space no grant of this caller reaches", &no_grant),
    ] {
        assert_eq!(*answer, absent, "{name} answers differently");
    }
}

/// SP-14: the public instance answers the same façade the endpoint surface answers, with no
/// token and nothing kept between calls.
#[tokio::test]
async fn a_public_space_instance_answers_the_same_facade() {
    let (status, challenge, body) = call(post(OPEN, message("tools/list"))).await;

    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(challenge.is_empty());
    let answer: Value = serde_json::from_str(&body).expect("a JSON-RPC answer");
    assert!(answer["result"]["tools"].is_array(), "{body}");
}

/// AG-32 on this surface: no challenge is sent, and no metadata route answers for a space
/// name either, so an MCP client that needs a token for a space instance is told nothing it
/// can act on. Deliberate for the 404 — a challenge would confirm the name — and a gap for
/// the client, so it is pinned here and recorded as a defect (chyby.md). The same space is
/// reachable through its endpoint slug, where the challenge is sent.
#[tokio::test]
async fn no_challenge_and_no_metadata_route_answers_for_a_space() {
    let (status, challenge, _) = call(post(CLOSED, message("tools/list"))).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(challenge.is_empty(), "no challenge on the space surface");

    for path in [
        format!("/cs/{CLOSED}/.well-known/oauth-protected-resource"),
        format!("/.well-known/oauth-protected-resource/cs/{CLOSED}"),
    ] {
        let (metadata, _, _) = call(
            Request::builder()
                .uri(&path)
                .body(Body::empty())
                .expect("a request"),
        )
        .await;
        assert_eq!(metadata, StatusCode::NOT_FOUND, "{path} answers");
    }
}

/// SP-06: no token of any shape turns the 404 into something else. A valid token of another
/// audience, a malformed one and none at all are one answer.
#[tokio::test]
async fn no_token_of_any_shape_changes_the_refusal() {
    let realm = common::Realm::new();
    let elsewhere = realm.workload_token("some-workload", json!("another-space"));
    let reference = call(post(CLOSED, message("tools/list"))).await;
    for header in [
        format!("Bearer {elsewhere}"),
        "Bearer not.a.token".to_owned(),
        "Basic YWRtaW46YWRtaW4=".to_owned(),
        "Bearer ".to_owned(),
    ] {
        let (status, challenge, body) = call(
            Request::builder()
                .method("POST")
                .uri(format!("/cs/{CLOSED}/mcp"))
                .header(axum::http::header::CONTENT_TYPE, "application/json")
                .header(axum::http::header::AUTHORIZATION, &header)
                .body(message("tools/list"))
                .expect("a request"),
        )
        .await;
        assert_eq!(status, reference.0, "{header:?}");
        assert_eq!(body, reference.2, "{header:?}");
        assert!(challenge.is_empty(), "{header:?}");
    }
}

/// SP-11: a token that verifies and carries no grant over this space does not find it either.
/// The PDP that enforces the request is the one that decides who may see it exists.
#[tokio::test]
async fn a_valid_token_without_a_grant_does_not_find_the_instance() {
    let realm = common::Realm::new();
    let employee = realm.mint(&json!({
        "iss": common::ISSUER,
        "sub": "an-employee",
        "aud": UNGRANTED,
        "preferred_username": "jana",
        "groups": [format!("/{UNGRANTED}")],
        "exp": common::in_seconds(300),
        "iat": common::in_seconds(-10),
    }));

    let (status, _, body) = call(
        Request::builder()
            .method("POST")
            .uri(format!("/cs/{UNGRANTED}/mcp"))
            .header(axum::http::header::CONTENT_TYPE, "application/json")
            .header(
                axum::http::header::AUTHORIZATION,
                format!("Bearer {employee}"),
            )
            .body(message("tools/list"))
            .expect("a request"),
    )
    .await;

    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
}

/// T-0166: the body is only read once the space has been admitted, so a stranger cannot make
/// the gateway parse anything by posting to a name they guessed. A malformed body on a space
/// they may not have is the same 404 as a well-formed one.
#[tokio::test]
async fn a_body_is_not_read_for_a_space_the_caller_may_not_have() {
    let reference = call(post(CLOSED, message("tools/list"))).await;
    for body in ["", "not json", "{", "[]", &"x".repeat(64 * 1024)] {
        let (status, _, answer) = call(post(CLOSED, body.to_owned())).await;
        assert_eq!(status, reference.0, "{body:.20?}");
        assert_eq!(answer, reference.2, "{body:.20?}");
    }
}

/// T-0166: on a space the caller does have, a body that is not a message is the JSON-RPC
/// parse error, the same as on the endpoint surface.
#[tokio::test]
async fn a_body_that_is_not_json_is_the_json_rpc_parse_error() {
    for body in ["", "not json", "{", "<html/>", "[1,2"] {
        let (status, _, answer) = call(post(OPEN, body.to_owned())).await;
        assert_eq!(status, StatusCode::OK, "{body:?} answered {answer}");
        let parsed: Value = serde_json::from_str(&answer).expect("a JSON-RPC answer");
        assert_eq!(parsed["error"]["code"], json!(-32700), "{body:?}");
    }
}

/// SP-19: a notification is acknowledged and nothing comes back, on this surface as on the
/// other one.
#[tokio::test]
async fn a_notification_is_acknowledged_and_nothing_more() {
    let (status, _, body) = call(post(
        OPEN,
        serde_json::to_vec(&json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }))
            .expect("a notification"),
    ))
    .await;

    assert_eq!(status, StatusCode::ACCEPTED);
    assert!(body.is_empty(), "{body}");
}

/// GW20, EP-21: the tenancy headers are gone before the space is resolved, so a forged one
/// cannot point a tool call at another space's data.
#[tokio::test]
async fn a_forged_tenant_header_changes_no_answer() {
    let honest = call(post(OPEN, message("tools/list"))).await;
    let forged = call(
        Request::builder()
            .method("POST")
            .uri(format!("/cs/{OPEN}/mcp"))
            .header(axum::http::header::CONTENT_TYPE, "application/json")
            .header("NGSILD-Tenant", CLOSED)
            .header("X-Forwarded-User", "spravca")
            .body(message("tools/list"))
            .expect("a request"),
    )
    .await;

    assert_eq!(honest.0, StatusCode::OK);
    assert_eq!(forged, honest);
}

/// SP-01: a space name is matched as written, so no near-miss of a real one resolves.
#[tokio::test]
async fn no_near_miss_of_a_space_name_resolves_to_the_instance() {
    let reference = call(post("mesto-neexistuje", message("tools/list"))).await;
    let upper = OPEN.to_uppercase();
    let padded = format!("%20{OPEN}");
    let nul = format!("{OPEN}%00");
    for name in [
        upper.as_str(),
        padded.as_str(),
        nul.as_str(),
        "ovzdusie.",
        "%2e%2e",
    ] {
        let (status, _, body) = call(post(name, message("tools/list"))).await;
        assert_eq!(status, reference.0, "{name:?} answered {body}");
    }
}

/// R20: the refusal names nothing — not the space asked for, not the broker, not the realm,
/// not the role whose grant it was.
#[tokio::test]
async fn the_refusal_names_nothing_of_the_deployment() {
    let (_, _, body) = call(post(UNGRANTED, message("tools/list"))).await;

    for secret in [
        UNGRANTED,
        "data-steward",
        "127.0.0.1",
        "realms/joinedcontext",
        "banskabystrica.sk",
    ] {
        assert!(!body.contains(secret), "{secret:?} leaked into {body}");
    }
}

/// The instance is a `POST` surface; the router refuses everything else before any of this
/// runs, and in particular a `GET`, which is what a Streamable HTTP client opens a session
/// with — and this deployment keeps none.
#[tokio::test]
async fn nothing_but_post_reaches_the_space_instance() {
    for method in ["GET", "PUT", "PATCH", "DELETE"] {
        let response = app()
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri(format!("/cs/{OPEN}/mcp"))
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
