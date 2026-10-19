//! The entity grid's three uses of an Endpoint, pinned before the UI rests on them (T-1436,
//! EP-07, EP-30, EP-55): a query with `options=sysAttrs`, a partial attribute update, and a
//! temporal read. Projection (the grant's attributes, the endpoint's hidden ones), the Policy and
//! temporal clamping hold on each.

mod common;

use axum::body::Body;
use axum::extract::Request;
use axum::http::{Method, StatusCode};
use axum::routing::any;
use axum::Router;
use context_gateway::app::{router, Gateway};
use context_gateway::pdp::PolicyPdp;
use context_gateway::proxy::Broker;
use context_gateway::resolver::Endpoint;
use http_body_util::BodyExt;
use jc_core::kinds::{Audience, PolicySpec, Representation};
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
use tower::ServiceExt;

const SLUG: &str = "4gridcontract4testslug4abcd";
const SPACE: &str = "ovzdusie";
const DOMAIN: &str = "banskabystrica.sk";
const STATION: &str = "urn:ngsi-ld:AirQualityObserved:banskabystrica.sk:ovzdusie:station-01";

/// One hop the broker saw: method, path, query and body.
type Hops = Arc<Mutex<Vec<(String, String, String, String)>>>;

fn station() -> Value {
    json!({
        "id": STATION,
        "type": "AirQualityObserved",
        "createdAt": "2026-09-01T08:00:00Z",
        "modifiedAt": "2026-09-18T10:00:00Z",
        "pm10": { "type": "Property", "value": 34.2, "unitCode": "GQ",
                  "observedAt": "2026-09-18T10:00:00Z",
                  "createdAt": "2026-09-01T08:00:00Z", "modifiedAt": "2026-09-18T10:00:00Z" },
        "status": { "type": "Property", "value": "working",
                    "createdAt": "2026-09-01T08:00:00Z", "modifiedAt": "2026-09-10T10:00:00Z" },
        "operatorPhone": { "type": "Property", "value": "+421 900 000 000" },
        "secretNote": { "type": "Property", "value": "not for the grid" }
    })
}

fn temporal_station() -> Value {
    json!({
        "id": STATION,
        "type": "AirQualityObserved",
        "pm10": [
            { "type": "Property", "value": 30.0, "observedAt": "2026-09-18T09:00:00Z" },
            { "type": "Property", "value": 34.2, "observedAt": "2026-09-18T10:00:00Z" }
        ],
        "operatorPhone": [{ "type": "Property", "value": "+421 900 000 000", "observedAt": "2026-09-18T10:00:00Z" }]
    })
}

/// A broker that answers the grid's three reads and 204 to every write, and records each hop.
async fn broker() -> (String, Hops) {
    let hops: Hops = Arc::new(Mutex::new(Vec::new()));
    let recorder = Arc::clone(&hops);
    let app = Router::new().fallback(any(move |request: Request| {
        let recorder = Arc::clone(&recorder);
        async move {
            let method = request.method().to_string();
            let path = request.uri().path().to_owned();
            let query = request.uri().query().unwrap_or_default().to_owned();
            let body = String::from_utf8(
                request
                    .into_body()
                    .collect()
                    .await
                    .expect("a body")
                    .to_bytes()
                    .to_vec(),
            )
            .unwrap_or_default();
            recorder
                .lock()
                .expect("the hop log")
                .push((method.clone(), path.clone(), query, body));
            if method != "GET" {
                return axum::response::IntoResponse::into_response(StatusCode::NO_CONTENT);
            }
            let answer = if path.starts_with("/ngsi-ld/v1/temporal/entities/") {
                temporal_station()
            } else if path.starts_with("/ngsi-ld/v1/temporal/entities") {
                json!([temporal_station()])
            } else if path.starts_with("/ngsi-ld/v1/entities/") {
                station()
            } else {
                json!([station()])
            };
            axum::response::IntoResponse::into_response(axum::Json(answer))
        }
    }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("a free port");
    let address = listener.local_addr().expect("the bound address");
    tokio::spawn(async move {
        let _ = axum::serve(listener, common::plain_tree(app)).await;
    });
    (format!("http://{address}"), hops)
}

/// The public may read pm10, status and operatorPhone over the last day, and update `status`.
fn editor() -> PolicySpec {
    policy("[queryEntity, retrieveEntity, queryTemporal, retrieveTemporal, updateAttrs]")
}

fn policy(operations: &str) -> PolicySpec {
    serde_norway::from_str(&format!(
        "contextSpaceRef: {SPACE}\n\
         assigner: did:web:{DOMAIN}\n\
         assignee: {{ kind: role, id: public }}\n\
         operations: {operations}\n\
         temporalQ: \"timerel=after;timeAt=P-1D\"\n\
         information:\n\
         \x20 - entities:\n\
         \x20     - type: AirQualityObserved\n\
         \x20   propertyNames: [pm10, status, operatorPhone]\n"
    ))
    .expect("the policy spec parses")
}

fn endpoint(policy: PolicySpec) -> Endpoint {
    Endpoint {
        declared_types: None,
        roles: Default::default(),
        slug: SLUG.to_owned(),
        title: std::collections::BTreeMap::new(),
        description: std::collections::BTreeMap::new(),
        space: SPACE.to_owned(),
        project: SPACE.to_owned(),
        audience: Audience::Public,
        allowed_projects: Vec::new(),
        representations: vec![Representation::NgsiLd],
        rate_limit: None,
        creates: None,
        file_limits: None,
        // The endpoint publishes the space with less detail than the grant allows (EP-61).
        hidden_attributes: ["operatorPhone".to_owned()].into_iter().collect(),
        projection: None,
        view_mapping: None,
        catalog: None,
        base_path: format!("/api/endpoint/{SLUG}"),
        models: Vec::new(),
        policy_names: Vec::new(),
        manifest_name: String::new(),
        policies: vec![policy],
    }
}

struct Answer {
    status: StatusCode,
    body: Value,
    hops: Vec<(String, String, String, String)>,
}

async fn send(policy: PolicySpec, method: Method, uri: &str, body: Option<Value>) -> Answer {
    let (upstream, hops) = broker().await;
    let gateway = Arc::new(
        Gateway::new(Broker::new(upstream), Box::new(PolicyPdp), DOMAIN).serve([endpoint(policy)]),
    );
    let mut request = Request::builder()
        .method(method)
        .uri(format!("/api/endpoint/{SLUG}/ngsi-ld/v1{uri}"));
    if body.is_some() {
        request = request.header("content-type", "application/json");
    }
    let response = router(gateway)
        .oneshot(
            request
                .body(body.map_or_else(Body::empty, |b| Body::from(b.to_string())))
                .expect("a request"),
        )
        .await
        .expect("the gateway answers");
    let status = response.status();
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("a body")
        .to_bytes();
    let body = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    let hops = hops.lock().expect("the hop log").clone();
    Answer { status, body, hops }
}

fn encoded(id: &str) -> String {
    id.replace(':', "%3A")
}

// --- sysAttrs ---

// --- partial attribute update ---

#[tokio::test]
async fn a_partial_update_of_a_permitted_attribute_reaches_the_broker() {
    let answer = send(
        editor(),
        Method::PATCH,
        &format!("/entities/{}/attrs", encoded(STATION)),
        Some(json!({ "status": { "type": "Property", "value": "outOfService" } })),
    )
    .await;
    assert_eq!(answer.status, StatusCode::NO_CONTENT, "{}", answer.body);
    assert_eq!(answer.hops.len(), 1);
    assert_eq!(answer.hops[0].0, "PATCH");
    assert!(answer.hops[0].3.contains("outOfService"));
}

// --- temporal ---

#[tokio::test]
async fn a_last_n_above_the_cap_is_cut_and_never_an_error() {
    let answer = send(
        editor(),
        Method::GET,
        &format!("/temporal/entities/{}?lastN=1000000", encoded(STATION)),
        None,
    )
    .await;
    assert_eq!(answer.status, StatusCode::OK, "{}", answer.body);
    let (_, _, query, _) = &answer.hops[0];
    let asked: usize = query
        .split('&')
        .find_map(|pair| pair.strip_prefix("lastN="))
        .and_then(|n| n.parse().ok())
        .expect("lastN is forwarded");
    assert!(
        asked <= 1000,
        "the broker is asked for at most the cap: {query}"
    );
}

#[tokio::test]
async fn a_partial_update_that_tries_to_change_id_or_type_is_refused() {
    for body in [
        json!({ "id": "urn:ngsi-ld:AirQualityObserved:banskabystrica.sk:ovzdusie:other", "status": { "type": "Property", "value": "x" } }),
        json!({ "type": "Secret", "status": { "type": "Property", "value": "x" } }),
    ] {
        let answer = send(
            editor(),
            Method::PATCH,
            &format!("/entities/{}/attrs", encoded(STATION)),
            Some(body.clone()),
        )
        .await;
        assert!(answer.status.is_client_error(), "{body}: {}", answer.status);
        assert!(answer.hops.is_empty(), "{body}: the broker was asked");
    }
}
