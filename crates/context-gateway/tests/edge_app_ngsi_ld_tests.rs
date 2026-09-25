//! Edge cases of `app::ngsi_ld` (T-1874, EP-26, MP-02).
//!
//! Contract, in one sentence: `ANY /api/endpoint/{slug}/ngsi-ld/v1/{*rest}` strips everything
//! the client claimed about its identity or tenancy, resolves the slug, refuses an endpoint
//! that does not serve NGSI-LD **exactly the way it refuses one that does not exist**,
//! authenticates, and renders every refusal as the CIM 009 clause 5.5.3 error document a
//! stock NGSI-LD client reads (EP-03, EP-05, EP-21, GW20, R20).
//!
//! The inputs it reads are the slug, the rest of the path, the query string, the
//! `Authorization` header and — only to delete them — the tenancy headers. What must not leak
//! here is the difference between *absent*, *not served* and *refused*: a slug is guessable,
//! so any of the three answering differently from the others is an enumeration oracle for
//! what the deployment runs. Every case below is a way of asking that question sideways.
//!
//! The broker for these tests is an address nothing listens on: if a refusal ever forwarded,
//! the case would fail on the bad gateway rather than pass quietly.

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

/// A public endpoint that serves the ETSI tree.
const OPEN: &str = "k4y7pq2mzt6vhx3nbwrs5cjd8f";
/// An endpoint of the same deployment that serves MCP only: it exists, and it is not here.
const MCP_ONLY: &str = "m3x8tq7nzv2hbw6rjs4cyd9gpk";
/// An endpoint that exists and needs a token.
const CLOSED: &str = "p9d2wc5kzn8mth4rqvb7xj3sfy";

fn policy(yaml: &str) -> PolicySpec {
    serde_norway::from_str(yaml).expect("the policy spec parses")
}

/// One public grant over one type, which is all these cases need to get past the PDP.
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
        catalog: None,
        policies: public_grant(),
    }
}

/// The deployment: three endpoints, a realm, and a broker nothing is listening on.
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
            endpoint(MCP_ONLY, Audience::Public, vec![Representation::Mcp]),
            endpoint(CLOSED, Audience::Organization, vec![Representation::NgsiLd]),
        ])
        .authenticate(
            Arc::new(realm.verifier()),
            ServiceAccounts::new(),
            Some("https://bb.example.sk".to_owned()),
        ),
    ))
}

/// One request, with the status, the content type and the body exactly as they were sent: a
/// leak is looked for in the bytes, not in the members a test remembers to read.
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

fn get(path: &str) -> Request<Body> {
    Request::builder()
        .uri(path)
        .body(Body::empty())
        .expect("a request")
}

/// EP-05, EP-03, R20: the first case, and the one most likely to be red — an endpoint that
/// exists and does not serve NGSI-LD must be refused with the same bytes as one that does not
/// exist at all. A difference of a single character here tells a stranger which slugs are real.
#[tokio::test]
async fn an_endpoint_that_serves_no_ngsi_ld_is_refused_like_one_that_is_not_there() {
    let (served, media, body) = call(get(&format!(
        "/api/endpoint/{MCP_ONLY}/ngsi-ld/v1/entities?type=AirQualityObserved"
    )))
    .await;
    let (absent, absent_media, absent_body) = call(get(
        "/api/endpoint/zzzzzzzzzzzzzzzzzzzzzzzzzz/ngsi-ld/v1/entities?type=AirQualityObserved",
    ))
    .await;

    assert_eq!(served, StatusCode::NOT_FOUND);
    assert_eq!(served, absent);
    assert_eq!(media, absent_media);
    assert_eq!(body, absent_body, "the two refusals are the same document");
    assert!(
        !body.contains(MCP_ONLY),
        "and neither of them repeats the slug: {body}",
    );
}

/// T-0272, CIM 009 5.5.3: the gateway's own refusal is an NGSI-LD error document, in
/// `application/json` and carrying the ETSI `type`, so a client that keys on `type` learns the
/// same thing whether the gateway or the broker refused.
#[tokio::test]
async fn a_refusal_is_rendered_the_way_an_ngsi_ld_client_reads_one() {
    let (status, media, body) = call(get(
        "/api/endpoint/does-not-exist/ngsi-ld/v1/entities?type=AirQualityObserved",
    ))
    .await;

    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(media, "application/json", "never problem+json here");
    let problem: Value = serde_json::from_str(&body).expect("a JSON document");
    assert_eq!(
        problem["type"],
        json!("https://uri.etsi.org/ngsi-ld/errors/ResourceNotFound")
    );
    assert!(problem["title"].is_string(), "{body}");
    assert!(problem["detail"].is_string(), "clause 6.3.3 wants one");
}

/// EP-03: every spelling of a slug that is not a slug is the same refusal. A slug is opaque,
/// so nothing about it is normalised — an upper-cased or percent-encoded one is simply another
/// name nothing answers to, and none of them is treated as the endpoint it resembles.
#[tokio::test]
async fn no_spelling_of_the_slug_resolves_to_an_endpoint_it_is_not() {
    let upper = OPEN.to_uppercase();
    let once = OPEN.replace('k', "%6b");
    let twice = OPEN.replace('k', "%256b");
    let padded = format!("%20{OPEN}");
    let trailing = format!("{OPEN}%20");
    let nul = format!("{OPEN}%00");
    let dotted = format!("{OPEN}.");
    for slug in [
        upper.as_str(),
        twice.as_str(),
        padded.as_str(),
        trailing.as_str(),
        nul.as_str(),
        dotted.as_str(),
        "",
        "..",
        "%2e%2e",
    ] {
        let (status, _, body) = call(get(&format!(
            "/api/endpoint/{slug}/ngsi-ld/v1/entities?type=AirQualityObserved"
        )))
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{slug:?} answered {body}");
    }

    // A single round of percent-encoding is decoded by the `Path` extractor, so `admit` does
    // resolve the endpoint — and the request is still 404, because the prefix this handler
    // builds carries the decoded slug while `uri.path()` carries the encoded one, so
    // `strip_prefix` finds nothing and no operation is named. Refusing is the safe side of
    // that mismatch, and it is the whole answer: no path of the tree is reachable this way.
    let (status, _, body) = call(get(&format!(
        "/api/endpoint/{once}/ngsi-ld/v1/entities?type=AirQualityObserved"
    )))
    .await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "an encoded slug names no operation: {body}",
    );
}

/// GW20, EP-21: the tenancy headers a client forged are gone before anything is resolved or
/// authenticated. The endpoint is public, so the request goes all the way to the broker — and
/// the broker is not listening, which is how the case proves the header never travelled: a
/// forged tenant cannot have been honoured by a hop that never happened, and what is asserted
/// instead is that the forgery changes nothing about the answer.
#[tokio::test]
async fn a_forged_tenant_header_changes_nothing() {
    let honest = call(get(&format!(
        "/api/endpoint/{OPEN}/ngsi-ld/v1/entities?type=AirQualityObserved"
    )))
    .await;
    let forged = call(
        Request::builder()
            .uri(format!(
                "/api/endpoint/{OPEN}/ngsi-ld/v1/entities?type=AirQualityObserved"
            ))
            .header("NGSILD-Tenant", "somebody-elses-space")
            .header("X-Forwarded-User", "admin")
            .body(Body::empty())
            .expect("a request"),
    )
    .await;

    assert_eq!(honest.0, StatusCode::BAD_GATEWAY, "the broker is away");
    assert_eq!(forged, honest, "and the forgery bought nothing");
}

/// PF-46: a token that verifies for another resource is not a token here. The endpoint is
/// public, and this is exactly why the case matters: `authenticate` refuses a presented token
/// it cannot verify rather than falling back to the anonymous role, so a caller cannot use a
/// wrong token as a way of being served as the public.
#[tokio::test]
async fn a_token_of_another_audience_is_refused_rather_than_served_as_the_public() {
    let realm = common::Realm::new();
    let elsewhere = realm.workload_token("other-endpoint", json!("some-other-slug"));
    let (status, media, body) = call(
        Request::builder()
            .uri(format!(
                "/api/endpoint/{OPEN}/ngsi-ld/v1/entities?type=AirQualityObserved"
            ))
            .header(
                axum::http::header::AUTHORIZATION,
                format!("Bearer {elsewhere}"),
            )
            .body(Body::empty())
            .expect("a request"),
    )
    .await;

    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(media, "application/json");
    assert!(
        !body.contains("some-other-slug") && !body.contains(&elsewhere),
        "and the refusal repeats nothing the caller presented: {body}",
    );
}

/// PF-46, GW22: no token on an endpoint whose audience is the organization is 401, and the
/// refusal says nothing about the endpoint beyond the fact that it wants a token.
#[tokio::test]
async fn an_anonymous_caller_of_a_closed_endpoint_is_refused_without_a_hint() {
    let (status, _, body) = call(get(&format!(
        "/api/endpoint/{CLOSED}/ngsi-ld/v1/entities?type=AirQualityObserved"
    )))
    .await;

    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert!(
        !body.contains("ovzdusie") && !body.contains(CLOSED),
        "no space, no slug, no project in the refusal: {body}",
    );
}

/// PF-45: a bearer credential that does not verify is refused, every spelling of it. None of
/// these is quietly downgraded to "nobody is calling", which on a public endpoint would serve
/// the caller whose token expired an hour ago as the anonymous public.
#[tokio::test]
async fn no_bearer_credential_that_fails_to_verify_is_served() {
    for header in [
        "Bearer not.a.token",
        "Bearer a.b",
        "Bearer a.b.c.d",
        "Bearer eyJhbGciOiJub25lIn0.e30.",
        "Bearer eyJhbGciOiJub25lIn0.e30.c2ln",
        "Bearer ....",
        "Bearer  padded.token.here",
    ] {
        let (status, _, body) = call(
            Request::builder()
                .uri(format!(
                    "/api/endpoint/{OPEN}/ngsi-ld/v1/entities?type=AirQualityObserved"
                ))
                .header(axum::http::header::AUTHORIZATION, header)
                .body(Body::empty())
                .expect("a request"),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::UNAUTHORIZED,
            "{header:?} was served instead of refused: {body}",
        );
    }
}

/// PF-45: a header that carries no bearer credential at all is the anonymous caller, which a
/// public endpoint admits (GW22). Written down because it is the one path where presenting
/// *something* still gets served, and the list is exactly what falls into it.
///
/// `auth::token::bearer` matches the scheme as the literal `Bearer `, so the lowercase
/// spelling RFC 7235 section 2.1 also allows falls in here with the rest: a client that writes
/// `bearer <token>` is served as the public instead of as itself. That is a downgrade rather
/// than an escalation — the anonymous role is what a stranger with no header gets — so it is
/// recorded as a defect and not as a hole (chyby.md).
#[tokio::test]
async fn a_header_that_is_not_a_bearer_credential_is_no_credential() {
    for header in [
        "",
        " ",
        "Bearer",
        "Bearer ",
        "Bearer   ",
        "bearer abc.def.ghi",
        "BEARER abc.def.ghi",
        "Basic YWRtaW46YWRtaW4=",
        "Negotiate abcdef",
        // A value that is not ASCII at all: `to_str` refuses it, so nothing is presented.
        "Bearer 你好.世界.签名",
    ] {
        let (status, _, body) = call(
            Request::builder()
                .uri(format!(
                    "/api/endpoint/{OPEN}/ngsi-ld/v1/entities?type=AirQualityObserved"
                ))
                .header(axum::http::header::AUTHORIZATION, header)
                .body(Body::empty())
                .expect("a request"),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::BAD_GATEWAY,
            "{header:?} is the anonymous caller, served until the broker is reached: {body}",
        );
    }

    // And the same header on an endpoint the anonymous caller may not have is 401, not a
    // seat at somebody else's table.
    let (closed, _, _) = call(
        Request::builder()
            .uri(format!(
                "/api/endpoint/{CLOSED}/ngsi-ld/v1/entities?type=AirQualityObserved"
            ))
            .header(axum::http::header::AUTHORIZATION, "bearer abc.def.ghi")
            .body(Body::empty())
            .expect("a request"),
    )
    .await;
    assert_eq!(closed, StatusCode::UNAUTHORIZED);
}

/// EP-05: the representation check is on the endpoint, not on the path, so no spelling of the
/// tree gets around it. Every one of these is the MCP-only endpoint, and every one is 404.
#[tokio::test]
async fn no_path_under_the_tree_gets_around_the_representation_check() {
    for rest in [
        "entities?type=AirQualityObserved",
        "entities/urn:ngsi-ld:AirQualityObserved:bb:ovzdusie:1",
        "entityOperations/query",
        "types",
        "attributes",
        "subscriptions",
        "temporal/entities?type=AirQualityObserved",
        "../../../mcp",
        "entities/../../types",
    ] {
        let (status, _, _) =
            call(get(&format!("/api/endpoint/{MCP_ONLY}/ngsi-ld/v1/{rest}"))).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{rest:?} was answered");
    }
}

/// EP-03: the two surfaces are two namespaces. A space name typed where a slug goes does not
/// resolve, and a slug typed where a space name goes does not either, so neither table can be
/// enumerated through the other (SP-01).
#[tokio::test]
async fn a_space_name_is_not_a_slug_and_a_slug_is_not_a_space_name() {
    let (by_space_name, _, _) = call(get(
        "/api/endpoint/ovzdusie/ngsi-ld/v1/entities?type=AirQualityObserved",
    ))
    .await;
    assert_eq!(by_space_name, StatusCode::NOT_FOUND);

    let (by_slug, _, _) = call(get(&format!(
        "/cs/{OPEN}/ngsi-ld/v1/entities?type=AirQualityObserved"
    )))
    .await;
    assert_eq!(by_slug, StatusCode::NOT_FOUND);
}

/// R20: the refusal of an unknown slug carries nothing about the deployment — no broker
/// address, no tenant, no realm, no file path. The whole body is read as text and searched,
/// because a member a test forgets to name is exactly where such a thing survives.
#[tokio::test]
async fn a_refusal_names_nothing_of_the_deployment() {
    let (_, _, body) = call(get(
        "/api/endpoint/does-not-exist/ngsi-ld/v1/entities?type=AirQualityObserved",
    ))
    .await;

    for secret in [
        "127.0.0.1",
        "ovzdusie",
        "banskabystrica.sk",
        "realms/joinedcontext",
        "/api/endpoint",
        "src/",
    ] {
        assert!(!body.contains(secret), "{secret:?} leaked into {body}");
    }
}

/// EP-21: a `Host` or a forwarding header the client wrote does not become the base of
/// anything. The deployment's own public URL is the only one the surface answers with, so a
/// caller cannot make the gateway mint a URL pointing at a host of their choosing.
#[tokio::test]
async fn a_client_supplied_host_does_not_reach_the_answer() {
    let (_, _, body) = call(
        Request::builder()
            .uri(format!(
                "/api/endpoint/{CLOSED}/ngsi-ld/v1/entities?type=AirQualityObserved"
            ))
            .header("X-Forwarded-Host", "evil.example")
            .header("X-Forwarded-Proto", "http")
            .header("Forwarded", "host=evil.example")
            .body(Body::empty())
            .expect("a request"),
    )
    .await;

    assert!(!body.contains("evil.example"), "{body}");
}

/// The same request twice, at the same time, on the surface that refuses it: two identical
/// refusals and no state between them. The handler keeps nothing per caller, so this is the
/// line that would go red if it ever started to.
#[tokio::test]
async fn the_same_refused_request_twice_at_once_is_refused_twice() {
    let path = format!("/api/endpoint/{CLOSED}/ngsi-ld/v1/entities?type=AirQualityObserved");
    let (first, second) = tokio::join!(call(get(&path)), call(get(&path)));

    assert_eq!(first.0, StatusCode::UNAUTHORIZED);
    assert_eq!(first, second);
}
