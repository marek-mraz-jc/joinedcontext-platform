//! T-3902: a space's quality rules on a write through an Endpoint (DM-87..DM-89, DM-93).
//!
//! The space caps `pm10` at 500 (flag) and refuses a negative one (reject). A refused write is a
//! 400 of type `quality-rule` naming the rule and the attribute, before the broker is asked; a
//! flagged one reaches the broker with `qualityFlags` on the attribute; a space without rules
//! forwards exactly what it was sent.

use axum::body::Body;
use axum::extract::Request;
use axum::http::{Method, StatusCode};
use axum::routing::any;
use axum::Router;
use context_gateway::app::{router, Gateway};
use context_gateway::pdp::PolicyPdp;
use context_gateway::proxy::Broker;
use context_gateway::resolver::{DeclaredTypes, Endpoint};
use context_gateway::units::UnitRules;
use jc_core::kinds::{Audience, MissingUnitCode, PolicySpec, QualityRule, Representation};
use serde_json::{json, Value};
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
         operations: [createEntity, appendAttrs, updateAttrs, upsertBatch, deleteAttrs]\n\
         information:\n\
         \x20 - entities:\n\
         \x20     - type: AirQualityObserved\n"
    ))
    .expect("the policy spec parses")
}

fn endpoint(quality: Vec<QualityRule>) -> Endpoint {
    Endpoint {
        declared_types: Some(
            DeclaredTypes {
                model: "bb-air-quality".into(),
                classes: ["AirQualityObserved".to_owned()].into(),
                units: UnitRules::from_schema(&schema(), MissingUnitCode::Fill),
                relationships: Default::default(),
                quality,
                schema: None,
            }
            .into(),
        ),
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
        creates: None,
        file_limits: None,
        hidden_attributes: Default::default(),
        projection: None,
        view_mapping: None,
        base_path: format!("/api/endpoint/{SLUG}"),
        models: Vec::new(),
        policy_names: Vec::new(),
        manifest_name: String::new(),
        policies: vec![policy()],
    }
}

struct Answer {
    status: StatusCode,
    body: Value,
    broker_saw: Vec<Value>,
}

async fn send(quality: Vec<QualityRule>, method: Method, uri: &str, body: Value) -> Answer {
    let (upstream, seen) = broker().await;
    let gateway = Arc::new(
        Gateway::new(Broker::new(upstream), Box::new(PolicyPdp), DOMAIN).serve([endpoint(quality)]),
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
    let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .expect("the body");
    let broker_saw = seen.lock().expect("the body log").clone();
    Answer {
        status,
        body: serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        broker_saw,
    }
}

fn rules() -> Vec<QualityRule> {
    serde_json::from_value(json!([
        {"name": "pm10-plausible", "entityType": "AirQualityObserved", "kind": "range", "attribute": "pm10", "max": 500},
        {"name": "pm10-positive", "entityType": "AirQualityObserved", "kind": "range", "attribute": "pm10", "min": 0, "outcome": "reject"}
    ]))
    .expect("the rules parse")
}

fn reading(pm10: f64) -> Value {
    json!({ "id": ID, "type": "AirQualityObserved",
        "pm10": { "type": "Property", "value": pm10, "unitCode": "GQ" } })
}

#[tokio::test]
async fn a_rejected_value_is_a_400_naming_the_rule_and_the_broker_is_not_asked() {
    let answer = send(rules(), Method::POST, "/entities", reading(-3.5)).await;
    assert_eq!(answer.status, StatusCode::BAD_REQUEST, "{}", answer.body);
    assert_eq!(
        answer.body["code"],
        json!("quality-rule"),
        "{}",
        answer.body
    );
    let detail = answer.body["detail"].as_str().unwrap_or_default();
    assert!(
        detail.contains("`pm10-positive`") && detail.contains("`pm10`"),
        "{detail}"
    );
    assert!(
        !answer.body.to_string().contains("-3.5"),
        "never the value: {}",
        answer.body
    );
    assert!(answer.broker_saw.is_empty());
}

#[tokio::test]
async fn a_flagged_value_reaches_the_broker_with_its_flag_and_a_clean_one_without() {
    let answer = send(rules(), Method::POST, "/entities", reading(900.0)).await;
    assert_eq!(answer.status, StatusCode::CREATED, "{}", answer.body);
    assert_eq!(
        answer.broker_saw[0]["pm10"]["qualityFlags"]["value"],
        json!(["pm10-plausible"])
    );
    let uri = format!("/entities/{ID}/attrs");
    let mut forged = json!({ "pm10": { "type": "Property", "value": 12.0, "unitCode": "GQ" } });
    forged["pm10"]["qualityFlags"] = json!({ "type": "Property", "value": ["pm10-plausible"] });
    let answer = send(rules(), Method::PATCH, &uri, forged).await;
    assert!(answer.status.is_success(), "{}", answer.body);
    assert!(
        answer.broker_saw[0]["pm10"].get("qualityFlags").is_none(),
        "a write that breaks nothing stores no flag: {}",
        answer.broker_saw[0]
    );
}

#[tokio::test]
async fn a_space_without_rules_forwards_what_it_was_sent() {
    let answer = send(Vec::new(), Method::POST, "/entities", reading(-3.5)).await;
    assert_eq!(answer.status, StatusCode::CREATED, "{}", answer.body);
    assert_eq!(answer.broker_saw, vec![reading(-3.5)]);
}

fn quarantining() -> Vec<QualityRule> {
    serde_json::from_value(json!([
        {"name": "pm10-hide", "entityType": "AirQualityObserved", "kind": "range", "attribute": "pm10", "max": 500, "outcome": "quarantine"}
    ]))
    .expect("the rules parse")
}

/// DM-90: a write a rule quarantines reaches the broker with the mark; on the one-attribute
/// path, which has nowhere to carry it, it is refused instead of stored unmarked.
#[tokio::test]
async fn a_quarantined_write_carries_the_mark_and_the_attribute_path_refuses_it() {
    let answer = send(quarantining(), Method::POST, "/entities", reading(900.0)).await;
    assert_eq!(answer.status, StatusCode::CREATED, "{}", answer.body);
    let stored = &answer.broker_saw[0];
    assert!(stored.get("quarantinedAt").is_some(), "{stored}");
    assert_eq!(stored["quarantineRules"]["value"], json!(["pm10-hide"]));

    let uri = format!("/entities/{ID}/attrs/pm10");
    let one = json!({ "type": "Property", "value": 900.0, "unitCode": "GQ" });
    let answer = send(quarantining(), Method::PATCH, &uri, one).await;
    assert_eq!(answer.status, StatusCode::BAD_REQUEST, "{}", answer.body);
    assert!(answer.broker_saw.is_empty(), "{:?}", answer.broker_saw);
}

/// DM-90: the mark is the gateway's. A writer's own copy is dropped, and a delete of it is
/// refused before the broker is asked, however the attribute is spelled.
#[tokio::test]
async fn a_writer_can_neither_forge_nor_delete_the_quarantine_mark() {
    let mut forged = reading(12.0);
    forged["quarantinedAt"] = json!({ "type": "Property", "value": "2026-10-12T00:00:00Z" });
    forged["quarantineRules"] = json!({ "type": "Property", "value": ["pm10-hide"] });
    let answer = send(quarantining(), Method::POST, "/entities", forged).await;
    assert_eq!(answer.status, StatusCode::CREATED, "{}", answer.body);
    let stored = &answer.broker_saw[0];
    assert!(
        stored.get("quarantinedAt").is_none() && stored.get("quarantineRules").is_none(),
        "{stored}"
    );

    // A space without rules strips it too: a rule added later must not find forged marks.
    let mut forged = reading(12.0);
    forged["quarantinedAt"] = json!({ "type": "Property", "value": "2026-10-12T00:00:00Z" });
    let answer = send(Vec::new(), Method::POST, "/entities", forged).await;
    assert!(
        answer.broker_saw[0].get("quarantinedAt").is_none(),
        "{}",
        answer.broker_saw[0]
    );

    for attribute in [
        "quarantinedAt",
        "quarantineRules",
        "https%3A%2F%2Furi.etsi.org%2Fngsi-ld%2Fdefault-context%2FquarantinedAt",
    ] {
        let uri = format!("/entities/{ID}/attrs/{attribute}");
        let answer = send(Vec::new(), Method::DELETE, &uri, Value::Null).await;
        assert_eq!(
            answer.status,
            StatusCode::BAD_REQUEST,
            "{attribute}: {}",
            answer.body
        );
        assert!(answer.broker_saw.is_empty(), "{attribute}");
    }
}

/// A caller the grant refuses learns nothing of the rules: 403, not the rule's 400.
#[tokio::test]
async fn a_forbidden_write_that_breaks_a_rule_answers_403() {
    let answer = send(
        rules(),
        Method::PUT,
        &format!("/entities/{ID}"),
        reading(-3.5),
    )
    .await;
    assert_eq!(answer.status, StatusCode::FORBIDDEN, "{}", answer.body);
    assert!(
        !answer.body.to_string().contains("pm10-positive"),
        "{}",
        answer.body
    );
    assert!(answer.broker_saw.is_empty());
}
