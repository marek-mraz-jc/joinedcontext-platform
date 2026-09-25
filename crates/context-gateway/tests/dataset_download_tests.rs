//! A file download of an endpoint whose grant narrows nothing (T-0379, EP-07, EP-09).
//!
//! `DEMO.md` offers `file.geojson` with no query string, and on the dev cluster that was a
//! `400`: the demo's `public-read` Policy names no entity type, so the gateway forwarded a
//! query that selected nothing and a conformant broker refused it under CIM 009 5.7.2. Every
//! test that covered the download used a grant that *did* name a type, so the case the demo
//! actually exercises had no test at all.
//!
//! What a download means is the fix. `file.geojson` names a dataset, not a query, so when
//! nothing else selects, the selector is every type the space holds. It can only narrow: the
//! PDP has already decided what this caller may see.

mod common;

use axum::body::Body;
use axum::http::{Request as HttpRequest, StatusCode};
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

/// The dev seed's own grant, word for word: anonymous callers may read, and nothing narrows
/// what they read. This is the shape that produced the `400`.
fn unrestricted_endpoint() -> Endpoint {
    Endpoint {
        roles: Default::default(),
        slug: SLUG.to_owned(),
        title: std::collections::BTreeMap::new(),
        description: std::collections::BTreeMap::new(),
        space: "ovzdusie".to_owned(),
        project: "ovzdusie".to_owned(),
        audience: Audience::Public,
        allowed_projects: Vec::new(),
        representations: vec![
            Representation::NgsiLd,
            Representation::GeoJson,
            Representation::Csv,
        ],
        rate_limit: None,
        file_limits: None,
        // Nothing narrows what the anonymous caller reads, which is the whole point of this
        // fixture: the endpoint hides no attribute either (EP-61).
        hidden_attributes: BTreeSet::new(),
        projection: None,
        base_path: format!("/api/endpoint/{SLUG}"),
        models: Vec::new(),
        view_mapping: None,
        catalog: None,
        policies: vec![serde_norway::from_str(
            r#"contextSpaceRef: ovzdusie
assigner: did:web:banskabystrica.sk
assignee: { kind: role, id: public }
operations: [queryEntity, retrieveEntity]
"#,
        )
        .expect("the policy spec parses")],
    }
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

fn station() -> Value {
    json!({
        "id": "urn:ngsi-ld:AirQualityObserved:banskabystrica.sk:ovzdusie:station-01",
        "type": "AirQualityObserved",
        "pm10": { "type": "Property", "value": 34.2 },
        "location": {
            "type": "GeoProperty",
            "value": { "type": "Point", "coordinates": [19.146, 48.736] }
        }
    })
}

async fn get(app: axum::Router, path: &str) -> (StatusCode, String) {
    let response = app
        .oneshot(
            HttpRequest::builder()
                .uri(path)
                .body(Body::empty())
                .expect("a request"),
        )
        .await
        .expect("the gateway answers");
    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), 16 * 1024 * 1024)
        .await
        .expect("a readable body");
    (status, String::from_utf8_lossy(&body).into_owned())
}

/// The URL `DEMO.md` prints. Before the fix this was the broker's `400`, because the gateway
/// forwarded `/entities?` with nothing in it.
#[tokio::test]
async fn a_bare_geojson_download_selects_the_types_the_space_holds() {
    let broker = BrokerStub::start(vec![json!([station()])]).await;
    let (status, body) = get(
        gateway(&broker.url, unrestricted_endpoint()),
        &format!("/api/endpoint/{SLUG}/file.geojson"),
    )
    .await;

    assert_eq!(status, StatusCode::OK, "{body}");
    let collection: Value = serde_json::from_str(&body).expect("a FeatureCollection");
    assert_eq!(collection["type"], json!("FeatureCollection"));
    assert_eq!(collection["features"].as_array().map(Vec::len), Some(1));

    // The selector came from the broker's own answer to "what is in here", and the entity
    // query that followed carries it. Neither hop is unselected.
    let hops = broker.hops();
    let types_hop = hops.first().expect("a first hop");
    assert_eq!(
        types_hop.path, "/ngsi-ld/v1/types",
        "the fallback asks the space what it holds first: {hops:?}"
    );
    // The question is asked of this endpoint's tenant and no other, or the type list of a
    // neighbouring space would become this endpoint's selector.
    assert_eq!(types_hop.tenant, "ovzdusie", "{types_hop:?}");
    let query = &hops.last().expect("an entity query").query;
    assert!(
        query.contains("type=AirQualityObserved"),
        "the entity query must select: {query}"
    );
}

/// The same rule for the tabular downloads, which page through the broker separately.
#[tokio::test]
async fn a_bare_csv_download_selects_the_same_way() {
    let broker = BrokerStub::start(vec![json!([station()])]).await;
    let (status, body) = get(
        gateway(&broker.url, unrestricted_endpoint()),
        &format!("/api/endpoint/{SLUG}/file.csv"),
    )
    .await;

    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body.contains("pm10"), "{body}");
    assert!(
        broker
            .hops()
            .iter()
            .any(|hop| hop.query.contains("type=AirQualityObserved")),
        "{:?}",
        broker.hops()
    );
}

/// The caller's `Accept` names what it wants back, not what the broker speaks: `jcctl publish
/// ckan` asks `file.csv` with `Accept: text/csv`, and forwarded to the broker that was a `406`.
#[tokio::test]
async fn the_callers_accept_never_reaches_the_broker() {
    for (path, accept) in [
        ("file.csv", "text/csv"),
        (
            "file.xlsx",
            "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        ),
        ("file.geojson", "application/geo+json"),
        ("file.zip", "application/zip"),
    ] {
        let broker = BrokerStub::start(vec![json!([station()])]).await;
        let mut endpoint = unrestricted_endpoint();
        endpoint
            .representations
            .extend([Representation::Xlsx, Representation::Zip]);
        let response = gateway(&broker.url, endpoint)
            .oneshot(
                HttpRequest::builder()
                    .uri(format!("/api/endpoint/{SLUG}/{path}"))
                    .header("Accept", accept)
                    .body(Body::empty())
                    .expect("a request"),
            )
            .await
            .expect("the gateway answers");
        assert_eq!(response.status(), StatusCode::OK, "{path}");
        let hops = broker.hops();
        assert!(!hops.is_empty(), "{path}: no hop");
        assert!(
            hops.iter().all(|hop| hop.accept == "application/json"),
            "{path}: {hops:?}"
        );
    }
}

/// A space with nothing in it is an empty file. Asking the broker for "no types at all"
/// would be an unselected query again, so the gateway does not ask.
#[tokio::test]
async fn an_empty_space_is_an_empty_file_and_not_a_second_question() {
    let broker = BrokerStub::start(vec![json!([])]).await;
    let (status, body) = get(
        gateway(&broker.url, unrestricted_endpoint()),
        &format!("/api/endpoint/{SLUG}/file.geojson"),
    )
    .await;

    assert_eq!(status, StatusCode::OK, "{body}");
    let collection: Value = serde_json::from_str(&body).expect("a FeatureCollection");
    assert_eq!(collection["features"].as_array().map(Vec::len), Some(0));
    let hops = broker.hops();
    assert_eq!(
        hops.iter()
            .filter(|hop| hop.path == "/ngsi-ld/v1/entities")
            .count(),
        0,
        "an empty type list means there is nothing to query: {hops:?}"
    );
}

/// The NGSI-LD surface is the API, not a download, and an unselected query there stays the
/// `400` CIM 009 5.7.2 asks for. A gateway that answered it would be less conformant than
/// the broker behind it.
#[tokio::test]
async fn the_ngsi_ld_surface_still_refuses_an_unselected_query() {
    let broker = BrokerStub::start(vec![json!([station()])]).await;
    let (status, body) = get(
        gateway(&broker.url, unrestricted_endpoint()),
        &format!("/api/endpoint/{SLUG}/ngsi-ld/v1/entities"),
    )
    .await;

    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(body.contains("5.7.2"), "the broker's own refusal: {body}");
    assert!(
        broker
            .hops()
            .iter()
            .all(|hop| hop.path != "/ngsi-ld/v1/types"),
        "the API surface never asks for a selector on the caller's behalf"
    );
}

/// A grant that names a type still decides, and the fallback never runs: the space may hold
/// more types than this caller may see, and asking for them all would widen the answer.
#[tokio::test]
async fn a_grant_that_narrows_is_never_widened_by_the_fallback() {
    let mut endpoint = unrestricted_endpoint();
    endpoint.policies = vec![serde_norway::from_str(
        r#"contextSpaceRef: ovzdusie
assigner: did:web:banskabystrica.sk
assignee: { kind: role, id: public }
operations: [queryEntity, retrieveEntity]
information:
  - entities:
      - type: AirQualityObserved
    propertyNames: [pm10, location]
"#,
    )
    .expect("the policy spec parses")];

    let broker = BrokerStub::start(vec![json!([station()])]).await;
    let (status, _) = get(
        gateway(&broker.url, endpoint),
        &format!("/api/endpoint/{SLUG}/file.geojson"),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    let hops = broker.hops();
    assert!(
        hops.iter().all(|hop| hop.path != "/ngsi-ld/v1/types"),
        "the grant already selected, so there was nothing to fall back to: {hops:?}"
    );
}
