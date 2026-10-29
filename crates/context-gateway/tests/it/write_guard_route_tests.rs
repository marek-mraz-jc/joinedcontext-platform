//! T-0806: an identifier alone is held to the grant, before the broker is asked (GW11, R24).
//!
//! A batch delete is an array of URN strings and an addressed write carries its id in the
//! path; neither has an entity body for the write guard to read, so each identifier is
//! checked on its own against the grant's types and patterns.

use crate::common;

use axum::body::Body;
use axum::extract::Request;
use axum::http::{Method, StatusCode};
use axum::response::IntoResponse;
use axum::routing::any;
use axum::Router;
use context_gateway::app::{router, Gateway};
use context_gateway::pdp::PolicyPdp;
use context_gateway::proxy::Broker;
use context_gateway::resolver::Endpoint;
use jc_core::kinds::{Audience, PolicySpec, Representation};
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
use tower::ServiceExt;

const SLUG: &str = "3ecozggnnhjlp5miouhia53mr2";
const SPACE: &str = "ovzdusie";
const DOMAIN: &str = "banskabystrica.sk";

type Hops = Arc<Mutex<Vec<String>>>;

/// A broker that records every hop and answers 204 to all of them.
async fn broker() -> (String, Hops) {
    let hops: Hops = Arc::new(Mutex::new(Vec::new()));
    let recorder = Arc::clone(&hops);
    let app = Router::new().fallback(any(move |request: Request| {
        let recorder = Arc::clone(&recorder);
        async move {
            recorder.lock().expect("the hop log").push(format!(
                "{} {}",
                request.method(),
                request.uri().path()
            ));
            StatusCode::NO_CONTENT
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

/// Anonymous callers may delete lamps, one by one or in a batch, and nothing else.
fn lamps_only() -> PolicySpec {
    serde_norway::from_str(&format!(
        "contextSpaceRef: {SPACE}\n\
         assigner: did:web:{DOMAIN}\n\
         assignee: {{ kind: role, id: public }}\n\
         operations: [deleteEntity, deleteBatch]\n\
         information:\n\
         \x20 - entities:\n\
         \x20     - type: Device\n\
         \x20       idPattern: \"^urn:ngsi-ld:Device:banskabystrica\\\\.sk:ovzdusie:lamps-.*$\"\n"
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
        project: SPACE.to_owned(),
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
        policies: vec![lamps_only()],
    }
}

async fn send(method: Method, uri: &str, body: Option<Value>) -> (StatusCode, Vec<String>) {
    send_with(method, uri, body, Some("test")).await
}

/// `send`, with `reason` as the JC-Reason a delete names (ADR-N-054 2.3, DM-113).
async fn send_with(
    method: Method,
    uri: &str,
    body: Option<Value>,
    reason: Option<&str>,
) -> (StatusCode, Vec<String>) {
    let (upstream, hops) = broker().await;
    let gateway = Arc::new(
        Gateway::new(Broker::new(upstream), Box::new(PolicyPdp), DOMAIN).serve([endpoint()]),
    );
    let mut request = Request::builder()
        .method(method)
        .uri(format!("/api/endpoint/{SLUG}/ngsi-ld/v1{uri}"));
    if body.is_some() {
        request = request.header("content-type", "application/ld+json");
    }
    if let Some(reason) = reason {
        request = request.header("jc-reason", reason);
    }
    let response = router(gateway)
        .oneshot(
            request
                .body(body.map_or_else(Body::empty, |b| Body::from(b.to_string())))
                .expect("a request"),
        )
        .await
        .expect("the gateway answers");
    let asked = hops.lock().expect("the hop log").clone();
    (response.status(), asked)
}

#[tokio::test]
async fn a_batch_delete_inside_the_grant_reaches_the_broker() {
    let (status, asked) = send(
        Method::POST,
        "/entityOperations/delete",
        Some(json!([
            "urn:ngsi-ld:Device:banskabystrica.sk:ovzdusie:lamps-7"
        ])),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(asked, vec!["POST /ngsi-ld/v1/entityOperations/delete"]);

    // The same batch without a reason is refused before the broker.
    let (status, asked) = send_with(
        Method::POST,
        "/entityOperations/delete",
        Some(json!([
            "urn:ngsi-ld:Device:banskabystrica.sk:ovzdusie:lamps-7"
        ])),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(asked.is_empty(), "the broker was asked: {asked:?}");
}

type Bodies = Arc<Mutex<Vec<Value>>>;

/// Anonymous callers may create and upsert lamps in a batch, and nothing else.
fn lamps_create() -> PolicySpec {
    serde_norway::from_str(&format!(
        "contextSpaceRef: {SPACE}\n\
         assigner: did:web:{DOMAIN}\n\
         assignee: {{ kind: role, id: public }}\n\
         operations: [createBatch, upsertBatch]\n\
         information:\n\
         \x20 - entities:\n\
         \x20     - type: Device\n\
         \x20       idPattern: \"^urn:ngsi-ld:Device:banskabystrica\\\\.sk:ovzdusie:lamps-.*$\"\n"
    ))
    .expect("the policy spec parses")
}

fn device(local: &str) -> Value {
    json!({
        "id": format!("urn:ngsi-ld:Device:{DOMAIN}:{SPACE}:{local}"),
        "type": "Device",
        "@context": "https://uri.etsi.org/ngsi-ld/v1/ngsi-ld-core-context.jsonld",
    })
}

fn id(local: &str) -> String {
    format!("urn:ngsi-ld:Device:{DOMAIN}:{SPACE}:{local}")
}

/// Sends one batch through the gateway to a broker that records every body it is sent and
/// answers what `answer` makes of it; returns the status, the JSON answer (Null when empty) and
/// the bodies the broker saw.
async fn batch(
    policy: PolicySpec,
    uri: &str,
    entities: Value,
    answer: fn(&Value) -> (StatusCode, Option<Value>),
) -> (StatusCode, Value, Vec<Value>) {
    let bodies: Bodies = Arc::new(Mutex::new(Vec::new()));
    let recorder = Arc::clone(&bodies);
    let app = Router::new().fallback(any(move |request: Request| {
        let recorder = Arc::clone(&recorder);
        async move {
            let bytes = axum::body::to_bytes(request.into_body(), usize::MAX)
                .await
                .expect("the forwarded body");
            let sent: Value = serde_json::from_slice(&bytes).expect("a JSON batch");
            let (status, body) = answer(&sent);
            recorder.lock().expect("the body log").push(sent);
            match body {
                Some(body) => (status, body.to_string()).into_response(),
                None => status.into_response(),
            }
        }
    }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("a free port");
    let address = listener.local_addr().expect("the bound address");
    tokio::spawn(async move {
        let _ = axum::serve(listener, common::plain_tree(app)).await;
    });
    let mut serving = endpoint();
    serving.policies = vec![policy];
    let gateway = Arc::new(
        Gateway::new(
            Broker::new(format!("http://{address}")),
            Box::new(PolicyPdp),
            DOMAIN,
        )
        .serve([serving]),
    );
    let response = router(gateway)
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri(format!("/api/endpoint/{SLUG}/ngsi-ld/v1{uri}"))
                .header("content-type", "application/ld+json")
                .body(Body::from(entities.to_string()))
                .expect("a request"),
        )
        .await
        .expect("the gateway answers");
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("the answer");
    let body = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    let seen = bodies.lock().expect("the body log").clone();
    (status, body, seen)
}

/// The broker's answer to a batch create it applies whole: 201 and the ids (CIM 009 5.6.7).
fn created(sent: &Value) -> (StatusCode, Option<Value>) {
    let ids: Vec<Value> = sent
        .as_array()
        .map(|entities| entities.iter().map(|entity| entity["id"].clone()).collect())
        .unwrap_or_default();
    (StatusCode::CREATED, Some(Value::Array(ids)))
}

#[tokio::test]
async fn a_batch_the_grants_permit_whole_is_the_brokers_own_answer() {
    let (status, body, bodies) = batch(
        lamps_create(),
        "/entityOperations/create",
        json!([device("lamps-1"), device("lamps-2")]),
        created,
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "no 207 wrapper");
    assert_eq!(body, json!([id("lamps-1"), id("lamps-2")]));
    assert_eq!(bodies.len(), 1);
}

/// Edge cases: a batch item that is no URN is a bad request, an empty batch is the broker's
/// to answer, and the attribute fragment of an addressed write is held to the path id.
#[tokio::test]
async fn a_malformed_batch_item_is_a_bad_request_and_an_empty_batch_is_forwarded() {
    let (status, asked) = send(
        Method::POST,
        "/entityOperations/delete",
        Some(json!(["not-a-urn"])),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(asked.is_empty(), "the broker was asked: {asked:?}");

    // A malformed id beside a granted one still refuses the batch whole: it is no grant
    // decision to divide by (PF-42).
    let (status, asked) = send(
        Method::POST,
        "/entityOperations/delete",
        Some(json!([
            "urn:ngsi-ld:Device:banskabystrica.sk:ovzdusie:lamps-7",
            "not-a-urn"
        ])),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(asked.is_empty(), "the broker was asked: {asked:?}");

    // An entry that names no entity leaves nothing to answer it under: the batch is malformed.
    for entries in [
        json!([7]),
        json!([{ "type": "Device" }]),
        json!([{ "id": 7 }]),
    ] {
        let (status, asked) = send(Method::POST, "/entityOperations/delete", Some(entries)).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(asked.is_empty(), "the broker was asked: {asked:?}");
    }

    let (status, asked) = send(Method::POST, "/entityOperations/delete", Some(json!([]))).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(asked.len(), 1, "an empty batch is the broker's to refuse");
}
