//! View-served reads asked with `Accept: application/geo+json` (T-4111, after T-4106).
//!
//! `view_page` forwarded the caller's Accept to `/x/views/<slug>/…`; asked for geo+json the view
//! answers NGSI-LD's FeatureCollection, which the paging read as no entity or as one wrong one.
//! The stub answers exactly so, which keeps the test red on the old code.

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

const SLUG: &str = "ajaezssvx3bwqgejgepmq4ig3frgan4e";

fn stops() -> Vec<Value> {
    (0..3)
        .map(|n| {
            json!({
                "id": format!("urn:ngsi-ld:GtfsStop:hel.fi:helsinki:{n}"),
                "type": "GtfsStop",
                "name": { "type": "Property", "value": format!("Stop {n}") },
                "location": {
                    "type": "GeoProperty",
                    "value": { "type": "Point", "coordinates": [24.94, 60.17] }
                }
            })
        })
        .collect()
}

/// The view, answering a FeatureCollection to geo+json and an entity array otherwise.
async fn view_broker() -> String {
    let app = Router::new().fallback(|request: Request| async move {
        let path = request.uri().path().to_owned();
        if path == "/ngsi-ld/v1/types" {
            return axum::Json(json!({ "type": "EntityTypeList", "typeList": ["GtfsStop"] }))
                .into_response();
        }
        if path != format!("/x/views/{SLUG}/ngsi-ld/v1/entities") {
            return StatusCode::NOT_FOUND.into_response();
        }
        let geojson = request
            .headers()
            .get(ACCEPT)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|accept| accept.contains("geo+json"));
        if geojson {
            let features: Vec<Value> = stops()
                .iter()
                .map(|e| json!({ "type": "Feature", "id": e["id"], "geometry": e["location"]["value"], "properties": {} }))
                .collect();
            return axum::Json(json!({ "type": "FeatureCollection", "features": features }))
                .into_response();
        }
        axum::Json(Value::Array(stops())).into_response()
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
        space: "helsinki".to_owned(),
        project: "helsinki".to_owned(),
        audience: Audience::Public,
        allowed_projects: Vec::new(),
        representations: vec![
            Representation::NgsiLd,
            Representation::Csv,
            Representation::Sta,
        ],
        rate_limit: None,
        creates: None,
        file_limits: None,
        hidden_attributes: Default::default(),
        projection: None,
        base_path: format!("/api/endpoint/{SLUG}"),
        models: Vec::new(),
        view_mapping: None,
        catalog: None,
        deprecation: None,
        policy_names: Vec::new(),
        manifest_name: String::new(),
        policies: vec![serde_norway::from_str(
            "contextSpaceRef: helsinki\n\
             assigner: did:web:hel.fi\n\
             assignee: { kind: role, id: public }\n\
             operations: [queryEntity, retrieveEntity]\n",
        )
        .expect("the policy spec parses")],
    }
}

async fn get(path: &str) -> (StatusCode, Vec<u8>) {
    let app = router(Arc::new(
        Gateway::new(
            Broker::new(&view_broker().await),
            Box::new(PolicyPdp),
            "hel.fi",
        )
        .serve([endpoint()]),
    ));
    let response = app
        .oneshot(
            HttpRequest::builder()
                .uri(format!("/api/endpoint/{SLUG}{path}"))
                .header(ACCEPT, "application/geo+json")
                .body(Body::empty())
                .expect("a request"),
        )
        .await
        .expect("the gateway answers");
    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), 4 * 1024 * 1024)
        .await
        .expect("a readable body");
    (status, body.to_vec())
}

#[tokio::test]
async fn a_view_served_csv_under_a_geojson_accept_holds_every_row() {
    let (status, body) = get("/file.csv?type=GtfsStop").await;
    let text = String::from_utf8_lossy(&body);
    assert_eq!(status, StatusCode::OK, "{text}");
    assert_eq!(
        text.lines()
            .filter(|line| line.contains("urn:ngsi-ld:GtfsStop:"))
            .count(),
        3,
        "{text}"
    );
}
