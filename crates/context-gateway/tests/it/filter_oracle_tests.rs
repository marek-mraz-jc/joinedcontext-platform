//! A filter is a read: filtering on an attribute a type may not serve tells the caller its value
//! (T-1862, owner's rule of 2026-09-18; MP-02, R9, EP-61).
//!
//! Before this, `q`, `attrs`, `orderBy` and `geoproperty` reached the broker exactly as the caller
//! wrote them and the answer was stripped afterwards. The rows that came back were the rows that
//! matched, so `q=age>30` over a type whose `age` this endpoint hides still said which entities
//! have an `age` over thirty — and bisecting the number reads the value itself, one comparison at
//! a time. The gateway now decides which types may be *considered* at all: a type stays in the
//! query only if every attribute the request filters or orders on is one it may serve, and a query
//! with no type left is answered as an attribute that does not exist would be answered.
//!
//! The fixture is the one the task names: a projection giving `User` `[age, name]` and `Vehicle`
//! `[weight, location]`, over a broker whose every entity carries every attribute, so a leak shows.

use crate::common;

use axum::body::Body;
use axum::extract::Request;
use axum::http::{Method, StatusCode};
use axum::routing::any;
use axum::Router;
use context_gateway::app::{router, Gateway};
use context_gateway::pdp::PolicyPdp;
use context_gateway::proxy::Broker;
use context_gateway::resolver::{Endpoint, Model};
use jc_core::envelope::ResourceEnvelope;
use jc_core::kinds::{Audience, ModelProjectionSpec, PolicySpec, Representation};
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
use tower::ServiceExt;

const SLUG: &str = "4hq2vbn7kzxr3m5tjw6cyd9pfs";
const SPACE: &str = "fleet";
const DOMAIN: &str = "hel.fi";

const VIEW: &str = r#"apiVersion: joinedcontext.com/v1alpha1
kind: ModelProjection
metadata:
  name: partner-view
  namespace: helsinki
spec:
  contextSpaceRef: fleet
  dataModelRef: { kind: DataModel, name: fleet, version: "1" }
  classes:
    - name: User
      slots: [age, name, homeLocation, ageGroup]
    - name: Vehicle
      slots: [weight, location]
"#;

fn projection() -> Arc<ModelProjectionSpec> {
    let parsed = ResourceEnvelope::<ModelProjectionSpec>::from_yaml(VIEW).expect("parses");
    parsed.validate().expect("valid");
    Arc::new(parsed.spec)
}

/// Every entity carries every attribute, so anything that leaks is visible in the answer.
fn everything() -> Value {
    json!([
        {
            "id": "urn:ngsi-ld:User:hel.fi:fleet:aino",
            "type": "User",
            "name": { "type": "Property", "value": "Aino" },
            "age": { "type": "Property", "value": 41 },
            "ageGroup": { "type": "Property", "value": "adult" },
            "weight": { "type": "Property", "value": 70 },
            "secretPin": { "type": "Property", "value": "1234" }
        },
        {
            "id": "urn:ngsi-ld:Vehicle:hel.fi:fleet:bus-01",
            "type": "Vehicle",
            "weight": { "type": "Property", "value": 12_000 },
            "age": { "type": "Property", "value": 7 },
            "secretPin": { "type": "Property", "value": "4321" }
        }
    ])
}

type Hops = Arc<Mutex<Vec<String>>>;

async fn broker() -> (String, Hops) {
    let hops: Hops = Arc::new(Mutex::new(Vec::new()));
    let recorder = Arc::clone(&hops);
    let app = Router::new().fallback(any(move |request: Request| {
        let recorder = Arc::clone(&recorder);
        async move {
            recorder
                .lock()
                .expect("the hop log")
                .push(request.uri().query().unwrap_or_default().to_owned());
            axum::Json(everything())
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

fn policy() -> PolicySpec {
    serde_norway::from_str(&format!(
        "contextSpaceRef: {SPACE}\n\
         assigner: did:web:{DOMAIN}\n\
         assignee: {{ kind: role, id: public }}\n\
         operations: [queryEntity, retrieveEntity, queryBatch, queryTemporal]\n"
    ))
    .expect("the policy spec parses")
}

fn endpoint(projected: bool, hidden: &[&str]) -> Endpoint {
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
        hidden_attributes: hidden.iter().map(|name| (*name).to_owned()).collect(),
        projection: projected.then(projection),
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
            classes: vec!["User".to_owned(), "Vehicle".to_owned()],
            json_schema: None,
            context: None,
        }],
        policy_names: Vec::new(),
        manifest_name: String::new(),
        policies: vec![policy()],
    }
}

/// One query, with the status, the answer and every query string the broker was asked.
async fn ask(endpoint: Endpoint, uri: &str) -> (StatusCode, Value, Vec<String>) {
    let (upstream, hops) = broker().await;
    let gateway = Arc::new(
        Gateway::new(Broker::new(upstream), Box::new(PolicyPdp), DOMAIN).serve([endpoint]),
    );
    let response = router(gateway)
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri(format!("/api/endpoint/{SLUG}{uri}"))
                .body(Body::empty())
                .expect("a request"),
        )
        .await
        .expect("the gateway answers");
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("a body");
    let body = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    let asked = hops.lock().expect("the hop log").clone();
    (status, body, asked)
}

/// The types one answer carries, in the order they came.
fn types(body: &Value) -> Vec<String> {
    body.as_array()
        .map(|entities| {
            entities
                .iter()
                .filter_map(|entity| entity["type"].as_str())
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

/// A filter on what every entity carries is not a reference to anything a grant could withhold.
#[tokio::test]
async fn filtering_on_the_structure_narrows_nothing() {
    let (status, body, asked) = ask(
        endpoint(true, &[]),
        "/ngsi-ld/v1/entities?type=User,Vehicle&q=createdAt%3E2026-01-01T00:00:00Z",
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        types(&body),
        vec!["User".to_owned(), "Vehicle".to_owned()],
        "{body}"
    );
    assert!(
        asked[0].contains("User") && asked[0].contains("Vehicle"),
        "{}",
        asked[0]
    );
}
