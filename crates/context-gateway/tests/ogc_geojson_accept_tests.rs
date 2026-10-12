//! OGC API Features items asked for with `Accept: application/geo+json` (T-4106, EP-31).
//!
//! On dev every collection answered an empty FeatureCollection to the media type OGC 17-069r4
//! names for items, the one QGIS sends: the gateway forwarded the caller's Accept to the broker,
//! the broker answered NGSI-LD's GeoJSON object instead of an entity array, and the gateway read
//! that object as no entity. The stub here answers exactly so, which keeps the test red on the
//! old code.

use axum::body::Body;
use axum::extract::Request;
use axum::http::{header::ACCEPT, Request as HttpRequest, StatusCode};
use axum::response::IntoResponse;
use axum::Router;
use context_gateway::app::{router, Gateway};
use context_gateway::pdp::PolicyPdp;
use context_gateway::proxy::Broker;
use context_gateway::resolver::Endpoint;
use jc_core::kinds::{Audience, Representation};
use serde_json::{json, Value};
use std::sync::Arc;
use tower::ServiceExt;

const SLUG: &str = "mluyob4nz52lok3ssk7pgn5vwt";

fn school(local: &str) -> Value {
    json!({
        "id": format!("urn:ngsi-ld:School:banskabystrica.sk:verejne:{local}"),
        "type": "School",
        "name": { "type": "Property", "value": format!("ZŠ {local}") },
        "location": {
            "type": "GeoProperty",
            "value": { "type": "Point", "coordinates": [19.155, 48.7285] }
        }
    })
}

/// A broker that, like NGSI-LD's, answers a FeatureCollection to geo+json and an array otherwise.
async fn broker() -> String {
    let app = Router::new().fallback(|request: Request| async move {
        let accept = request
            .headers()
            .get(ACCEPT)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_owned();
        let rows = vec![school("1"), school("2")];
        if request.uri().path() == "/ngsi-ld/v1/types" {
            return axum::Json(json!({ "type": "EntityTypeList", "typeList": ["School"] }))
                .into_response();
        }
        if accept.contains("geo+json") {
            let features: Vec<Value> = rows
                .iter()
                .map(|e| json!({ "type": "Feature", "id": e["id"], "geometry": e["location"]["value"], "properties": {} }))
                .collect();
            return axum::Json(json!({ "type": "FeatureCollection", "features": features }))
                .into_response();
        }
        axum::Json(Value::Array(rows)).into_response()
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("a free port");
    let port = listener.local_addr().expect("an address").port();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    format!("http://127.0.0.1:{port}")
}

fn endpoint() -> Endpoint {
    Endpoint {
        declared_types: None,
        roles: Default::default(),
        slug: SLUG.to_owned(),
        title: std::collections::BTreeMap::new(),
        description: std::collections::BTreeMap::new(),
        space: "verejne".to_owned(),
        project: "verejne".to_owned(),
        audience: Audience::Public,
        allowed_projects: Vec::new(),
        representations: vec![Representation::NgsiLd, Representation::OgcFeatures],
        rate_limit: None,
        creates: None,
        file_limits: None,
        hidden_attributes: Default::default(),
        projection: None,
        base_path: format!("/api/endpoint/{SLUG}"),
        models: Vec::new(),
        view_mapping: None,
        catalog: None,
        policy_names: Vec::new(),
        policies: vec![serde_norway::from_str(
            "contextSpaceRef: verejne\n\
             assigner: did:web:banskabystrica.sk\n\
             assignee: { kind: role, id: public }\n\
             operations: [queryEntity, retrieveEntity]\n",
        )
        .expect("the policy spec parses")],
    }
}

async fn get(path: &str, accept: &str) -> (StatusCode, Value) {
    let app = router(Arc::new(
        Gateway::new(
            Broker::new(&broker().await),
            Box::new(PolicyPdp),
            "banskabystrica.sk",
        )
        .serve([endpoint()]),
    ));
    let response = app
        .oneshot(
            HttpRequest::builder()
                .uri(format!("/api/endpoint/{SLUG}/ogc/features{path}"))
                .header(ACCEPT, accept)
                .body(Body::empty())
                .expect("a request"),
        )
        .await
        .expect("the gateway answers");
    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .expect("a readable body");
    (status, serde_json::from_slice(&body).unwrap_or(Value::Null))
}

#[tokio::test]
async fn items_under_a_geojson_accept_carry_the_features() {
    for accept in ["application/geo+json", "application/json", "*/*"] {
        let (status, page) = get("/collections/School/items?limit=10", accept).await;
        assert_eq!(status, StatusCode::OK, "{accept}: {page}");
        assert_eq!(
            page["features"].as_array().map(Vec::len),
            Some(2),
            "{accept}: {page}"
        );
        assert_eq!(page["numberReturned"], json!(2), "{accept}: {page}");
    }
}

#[tokio::test]
async fn collections_under_a_geojson_accept_list_the_located_type() {
    let (status, answer) = get("/collections", "application/geo+json").await;
    assert_eq!(status, StatusCode::OK, "{answer}");
    let ids: Vec<&str> = answer["collections"]
        .as_array()
        .expect("a collection list")
        .iter()
        .filter_map(|c| c["id"].as_str())
        .collect();
    assert_eq!(ids, ["School"], "{answer}");
}

#[tokio::test]
async fn one_item_under_a_geojson_accept_is_found() {
    let id = "urn:ngsi-ld:School:banskabystrica.sk:verejne:1";
    let (status, feature) = get(
        &format!("/collections/School/items/{id}"),
        "application/geo+json",
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{feature}");
    assert_eq!(feature["id"], json!(id), "{feature}");
}
