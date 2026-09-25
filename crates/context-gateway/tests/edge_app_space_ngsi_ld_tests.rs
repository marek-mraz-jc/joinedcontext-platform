//! Edge cases of `app::space_ngsi_ld` (T-1875, EP-26, MP-02).
//!
//! Contract, in one sentence: `ANY /cs/{space}/ngsi-ld/v1/{*rest}` is the same ETSI tree under
//! a space name instead of an opaque slug — and because a space name is a guessable word
//! (`ovzdusie`, `helsinki`, `air-quality`), `admit_space` collapses **every** way in which the
//! caller may not have it into one answer: the space is not there, it does not serve NGSI-LD,
//! the token does not verify, or no grant of the caller's reaches it, and all four are the
//! same 404 (SP-01, SP-06, SP-11, R20).
//!
//! That is the whole security property of this handler, and the cases below are the ways of
//! asking which of the four it was. The inputs are the space name, the rest of the path, the
//! query, the `Authorization` header, and the tenancy headers it deletes before anything else.
//!
//! The broker is an address nothing listens on, so a request that was served rather than
//! refused shows up as a bad gateway and cannot pass quietly.

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use context_gateway::app::{router, Gateway};
use context_gateway::auth::accounts::ServiceAccounts;
use context_gateway::pdp::PolicyPdp;
use context_gateway::proxy::Broker;
use context_gateway::resolver::{Endpoint, Space};
use jc_core::kinds::{Audience, PolicySpec, Representation};
use serde_json::json;
use std::sync::Arc;
use tower::ServiceExt;

/// A space anyone may read.
const OPEN: &str = "ovzdusie";
/// A space of the same deployment that serves MCP only.
const MCP_ONLY: &str = "doprava";
/// A space that exists and admits nobody who is not signed in.
const CLOSED: &str = "socialne-sluzby";
/// A space that exists, admits the anonymous caller, and grants them nothing.
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

/// A grant that reaches this space and belongs to somebody else, so the anonymous caller is
/// admitted by the audience and refused by `discoverable` (SP-11).
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
            catalog: None,
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
                MCP_ONLY,
                Audience::Public,
                vec![Representation::Mcp],
                public_grant(MCP_ONLY),
            ),
            space(
                CLOSED,
                Audience::Organization,
                vec![Representation::NgsiLd],
                public_grant(CLOSED),
            ),
            space(
                UNGRANTED,
                Audience::Public,
                vec![Representation::NgsiLd],
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

fn tree(space: &str) -> String {
    format!("/cs/{space}/ngsi-ld/v1/entities?type=AirQualityObserved")
}

fn get(path: &str) -> Request<Body> {
    Request::builder()
        .uri(path)
        .body(Body::empty())
        .expect("a request")
}

fn with_token(path: &str, token: &str) -> Request<Body> {
    Request::builder()
        .uri(path)
        .header(axum::http::header::AUTHORIZATION, format!("Bearer {token}"))
        .body(Body::empty())
        .expect("a request")
}

/// SP-06, SP-11, R20: the first case, and the one that carries the handler. Four different
/// reasons for refusing, one document: a stranger who walks the dictionary learns nothing
/// about which of the four a given name fell into, and therefore nothing about what runs here.
#[tokio::test]
async fn every_reason_for_refusing_a_space_is_the_same_refusal() {
    let absent = call(get(&tree("mesto-neexistuje"))).await;
    let not_served = call(get(&tree(MCP_ONLY))).await;
    let needs_a_token = call(get(&tree(CLOSED))).await;
    let no_grant = call(get(&tree(UNGRANTED))).await;

    assert_eq!(absent.0, StatusCode::NOT_FOUND);
    for (name, answer) in [
        ("a space that serves no NGSI-LD", &not_served),
        ("a space that needs a token", &needs_a_token),
        ("a space no grant of this caller reaches", &no_grant),
    ] {
        assert_eq!(*answer, absent, "{name} answers differently");
    }
}

/// SP-06: there is no 401 on this surface at all. A space name is a word, so telling a caller
/// that *this* one needs a token is telling them the word was right.
#[tokio::test]
async fn a_space_that_needs_a_token_never_answers_401() {
    let realm = common::Realm::new();
    let elsewhere = realm.workload_token("some-workload", json!("another-space"));
    for request in [
        get(&tree(CLOSED)),
        with_token(&tree(CLOSED), &elsewhere),
        with_token(&tree(CLOSED), "not.a.token"),
        with_token(&tree(OPEN), &elsewhere),
        with_token(&tree(OPEN), "not.a.token"),
    ] {
        let (status, _, body) = call(request).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    }
}

/// T-0272: even collapsed to 404, the refusal is the CIM 009 error document, because the
/// client on this surface is a stock NGSI-LD client and reads `type`.
#[tokio::test]
async fn the_collapsed_refusal_is_still_an_ngsi_ld_error_document() {
    let (status, media, body) = call(get(&tree("mesto-neexistuje"))).await;

    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(media, "application/json");
    let problem: serde_json::Value = serde_json::from_str(&body).expect("a JSON document");
    assert_eq!(
        problem["type"],
        json!("https://uri.etsi.org/ngsi-ld/errors/ResourceNotFound")
    );
    assert!(problem["detail"].is_string());
}

/// SP-01: a space name is matched as it is written. Nothing is trimmed, lower-cased or
/// decoded twice on the way in, so no near-miss of a real name resolves to it.
#[tokio::test]
async fn no_near_miss_of_a_space_name_resolves_to_the_space() {
    let upper = OPEN.to_uppercase();
    let padded = format!("%20{OPEN}");
    let trailing = format!("{OPEN}%20");
    let twice = format!("%256f{}", &OPEN[1..]);
    let nul = format!("{OPEN}%00");
    for name in [
        upper.as_str(),
        padded.as_str(),
        trailing.as_str(),
        twice.as_str(),
        nul.as_str(),
        "ovzdusie.",
        "ovzdusie/",
        "..",
        "%2e%2e",
        "ovzdusie%2f..",
    ] {
        let (status, _, body) = call(get(&tree(name))).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{name:?} answered {body}");
    }
}

/// GW20, EP-21: the tenancy the client claimed is gone before the space is resolved, so a
/// forged `NGSILD-Tenant` cannot move a granted request to another space's data.
#[tokio::test]
async fn a_forged_tenant_header_does_not_move_the_request() {
    let honest = call(get(&tree(OPEN))).await;
    let forged = call(
        Request::builder()
            .uri(tree(OPEN))
            .header("NGSILD-Tenant", CLOSED)
            .header("NGSILD-Path", "/cs/socialne-sluzby")
            .header("X-Forwarded-User", "spravca")
            .body(Body::empty())
            .expect("a request"),
    )
    .await;

    assert_eq!(honest.0, StatusCode::BAD_GATEWAY, "the broker is away");
    assert_eq!(forged, honest);
}

/// SP-01, EP-03: the two tables are two namespaces. A space name in the slug position and a
/// slug in the space position both miss, so neither table is reachable through the other.
#[tokio::test]
async fn the_space_table_is_not_reachable_through_the_endpoint_table() {
    let (as_slug, _, _) = call(get(
        "/api/endpoint/ovzdusie/ngsi-ld/v1/entities?type=AirQualityObserved",
    ))
    .await;
    assert_eq!(as_slug, StatusCode::NOT_FOUND);

    // And the space surface has no endpoint of its own to be addressed by: this deployment
    // registers spaces only, so every slug misses.
    let (as_space, _, _) = call(get(&tree("k4y7pq2mzt6vhx3nbwrs5cjd8f"))).await;
    assert_eq!(as_space, StatusCode::NOT_FOUND);
}

/// SP-06: no path under the tree gets around the collapse, and none of them ever answers
/// anything but the one document for a space the caller may not have.
#[tokio::test]
async fn no_path_under_the_tree_answers_differently() {
    let reference = call(get(&tree("mesto-neexistuje"))).await;
    for rest in [
        "entities",
        "entities/urn:ngsi-ld:AirQualityObserved:bb:ovzdusie:1",
        "entityOperations/query",
        "types",
        "attributes",
        "subscriptions",
        "temporal/entities",
        "csourceRegistrations",
        "",
        "/",
    ] {
        for name in [CLOSED, UNGRANTED, "mesto-neexistuje"] {
            let (status, _, _) = call(get(&format!("/cs/{name}/ngsi-ld/v1/{rest}"))).await;
            assert_eq!(status, reference.0, "/cs/{name}/ngsi-ld/v1/{rest}");
        }
    }
}

/// R20: the refusal names nothing of the deployment — not the space that was asked for, not
/// the broker, not the realm.
#[tokio::test]
async fn the_refusal_names_nothing_of_the_deployment() {
    let (_, _, body) = call(get(&tree(CLOSED))).await;

    for secret in [
        CLOSED,
        "127.0.0.1",
        "banskabystrica.sk",
        "realms/joinedcontext",
        "data-steward",
    ] {
        assert!(!body.contains(secret), "{secret:?} leaked into {body}");
    }
}

/// SP-11: `discoverable` asks the very PDP that enforces the request, so a space whose only
/// grant belongs to a role the caller does not hold is as good as absent — including for a
/// caller who holds a perfectly valid token of this deployment.
#[tokio::test]
async fn a_valid_token_without_a_grant_still_does_not_find_the_space() {
    let realm = common::Realm::new();
    let realm_token = realm.mint(&json!({
        "iss": common::ISSUER,
        "sub": "an-employee",
        "aud": UNGRANTED,
        "preferred_username": "jana",
        "groups": [format!("/{UNGRANTED}")],
        "exp": common::in_seconds(300),
        "iat": common::in_seconds(-10),
    }));

    let (status, _, body) = call(with_token(&tree(UNGRANTED), &realm_token)).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
}

/// The same refused request twice at once is refused twice, with nothing kept between them:
/// the handler holds no state per caller and no state per space name that a second call could
/// read.
#[tokio::test]
async fn the_same_refused_request_twice_at_once_is_refused_twice() {
    let (first, second) = tokio::join!(call(get(&tree(CLOSED))), call(get(&tree(CLOSED))));

    assert_eq!(first.0, StatusCode::NOT_FOUND);
    assert_eq!(first, second);
}

/// EP-21: a `Host` or forwarding header the client wrote never reaches the answer, so nobody
/// can make this surface mint a URL pointing at a host of their choosing.
#[tokio::test]
async fn a_client_supplied_host_does_not_reach_the_answer() {
    let (_, _, body) = call(
        Request::builder()
            .uri(tree(CLOSED))
            .header("X-Forwarded-Host", "evil.example")
            .header("Forwarded", "host=evil.example")
            .body(Body::empty())
            .expect("a request"),
    )
    .await;

    assert!(!body.contains("evil.example"), "{body}");
}
