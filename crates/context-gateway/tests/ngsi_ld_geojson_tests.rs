//! CIM 009 6.3.15 on the NGSI-LD surface: `Accept: application/geo+json` answers a GeoJSON
//! `Feature` for one entity and a `FeatureCollection` for a query (T-2583, TS-12, EP-10).
//!
//! The broker is asked for JSON and the gateway renders the answer after the projection, so the
//! GeoJSON carries exactly what the JSON would: no attribute the grant hides, and the status the
//! JSON read would have had (a miss stays the same 404).

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
const ID: &str = "urn:ngsi-ld:AirQualityObserved:hel.fi:ovzdusie:station-01";
const READ: &str = r#"contextSpaceRef: ovzdusie
assigner: did:web:hel.fi
assignee: { kind: role, id: public }
operations: [queryEntity, retrieveEntity]
"#;

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
        representations: vec![Representation::NgsiLd],
        rate_limit: None,
        file_limits: None,
        hidden_attributes: BTreeSet::from(["operator".to_owned()]),
        projection: None,
        base_path: format!("/api/endpoint/{SLUG}"),
        models: Vec::new(),
        view_mapping: None,
        catalog: None,
        policies: vec![serde_norway::from_str(READ).expect("the policy parses")],
    }
}

fn station() -> Value {
    json!({
        "id": ID,
        "type": "AirQualityObserved",
        "pm10": { "type": "Property", "value": 34.2 },
        "operator": { "type": "Property", "value": "SHMU" },
        "location": {
            "type": "GeoProperty",
            "value": { "type": "Point", "coordinates": [24.94, 60.17] }
        }
    })
}

async fn ask(broker: &BrokerStub, path: &str) -> (StatusCode, String, String) {
    let app = router(Arc::new(
        Gateway::new(Broker::new(&broker.url), Box::new(PolicyPdp), "hel.fi").serve([endpoint()]),
    ));
    let response = app
        .oneshot(
            HttpRequest::builder()
                .uri(format!("/api/endpoint/{SLUG}/ngsi-ld/v1{path}"))
                .header("Accept", "application/geo+json")
                .body(Body::empty())
                .expect("a request"),
        )
        .await
        .expect("the gateway answers");
    let status = response.status();
    let media = response
        .headers()
        .get("content-type")
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .expect("a body");
    (status, media, String::from_utf8_lossy(&body).into_owned())
}

#[tokio::test]
async fn one_entity_asked_for_as_geojson_is_a_feature_without_what_the_grant_hides() {
    let broker = BrokerStub::start(vec![station()]).await;
    let (status, media, body) = ask(&broker, &format!("/entities/{ID}")).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(media.starts_with("application/geo+json"), "{media}");
    let feature: Value = serde_json::from_str(&body).expect("json");
    assert_eq!(feature["type"], "Feature", "{body}");
    assert_eq!(feature["id"], ID);
    assert_eq!(feature["geometry"]["type"], "Point");
    assert!(feature["properties"].get("pm10").is_some(), "{body}");
    assert!(
        !body.contains("SHMU"),
        "a hidden attribute came out as GeoJSON: {body}"
    );
    // The broker was asked for what the gateway can project, not for its own GeoJSON.
    assert!(
        broker
            .hops()
            .iter()
            .all(|hop| !hop.accept.contains("geo+json")),
        "{:?}",
        broker.hops()
    );
}

#[tokio::test]
async fn a_query_asked_for_as_geojson_is_a_feature_collection() {
    let broker = BrokerStub::start(vec![json!([station()])]).await;
    let (status, media, body) = ask(&broker, "/entities?type=AirQualityObserved").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(media.starts_with("application/geo+json"), "{media}");
    let collection: Value = serde_json::from_str(&body).expect("json");
    assert_eq!(collection["type"], "FeatureCollection", "{body}");
    assert_eq!(collection["features"][0]["id"], ID);
    assert!(!body.contains("SHMU"), "{body}");
}
