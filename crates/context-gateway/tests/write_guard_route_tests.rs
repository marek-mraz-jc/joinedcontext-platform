//! T-0806: an identifier alone is held to the grant, before the broker is asked (GW11, R24).
//!
//! A batch delete is an array of URN strings and an addressed write carries its id in the
//! path; neither has an entity body for the write guard to read, so each identifier is
//! checked on its own against the grant's types and patterns.

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
    // Every entity refused: the batch is answered per entity, and the broker never asked (GW18).
    assert_eq!(status, StatusCode::MULTI_STATUS);
    assert!(asked.is_empty(), "the broker was asked: {asked:?}");
}

#[tokio::test]
async fn a_mixed_batch_delete_applies_the_granted_entity_and_answers_the_other() {
    // One lamp and one traffic light: the lamp is deleted, the traffic light answered as
    // refused (GW18; GW17 judges each entity whole, not the batch).
    let (status, body, bodies) = batch(
        lamps_only(),
        "/entityOperations/delete",
        json!([
            "urn:ngsi-ld:Device:banskabystrica.sk:ovzdusie:lamps-7",
            "urn:ngsi-ld:Device:banskabystrica.sk:ovzdusie:traffic-1"
        ]),
        |_| (StatusCode::NO_CONTENT, None),
    )
    .await;
    assert_eq!(status, StatusCode::MULTI_STATUS);
    assert_eq!(
        bodies,
        vec![json!([
            "urn:ngsi-ld:Device:banskabystrica.sk:ovzdusie:lamps-7"
        ])],
        "only the granted entity reaches the broker"
    );
    assert_eq!(
        body["success"],
        json!(["urn:ngsi-ld:Device:banskabystrica.sk:ovzdusie:lamps-7"])
    );
    assert_eq!(
        body["errors"][0]["entityId"],
        "urn:ngsi-ld:Device:banskabystrica.sk:ovzdusie:traffic-1"
    );
    assert_eq!(body["errors"][0]["error"]["status"], 403);
    assert_eq!(body["errors"].as_array().map(Vec::len), Some(1));
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
        let _ = axum::serve(listener, app).await;
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
async fn batch_207_mixed_outcomes() {
    let (status, body, bodies) = batch(
        lamps_create(),
        "/entityOperations/create",
        json!([device("lamps-1"), device("traffic-1"), device("lamps-2")]),
        created,
    )
    .await;
    assert_eq!(status, StatusCode::MULTI_STATUS);
    assert_eq!(
        bodies,
        vec![json!([device("lamps-1"), device("lamps-2")])],
        "the broker is sent the permitted entities, and only them"
    );
    assert_eq!(body["success"], json!([id("lamps-1"), id("lamps-2")]));
    assert_eq!(
        body["errors"],
        json!([{
            "entityId": id("traffic-1"),
            "error": {
                "type": "https://joinedcontext.com/errors/forbidden",
                "title": "Access Denied by Policy",
                "status": 403,
            },
        }])
    );
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

#[tokio::test]
async fn a_batch_the_grants_refuse_whole_is_207_with_every_entry_an_error() {
    let (status, body, bodies) = batch(
        lamps_create(),
        "/entityOperations/create",
        json!([device("traffic-1"), device("traffic-2")]),
        created,
    )
    .await;
    assert_eq!(status, StatusCode::MULTI_STATUS);
    assert!(bodies.is_empty(), "the broker was asked: {bodies:?}");
    assert_eq!(body["success"], json!([]));
    let errors = body["errors"].as_array().expect("an errors list");
    assert_eq!(errors.len(), 2);
    for (entry, local) in errors.iter().zip(["traffic-1", "traffic-2"]) {
        assert_eq!(entry["entityId"], id(local));
        assert_eq!(entry["error"]["status"], 403);
    }
}

#[tokio::test]
async fn the_brokers_own_207_is_merged_with_the_gateways_refusals() {
    let (status, body, _) = batch(
        lamps_create(),
        "/entityOperations/upsert",
        json!([device("lamps-1"), device("traffic-1"), device("lamps-2")]),
        |_| {
            (
                StatusCode::MULTI_STATUS,
                Some(json!({
                    "success": [id("lamps-1")],
                    "errors": [{
                        "entityId": id("lamps-2"),
                        "error": { "type": "https://uri.etsi.org/ngsi-ld/errors/BadRequestData",
                                   "title": "Bad request data", "status": 400 },
                    }],
                })),
            )
        },
    )
    .await;
    assert_eq!(status, StatusCode::MULTI_STATUS);
    assert_eq!(body["success"], json!([id("lamps-1")]));
    let failed: Vec<&Value> = body["errors"]
        .as_array()
        .expect("an errors list")
        .iter()
        .map(|entry| &entry["entityId"])
        .collect();
    assert_eq!(failed, vec![&json!(id("lamps-2")), &json!(id("traffic-1"))]);
}

#[tokio::test]
async fn a_broker_refusal_of_the_forwarded_part_is_answered_for_each_of_its_entities() {
    let (status, body, _) = batch(
        lamps_create(),
        "/entityOperations/create",
        json!([device("lamps-1"), device("traffic-1")]),
        |_| {
            (
                StatusCode::CONFLICT,
                Some(
                    json!({ "type": "https://uri.etsi.org/ngsi-ld/errors/AlreadyExists",
                             "title": "Already exists", "status": 409 }),
                ),
            )
        },
    )
    .await;
    assert_eq!(status, StatusCode::MULTI_STATUS);
    assert_eq!(body["success"], json!([]));
    assert_eq!(body["errors"][0]["entityId"], id("lamps-1"));
    assert_eq!(body["errors"][0]["error"]["status"], 409);
    assert_eq!(body["errors"][1]["entityId"], id("traffic-1"));
    assert_eq!(body["errors"][1]["error"]["status"], 403);
}

#[tokio::test]
async fn a_broker_failure_under_a_divided_batch_is_not_passed_on_verbatim() {
    let (status, body, _) = batch(
        lamps_create(),
        "/entityOperations/create",
        json!([device("lamps-1"), device("traffic-1")]),
        |_| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Some(json!({ "detail": "pg: relation lamps-1 locked" })),
            )
        },
    )
    .await;
    assert_eq!(status, StatusCode::MULTI_STATUS);
    assert_eq!(body["errors"][0]["entityId"], id("lamps-1"));
    assert_eq!(body["errors"][0]["error"]["status"], 500);
    assert!(
        !body.to_string().contains("pg: relation"),
        "the broker's own words leaked: {body}"
    );
}
