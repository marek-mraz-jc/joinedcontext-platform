//! Edge cases of `app::access` (T-1879, EP-26, MP-02).
//!
//! Contract, in one sentence: `GET /api/endpoint/{slug}/access` answers what **this** caller
//! may do here, from the same PDP that enforces it, in whichever of the four representations
//! the `Accept` header asks for — AuthZEN, ODRL as JSON-LD, the same policy as Turtle, or the
//! residual as a UCAST tree (T-0163, EP-55…EP-60).
//!
//! The inputs are the slug, the `Authorization` header and `Accept`. Two things must not leak:
//! a grant that is not this caller's — the document is the one place where the platform writes
//! its rules down for a stranger to read — and, through the choice of representation, a
//! difference between the four: all four are computed from one PDP answer, so what one of
//! them shows the others must show too, and none of them may show more than the caller holds.
//!
//! Since T-3649 the document is the broker's view's (plan 5.6): what the caller holds, and that
//! no representation shows another subject's rule, is the engine's to hold and tested there
//! (Antares `addons/joinedcontext-policy/tests/access.rs`, T-3638). What stays here is the
//! gateway's half: admission, the negotiation, and what it adds to or keeps out of the answer.
//!
//! `access_format` reads the offers in the order they are written and never a `q` value.
//! That is deliberate — the access surface always has an answer and a 406 would tell a caller
//! nothing — and it is pinned below, because a client that writes its preferences as weights
//! gets the first name it happened to spell rather than the one it preferred.

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

/// A public endpoint whose grants are the public one and one nobody here holds.
const OPEN: &str = "k4y7pq2mzt6vhx3nbwrs5cjd8f";
/// An endpoint that needs a token.
const CLOSED: &str = "p9d2wc5kzn8mth4rqvb7xj3sfy";

const ODRL_JSON: &str = "application/odrl+json";
const TURTLE: &str = "text/turtle";
const GRANT_AST: &str = "application/vnd.joinedcontext.grant-ast+json";

fn policy(yaml: &str) -> PolicySpec {
    serde_norway::from_str(yaml).expect("the policy spec parses")
}

/// One grant the anonymous public holds, and one that belongs to a role it does not: the
/// second is what every case below looks for in the answer.
fn two_grants(space: &str) -> Vec<PolicySpec> {
    vec![
        policy(&format!(
            r#"contextSpaceRef: {space}
assigner: did:web:banskabystrica.sk
assignee: {{ kind: role, id: public }}
operations: [queryEntity, retrieveEntity]
information:
  - entities:
      - type: AirQualityObserved
    propertyNames: [pm10]
"#
        )),
        policy(&format!(
            r#"contextSpaceRef: {space}
assigner: did:web:tajny-uradnik.sk
assignee: {{ kind: role, id: data-steward }}
operations: [createEntity, deleteEntity, updateAttrs]
information:
  - entities:
      - type: InternalIncident
    propertyNames: [internalNote, severity]
"#
        )),
    ]
}

fn endpoint(slug: &str, audience: Audience) -> Endpoint {
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
        representations: vec![Representation::NgsiLd],
        rate_limit: None,
        creates: None,
        file_limits: None,
        hidden_attributes: Default::default(),
        projection: None,
        base_path: format!("/api/endpoint/{slug}"),
        models: Vec::new(),
        view_mapping: None,
        catalog: None,
        deprecation: None,
        feeds: Vec::new(),
        policy_names: Vec::new(),
        manifest_name: String::new(),
        policies: two_grants("ovzdusie"),
    }
}

/// The deployment, trusting one realm: the caller's token has to be minted by the same
/// `Realm` the router was built with, so the fixture takes it rather than making its own.
///
/// The broker's view answers every `access` hop with an empty document of the engine's shape
/// (T-3649): what the caller holds is the engine's to say (Antares
/// `addons/joinedcontext-policy/tests/access.rs`); these tests hold the gateway's half, the
/// negotiation and what it adds or keeps out.
async fn app_of(realm: &common::Realm) -> axum::Router {
    let broker = common::BrokerStub::start(vec![json!({
        "subject": { "type": "role", "id": "public" },
        "resource": { "type": "view", "id": OPEN },
        "permissions": [],
        "prohibitions": []
    })])
    .await;
    router(Arc::new(
        Gateway::new(
            Broker::new(broker.url),
            Box::new(PolicyPdp),
            "banskabystrica.sk",
        )
        .serve([
            endpoint(OPEN, Audience::Public),
            endpoint(CLOSED, Audience::Organization),
        ])
        .authenticate(
            Arc::new(realm.verifier()),
            ServiceAccounts::new(),
            Some("https://bb.example.sk".to_owned()),
        ),
    ))
}

async fn app() -> axum::Router {
    app_of(&common::Realm::new()).await
}

async fn call(request: Request<Body>) -> (StatusCode, String, String) {
    call_on(app().await, request).await
}

async fn call_on(app: axum::Router, request: Request<Body>) -> (StatusCode, String, String) {
    let response = app.oneshot(request).await.expect("the gateway answers");
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

fn accepting(slug: &str, accept: Option<&str>) -> Request<Body> {
    let mut builder = Request::builder().uri(format!("/api/endpoint/{slug}/access"));
    if let Some(accept) = accept {
        builder = builder.header(axum::http::header::ACCEPT, accept);
    }
    builder.body(Body::empty()).expect("a request")
}

/// EP-56: with no `Accept` at all the caller gets the AuthZEN document, because a surface
/// whose whole purpose is to tell somebody what they may do has to answer.
#[tokio::test]
async fn no_accept_header_is_the_authzen_document() {
    let (status, media, body) = call(accepting(OPEN, None)).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(media, "application/json");
    let document: Value = serde_json::from_str(&body).expect("a JSON document");
    assert_eq!(document["resource"]["id"], json!(OPEN));
    assert!(document["permissions"].is_array(), "{body}");
    assert!(document["prohibitions"].is_array(), "{body}");
}

/// EP-56, EP-57, EP-58: each media type this surface serves is served, and each is labelled
/// with the type it is, so a client that dispatches on `Content-Type` reads the right parser.
#[tokio::test]
async fn every_representation_the_surface_serves_is_labelled_as_itself() {
    for (accept, expected) in [
        ("application/json", "application/json"),
        ("application/ld+json", "application/json"),
        (ODRL_JSON, ODRL_JSON),
        (TURTLE, TURTLE),
        (GRANT_AST, GRANT_AST),
    ] {
        let (status, media, body) = call(accepting(OPEN, Some(accept))).await;
        assert_eq!(status, StatusCode::OK, "{accept}");
        assert_eq!(media, expected, "{accept} answered {body}");
    }
}

/// EP-56: anything this surface does not serve is the default document rather than a 406. The
/// list is the ways a client gets that wrong — a type nobody serves, an empty header, a
/// wildcard, a header full of spaces — and every one of them still leaves the caller with an
/// answer they can read.
#[tokio::test]
async fn an_accept_header_this_surface_cannot_serve_is_still_answered() {
    for accept in [
        "",
        " ",
        "*/*",
        "text/html",
        "application/xml",
        "application/octet-stream",
        ",",
        ";;;",
        "text/turtle_not_really",
        "application/JSON",
    ] {
        let (status, media, body) = call(accepting(OPEN, Some(accept))).await;
        assert_eq!(status, StatusCode::OK, "{accept:?} answered {body}");
        assert_eq!(
            media, "application/json",
            "{accept:?} answered {media}: {body}",
        );
    }
}

/// EP-56: the offers are read in the order they are written and a `q` weight is not read at
/// all. Pinned rather than argued with, because the surface always answering is the rule that
/// matters — and because a client whose preferences are weights gets the first name it spelled.
#[tokio::test]
async fn the_first_offer_this_surface_serves_wins_whatever_the_weights_say() {
    let (_, first, _) = call(accepting(
        OPEN,
        Some(&format!("{TURTLE}, application/json")),
    ))
    .await;
    assert_eq!(first, TURTLE);

    let (_, weighted, _) = call(accepting(
        OPEN,
        Some(&format!("application/json;q=0.1, {TURTLE};q=1.0")),
    ))
    .await;
    assert_eq!(
        weighted, "application/json",
        "the weight is not read; the order is",
    );

    // A parameter on the offer is ignored, which is what makes the ODRL profile reachable
    // when a client writes a charset beside it.
    let (_, parameterised, _) =
        call(accepting(OPEN, Some(&format!("{TURTLE};charset=utf-8")))).await;
    assert_eq!(parameterised, TURTLE);
}

/// PF-46, EP-03: an endpoint nobody may have refuses before any document is built, and an
/// unknown slug is the ordinary 404 — neither of them is a way of reading the rules of an
/// endpoint the caller cannot reach.
#[tokio::test]
async fn an_endpoint_the_caller_may_not_have_produces_no_document() {
    let (absent, _, _) = call(accepting("zzzzzzzzzzzzzzzzzzzzzzzzzz", None)).await;
    assert_eq!(absent, StatusCode::NOT_FOUND);

    for accept in [None, Some(ODRL_JSON), Some(TURTLE), Some(GRANT_AST)] {
        let (status, _, body) = call(accepting(CLOSED, accept)).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{accept:?}");
        assert!(
            !body.contains("pm10") && !body.contains("InternalIncident"),
            "{accept:?} wrote the rules into its refusal: {body}",
        );
    }
}

/// R20: the document names the endpoint the caller already typed and the space behind it, and
/// nothing of the deployment — no broker address, no realm URL, no file path.
#[tokio::test]
async fn the_document_names_nothing_of_the_deployment() {
    for accept in [None, Some(ODRL_JSON), Some(TURTLE), Some(GRANT_AST)] {
        let (_, _, body) = call(accepting(OPEN, accept)).await;
        for secret in ["127.0.0.1", "realms/joinedcontext", "src/", "crates/"] {
            assert!(
                !body.contains(secret),
                "{accept:?} leaked {secret:?}: {body}"
            );
        }
    }
}

/// GW20: the tenancy headers are stripped first, so a forged one cannot make this surface
/// describe another space's rules.
#[tokio::test]
async fn a_forged_tenant_header_does_not_change_the_document() {
    let honest = call(accepting(OPEN, None)).await;
    let forged = call(
        Request::builder()
            .uri(format!("/api/endpoint/{OPEN}/access"))
            .header("NGSILD-Tenant", "somebody-elses-space")
            .header("X-Forwarded-User", "spravca")
            .header("X-Forwarded-Groups", "data-steward")
            .body(Body::empty())
            .expect("a request"),
    )
    .await;

    assert_eq!(honest.0, StatusCode::OK);
    assert_eq!(forged, honest);
}

/// An `Accept` header that is long, repeated or full of offers is read and answered like any
/// other: the loop is bounded by the header the client could send, and nothing in it is kept.
#[tokio::test]
async fn a_pathological_accept_header_is_answered_like_any_other() {
    let many = std::iter::repeat_n("application/xml", 500)
        .collect::<Vec<_>>()
        .join(",");
    let long = format!("text/{}", "a".repeat(4000));
    for accept in [many.as_str(), long.as_str(), &format!("{many},{TURTLE}")] {
        let (status, _, _) = call(accepting(OPEN, Some(accept))).await;
        assert_eq!(status, StatusCode::OK, "{accept:.40}");
    }

    // The last of the three ends in Turtle, and the five hundred offers before it do not stop
    // the surface from finding it.
    let (_, media, _) = call(accepting(OPEN, Some(&format!("{many},{TURTLE}")))).await;
    assert_eq!(media, TURTLE);
}

/// RFC 9110 section 9.1: the access surface is read-only. Nothing but `GET` reaches it, so it
/// cannot be used to post a body at a route that answers what a caller may do.
#[tokio::test]
async fn nothing_but_get_reaches_the_access_surface() {
    for method in ["POST", "PUT", "PATCH", "DELETE"] {
        let response = app()
            .await
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri(format!("/api/endpoint/{OPEN}/access"))
                    .body(Body::from("{}"))
                    .expect("a request"),
            )
            .await
            .expect("the gateway answers");
        assert_eq!(
            response.status(),
            StatusCode::METHOD_NOT_ALLOWED,
            "{method} reached the access surface",
        );
    }
}
