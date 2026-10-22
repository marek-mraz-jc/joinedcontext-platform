//! T-4011: every write through an Endpoint checked against the slot constraints of the space's
//! model (DM-86, DM-27), with the compilation the Portal's pipeline stage checks a record
//! against (`jc_manifest::model_schema`).
//!
//! The model holds `pm10` between 0 and 1000, `window` to `day` or `month`, `stationCode` to a
//! pattern and requires `dateObserved`. A broken constraint is a 400 of type `quality-rule`
//! naming the SHACL component and the attribute, never the value, before the broker is asked.

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
use jc_core::kinds::{Audience, MissingUnitCode, PolicySpec, Representation};
use jc_manifest::model_schema::ModelSchema;
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
use tower::ServiceExt;

const SLUG: &str = "k7r2m4xq9vbn3tdw6hcy5pajfe";
const SPACE: &str = "ovzdusie";
const DOMAIN: &str = "banskabystrica.sk";
const ID: &str = "urn:ngsi-ld:AirQualityObserved:banskabystrica.sk:ovzdusie:st-1";

fn model() -> Value {
    json!({ "definitions": {
        "Window": { "enum": ["day", "month"], "type": "string" },
        "AirQualityObserved": {
            "properties": {
                "dateObserved": { "type": "string", "format": "date-time", "x-ngsi-ld-kind": "Property" },
                "pm10": { "type": ["number", "null"], "minimum": 0, "maximum": 1000, "x-ngsi-ld-kind": "Property",
                          "x-unit": { "exactMappings": ["ucefact:GQ"] } },
                "window": { "$ref": "#/definitions/Window", "x-ngsi-ld-kind": "Property" },
                "stationCode": { "type": "string", "pattern": "^[A-Z]{2}[0-9]{3}$", "x-ngsi-ld-kind": "Property" }
            },
            "required": ["id", "dateObserved"]
        }
    } })
}

/// A model of the same class that constrains nothing but the attributes' kinds.
fn unconstrained() -> Value {
    json!({ "definitions": { "AirQualityObserved": { "properties": {
        "pm10": { "x-ngsi-ld-kind": "Property" }
    } } } })
}

fn compiled(model: &Value) -> Arc<ModelSchema> {
    Arc::new(ModelSchema::compile_for_space(
        "bb-air-quality",
        "1.0.0",
        model,
        &["AirQualityObserved".to_owned()],
        true,
        MissingUnitCode::Fill,
    ))
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
         operations: [createEntity, appendAttrs, updateAttrs, upsertBatch, createBatch, replaceEntity]\n\
         information:\n\
         \x20 - entities:\n\
         \x20     - type: AirQualityObserved\n"
    ))
    .expect("the policy spec parses")
}

fn endpoint(model: Option<&Value>) -> Endpoint {
    Endpoint {
        declared_types: Some(
            DeclaredTypes {
                model: "bb-air-quality".into(),
                classes: ["AirQualityObserved".to_owned()].into(),
                units: model
                    .map(|model| UnitRules::from_schema(model, MissingUnitCode::Fill))
                    .unwrap_or_default(),
                relationships: Default::default(),
                quality: Vec::new(),
                schema: model.map(compiled),
            }
            .into(),
        ),
        catalog: None,
        deprecation: None,
        feeds: Vec::new(),
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

async fn send(model: Option<&Value>, method: Method, uri: &str, body: Value) -> Answer {
    let (upstream, seen) = broker().await;
    let gateway = Arc::new(
        Gateway::new(Broker::new(upstream), Box::new(PolicyPdp), DOMAIN).serve([endpoint(model)]),
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

fn reading() -> Value {
    json!({ "id": ID, "type": "AirQualityObserved",
        "dateObserved": { "type": "Property", "value": "2026-10-12T06:00:00Z" },
        "pm10": { "type": "Property", "value": 18.4, "unitCode": "GQ" },
        "window": { "type": "Property", "value": "day" },
        "stationCode": { "type": "Property", "value": "BB001" } })
}

/// `reading()` with `attribute` set to `value`, or left out for `None`.
fn with(attribute: &str, value: Option<Value>) -> Value {
    let mut entity = reading();
    match value {
        Some(value) => entity[attribute] = json!({ "type": "Property", "value": value }),
        None => {
            entity.as_object_mut().map(|e| e.remove(attribute));
        }
    }
    entity
}

/// The component and the attribute a refusal names.
fn refused(answer: &Answer) -> (String, String) {
    assert_eq!(answer.status, StatusCode::BAD_REQUEST, "{}", answer.body);
    assert_eq!(
        answer.body["code"],
        json!("quality-rule"),
        "{}",
        answer.body
    );
    assert!(answer.broker_saw.is_empty(), "the broker is not asked");
    let named = |key: &str| answer.body[key].as_str().unwrap_or_default().to_owned();
    (named("rule"), named("field"))
}

#[tokio::test]
async fn a_value_under_the_minimum_is_a_400_naming_the_component_and_never_the_value() {
    let model = model();
    let entity = with("pm10", Some(json!(-7.25)));
    let answer = send(Some(&model), Method::POST, "/entities", entity).await;
    assert_eq!(refused(&answer), ("sh:minInclusive".into(), "pm10".into()));
    let detail = answer.body["detail"].as_str().unwrap_or_default();
    assert!(
        detail.contains("pm10") && detail.contains("DM-86"),
        "{detail}"
    );
    assert!(
        !answer.body.to_string().contains("7.25"),
        "never the value: {}",
        answer.body
    );
}

#[tokio::test]
async fn each_slot_constraint_refuses_by_its_shacl_component() {
    let model = model();
    for (entity, want) in [
        (with("pm10", Some(json!(1500))), ("sh:maxInclusive", "pm10")),
        (with("pm10", Some(json!("high"))), ("sh:datatype", "pm10")),
        (with("window", Some(json!("week"))), ("sh:in", "window")),
        (
            with("stationCode", Some(json!("bb-1"))),
            ("sh:pattern", "stationCode"),
        ),
        (with("dateObserved", None), ("sh:minCount", "dateObserved")),
    ] {
        let answer = send(Some(&model), Method::POST, "/entities", entity).await;
        assert_eq!(refused(&answer), (want.0.into(), want.1.into()));
    }
}

#[tokio::test]
async fn a_valid_entity_in_any_form_reaches_the_broker() {
    let model = model();
    let concise = json!({ "id": ID,
        "type": "AirQualityObserved",
        "dateObserved": { "@type": "DateTime", "@value": "2026-10-12T06:00:00Z" },
        "pm10": 3, "window": "month", "stationCode": { "value": "BB002" } });
    let typed = with(
        "dateObserved",
        Some(json!({ "@type": "DateTime", "@value": "2026-10-12T06:00:00Z" })),
    );
    for (uri, body) in [
        ("/entities", reading()),
        ("/entities", concise),
        ("/entities", typed),
        ("/entityOperations/create", json!([reading()])),
    ] {
        let answer = send(Some(&model), Method::POST, uri, body).await;
        assert_eq!(answer.status, StatusCode::CREATED, "{uri}: {}", answer.body);
        assert_eq!(answer.broker_saw.len(), 1);
    }
}

#[tokio::test]
async fn a_fragment_is_checked_for_what_it_carries_and_never_for_what_is_required() {
    let model = model();
    let attrs = format!("/entities/{ID}/attrs");
    let pm10 = json!({ "pm10": { "type": "Property", "value": 4 } });
    let answer = send(Some(&model), Method::POST, &attrs, pm10).await;
    assert_eq!(answer.status, StatusCode::CREATED, "{}", answer.body);
    let upsert = json!([{ "id": ID, "type": "AirQualityObserved", "pm10": 5 }]);
    let answer = send(
        Some(&model),
        Method::POST,
        "/entityOperations/upsert",
        upsert,
    )
    .await;
    assert_eq!(answer.status, StatusCode::CREATED, "{}", answer.body);

    let below = json!({ "pm10": { "type": "Property", "value": -1 } });
    let answer = send(Some(&model), Method::POST, &attrs, below).await;
    assert_eq!(refused(&answer), ("sh:minInclusive".into(), "pm10".into()));
    let bare = json!({ "type": "Property", "value": "week" });
    let answer = send(
        Some(&model),
        Method::PATCH,
        &format!("{attrs}/window"),
        bare,
    )
    .await;
    assert_eq!(refused(&answer), ("sh:in".into(), "window".into()));
}

#[tokio::test]
async fn every_instance_of_an_attribute_is_checked() {
    let model = model();
    let mut two = reading();
    two["pm10"] = json!([
        { "type": "Property", "value": 3, "datasetId": "urn:ngsi-ld:Dataset:a" },
        { "type": "Property", "value": -3, "datasetId": "urn:ngsi-ld:Dataset:b" }
    ]);
    let answer = send(Some(&model), Method::POST, "/entities", two).await;
    assert_eq!(refused(&answer), ("sh:minInclusive".into(), "pm10".into()));
}

/// T-4011: a batch is judged per entity (GW18's 207): the refused member is named in `errors`
/// with its component and never its value, the rest reaches the broker. A pipeline's page of
/// 1000 no longer loses 999 good records to one bad one.
#[tokio::test]
async fn a_batch_forwards_its_valid_members_and_names_each_refused_one() {
    let model = model();
    let other = "urn:ngsi-ld:AirQualityObserved:banskabystrica.sk:ovzdusie:st-2";
    let mut week = with("window", Some(json!("week")));
    week["id"] = json!(other);
    for uri in ["/entityOperations/upsert", "/entityOperations/create"] {
        let batch = json!([reading(), week.clone()]);
        let answer = send(Some(&model), Method::POST, uri, batch).await;
        assert_eq!(
            answer.status,
            StatusCode::MULTI_STATUS,
            "{uri}: {}",
            answer.body
        );
        let errors = answer.body["errors"].as_array().expect("errors").clone();
        assert_eq!(errors.len(), 1, "{}", answer.body);
        assert_eq!(errors[0]["entityId"], json!(other));
        assert_eq!(errors[0]["error"]["rule"], json!("sh:in"));
        assert!(
            !answer.body.to_string().contains("week"),
            "never the value: {}",
            answer.body
        );
        assert_eq!(answer.broker_saw.len(), 1);
        let forwarded = answer.broker_saw[0].as_array().expect("a batch").clone();
        assert_eq!(forwarded.len(), 1);
        assert_eq!(forwarded[0]["id"], json!(ID));
    }
}

#[tokio::test]
async fn a_batch_refused_member_by_member_never_reaches_the_broker() {
    let model = model();
    let batch = json!([
        with("pm10", Some(json!(-1))),
        with("window", Some(json!("week")))
    ]);
    let answer = send(
        Some(&model),
        Method::POST,
        "/entityOperations/upsert",
        batch,
    )
    .await;
    assert_eq!(answer.status, StatusCode::MULTI_STATUS, "{}", answer.body);
    let rules: Vec<_> = answer.body["errors"]
        .as_array()
        .expect("errors")
        .iter()
        .map(|error| error["error"]["rule"].clone())
        .collect();
    assert_eq!(rules, [json!("sh:minInclusive"), json!("sh:in")]);
    assert_eq!(answer.body["success"], json!([]));
    assert!(answer.broker_saw.is_empty(), "the broker is not asked");
}

#[tokio::test]
async fn a_model_without_constraints_refuses_nothing_new() {
    let loose = unconstrained();
    let bare = json!({ "id": ID, "type": "AirQualityObserved",
        "pm10": { "type": "Property", "value": -9 } });
    for model in [None, Some(&loose)] {
        let answer = send(model, Method::POST, "/entities", bare.clone()).await;
        assert_eq!(answer.status, StatusCode::CREATED, "{}", answer.body);
    }
    let undeclared = json!({ "id": ID, "type": "AirQualityObserved",
        "dateObserved": { "type": "Property", "value": "2026-10-12T06:00:00Z" },
        "note": "not in the model" });
    let model = model();
    let answer = send(Some(&model), Method::POST, "/entities", undeclared).await;
    assert_eq!(
        answer.status,
        StatusCode::CREATED,
        "an undeclared attribute: {}",
        answer.body
    );
}

/// The Portal's pipeline stage checks a record with the same compilation: the gateway's
/// refusal is the first slot constraint `ModelSchema::check` names on it.
#[tokio::test]
async fn the_gateway_and_the_shared_check_reach_one_verdict() {
    let model = model();
    let record = with("stationCode", Some(json!("x")));
    let checked = compiled(&model).check(&record);
    let first = checked.first().map(|p| (p.rule.clone(), p.path.clone()));
    let answer = send(Some(&model), Method::POST, "/entities", record).await;
    assert_eq!(Some(refused(&answer)), first, "{checked:?}");
}

/// DM-88: a caller the Endpoint's Policy refuses learns that, and not which member of its batch
/// broke which rule: the Policy grants no `updateBatch`.
#[tokio::test]
async fn a_batch_the_policy_refuses_names_no_rule_of_its_members() {
    let model = model();
    let batch = json!([reading(), with("window", Some(json!("week")))]);
    let answer = send(
        Some(&model),
        Method::POST,
        "/entityOperations/update",
        batch,
    )
    .await;
    assert!(
        answer.status.is_client_error(),
        "{}: {}",
        answer.status,
        answer.body
    );
    let said = answer.body.to_string();
    assert!(
        !said.contains("sh:in") && !said.contains("window"),
        "{said}"
    );
    assert!(answer.broker_saw.is_empty(), "the broker is not asked");
}

/// DM-131: the system members of DM-130 are no slots. An entity carrying every one of them, and
/// an attribute carrying every attribute member, reaches the broker; an `expiresAt` that is not
/// an RFC 3339 instant is refused at either level.
#[tokio::test]
async fn system_members_pass_and_a_malformed_instant_is_refused() {
    let model = model();
    let at = json!("2026-10-21T06:00:00Z");
    let mut entity = reading();
    for member in jc_core::system_members::INSTANT_MEMBERS {
        if jc_core::system_members::is_entity_system_member(member) {
            entity[member] = at.clone();
        }
        entity["pm10"][member] = at.clone();
    }
    entity["scope"] = json!("/geo/SK/BB");
    entity["pm10"]["datasetId"] = json!("urn:ngsi-ld:Dataset:raw");
    entity["pm10"]["instanceId"] = json!("urn:ngsi-ld:Instance:1");
    let answer = send(Some(&model), Method::POST, "/entities", entity).await;
    assert_eq!(answer.status, StatusCode::CREATED, "{}", answer.body);

    let mut bad = reading();
    bad["expiresAt"] = json!("tomorrow");
    let answer = send(Some(&model), Method::POST, "/entities", bad).await;
    assert_eq!(refused(&answer), ("sh:datatype".into(), "expiresAt".into()));
    let mut bad = reading();
    bad["pm10"]["expiresAt"] = json!("2026-13-01T00:00:00Z");
    let answer = send(Some(&model), Method::POST, "/entities", bad).await;
    assert_eq!(refused(&answer), ("sh:datatype".into(), "pm10".into()));
}
