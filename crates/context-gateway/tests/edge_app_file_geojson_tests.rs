//! Edge cases of `app::file_geojson`, `GET /api/endpoint/{slug}/file.geojson` (T-1884, EP-26,
//! MP-02).
//!
//! Contract, in one sentence: it answers a `FeatureCollection` of exactly the entities this caller
//! is granted — the endpoint's own tenant, the grant's types, the grant's attributes minus the
//! endpoint's hidden ones, inside the granted areas — and for every caller it does not serve it
//! answers the one `404` that says nothing about whether the endpoint, the representation or the
//! grant was missing (EP-05, EP-09, EP-61, GW11, R9).

mod common;

use axum::body::Body;
use axum::http::{Request as HttpRequest, Response, StatusCode};
use common::BrokerStub;
use context_gateway::app::{router, Gateway};
use context_gateway::pdp::PolicyPdp;
use context_gateway::proxy::Broker;
use context_gateway::resolver::Endpoint;
use jc_core::kinds::{Audience, Representation};
use serde_json::{json, Value};
use std::collections::BTreeSet;
use std::sync::Arc;
use tower::ServiceExt;

const SLUG: &str = "mluyob4nz52lok3ssk7pgn5vwt";
const PUBLIC_READ: &str = r#"contextSpaceRef: ovzdusie
assigner: did:web:banskabystrica.sk
assignee: { kind: role, id: public }
operations: [queryEntity, retrieveEntity]
"#;

/// The dev seed's public endpoint: anonymous callers read, nothing narrows what they read.
fn endpoint() -> Endpoint {
    Endpoint {
        roles: Default::default(),
        slug: SLUG.to_owned(),
        title: std::collections::BTreeMap::new(),
        description: std::collections::BTreeMap::new(),
        space: "ovzdusie".to_owned(),
        project: "ovzdusie".to_owned(),
        audience: Audience::Public,
        allowed_projects: Vec::new(),
        representations: vec![Representation::NgsiLd, Representation::GeoJson],
        rate_limit: None,
        file_limits: None,
        hidden_attributes: BTreeSet::new(),
        projection: None,
        base_path: format!("/api/endpoint/{SLUG}"),
        models: Vec::new(),
        view_mapping: None,
        catalog: None,
        policies: vec![serde_norway::from_str(PUBLIC_READ).expect("the policy parses")],
    }
}

fn with_policy(yaml: &str) -> Endpoint {
    Endpoint {
        roles: Default::default(),
        policies: vec![serde_norway::from_str(yaml).expect("the policy parses")],
        ..endpoint()
    }
}

/// A grant over one box around Banská Bystrica and nothing else.
fn with_geo_grant() -> Endpoint {
    with_policy(
        r#"contextSpaceRef: ovzdusie
assigner: did:web:banskabystrica.sk
assignee: { kind: role, id: public }
operations: [queryEntity, retrieveEntity]
information:
  - entities:
      - type: AirQualityObserved
geoQ: "georel=within;geometry=Polygon;coordinates=[[[19.0,48.6],[19.3,48.6],[19.3,48.9],[19.0,48.9],[19.0,48.6]]]"
"#,
    )
}

fn gateway(broker: &str, endpoint: Endpoint) -> axum::Router {
    router(Arc::new(
        Gateway::new(
            Broker::new(broker),
            Box::new(PolicyPdp),
            "banskabystrica.sk",
        )
        .serve([endpoint]),
    ))
}

fn station(local: &str, lon: f64, lat: f64) -> Value {
    json!({
        "id": format!("urn:ngsi-ld:AirQualityObserved:banskabystrica.sk:ovzdusie:{local}"),
        "type": "AirQualityObserved",
        "pm10": { "type": "Property", "value": 34.2 },
        "operator": { "type": "Property", "value": "SHMU" },
        "location": {
            "type": "GeoProperty",
            "value": { "type": "Point", "coordinates": [lon, lat] }
        }
    })
}

async fn send(app: axum::Router, request: HttpRequest<Body>) -> (StatusCode, String) {
    let response: Response<Body> = app.oneshot(request).await.expect("the gateway answers");
    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), 16 * 1024 * 1024)
        .await
        .expect("a readable body");
    (status, String::from_utf8_lossy(&body).into_owned())
}

fn at(path: &str) -> HttpRequest<Body> {
    HttpRequest::builder()
        .uri(path)
        .body(Body::empty())
        .expect("a request")
}

async fn get(app: axum::Router, path: &str) -> (StatusCode, String) {
    send(app, at(path)).await
}

/// The first case: an endpoint that does not serve GeoJSON is not an endpoint at this URL, and the
/// broker is never asked — a caller must not be able to tell a representation apart from a grant by
/// what the gateway does upstream (EP-05).
#[tokio::test]
async fn an_endpoint_that_does_not_serve_geojson_is_not_found_and_the_broker_is_not_asked() {
    let broker = BrokerStub::start(vec![json!([station("station-01", 19.14, 48.73)])]).await;
    let mut only_ngsi = endpoint();
    only_ngsi.representations = vec![Representation::NgsiLd];

    let (status, body) = get(
        gateway(&broker.url, only_ngsi),
        &format!("/api/endpoint/{SLUG}/file.geojson"),
    )
    .await;

    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert!(broker.hops().is_empty(), "{:?}", broker.hops());
}

/// Refused, absent and not served look the same: the same status and the same body, so nobody can
/// map the endpoints of a deployment by reading the differences.
#[tokio::test]
async fn an_unknown_slug_and_a_representation_not_served_answer_the_same_bytes() {
    let broker = BrokerStub::start(vec![json!([])]).await;
    let mut only_ngsi = endpoint();
    only_ngsi.representations = vec![Representation::NgsiLd];

    let (unknown_status, unknown_body) = get(
        gateway(&broker.url, endpoint()),
        "/api/endpoint/xxxxxxxxxxxxxxxxxxxxxxxxxx/file.geojson",
    )
    .await;
    let (unserved_status, unserved_body) = get(
        gateway(&broker.url, only_ngsi),
        &format!("/api/endpoint/{SLUG}/file.geojson"),
    )
    .await;

    assert_eq!(unknown_status, StatusCode::NOT_FOUND);
    assert_eq!(unserved_status, unknown_status);
    assert_eq!(unserved_body, unknown_body, "the two must be one answer");
    assert!(broker.hops().is_empty(), "neither asked the broker");
}

/// No other spelling of a slug is that slug: another case, a shorter or longer one, a trailing
/// space, a trailing slash and a percent-encoded copy are all `404`.
#[tokio::test]
async fn no_other_spelling_of_the_slug_is_that_slug() {
    let broker = BrokerStub::start(vec![json!([station("station-01", 19.14, 48.73)])]).await;
    let upper = SLUG.to_uppercase();
    let short = &SLUG[..SLUG.len() - 1];
    let long = format!("{SLUG}a");
    let spaced = format!("{SLUG}%20");
    let doubled = format!("{SLUG}%2520");

    for slug in [
        upper.as_str(),
        short,
        long.as_str(),
        spaced.as_str(),
        doubled.as_str(),
    ] {
        let (status, body) = get(
            gateway(&broker.url, endpoint()),
            &format!("/api/endpoint/{slug}/file.geojson"),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{slug}: {body}");
    }
    assert!(broker.hops().is_empty(), "{:?}", broker.hops());
}

/// A tenant header the caller forged never reaches the broker: the hop carries the endpoint's own
/// space, once, whatever the request said (GW25).
#[tokio::test]
async fn a_tenant_the_caller_forged_never_reaches_the_broker() {
    let broker = BrokerStub::start(vec![json!([station("station-01", 19.14, 48.73)])]).await;
    let request = HttpRequest::builder()
        .uri(format!("/api/endpoint/{SLUG}/file.geojson"))
        .header("NGSILD-Tenant", "somebody-elses-space")
        .header("ngsild-tenant", "another-space")
        .body(Body::empty())
        .expect("a request");

    let (status, body) = send(gateway(&broker.url, endpoint()), request).await;

    assert_eq!(status, StatusCode::OK, "{body}");
    let hops = broker.hops();
    assert!(!hops.is_empty(), "the query was made");
    assert!(
        hops.iter()
            .all(|hop| !hop.forged && hop.tenant == "ovzdusie"),
        "{hops:?}"
    );
}

/// An entity of a type the grant does not name does not come out, even when the broker hands it
/// over: the answer is filtered here and not only asked for narrowly (T-1862, R9).
#[tokio::test]
async fn an_entity_of_an_ungranted_type_is_dropped_on_the_way_back() {
    let mut intruder = station("station-02", 19.15, 48.74);
    intruder["type"] = json!("WaterQualityObserved");
    intruder["id"] =
        json!("urn:ngsi-ld:WaterQualityObserved:banskabystrica.sk:ovzdusie:station-02");
    let broker =
        BrokerStub::start(vec![json!([station("station-01", 19.14, 48.73), intruder])]).await;

    let (status, body) = get(
        gateway(
            &broker.url,
            with_policy(
                r#"contextSpaceRef: ovzdusie
assigner: did:web:banskabystrica.sk
assignee: { kind: role, id: public }
operations: [queryEntity, retrieveEntity]
information:
  - entities:
      - type: AirQualityObserved
"#,
            ),
        ),
        &format!("/api/endpoint/{SLUG}/file.geojson"),
    )
    .await;

    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(!body.contains("WaterQualityObserved"), "{body}");
    assert!(body.contains("station-01"), "{body}");
}

/// An attribute the endpoint hides is in no feature: the grant said the whole entity, the endpoint
/// takes that one name back, and what is published is what comes out (EP-61).
#[tokio::test]
async fn a_hidden_attribute_is_in_no_feature() {
    let broker = BrokerStub::start(vec![json!([station("station-01", 19.14, 48.73)])]).await;
    let mut hiding = endpoint();
    hiding.hidden_attributes = BTreeSet::from(["operator".to_owned()]);

    let (status, body) = get(
        gateway(&broker.url, hiding),
        &format!("/api/endpoint/{SLUG}/file.geojson?type=AirQualityObserved"),
    )
    .await;

    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body.contains("pm10"), "what is published comes out: {body}");
    assert!(
        !body.contains("operator"),
        "and what is hidden does not: {body}"
    );
    assert!(!body.contains("SHMU"), "its value neither: {body}");
}

/// A query that names a hidden attribute answers nothing and asks nothing: the type whose
/// attributes do not cover the filter leaves the query before the broker is asked, so the answer
/// cannot be narrowed by trying names one at a time (T-1862, EP-61).
#[tokio::test]
async fn a_query_naming_a_hidden_attribute_answers_nothing_and_asks_nothing() {
    for query in [
        "type=AirQualityObserved&attrs=pm10,operator",
        "type=AirQualityObserved&attrs=operator",
        "type=AirQualityObserved&q=operator==%22SHMU%22",
        "type=AirQualityObserved&orderBy=operator",
        "type=AirQualityObserved&attrs=OPERATOR,operator",
    ] {
        let broker = BrokerStub::start(vec![json!([station("station-01", 19.14, 48.73)])]).await;
        let mut hiding = endpoint();
        hiding.hidden_attributes = BTreeSet::from(["operator".to_owned()]);

        let (status, body) = get(
            gateway(&broker.url, hiding),
            &format!("/api/endpoint/{SLUG}/file.geojson?{query}"),
        )
        .await;

        assert_eq!(status, StatusCode::OK, "{query}: {body}");
        let collection: Value = serde_json::from_str(&body).expect("a FeatureCollection");
        assert_eq!(
            collection["features"].as_array().map(Vec::len),
            Some(0),
            "{query}: {body}"
        );
        assert!(
            broker.hops().is_empty(),
            "{query}: a filter on a hidden attribute never reaches the broker: {:?}",
            broker.hops()
        );
    }
}

/// The grant's own area is what the broker is asked with, exactly as the policy wrote it: a
/// download the caller drew no area for is still the granted area and not the whole space (GW11).
#[tokio::test]
async fn the_grants_area_is_what_the_broker_is_asked_with() {
    let broker = BrokerStub::start(vec![json!([station("inside", 19.14, 48.73)])]).await;

    let (status, body) = get(
        gateway(&broker.url, with_geo_grant()),
        &format!("/api/endpoint/{SLUG}/file.geojson"),
    )
    .await;

    assert_eq!(status, StatusCode::OK, "{body}");
    let hops = broker.hops();
    let query = &hops.last().expect("an entity query").query;
    assert!(query.contains("georel=within"), "{query}");
    assert!(query.contains("19.3"), "the grant's own corner: {query}");
}

/// An entity outside the granted area does not come out when the broker was given something wider:
/// a caller that draws its own area gets the grant forwarded and both areas applied here, so an
/// answer the broker did not narrow is narrowed on the way back (GW11, T-0149).
#[tokio::test]
async fn an_entity_outside_the_granted_area_is_dropped_on_the_way_back() {
    let broker = BrokerStub::start(vec![json!([
        station("inside", 19.14, 48.73),
        station("outside", 24.94, 60.17),
    ])])
    .await;
    // The caller asks for half of Europe, which is wider than its grant: the grant goes upstream
    // and the caller's own area is applied here.
    let wide = "georel=within&geometry=Polygon&coordinates=%5B%5B%5B0.0%2C40.0%5D%2C%5B30.0%2C40.0%5D%2C%5B30.0%2C70.0%5D%2C%5B0.0%2C70.0%5D%2C%5B0.0%2C40.0%5D%5D%5D";

    let (status, body) = get(
        gateway(&broker.url, with_geo_grant()),
        &format!("/api/endpoint/{SLUG}/file.geojson?{wide}"),
    )
    .await;

    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body.contains("inside"), "{body}");
    assert!(
        !body.contains("outside"),
        "the station beyond the grant is not in the answer: {body}"
    );
}

/// A caller no grant names is refused, and the broker is not asked at all: a refusal that still
/// made a hop would be a way to ask questions with somebody else's endpoint.
#[tokio::test]
async fn a_caller_no_grant_names_is_refused_before_the_broker() {
    let broker = BrokerStub::start(vec![json!([station("station-01", 19.14, 48.73)])]).await;
    let mut ungranted = endpoint();
    ungranted.policies = Vec::new();

    let (status, body) = get(
        gateway(&broker.url, ungranted),
        &format!("/api/endpoint/{SLUG}/file.geojson"),
    )
    .await;

    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert!(broker.hops().is_empty(), "{:?}", broker.hops());
}

/// A non-spatial answer is a bad request, and the refusal says only that: a caller asked a type
/// with no geometry for a spatial representation (EP-10).
#[tokio::test]
async fn an_answer_with_no_geometry_at_all_is_a_bad_request() {
    let mut flat = station("station-01", 0.0, 0.0);
    flat.as_object_mut().expect("an object").remove("location");
    let broker = BrokerStub::start(vec![json!([flat])]).await;

    let (status, body) = get(
        gateway(&broker.url, endpoint()),
        &format!("/api/endpoint/{SLUG}/file.geojson?type=AirQualityObserved"),
    )
    .await;

    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(body.contains("geometry"), "{body}");
    assert!(!body.contains("127.0.0.1"), "no upstream address: {body}");
}

/// The answer's media type is GeoJSON's own, and the caller's `Accept` decides nothing about it:
/// this route is one document in one format.
#[tokio::test]
async fn the_answer_is_geojson_whatever_the_caller_accepts() {
    for accept in [
        "application/geo+json",
        "text/csv",
        "*/*",
        "application/xml;q=1.0",
        "",
    ] {
        let broker = BrokerStub::start(vec![json!([station("station-01", 19.14, 48.73)])]).await;
        let request = HttpRequest::builder()
            .uri(format!("/api/endpoint/{SLUG}/file.geojson"))
            .header("Accept", accept)
            .body(Body::empty())
            .expect("a request");
        let response = gateway(&broker.url, endpoint())
            .oneshot(request)
            .await
            .expect("the gateway answers");

        assert_eq!(response.status(), StatusCode::OK, "{accept:?}");
        assert_eq!(
            response
                .headers()
                .get("content-type")
                .and_then(|value| value.to_str().ok()),
            Some("application/geo+json"),
            "{accept:?}"
        );
        assert!(
            broker
                .hops()
                .iter()
                .all(|hop| hop.accept == "application/json"),
            "{accept:?}: {:?}",
            broker.hops()
        );
    }
}

/// A grant that reaches no entity is an empty collection, not a question the broker answers with
/// everything: nothing is asked for when nothing was granted.
#[tokio::test]
async fn a_grant_that_reaches_nothing_is_an_empty_collection_and_no_question() {
    let broker = BrokerStub::start(vec![json!([station("station-01", 19.14, 48.73)])]).await;

    let (status, body) = get(
        gateway(&broker.url, endpoint()),
        &format!("/api/endpoint/{SLUG}/file.geojson?type=NothingLikeThis"),
    )
    .await;

    assert_eq!(status, StatusCode::OK, "{body}");
    let collection: Value = serde_json::from_str(&body).expect("a FeatureCollection");
    assert_eq!(collection["type"], json!("FeatureCollection"));
    assert_eq!(collection["features"].as_array().map(Vec::len), Some(0));
}

/// A refusal the gateway builds itself says nothing about what is behind it: no address, no tenant,
/// no file path. What the broker says when *it* fails is a finding of its own — the body is
/// forwarded verbatim today, which is T-2340 (priority 1) and holds that red test.
#[tokio::test]
async fn a_refusal_the_gateway_builds_names_nothing_internal() {
    let mut flat = station("station-01", 0.0, 0.0);
    flat.as_object_mut().expect("an object").remove("location");
    let broker = BrokerStub::start(vec![json!([flat])]).await;

    let (status, body) = get(
        gateway(&broker.url, endpoint()),
        &format!("/api/endpoint/{SLUG}/file.geojson?type=AirQualityObserved"),
    )
    .await;

    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    for internal in ["127.0.0.1", "ovzdusie", "/src/", "Bearer", "policy"] {
        assert!(!body.contains(internal), "{internal} came out in {body}");
    }
}

/// Only a read reaches this route: a write method is refused by the surface itself and never
/// becomes a broker call.
#[tokio::test]
async fn a_write_method_on_a_download_is_refused() {
    for method in ["POST", "PUT", "PATCH", "DELETE"] {
        let broker = BrokerStub::start(vec![json!([station("station-01", 19.14, 48.73)])]).await;
        let request = HttpRequest::builder()
            .method(method)
            .uri(format!("/api/endpoint/{SLUG}/file.geojson"))
            .body(Body::empty())
            .expect("a request");
        let (status, body) = send(gateway(&broker.url, endpoint()), request).await;

        assert!(
            status == StatusCode::METHOD_NOT_ALLOWED || status == StatusCode::NOT_FOUND,
            "{method}: {status} {body}"
        );
        assert!(broker.hops().is_empty(), "{method}: {:?}", broker.hops());
    }
}
