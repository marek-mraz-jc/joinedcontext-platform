//! T-2810: a quantity is written in its model's unit or not at all (DM-06).
//!
//! The space's model measures `pm10` in `GQ` (its generated JSON Schema says so in `x-unit`). A
//! write through an Endpoint that says another code is a 400 naming the attribute and both codes,
//! before the broker is asked; a write without a code is given the model's and the answer names
//! it, unless the space's `spec.missingUnitCode` is `refuse`.

use axum::body::Body;
use axum::extract::Request;
use axum::http::{Method, StatusCode};
use axum::routing::any;
use axum::Router;
use context_gateway::app::{router, Gateway};
use context_gateway::pdp::PolicyPdp;
use context_gateway::proxy::Broker;
use context_gateway::resolver::{DeclaredTypes, Endpoint};
use context_gateway::store;
use context_gateway::units::{UnitRules, FILLED_HEADER};
use jc_core::kinds::{Audience, MissingUnitCode, PolicySpec, Representation};
use serde_json::{json, Value};
use std::path::Path;
use std::sync::{Arc, Mutex};
use tower::ServiceExt;

const SLUG: &str = "k7r2m4xq9vbn3tdw6hcy5pajfe";
const SPACE: &str = "ovzdusie";
const DOMAIN: &str = "banskabystrica.sk";
const ID: &str = "urn:ngsi-ld:AirQualityObserved:banskabystrica.sk:ovzdusie:st-1";

fn schema() -> Value {
    json!({ "definitions": { "AirQualityObserved": { "properties": {
        "pm10": { "x-ngsi-ld-kind": "Property", "x-unit": { "exactMappings": ["ucefact:GQ", "qudt-unit:MicroGM-PER-M3"] } },
        "name": { "x-ngsi-ld-kind": "Property" }
    } } } })
}

type Seen = Arc<Mutex<Vec<Value>>>;

/// A broker that records every body it is sent and answers 201.
async fn broker() -> (String, Seen) {
    let seen: Seen = Arc::new(Mutex::new(Vec::new()));
    let recorder = Arc::clone(&seen);
    let app = Router::new().fallback(any(move |request: Request| {
        let recorder = Arc::clone(&recorder);
        async move {
            let bytes = axum::body::to_bytes(request.into_body(), 1 << 20)
                .await
                .unwrap_or_default();
            recorder
                .lock()
                .expect("the body log")
                .push(serde_json::from_slice(&bytes).unwrap_or(Value::Null));
            StatusCode::CREATED
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

fn policy() -> PolicySpec {
    serde_norway::from_str(&format!(
        "contextSpaceRef: {SPACE}\n\
         assigner: did:web:{DOMAIN}\n\
         assignee: {{ kind: role, id: public }}\n\
         operations: [createEntity, appendAttrs, updateAttrs, upsertBatch]\n\
         information:\n\
         \x20 - entities:\n\
         \x20     - type: AirQualityObserved\n"
    ))
    .expect("the policy spec parses")
}

fn endpoint(missing: MissingUnitCode) -> Endpoint {
    Endpoint {
        declared_types: Some(DeclaredTypes {
            model: "bb-air-quality".into(),
            classes: ["AirQualityObserved".to_owned()].into(),
            units: UnitRules::from_schema(&schema(), missing),
            relationships: Default::default(),
        }),
        catalog: None,
        roles: Default::default(),
        slug: SLUG.to_owned(),
        title: Default::default(),
        description: Default::default(),
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
        policies: vec![policy()],
    }
}

struct Answer {
    status: StatusCode,
    filled: Option<String>,
    body: Value,
    broker_saw: Vec<Value>,
}

async fn send(missing: MissingUnitCode, method: Method, uri: &str, body: Value) -> Answer {
    let (upstream, seen) = broker().await;
    let gateway = Arc::new(
        Gateway::new(Broker::new(upstream), Box::new(PolicyPdp), DOMAIN).serve([endpoint(missing)]),
    );
    let response = router(gateway)
        .oneshot(
            Request::builder()
                .method(method)
                .uri(format!("/api/endpoint/{SLUG}/ngsi-ld/v1{uri}"))
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .expect("a request"),
        )
        .await
        .expect("the gateway answers");
    let status = response.status();
    let filled = response
        .headers()
        .get(FILLED_HEADER)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .expect("the body");
    let broker_saw = seen.lock().expect("the body log").clone();
    Answer {
        status,
        filled,
        body: serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        broker_saw,
    }
}

fn reading(pm10: Value) -> Value {
    json!({ "id": ID, "type": "AirQualityObserved", "pm10": pm10 })
}

#[tokio::test]
async fn a_wrong_unit_code_is_a_400_naming_both_codes_and_the_broker_is_not_asked() {
    let answer = send(
        MissingUnitCode::Fill,
        Method::POST,
        "/entities",
        reading(json!({ "type": "Property", "value": 0.03, "unitCode": "GP" })),
    )
    .await;
    assert_eq!(answer.status, StatusCode::BAD_REQUEST);
    let detail = answer.body["detail"].as_str().unwrap_or_default();
    assert!(
        detail.contains("`pm10`") && detail.contains("`GQ`") && detail.contains("`GP`"),
        "{detail}"
    );
    assert!(
        !detail.contains("st-1"),
        "the refusal names the attribute, not the entity: {detail}"
    );
    assert!(answer.broker_saw.is_empty());
}

#[tokio::test]
async fn a_missing_unit_code_is_filled_and_the_answer_says_so() {
    let answer = send(
        MissingUnitCode::Fill,
        Method::POST,
        "/entities",
        reading(json!({ "type": "Property", "value": 34 })),
    )
    .await;
    assert_eq!(answer.status, StatusCode::CREATED);
    assert_eq!(answer.filled.as_deref(), Some("pm10=GQ"));
    assert_eq!(answer.broker_saw[0]["pm10"]["unitCode"], "GQ");
}

#[tokio::test]
async fn a_strict_space_refuses_a_missing_unit_code() {
    let answer = send(
        MissingUnitCode::Refuse,
        Method::POST,
        &format!("/entities/{ID}/attrs"),
        json!({ "pm10": { "type": "Property", "value": 34 } }),
    )
    .await;
    assert_eq!(answer.status, StatusCode::BAD_REQUEST);
    assert!(answer.body["detail"]
        .as_str()
        .unwrap_or_default()
        .contains("needs unitCode `GQ`"));
    assert!(answer.broker_saw.is_empty());
}

#[tokio::test]
async fn a_write_in_the_right_unit_goes_on_unchanged_and_names_nothing() {
    let sent = reading(json!({ "type": "Property", "value": 34, "unitCode": "GQ" }));
    let answer = send(
        MissingUnitCode::Refuse,
        Method::POST,
        "/entities",
        sent.clone(),
    )
    .await;
    assert_eq!(answer.status, StatusCode::CREATED);
    assert_eq!(answer.filled, None);
    assert_eq!(answer.broker_saw, vec![sent]);
}

const MODEL: &str = r#"apiVersion: joinedcontext.com/v1alpha1
kind: DataModel
metadata:
  name: bb-air-quality
  namespace: ovzdusie
spec:
  contextSpaceRef: ovzdusie
  linkml: bb-air-quality.linkml.yaml
  version: 1.0.0
  lifecycle: draft
  classes: [AirQualityObserved]
  artifacts:
    jsonSchema: bb-air-quality.schema.json
"#;

#[test]
fn the_table_reads_the_units_from_the_committed_schema_and_the_space_setting() {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock after the epoch")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("gw-units-{now}"));
    std::fs::create_dir_all(&dir).expect("create the repository");
    let write =
        |name: &str, body: &str| std::fs::write(Path::new(&dir).join(name), body).expect("write");
    write(
        "space.yaml",
        "apiVersion: joinedcontext.com/v1alpha1\nkind: ContextSpace\nmetadata:\n  name: ovzdusie\n  namespace: ovzdusie\n\
         spec:\n  urnSegment: ovzdusie\n  missingUnitCode: refuse\n  dataModelRef: { kind: DataModel, name: bb-air-quality }\n",
    );
    write("model.yaml", MODEL);
    write("bb-air-quality.schema.json", &schema().to_string());

    let (_, spaces, ..) = store::load(&dir).expect("the repository loads");
    let units = &spaces
        .first()
        .and_then(|space| space.endpoint.declared_types.as_ref())
        .expect("the space names its model")
        .units;
    assert_eq!(units.missing, MissingUnitCode::Refuse);
    assert_eq!(units.by_class["AirQualityObserved"]["pm10"], "GQ");
    assert!(!units.by_class["AirQualityObserved"].contains_key("name"));
}
