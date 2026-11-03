//! The gateway passes only the metadata members a caller's grant lists per attribute (ADR-N-066
//! 2.4, DM-139) and a public version answer carries no actor and a reason only where the
//! attribute lists it (DM-151), through the NGSI-LD view surface.

use crate::common;

use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use context_gateway::app::{router, Gateway};
use context_gateway::pdp::PolicyPdp;
use context_gateway::proxy::Broker;
use context_gateway::resolver::Endpoint;
use jc_core::kinds::{Audience, PolicySpec, Representation};
use serde_json::{json, Value};
use std::sync::Arc;
use tower::ServiceExt;

const OPEN: &str = "eeeeeeeeeeeeeeeeeeeeeeeeee";
const ENTITY: &str = "urn:ngsi-ld:AirQualityObserved:st-1";

fn public_grant() -> PolicySpec {
    serde_norway::from_str(
        "contextSpaceRef: ovzdusie\nassigner: did:web:banskabystrica.sk\n\
         assignee: { kind: role, id: public }\n\
         operations: [queryEntity, retrieveEntity, queryTemporal, retrieveTemporal]\n\
         actions: [readVersions]\n\
         information:\n- entities: [{ type: AirQualityObserved }]\n  attributes:\n    \
         name: { versions: [read], metadata: [observedAt, reason] }\n    \
         pm10: { metadata: [observedAt] }\n",
    )
    .expect("the policy spec parses")
}

fn endpoint() -> Endpoint {
    Endpoint {
        declared_types: None,
        roles: Default::default(),
        slug: OPEN.to_owned(),
        title: Default::default(),
        description: Default::default(),
        space: "ovzdusie".to_owned(),
        project: "ovzdusie".to_owned(),
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
        page: None,
        base_path: format!("/api/endpoint/{OPEN}"),
        models: Vec::new(),
        policy_names: Vec::new(),
        manifest_name: String::new(),
        policies: vec![public_grant()],
    }
}

fn version(reason: &str) -> Value {
    json!({
        "type": "Property", "value": 41, "observedAt": "2026-11-01T10:00:00Z", "unitCode": "GQ",
        "instanceId": "urn:ngsi-ld:i:1", "supersededAt": "2026-11-01T11:00:00Z",
        "modifiedBy": { "organization": "city", "actorKind": "person", "actor": "0f5a" },
        "provenance": { "writtenAt": "2026-11-01T10:00:01Z", "operation": "update",
                        "organization": "city", "actorKind": "person", "actor": "0f5a",
                        "reason": reason, "channel": "ui" }
    })
}

async fn get(broker: &str, path: &str) -> (StatusCode, Value) {
    let gateway = Gateway::new(
        Broker::new(broker),
        Box::new(PolicyPdp),
        "banskabystrica.sk",
    )
    .serve(vec![endpoint()]);
    let request = Request::builder()
        .method(Method::GET)
        .uri(format!("/api/endpoint/{OPEN}/ngsi-ld/v1{path}"))
        .body(Body::empty())
        .expect("a request");
    let response = router(Arc::new(gateway))
        .oneshot(request)
        .await
        .expect("the gateway answers");
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .expect("a body");
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

#[tokio::test]
async fn a_public_version_answer_keeps_only_the_listed_members() {
    let broker = common::BrokerStub::start(vec![json!({
        "id": ENTITY, "type": "AirQualityObserved",
        "name": [version("renamed after the survey")],
        "pm10": [version("Sensor stuck, checked by Jana")],
    })])
    .await;
    let (status, body) = get(
        &broker.url,
        &format!("/temporal/entities/{ENTITY}?versions=all"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let name = &body["name"][0];
    let pm10 = &body["pm10"][0];
    assert_eq!(name["provenance"]["reason"], "renamed after the survey");
    assert!(pm10["provenance"].get("reason").is_none(), "{pm10}");
    for attribute in [name, pm10] {
        assert!(
            attribute["provenance"].get("actor").is_none(),
            "{attribute}"
        );
        assert!(
            attribute["provenance"].get("channel").is_none(),
            "{attribute}"
        );
        assert!(attribute.get("modifiedBy").is_none(), "{attribute}");
        assert!(attribute.get("unitCode").is_none(), "{attribute}");
        assert_eq!(attribute["observedAt"], "2026-11-01T10:00:00Z");
        assert_eq!(attribute["provenance"]["actorKind"], "person");
        assert_eq!(attribute["value"], 41);
    }
}

#[tokio::test]
async fn a_query_on_a_stripped_member_is_refused_before_the_broker() {
    let broker = common::BrokerStub::start(vec![json!([])]).await;
    let (status, body) = get(
        &broker.url,
        "/entities?type=AirQualityObserved&q=pm10.unitCode%3D%3D%22GQ%22",
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(
        body["detail"]
            .as_str()
            .unwrap_or_default()
            .contains("pm10.unitCode"),
        "{body}"
    );
    assert!(broker.hops().is_empty(), "{:?}", broker.hops());
    let (status, _) = get(
        &broker.url,
        "/entities?type=AirQualityObserved&q=pm10.observedAt%3E%3D2026-11-01T00:00:00Z",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
}
