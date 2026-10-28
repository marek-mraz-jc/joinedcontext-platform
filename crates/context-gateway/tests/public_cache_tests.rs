//! Which answers a shared cache may keep (T-5900, R9).
//!
//! The edge may keep an anonymous read of data for `JC_GATEWAY_PUBLIC_MAX_AGE` seconds: without a
//! token every caller is the Endpoint's one anonymous subject, so the answer is the same for all
//! of them. Everything else stays `private, no-store`: a caller with a token, a cookie, a write, a
//! refusal, a document that is not data, and every answer while the deployment sets no age.

mod common;

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use axum::routing::any;
use axum::Router;
use context_gateway::app::{router, Gateway};
use context_gateway::auth::accounts::ServiceAccounts;
use context_gateway::pdp::PolicyPdp;
use context_gateway::proxy::Broker;
use context_gateway::resolver::Endpoint;
use jc_core::kinds::{Audience, PolicySpec, Representation};
use serde_json::{json, Value};
use std::sync::Arc;
use tower::ServiceExt;

const SLUG: &str = "k4y7pq2mzt6vhx3nbwrs5cjd8f";
const DOMAIN: &str = "bb.sk";

fn station() -> Value {
    json!({
        "id": "urn:ngsi-ld:AirQualityObserved:bb.sk:ovzdusie:station-01",
        "type": "AirQualityObserved",
        "pm10": { "type": "Property", "value": 34.2 },
    })
}

async fn broker() -> String {
    let app = Router::new().fallback(any(|uri: axum::http::Uri| async move {
        if uri.path() == "/access" {
            axum::Json(json!({ "subject": { "type": "role", "id": "public" },
                               "resource": { "type": "view", "id": SLUG },
                               "permissions": [], "prohibitions": [] }))
        } else {
            axum::Json(json!([station()]))
        }
    }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("a free port");
    let address = listener.local_addr().expect("an address");
    tokio::spawn(async move {
        let _ = axum::serve(listener, common::plain_tree(app)).await;
    });
    format!("http://{address}")
}

fn endpoint() -> Endpoint {
    let policy: PolicySpec = serde_norway::from_str(
        "contextSpaceRef: ovzdusie\n\
         assigner: did:web:bb.sk\n\
         assignee: { kind: role, id: public }\n\
         operations: [queryEntity, retrieveEntity]\n\
         information:\n  - entities:\n      - type: AirQualityObserved\n",
    )
    .expect("the policy spec parses");
    Endpoint {
        declared_types: None,
        roles: Default::default(),
        slug: SLUG.to_owned(),
        title: Default::default(),
        description: Default::default(),
        space: "ovzdusie".to_owned(),
        project: "ovzdusie".to_owned(),
        audience: Audience::Public,
        allowed_projects: Vec::new(),
        representations: vec![Representation::NgsiLd],
        rate_limit: None,
        creates: None,
        file_limits: None,
        hidden_attributes: Default::default(),
        projection: None,
        view_mapping: None,
        catalog: None,
        deprecation: None,
        feeds: Vec::new(),
        calendars: Vec::new(),
        map_defaults: Default::default(),
        base_path: format!("/api/endpoint/{SLUG}"),
        models: Vec::new(),
        policy_names: Vec::new(),
        manifest_name: String::new(),
        policies: vec![policy],
    }
}

async fn app(realm: &common::Realm, max_age: u32) -> Router {
    let mut gateway = Gateway::new(Broker::new(broker().await), Box::new(PolicyPdp), DOMAIN)
        .serve([endpoint()])
        .authenticate(Arc::new(realm.verifier()), ServiceAccounts::new(), None);
    gateway.public_max_age = max_age;
    router(Arc::new(gateway))
}

fn entities() -> String {
    format!("/api/endpoint/{SLUG}/ngsi-ld/v1/entities?type=AirQualityObserved")
}

/// Status, `Cache-Control` and `Vary` of one request.
async fn sent(app: Router, request: Request<Body>) -> (StatusCode, String, String) {
    let response = app.oneshot(request).await.expect("the gateway answers");
    let named = |name| {
        response
            .headers()
            .get(name)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_owned()
    };
    let (cache, vary) = (named(header::CACHE_CONTROL), named(header::VARY));
    (response.status(), cache, vary)
}

fn get(uri: &str) -> axum::http::request::Builder {
    Request::builder().uri(uri)
}

#[tokio::test]
async fn an_anonymous_read_of_data_may_be_kept_as_long_as_the_deployment_says() {
    let realm = common::Realm::new();
    let (status, cache, vary) = sent(
        app(&realm, 5).await,
        get(&entities()).body(Body::empty()).expect("a request"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(cache, "public, max-age=5");
    // A cache that honours `Vary` still keeps a caller with a token apart.
    for named in [
        "Authorization",
        "Accept",
        "Accept-Language",
        "Accept-Encoding",
        "Link",
        "Prefer",
    ] {
        assert!(vary.contains(named), "Vary does not name {named}: {vary}");
    }
}

#[tokio::test]
async fn no_age_set_keeps_every_answer_private() {
    let realm = common::Realm::new();
    let (status, cache, _) = sent(
        app(&realm, 0).await,
        get(&entities()).body(Body::empty()).expect("a request"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(cache, "private, no-store");
}

#[tokio::test]
async fn a_read_with_a_token_stays_private() {
    // The answer is this person's grants intersected with the URL: a shared cache must never
    // replay it to anyone else, the anonymous caller included.
    let realm = common::Realm::new();
    let token = realm.mint(&json!({
        "iss": common::ISSUER,
        "sub": "a-reader",
        "aud": SLUG,
        "preferred_username": "jana",
        "exp": common::in_seconds(300),
        "iat": common::in_seconds(-10),
    }));
    let (status, cache, _) = sent(
        app(&realm, 5).await,
        get(&entities())
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .body(Body::empty())
            .expect("a request"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(cache, "private, no-store");
}

#[tokio::test]
async fn a_read_with_a_cookie_stays_private() {
    let realm = common::Realm::new();
    let (status, cache, _) = sent(
        app(&realm, 5).await,
        get(&entities())
            .header(header::COOKIE, "session=abc")
            .body(Body::empty())
            .expect("a request"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(cache, "private, no-store");
}

#[tokio::test]
async fn a_write_a_refusal_and_a_document_stay_private() {
    let realm = common::Realm::new();
    let post = Request::builder()
        .method("POST")
        .uri(format!("/api/endpoint/{SLUG}/ngsi-ld/v1/entities"))
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(station().to_string()))
        .expect("a request");
    // Whatever the write answered, a POST is never kept.
    let (status, cache, _) = sent(app(&realm, 5).await, post).await;
    assert_eq!(cache, "private, no-store", "a write answered {status}");

    let unknown = get("/api/endpoint/zzzzzzzzzzzzzzzzzzzzzzzzzz/ngsi-ld/v1/entities?type=X")
        .body(Body::empty())
        .expect("a request");
    let (status, cache, _) = sent(app(&realm, 5).await, unknown).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(cache, "private, no-store");

    let record = get(&format!("/api/endpoint/{SLUG}"))
        .body(Body::empty())
        .expect("a request");
    let (_, cache, _) = sent(app(&realm, 5).await, record).await;
    assert_eq!(
        cache, "private, no-store",
        "the Endpoint record is not data"
    );
}

#[tokio::test]
async fn a_read_with_a_header_that_shapes_the_answer_stays_private() {
    // T-5900 review: `Link` names the @context a compacted answer is written in, so a shared
    // cache keyed on the URL would replay it to a plain read. Any header the gateway does not know
    // to be harmless keeps the answer private.
    let realm = common::Realm::new();
    for (name, value) in [
        ("link", "<https://example.org/ctx.jsonld>; rel=\"http://www.w3.org/ns/json-ld#context\"; type=\"application/ld+json\""),
        ("prefer", "ngsi-ld=1.9"),
        ("ngsild-tenant", "other"),
        ("x-anything", "1"),
    ] {
        let (status, cache, _) = sent(
            app(&realm, 5).await,
            get(&entities())
                .header(name, value)
                .body(Body::empty())
                .expect("a request"),
        )
        .await;
        assert_eq!(cache, "private, no-store", "{name} answered {status}");
    }
    // The headers in Vary, the edge's forwarding and the mesh's own keep it shareable.
    let (status, cache, _) = sent(
        app(&realm, 5).await,
        get(&entities())
            .header("accept", "application/geo+json")
            .header("accept-language", "sk")
            .header("x-forwarded-for", "203.0.113.7")
            .header(
                "l5d-client-id",
                "apisix.apisix.serviceaccount.identity.linkerd.cluster.local",
            )
            .body(Body::empty())
            .expect("a request"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(cache, "public, max-age=5");
}
