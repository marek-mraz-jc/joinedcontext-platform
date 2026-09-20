//! Edge cases of `app::endpoint_record` (T-1881, EP-26, MP-02).
//!
//! Contract, in one sentence: `GET /api/endpoint/{slug}` — and the same URL with the trailing
//! slash, because EP-01 writes it one way and every client that stores a base URL drops it —
//! is the DCAT-AP record of one endpoint, in JSON-LD, Turtle or HTML as the `Accept` header
//! asks, built on the schema catalogue this caller may read (T-0337, T-0338, EP-27, EP-68).
//!
//! The inputs are the slug, the `Authorization` header and `Accept`. What must not leak is
//! anything of the model the caller has no grant over: the record names its own
//! distributions, and a class or a digest of a class the caller may not read would tell them
//! what the space holds. What must not happen is a caller's own input reaching the HTML
//! page — `admit` resolves the slug before the page is built, so what is rendered is the
//! record's own name and never the string that was typed.

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

/// A public endpoint whose model has one class the public grant names and one it does not.
const OPEN: &str = "k4y7pq2mzt6vhx3nbwrs5cjd8f";
/// A public endpoint whose manifest title is markup, which is what a project member can put
/// into one: the HTML page is the only representation that would render it.
const MARKUP: &str = "t7v2qn9khz4mxc6bwdr8sj3fpy";
/// The same, needing a token.
const CLOSED: &str = "p9d2wc5kzn8mth4rqvb7xj3sfy";
const HOST: &str = "https://bb.example.sk";

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

fn endpoint(slug: &str, audience: Audience) -> Endpoint {
    Endpoint {
        slug: slug.to_owned(),
        title: Default::default(),
        description: Default::default(),
        space: "ovzdusie".to_owned(),
        project: "ovzdusie".to_owned(),
        audience,
        allowed_projects: Vec::new(),
        representations: vec![Representation::NgsiLd, Representation::Mcp],
        rate_limit: None,
        file_limits: None,
        hidden_attributes: Default::default(),
        projection: None,
        base_path: format!("/api/endpoint/{slug}"),
        models: vec![air_quality()],
        view_mapping: None,
        policies: vec![policy(
            r#"contextSpaceRef: ovzdusie
assigner: did:web:banskabystrica.sk
assignee: { kind: role, id: public }
operations: [queryEntity, retrieveEntity]
information:
  - entities:
      - type: AirQualityObserved
    propertyNames: [pm10]
"#,
        )],
    }
}

/// The same endpoint with a title and a description a project member wrote as markup.
fn markup_titled() -> Endpoint {
    let mut title = std::collections::BTreeMap::new();
    title.insert(
        "en".to_owned(),
        "<script>alert(1)</script><img src=x onerror=alert(2)>".to_owned(),
    );
    let mut description = std::collections::BTreeMap::new();
    description.insert(
        "en".to_owned(),
        "</p><a href=\"javascript:alert(3)\">x</a>".to_owned(),
    );
    Endpoint {
        title,
        description,
        ..endpoint(MARKUP, Audience::Public)
    }
}

fn app_of(realm: &common::Realm) -> axum::Router {
    router(Arc::new(
        Gateway::new(
            Broker::new("http://127.0.0.1:1"),
            Box::new(PolicyPdp),
            "banskabystrica.sk",
        )
        .serve([
            endpoint(OPEN, Audience::Public),
            endpoint(CLOSED, Audience::Organization),
            markup_titled(),
        ])
        .authenticate(
            Arc::new(realm.verifier()),
            ServiceAccounts::new(),
            Some(HOST.to_owned()),
        ),
    ))
}

fn app() -> axum::Router {
    app_of(&common::Realm::new())
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

async fn call(request: Request<Body>) -> (StatusCode, String, String) {
    call_on(app(), request).await
}

fn record(path: &str, accept: Option<&str>) -> Request<Body> {
    let mut builder = Request::builder().uri(path);
    if let Some(accept) = accept {
        builder = builder.header(axum::http::header::ACCEPT, accept);
    }
    builder.body(Body::empty()).expect("a request")
}

/// EP-68, EP-47: the first case, and the one most likely to be red. The record is built on
/// `schema::visible`, so a class this caller holds no grant over is in none of the three
/// representations — not as a distribution, not as a link, not as a digest.
#[tokio::test]
async fn no_representation_of_the_record_names_a_class_the_caller_may_not_read() {
    for accept in [None, Some("text/turtle"), Some("text/html")] {
        let (status, _, body) = call(record(&format!("/api/endpoint/{OPEN}"), accept)).await;
        assert_eq!(status, StatusCode::OK, "{accept:?} answered {body}");

        for secret in ["InternalIncident", "internalNote", "severity"] {
            assert!(
                !body.contains(secret),
                "{accept:?} leaked {secret:?}: {body}",
            );
        }
        // The record is not empty either, so the case is not passing on nothing.
        assert!(body.contains(OPEN), "{accept:?} named no endpoint: {body}");
    }
}

/// EP-01: the two spellings of the URL are the same record. A client that stored the base URL
/// without the slash must not get a different document from one that kept it.
#[tokio::test]
async fn both_spellings_of_the_url_are_the_same_record() {
    let bare = call(record(&format!("/api/endpoint/{OPEN}"), None)).await;
    let slashed = call(record(&format!("/api/endpoint/{OPEN}/"), None)).await;

    assert_eq!(bare.0, StatusCode::OK);
    assert_eq!(bare, slashed);
}

/// EP-27: each of the three representations is served and labelled as itself, so a catalogue
/// harvester, an RDF store and a browser each get something they can read.
#[tokio::test]
async fn each_representation_is_served_and_labelled_as_itself() {
    for (accept, expected) in [
        (None, "application/ld+json"),
        (Some("application/ld+json"), "application/ld+json"),
        (Some("application/json"), "application/ld+json"),
        (Some("text/turtle"), "text/turtle"),
        (Some("text/html"), "text/html"),
        (Some("application/xhtml+xml"), "text/html"),
        // A browser's real header, which names HTML first.
        (
            Some("text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8"),
            "text/html",
        ),
    ] {
        let (status, media, body) = call(record(&format!("/api/endpoint/{OPEN}"), accept)).await;
        assert_eq!(status, StatusCode::OK, "{accept:?}");
        assert!(
            media.starts_with(expected),
            "{accept:?} answered {media}: {body:.200}",
        );
    }
}

/// EP-27: an `Accept` this surface cannot serve is the JSON-LD record rather than a 406, the
/// same rule the access surface follows — a record nobody can read is worse than one in a
/// format they did not ask for.
#[tokio::test]
async fn an_accept_header_this_surface_cannot_serve_is_still_answered() {
    for accept in [
        "",
        " ",
        "*/*",
        "application/pdf",
        "application/octet-stream",
        ",",
        ";;;",
        "text/turtle_not_really",
        &"text/".to_owned().repeat(500),
    ] {
        let (status, media, _) = call(record(&format!("/api/endpoint/{OPEN}"), Some(accept))).await;
        assert_eq!(status, StatusCode::OK, "{accept:.30?}");
        assert!(media.starts_with("application/ld+json"), "{accept:.30?}");
    }
}

/// EP-27: the HTML page is a page, and every value it interpolates is escaped. The slug the
/// caller typed never reaches it at all — `admit` resolved it first, so what is rendered is
/// the endpoint's own record.
#[tokio::test]
async fn the_html_page_carries_no_unescaped_markup() {
    let (status, media, body) =
        call(record(&format!("/api/endpoint/{OPEN}"), Some("text/html"))).await;

    assert_eq!(status, StatusCode::OK);
    assert!(media.starts_with("text/html"), "{media}");
    assert!(body.starts_with("<!doctype html>"), "{body:.80}");

    // The same page for an endpoint whose manifest title and description are markup. A
    // project member writes those, and this is the one representation that would render
    // them: every one of these has to come back escaped.
    let (status, _, hostile) = call(record(
        &format!("/api/endpoint/{MARKUP}"),
        Some("text/html"),
    ))
    .await;
    assert_eq!(status, StatusCode::OK);
    // Nothing of either string opens a tag: every `<` of theirs comes back as `&lt;`, so an
    // attribute like `onerror=` is left sitting in text where a browser reads it as words.
    for forbidden in ["<script", "<img", "<a href=\"javascript:", "</p><a"] {
        assert!(!hostile.contains(forbidden), "{forbidden:?} in {hostile}");
    }
    assert!(
        !hostile.contains("href=\"javascript:"),
        "no link of theirs survives as a link: {hostile}",
    );
    assert!(
        hostile.contains("&lt;script&gt;"),
        "the title is still shown, as text: {hostile}",
    );
}

/// EP-03, PF-46: an unknown slug is 404 and an endpoint that needs a token is 401, and
/// neither answer carries a word of the record that was asked for.
#[tokio::test]
async fn an_endpoint_the_caller_may_not_have_produces_no_record() {
    for accept in [None, Some("text/turtle"), Some("text/html")] {
        let (absent, _, absent_body) =
            call(record("/api/endpoint/zzzzzzzzzzzzzzzzzzzzzzzzzz", accept)).await;
        assert_eq!(absent, StatusCode::NOT_FOUND, "{accept:?}");

        let (closed, _, closed_body) =
            call(record(&format!("/api/endpoint/{CLOSED}"), accept)).await;
        assert_eq!(closed, StatusCode::UNAUTHORIZED, "{accept:?}");

        for body in [&absent_body, &closed_body] {
            for secret in ["ovzdusie", "AirQualityObserved", "dcat:Dataset"] {
                assert!(!body.contains(secret), "{secret:?} leaked into {body}");
            }
        }
    }
}

/// R20: the record names the endpoint, its space and the URLs a client follows — and nothing
/// of the deployment behind it: no broker, no realm, no file path.
#[tokio::test]
async fn the_record_names_nothing_of_the_deployment() {
    for accept in [None, Some("text/turtle"), Some("text/html")] {
        let (_, _, body) = call(record(&format!("/api/endpoint/{OPEN}"), accept)).await;
        for secret in ["127.0.0.1", "realms/joinedcontext", "crates/", "src/"] {
            assert!(
                !body.contains(secret),
                "{accept:?} leaked {secret:?}: {body}"
            );
        }
    }
}

/// EP-27: the URLs in the record are built from the deployment's own public URL, never from a
/// header the caller wrote, so a harvester cannot be pointed at a host of somebody's choosing.
#[tokio::test]
async fn a_client_supplied_host_does_not_become_the_records_base() {
    for accept in [None, Some("text/turtle"), Some("text/html")] {
        let mut builder = Request::builder()
            .uri(format!("/api/endpoint/{OPEN}"))
            .header("Host", "evil.example")
            .header("X-Forwarded-Host", "evil.example")
            .header("Forwarded", "host=evil.example");
        if let Some(accept) = accept {
            builder = builder.header(axum::http::header::ACCEPT, accept);
        }
        let (_, _, body) = call(builder.body(Body::empty()).expect("a request")).await;
        assert!(!body.contains("evil.example"), "{accept:?}: {body}");
        assert!(body.contains(HOST), "{accept:?}: {body}");
    }
}

/// GW20: the tenancy headers are stripped first, so a forged one cannot make the record
/// describe another space.
#[tokio::test]
async fn a_forged_tenant_header_does_not_change_the_record() {
    let honest = call(record(&format!("/api/endpoint/{OPEN}"), None)).await;
    let forged = call(
        Request::builder()
            .uri(format!("/api/endpoint/{OPEN}"))
            .header("NGSILD-Tenant", "somebody-elses-space")
            .header("X-Forwarded-User", "spravca")
            .body(Body::empty())
            .expect("a request"),
    )
    .await;

    assert_eq!(honest.0, StatusCode::OK);
    assert_eq!(forged, honest);
}

/// EP-68: the record is the caller's own. A token that carries a grant over the second class
/// sees it; the anonymous caller does not, from the same code.
#[tokio::test]
async fn the_record_widens_with_the_callers_grants() {
    let realm = common::Realm::new();
    let member = realm.mint(&json!({
        "iss": common::ISSUER,
        "sub": "an-employee",
        "aud": CLOSED,
        "preferred_username": "jana",
        "groups": ["/ovzdusie"],
        "exp": common::in_seconds(300),
        "iat": common::in_seconds(-10),
    }));

    let (status, _, body) = call_on(
        app_of(&realm),
        Request::builder()
            .uri(format!("/api/endpoint/{CLOSED}"))
            .header(
                axum::http::header::AUTHORIZATION,
                format!("Bearer {member}"),
            )
            .body(Body::empty())
            .expect("a request"),
    )
    .await;

    assert_eq!(status, StatusCode::OK, "{body}");
    let document: Value = serde_json::from_str(&body).expect("a JSON-LD record");
    assert_eq!(document["@type"], json!("dcat:Dataset"), "{body}");
    // The token carries no role, so the public grant is still all it holds, and the class
    // nobody granted is still absent: a login never widens by itself (GW22).
    assert!(!body.contains("InternalIncident"), "{body}");
}

/// The record is read-only. Nothing but `GET` reaches either spelling of its URL.
#[tokio::test]
async fn nothing_but_get_reaches_the_record() {
    for path in [
        format!("/api/endpoint/{OPEN}"),
        format!("/api/endpoint/{OPEN}/"),
    ] {
        for method in ["POST", "PUT", "PATCH", "DELETE"] {
            let response = app()
                .oneshot(
                    Request::builder()
                        .method(method)
                        .uri(&path)
                        .body(Body::from("{}"))
                        .expect("a request"),
                )
                .await
                .expect("the gateway answers");
            assert_eq!(
                response.status(),
                StatusCode::METHOD_NOT_ALLOWED,
                "{method} {path}",
            );
        }
    }
}
