//! A grant's type and an answer's type are compared as IRIs (T-3473; EP-26, R24, T-2130).
//!
//! The gateway admits only the NGSI-LD core context (T-3287), so a type name means what the
//! core context makes of it: a term is in the default vocabulary, an absolute IRI is itself. A
//! retrieve by id names no type, so the answer's type is the only type check, and an Entity of
//! `https://other.example/Depot` is not one of the `Depot` a grant names, however its last
//! segment reads.

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
use std::sync::{Arc, Mutex};
use tower::ServiceExt;

const SLUG: &str = "4kq7wz2mtd9xbn5rfhj3cv8pgs";
const SPACE: &str = "fleet";
const DOMAIN: &str = "hel.fi";
const ID: &str = "urn:ngsi-ld:Depot:hel.fi:fleet:north";
const DEFAULT_VOCAB: &str = "https://uri.etsi.org/ngsi-ld/default-context/";

/// A broker that answers `entity` to whatever it is asked, keeping every body it was sent.
async fn broker(entity: Value) -> (String, Arc<Mutex<Vec<String>>>) {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let kept = seen.clone();
    let app = Router::new().fallback(any(move |request: Request| {
        let entity = entity.clone();
        let kept = kept.clone();
        async move {
            let bytes = axum::body::to_bytes(request.into_body(), usize::MAX)
                .await
                .unwrap_or_default();
            if let Ok(mut bodies) = kept.lock() {
                bodies.push(String::from_utf8_lossy(&bytes).into_owned());
            }
            let answer = if request_is_a_list(&bytes) {
                json!([entity])
            } else {
                entity
            };
            axum::Json(answer)
        }
    }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("a free port");
    let address = listener.local_addr().expect("the bound address");
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (format!("http://{address}"), seen)
}

/// A batch query is answered with a list; a retrieve with the entity itself.
fn request_is_a_list(body: &[u8]) -> bool {
    !body.is_empty()
}

fn endpoint(granted: &str) -> Endpoint {
    let policy: PolicySpec = serde_norway::from_str(&format!(
        "contextSpaceRef: {SPACE}\n\
         assigner: did:web:{DOMAIN}\n\
         assignee: {{ kind: role, id: public }}\n\
         operations: [queryEntity, retrieveEntity, queryBatch, retrieveTemporal, queryTemporal]\n\
         information:\n\
         \x20 - entities:\n\
         \x20     - type: {granted}\n"
    ))
    .expect("the policy spec parses");
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
        representations: vec![Representation::NgsiLd],
        rate_limit: None,
        creates: None,
        file_limits: None,
        hidden_attributes: Default::default(),
        projection: None,
        view_mapping: None,
        catalog: None,
        base_path: format!("/api/endpoint/{SLUG}"),
        models: vec![Model {
            name: "fleet".to_owned(),
            version: "1.0.0".to_owned(),
            major: 1,
            classes: vec!["Vehicle".to_owned(), "Depot".to_owned()],
            json_schema: None,
            context: None,
        }],
        policies: vec![policy],
    }
}

/// One request under a grant on `granted`, against a broker answering the entity `id` typed
/// `typed`.
async fn ask(
    granted: &str,
    id: &str,
    typed: Value,
    method: Method,
    uri: &str,
    body: Option<Value>,
) -> (StatusCode, String, Vec<String>) {
    let entity = json!({
        "id": id,
        "type": typed,
        "name": { "type": "Property", "value": "North depot" }
    });
    let (upstream, seen) = broker(entity).await;
    let gateway = Arc::new(
        Gateway::new(Broker::new(upstream), Box::new(PolicyPdp), DOMAIN).serve([endpoint(granted)]),
    );
    let mut builder = Request::builder()
        .method(method)
        .uri(format!("/api/endpoint/{SLUG}{uri}"));
    if body.is_some() {
        builder = builder.header("content-type", "application/json");
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
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("a body");
    let sent = seen.lock().map(|bodies| bodies.clone()).unwrap_or_default();
    (status, String::from_utf8_lossy(&bytes).into_owned(), sent)
}

async fn retrieve(granted: &str, typed: Value) -> (StatusCode, String) {
    retrieve_id(granted, ID, typed).await
}

async fn retrieve_id(granted: &str, id: &str, typed: Value) -> (StatusCode, String) {
    let (status, answer, _) = ask(
        granted,
        id,
        typed,
        Method::GET,
        &format!("/ngsi-ld/v1/entities/{id}"),
        None,
    )
    .await;
    (status, answer)
}

#[tokio::test]
async fn an_entity_of_another_vocabularys_same_named_type_is_not_found() {
    let (status, answer) = retrieve("Depot", json!("https://other.example/Depot")).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{answer}");
    assert!(!answer.contains("North depot"), "{answer}");
    let (status, answer) = retrieve("Depot", json!("https://other.example/ns#Depot")).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{answer}");
}

#[tokio::test]
async fn the_granted_type_is_served_compacted_or_expanded() {
    for typed in [json!("Depot"), json!(format!("{DEFAULT_VOCAB}Depot"))] {
        let (status, answer) = retrieve("Depot", typed.clone()).await;
        assert_eq!(status, StatusCode::OK, "{typed}: {answer}");
        assert!(answer.contains("North depot"), "{typed}: {answer}");
    }
}

#[tokio::test]
async fn a_multi_typed_entity_is_served_by_the_type_it_is_granted_as() {
    let (status, answer) = retrieve_id(
        "Vehicle",
        "urn:ngsi-ld:Vehicle:hel.fi:fleet:bus-01",
        json!(["https://other.example/Depot", "Vehicle"]),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{answer}");
}

/// A compact IRI names a prefix the core context does not define: the gateway cannot tell
/// which type it is, so it is not served.
#[tokio::test]
async fn a_type_that_cannot_be_expanded_is_not_served() {
    let (status, answer) = retrieve("Depot", json!("other:Depot")).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{answer}");
    let (status, answer) = retrieve("Depot", json!("")).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{answer}");
}

/// The batch query's selector is judged the same way before the broker is asked: an entry for
/// another vocabulary's `Depot` is not one of the grant's.
#[tokio::test]
async fn a_batch_query_for_another_vocabularys_type_is_not_forwarded() {
    let (status, answer, sent) = ask(
        "Depot",
        ID,
        json!("https://other.example/Depot"),
        Method::POST,
        "/ngsi-ld/v1/entityOperations/query",
        Some(json!({
            "type": "Query",
            "entities": [{ "type": "https://other.example/Depot" }]
        })),
    )
    .await;
    assert!(status.is_success(), "{status}: {answer}");
    assert!(!answer.contains("North depot"), "{answer}");
    assert!(
        sent.iter().all(|body| !body.contains("other.example")),
        "the selector reached the broker: {sent:?}"
    );
}
