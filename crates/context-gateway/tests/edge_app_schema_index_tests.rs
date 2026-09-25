//! Edge cases of `app::schema_index` (T-1882, EP-26, MP-02).
//!
//! Contract, in one sentence: `GET /api/endpoint/{slug}/schema/index.json` is the catalogue of
//! what this endpoint publishes about its data, narrowed by `schema::visible` to what **this**
//! caller may read, and served with the strong `ETag` of exactly the bytes that were sent
//! (T-0162, EP-46, EP-51).
//!
//! The inputs are the slug, the `Authorization` header and `If-None-Match`. What must not leak
//! is a class or a slot the caller holds no grant over — the index is a description of the
//! data, so a name in it is the same disclosure the data would have been. What must not go
//! wrong is the revalidation: the document is a projection of the policy set and therefore
//! never immutable, so the `ETag` has to be per caller and per grant, and a 304 must only
//! ever mean "the bytes you hold are the bytes I would send *you*".

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use context_gateway::app::{router, Gateway};
use context_gateway::auth::accounts::ServiceAccounts;
use context_gateway::pdp::PolicyPdp;
use context_gateway::proxy::Broker;
use context_gateway::resolver::{Endpoint, Model};
use jc_core::kinds::{Audience, PolicySpec, Representation};
use serde_json::{json, Value};
use std::sync::Arc;
use tower::ServiceExt;

/// A public endpoint: the anonymous caller reads one class and one of its slots.
const OPEN: &str = "k4y7pq2mzt6vhx3nbwrs5cjd8f";
/// The same endpoint, needing a token, so a signed-in caller's index can be compared.
const CLOSED: &str = "p9d2wc5kzn8mth4rqvb7xj3sfy";

fn policy(yaml: &str) -> PolicySpec {
    serde_norway::from_str(yaml).expect("the policy spec parses")
}

fn air_quality() -> Model {
    Model {
        name: "bb-air-quality".to_owned(),
        version: "1.4.0".to_owned(),
        major: 1,
        classes: vec![
            "AirQualityObserved".to_owned(),
            "InternalIncident".to_owned(),
        ],
        json_schema: Some(json!({
            "$schema": "http://json-schema.org/draft-07/schema#",
            "$defs": {
                "AirQualityObserved": {
                    "type": "object",
                    "properties": {
                        "id": { "type": "string" },
                        "type": { "const": "AirQualityObserved" },
                        "pm10": { "type": "number" },
                        "internalNote": { "type": "string" },
                    },
                },
                "InternalIncident": {
                    "type": "object",
                    "properties": { "severity": { "type": "string" } },
                },
            },
        })),
        context: Some(json!({
            "@context": {
                "AirQualityObserved": "https://bb.example.sk/schema/air-quality/AirQualityObserved",
                "InternalIncident": "https://bb.example.sk/schema/air-quality/InternalIncident",
                "pm10": "https://bb.example.sk/schema/air-quality/pm10",
                "internalNote": "https://bb.example.sk/schema/air-quality/internalNote",
            }
        })),
    }
}

fn endpoint(slug: &str, audience: Audience, policies: Vec<PolicySpec>) -> Endpoint {
    Endpoint {
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
        file_limits: None,
        hidden_attributes: Default::default(),
        projection: None,
        base_path: format!("/api/endpoint/{slug}"),
        models: vec![air_quality()],
        view_mapping: None,
        catalog: None,
        policies,
    }
}

fn narrow_grant() -> Vec<PolicySpec> {
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

/// The same endpoint under a grant that names both classes, so the two indexes can be
/// compared: what one caller sees and the other does not is exactly the narrowing.
fn wide_grant() -> Vec<PolicySpec> {
    vec![policy(
        r#"contextSpaceRef: ovzdusie
assigner: did:web:banskabystrica.sk
assignee: { kind: role, id: public }
operations: [queryEntity, retrieveEntity]
information:
  - entities:
      - type: AirQualityObserved
      - type: InternalIncident
"#,
    )]
}

/// A second public endpoint over the same model with the wider grant.
const WIDE: &str = "w8k3zq6nxv2htb5rjs9cyd4gpm";

fn app_of(realm: &common::Realm) -> axum::Router {
    router(Arc::new(
        Gateway::new(
            Broker::new("http://127.0.0.1:1"),
            Box::new(PolicyPdp),
            "banskabystrica.sk",
        )
        .serve([
            endpoint(OPEN, Audience::Public, narrow_grant()),
            endpoint(WIDE, Audience::Public, wide_grant()),
            endpoint(CLOSED, Audience::Organization, narrow_grant()),
        ])
        .authenticate(
            Arc::new(realm.verifier()),
            ServiceAccounts::new(),
            Some("https://bb.example.sk".to_owned()),
        ),
    ))
}

fn app() -> axum::Router {
    app_of(&common::Realm::new())
}

/// The status, the `ETag`, the `Cache-Control` and the body.
async fn call(request: Request<Body>) -> (StatusCode, String, String, String) {
    let response = app().oneshot(request).await.expect("the gateway answers");
    let status = response.status();
    let header = |name: axum::http::HeaderName| {
        response
            .headers()
            .get(name)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_owned()
    };
    let etag = header(axum::http::header::ETAG);
    let cache = header(axum::http::header::CACHE_CONTROL);
    let body = axum::body::to_bytes(response.into_body(), 256 * 1024)
        .await
        .expect("a readable body");
    (
        status,
        etag,
        cache,
        String::from_utf8_lossy(&body).into_owned(),
    )
}

fn index(slug: &str) -> Request<Body> {
    Request::builder()
        .uri(format!("/api/endpoint/{slug}/schema/index.json"))
        .body(Body::empty())
        .expect("a request")
}

fn index_if_none_match(slug: &str, presented: &str) -> Request<Body> {
    Request::builder()
        .uri(format!("/api/endpoint/{slug}/schema/index.json"))
        .header(axum::http::header::IF_NONE_MATCH, presented)
        .body(Body::empty())
        .expect("a request")
}

/// EP-46, EP-47: the first case, and the one most likely to be red. The index describes only
/// what the caller may read — the second class of the model and the slot nobody granted are in
/// none of it, not as a name and not as a digest.
#[tokio::test]
async fn the_index_names_nothing_the_caller_may_not_read() {
    let (status, _, _, body) = call(index(OPEN)).await;

    assert_eq!(status, StatusCode::OK);
    for secret in ["InternalIncident", "internalNote", "severity"] {
        assert!(!body.contains(secret), "{secret:?} leaked into {body}");
    }
    // And it is not an empty document: the class that was granted is described.
    let document: Value = serde_json::from_str(&body).expect("a JSON document");
    assert!(
        body.contains("bb-air-quality"),
        "the model it does serve is named: {body}",
    );
    assert!(document.is_object(), "{body}");
}

/// EP-51: the `ETag` is the digest of the bytes that were sent, so two callers with two
/// grants get two different tags. A shared tag would let one caller's 304 stand for the
/// other's document.
#[tokio::test]
async fn two_grants_are_two_documents_with_two_etags() {
    let narrow = call(index(OPEN)).await;
    let wide = call(index(WIDE)).await;

    assert_eq!(narrow.0, StatusCode::OK);
    assert_eq!(wide.0, StatusCode::OK);
    assert!(!narrow.1.is_empty(), "an index carries an ETag");
    assert_ne!(narrow.1, wide.1, "two projections, two tags");
    assert!(wide.3.contains("InternalIncident"), "{}", wide.3);
    assert!(!narrow.3.contains("InternalIncident"), "{}", narrow.3);
}

/// EP-51: the tag a caller was given brings back a 304 with no body, and the 304 still names
/// the tag, so the client keeps revalidating against the right one.
#[tokio::test]
async fn the_tag_that_was_given_revalidates() {
    let (_, etag, _, _) = call(index(OPEN)).await;
    let (status, returned, cache, body) = call(index_if_none_match(OPEN, &etag)).await;

    assert_eq!(status, StatusCode::NOT_MODIFIED);
    assert_eq!(returned, etag);
    // `no-cache` because the document is a projection of the policy set and therefore never
    // immutable; `private` from the response layer, because it is one caller's projection and
    // a shared cache holding it would serve it to the next caller (R22).
    assert_eq!(cache, "private, no-cache");
    assert!(body.is_empty(), "a 304 carries no body: {body}");
}

/// RFC 9110 13.1.2: the weak form of the caller's own tag revalidates, a list containing it
/// revalidates, and `*` revalidates. Nothing else does — in particular not another caller's
/// tag, which is the case that would hand one grant's document to another grant.
#[tokio::test]
async fn only_a_tag_that_names_this_document_revalidates() {
    let (_, mine, _, _) = call(index(OPEN)).await;
    let (_, somebody_elses, _, _) = call(index(WIDE)).await;

    for presented in [
        mine.clone(),
        format!("W/{mine}"),
        format!("\"other\", {mine}"),
        format!("{mine}, \"other\""),
        "*".to_owned(),
    ] {
        let (status, _, _, _) = call(index_if_none_match(OPEN, &presented)).await;
        assert_eq!(status, StatusCode::NOT_MODIFIED, "{presented}");
    }

    for presented in [
        somebody_elses,
        "\"\"".to_owned(),
        String::new(),
        " ".to_owned(),
        "\"deadbeef\"".to_owned(),
        mine.trim_matches('"').to_owned(),
        format!("{}x\"", mine.trim_end_matches('"')),
        "**".to_owned(),
        "W/*".to_owned(),
    ] {
        let (status, _, _, _) = call(index_if_none_match(OPEN, &presented)).await;
        assert_eq!(
            status,
            StatusCode::OK,
            "{presented:?} revalidated something it does not name",
        );
    }
}

/// EP-51: the tag follows the document, so a caller holding one grant's tag never gets a 304
/// for another grant's index. This is the same property as above, read from the other end —
/// with the tag presented against the endpoint it came from, and against the one it did not.
#[tokio::test]
async fn one_grants_tag_never_revalidates_another_grants_index() {
    let (_, narrow_tag, _, _) = call(index(OPEN)).await;

    let (same, _, _, _) = call(index_if_none_match(OPEN, &narrow_tag)).await;
    assert_eq!(same, StatusCode::NOT_MODIFIED);

    let (other, _, _, body) = call(index_if_none_match(WIDE, &narrow_tag)).await;
    assert_eq!(other, StatusCode::OK, "the wider index is sent in full");
    assert!(body.contains("InternalIncident"), "{body}");
}

/// EP-03, PF-46: an unknown slug is 404 and an endpoint that needs a token is 401, and
/// neither of them carries a word of a catalogue.
#[tokio::test]
async fn an_endpoint_the_caller_may_not_have_publishes_no_index() {
    let (absent, absent_tag, _, absent_body) = call(index("zzzzzzzzzzzzzzzzzzzzzzzzzz")).await;
    assert_eq!(absent, StatusCode::NOT_FOUND);
    assert!(absent_tag.is_empty(), "a refusal carries no ETag");

    let (closed, _, _, closed_body) = call(index(CLOSED)).await;
    assert_eq!(closed, StatusCode::UNAUTHORIZED);

    for body in [&absent_body, &closed_body] {
        for secret in ["bb-air-quality", "AirQualityObserved", "pm10"] {
            assert!(!body.contains(secret), "{secret:?} leaked into {body}");
        }
    }
}

/// A refusal is not revalidated either: presenting a tag with an unknown slug does not turn
/// the 404 into a 304, which would tell the caller their cached copy is still current and
/// therefore that the endpoint is still there.
#[tokio::test]
async fn a_refusal_is_never_answered_as_not_modified() {
    let (_, etag, _, _) = call(index(OPEN)).await;

    for slug in ["zzzzzzzzzzzzzzzzzzzzzzzzzz", CLOSED] {
        for presented in [etag.as_str(), "*"] {
            let (status, _, _, _) = call(index_if_none_match(slug, presented)).await;
            assert!(
                status == StatusCode::NOT_FOUND || status == StatusCode::UNAUTHORIZED,
                "{slug} with {presented:?} answered {status}",
            );
        }
    }
}

/// EP-46: the index is JSON and says so, and nothing the caller writes in `Accept` changes
/// that — the route names its own format in the path.
#[tokio::test]
async fn the_accept_header_does_not_change_the_index() {
    let plain = call(index(OPEN)).await;
    for accept in ["text/turtle", "text/html", "application/xml", "*/*", ""] {
        let response = app()
            .oneshot(
                Request::builder()
                    .uri(format!("/api/endpoint/{OPEN}/schema/index.json"))
                    .header(axum::http::header::ACCEPT, accept)
                    .body(Body::empty())
                    .expect("a request"),
            )
            .await
            .expect("the gateway answers");
        assert_eq!(response.status(), StatusCode::OK, "{accept:?}");
        assert_eq!(
            response
                .headers()
                .get(axum::http::header::CONTENT_TYPE)
                .and_then(|value| value.to_str().ok()),
            Some("application/json"),
            "{accept:?}",
        );
        let body = axum::body::to_bytes(response.into_body(), 256 * 1024)
            .await
            .expect("a readable body");
        assert_eq!(String::from_utf8_lossy(&body), plain.3, "{accept:?}");
    }
}

/// R20: the index describes the model and nothing of the deployment behind it.
#[tokio::test]
async fn the_index_names_nothing_of_the_deployment() {
    let (_, _, _, body) = call(index(OPEN)).await;

    for secret in ["127.0.0.1", "realms/joinedcontext", "crates/", "src/"] {
        assert!(!body.contains(secret), "{secret:?} leaked into {body}");
    }
}

/// GW20: the tenancy headers are stripped first, so a forged one cannot make the catalogue
/// describe another space's model.
#[tokio::test]
async fn a_forged_tenant_header_does_not_change_the_index() {
    let honest = call(index(OPEN)).await;
    let forged = call(
        Request::builder()
            .uri(format!("/api/endpoint/{OPEN}/schema/index.json"))
            .header("NGSILD-Tenant", "somebody-elses-space")
            .header("X-Forwarded-Groups", "data-steward")
            .body(Body::empty())
            .expect("a request"),
    )
    .await;

    assert_eq!(honest.0, StatusCode::OK);
    assert_eq!(forged, honest);
}

/// The catalogue is read-only, and the path is exact: nothing but `GET` on this one spelling
/// reaches it.
#[tokio::test]
async fn nothing_but_a_get_on_the_exact_path_reaches_the_index() {
    for method in ["POST", "PUT", "PATCH", "DELETE"] {
        let response = app()
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri(format!("/api/endpoint/{OPEN}/schema/index.json"))
                    .body(Body::from("{}"))
                    .expect("a request"),
            )
            .await
            .expect("the gateway answers");
        assert_eq!(
            response.status(),
            StatusCode::METHOD_NOT_ALLOWED,
            "{method} reached the index",
        );
    }

    for path in [
        format!("/api/endpoint/{OPEN}/schema/index.json/"),
        format!("/api/endpoint/{OPEN}/schema/"),
        format!("/api/endpoint/{OPEN}/schema"),
        format!("/api/endpoint/{OPEN}/schema/index.JSON"),
    ] {
        let response = app()
            .oneshot(
                Request::builder()
                    .uri(&path)
                    .body(Body::empty())
                    .expect("a request"),
            )
            .await
            .expect("the gateway answers");
        assert_eq!(response.status(), StatusCode::NOT_FOUND, "{path} answered");
    }
}
