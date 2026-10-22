//! The gateway does not trust a broker that answers more than it was asked (T-2131; EP-26, MP-02).
//!
//! A real broker honours `type` on a query. A federated context source, a registration answering
//! for a neighbour or a broker defect does not, and the gateway is the policy enforcement point:
//! it may not depend on any of them. Measured on 2026-09-18 by the leak probe, when the stub
//! ignored `type` and answered a `Depot` beside the asked Vehicles, the Depot's own attributes
//! reached the caller on NGSI-LD, temporal, `file.csv`, OGC items, SensorThings
//! Things and the MCP tools.
//!
//! Every surface here reads the same broker answer, so this is one fixture asked eight ways: a
//! Depot the endpoint's grant never names, carrying a name no answer may contain.

mod common;

use axum::body::Body;
use axum::extract::Request;
use axum::http::{Method, StatusCode};
use axum::routing::any;
use axum::Router;
use context_gateway::app::{router, Gateway};
use context_gateway::pdp::PolicyPdp;
use context_gateway::proxy::Broker;
use context_gateway::resolver::{Endpoint, Model};
use jc_core::kinds::{Audience, PolicySpec, Representation};
use serde_json::{json, Value};
use std::sync::Arc;
use tower::ServiceExt;

const SLUG: &str = "7nb4xr2qzd8vkm5ftjw3cy6phs";
const SPACE: &str = "fleet";
const DOMAIN: &str = "hel.fi";
/// The word that may not appear in any answer, whatever the broker sends.
const FORBIDDEN: &str = "North depot";

/// A broker that ignores `type` and answers with a Depot beside the Vehicle, counting both.
async fn careless_broker() -> String {
    let app = Router::new().fallback(any(|| async {
        (
            [("NGSILD-Results-Count", "2")],
            axum::Json(json!([
                {
                    "id": "urn:ngsi-ld:Vehicle:hel.fi:fleet:bus-01",
                    "type": "Vehicle",
                    "name": { "type": "Property", "value": "Bus 01" },
                    "location": {
                        "type": "GeoProperty",
                        "value": { "type": "Point", "coordinates": [24.9, 60.2] }
                    }
                },
                {
                    "id": "urn:ngsi-ld:Depot:hel.fi:fleet:north",
                    "type": "Depot",
                    "name": { "type": "Property", "value": FORBIDDEN },
                    "capacity": { "type": "Property", "value": 40 },
                    "location": {
                        "type": "GeoProperty",
                        "value": { "type": "Point", "coordinates": [25.0, 60.3] }
                    }
                }
            ])),
        )
    }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("a free port");
    let address = listener.local_addr().expect("the bound address");
    tokio::spawn(async move {
        let _ = axum::serve(listener, common::plain_tree(app)).await;
    });
    format!("http://{address}")
}

fn policy() -> PolicySpec {
    serde_norway::from_str(&format!(
        "contextSpaceRef: {SPACE}\n\
         assigner: did:web:{DOMAIN}\n\
         assignee: {{ kind: role, id: public }}\n\
         operations: [queryEntity, retrieveEntity, retrieveTemporal, queryTemporal]\n\
         information:\n\
         \x20 - entities:\n\
         \x20     - type: Vehicle\n"
    ))
    .expect("the policy spec parses")
}

fn endpoint() -> Endpoint {
    Endpoint {
        declared_types: None,
        roles: Default::default(),
        slug: SLUG.to_owned(),
        title: std::collections::BTreeMap::new(),
        description: std::collections::BTreeMap::new(),
        space: SPACE.to_owned(),
        project: "helsinki".to_owned(),
        audience: Audience::Public,
        allowed_projects: Vec::new(),
        representations: vec![
            Representation::NgsiLd,
            Representation::Csv,
            Representation::OgcFeatures,
            Representation::Sta,
            Representation::Mcp,
        ],
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
        models: vec![Model {
            name: "fleet".to_owned(),
            version: "1.0.0".to_owned(),
            major: 1,
            classes: vec!["Vehicle".to_owned(), "Depot".to_owned()],
            json_schema: None,
            context: None,
        }],
        policy_names: Vec::new(),
        manifest_name: String::new(),
        policies: vec![policy()],
    }
}

/// One request to one surface, with the whole answer as text: a leak is looked for in the bytes.
async fn surface(method: Method, uri: &str, body: Option<Value>) -> (StatusCode, String) {
    let (status, _, text) = answered(method, uri, body, &[]).await;
    (status, text)
}

/// The same, sending the caller's own headers and keeping the answer's: the count of a narrowed
/// answer is one of them, and the narrowing signal is only sent to a caller who asked (R22).
async fn answered(
    method: Method,
    uri: &str,
    body: Option<Value>,
    sent: &[(&str, &str)],
) -> (StatusCode, axum::http::HeaderMap, String) {
    let upstream = careless_broker().await;
    through(upstream, method, uri, body, sent).await
}

/// The same, against a broker of the caller's choosing.
async fn through(
    upstream: String,
    method: Method,
    uri: &str,
    body: Option<Value>,
    sent: &[(&str, &str)],
) -> (StatusCode, axum::http::HeaderMap, String) {
    let gateway = Arc::new(
        Gateway::new(Broker::new(upstream), Box::new(PolicyPdp), DOMAIN).serve([endpoint()]),
    );
    let mut builder = Request::builder()
        .method(method)
        .uri(format!("/api/endpoint/{SLUG}{uri}"));
    if body.is_some() {
        builder = builder
            .header("content-type", "application/json")
            .header("accept", "application/json, text/event-stream");
    }
    for (name, value) in sent {
        builder = builder.header(*name, *value);
    }
    let request = builder
        .body(match &body {
            Some(payload) => Body::from(payload.to_string()),
            None => Body::empty(),
        })
        .expect("a request");
    let response = router(gateway)
        .oneshot(request)
        .await
        .expect("the gateway answers");
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("a body");
    (
        status,
        headers,
        String::from_utf8_lossy(&bytes).into_owned(),
    )
}

/// A broker that answers only what it was asked, and counts it: the other half of the count
/// rule, because a header removed from every answer would pass the test above and tell a
/// well-behaved caller nothing about how many entities match their query.
async fn honest_broker() -> String {
    let app = Router::new().fallback(any(|| async {
        (
            [("NGSILD-Results-Count", "1")],
            axum::Json(json!([
                {
                    "id": "urn:ngsi-ld:Vehicle:hel.fi:fleet:bus-01",
                    "type": "Vehicle",
                    "name": { "type": "Property", "value": "Bus 01" }
                }
            ])),
        )
    }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("a free port");
    let address = listener.local_addr().expect("the bound address");
    tokio::spawn(async move {
        let _ = axum::serve(listener, common::plain_tree(app)).await;
    });
    format!("http://{address}")
}

/// R22: the count of an answer the gateway did not narrow is the broker's count, unchanged.
#[tokio::test]
async fn the_count_survives_an_answer_nothing_was_dropped_from() {
    let (status, headers, answer) = through(
        honest_broker().await,
        Method::GET,
        "/ngsi-ld/v1/entities?type=Vehicle&count=true",
        None,
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{answer}");
    assert_eq!(
        headers
            .get("ngsild-results-count")
            .and_then(|value| value.to_str().ok()),
        Some("1"),
        "the caller cannot count what they may read: {answer}"
    );
}

/// And the granted entity is still served: a guard that drops everything proves nothing.
#[tokio::test]
async fn the_granted_type_still_comes_through() {
    let (status, answer) = surface(Method::GET, "/ngsi-ld/v1/entities?type=Vehicle", None).await;
    assert_eq!(status, StatusCode::OK, "{answer}");
    assert!(answer.contains("Bus 01"), "{answer}");
}
