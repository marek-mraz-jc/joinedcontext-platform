//! T-0806: an identifier alone is held to the grant, before the broker is asked (GW11, R24).
//!
//! A batch delete is an array of URN strings and an addressed write carries its id in the
//! path; neither has an entity body for the write guard to read, so each identifier is
//! checked on its own against the grant's types and patterns.

use axum::body::Body;
use axum::extract::Request;
use axum::http::{Method, StatusCode};
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
        let _ = axum::serve(listener, app).await;
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
        file_limits: None,
        hidden_attributes: Default::default(),
        projection: None,
        view_mapping: None,
        base_path: format!("/api/endpoint/{SLUG}"),
        models: Vec::new(),
        policies: vec![lamps_only()],
    }
}

async fn send(method: Method, uri: &str, body: Option<Value>) -> (StatusCode, Vec<String>) {
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
async fn a_batch_delete_of_an_ungranted_type_is_refused_without_a_broker_hop() {
    let (status, asked) = send(
        Method::POST,
        "/entityOperations/delete",
        Some(json!(["urn:ngsi-ld:Secret:banskabystrica.sk:ovzdusie:1"])),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert!(asked.is_empty(), "the broker was asked: {asked:?}");

    // One lamp and one traffic light: the batch is refused whole (GW17).
    let (status, asked) = send(
        Method::POST,
        "/entityOperations/delete",
        Some(json!([
            "urn:ngsi-ld:Device:banskabystrica.sk:ovzdusie:lamps-7",
            "urn:ngsi-ld:Device:banskabystrica.sk:ovzdusie:traffic-1"
        ])),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert!(asked.is_empty(), "the broker was asked: {asked:?}");
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
}

#[tokio::test]
async fn an_addressed_delete_outside_the_pattern_or_type_never_reaches_the_broker() {
    for id in [
        "urn:ngsi-ld:Device:banskabystrica.sk:ovzdusie:traffic-1",
        "urn:ngsi-ld:Secret:banskabystrica.sk:ovzdusie:1",
    ] {
        let (status, asked) = send(Method::DELETE, &format!("/entities/{id}"), None).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{id}");
        assert!(asked.is_empty(), "{id}: the broker was asked: {asked:?}");
    }
    let (status, asked) = send(
        Method::DELETE,
        "/entities/urn:ngsi-ld:Device:banskabystrica.sk:ovzdusie:lamps-7",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(asked.len(), 1);
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

    let (status, asked) = send(Method::POST, "/entityOperations/delete", Some(json!([]))).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(asked.len(), 1, "an empty batch is the broker's to refuse");
}

#[tokio::test]
async fn an_attribute_fragment_for_an_entity_outside_the_pattern_never_reaches_the_broker() {
    let (status, asked) = send(
        Method::PATCH,
        "/entities/urn:ngsi-ld:Device:banskabystrica.sk:ovzdusie:traffic-1/attrs",
        Some(json!({ "status": { "type": "Property", "value": "off" } })),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert!(asked.is_empty(), "the broker was asked: {asked:?}");
}
