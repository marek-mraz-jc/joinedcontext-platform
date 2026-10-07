//! EP-99 (T-3265): `GET /api/endpoint/{slug}/openapi.json` is an OpenAPI 3.1 document of the
//! Endpoint's NGSI-LD read surface, generated from its model and projected to the caller: a
//! type or a slot the caller holds no grant over appears nowhere in it, and the document is
//! valid against the OpenAPI 3.1 schema.

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use context_gateway::app::{router, Gateway};
use context_gateway::auth::accounts::ServiceAccounts;
use context_gateway::pdp::PolicyPdp;
use context_gateway::proxy::Broker;
use context_gateway::resolver::{Endpoint, Model};
use jc_core::kinds::{Audience, PolicySpec, Representation};
use serde_json::{json, Value};
use std::sync::Arc;
use tower::ServiceExt;

const OPEN: &str = "k4y7pq2mzt6vhx3nbwrs5cjd8f";
const CLOSED: &str = "p9d2wc5kzn8mth4rqvb7xj3sfy";

fn policy(yaml: &str) -> PolicySpec {
    serde_norway::from_str(yaml).expect("the policy spec parses")
}

fn air_quality() -> Model {
    Model {
        name: "bb-air-quality".to_owned(),
        version: "1.4.0".to_owned(),
        major: 1,
        classes: vec![
            "AirQualityObserved".to_owned(),
            "InternalIncident".to_owned(),
        ],
        json_schema: Some(json!({
            "$schema": "http://json-schema.org/draft-07/schema#",
            "$defs": {
                "AirQualityObserved": {
                    "type": "object",
                    "properties": {
                        "id": { "type": "string" },
                        "type": { "const": "AirQualityObserved" },
                        "pm10": { "type": "number" },
                        "internalNote": { "type": "string" },
                    },
                },
                "InternalIncident": {
                    "type": "object",
                    "properties": { "severity": { "type": "string" } },
                },
            },
        })),
        context: Some(json!({
            "@context": {
                "AirQualityObserved": "https://bb.example.sk/schema/air-quality/AirQualityObserved",
                "InternalIncident": "https://bb.example.sk/schema/air-quality/InternalIncident",
                "pm10": "https://bb.example.sk/schema/air-quality/pm10",
                "internalNote": "https://bb.example.sk/schema/air-quality/internalNote",
            }
        })),
    }
}

fn endpoint(slug: &str, audience: Audience, policies: Vec<PolicySpec>) -> Endpoint {
    Endpoint {
        declared_types: None,
        roles: Default::default(),
        slug: slug.to_owned(),
        title: Default::default(),
        description: Default::default(),
        space: "ovzdusie".to_owned(),
        project: "ovzdusie".to_owned(),
        audience,
        allowed_projects: Vec::new(),
        representations: vec![Representation::NgsiLd],
        rate_limit: None,
        creates: None,
        file_limits: None,
        hidden_attributes: Default::default(),
        projection: None,
        base_path: format!("/api/endpoint/{slug}"),
        models: vec![air_quality()],
        view_mapping: None,
        catalog: None,
        policies,
    }
}

fn narrow_grant() -> Vec<PolicySpec> {
    vec![policy(
        r#"contextSpaceRef: ovzdusie
assigner: did:web:banskabystrica.sk
assignee: { kind: role, id: public }
operations: [queryEntity, retrieveEntity]
information:
  - entities:
      - type: AirQualityObserved
    propertyNames: [pm10]
"#,
    )]
}

/// The same endpoint under a grant that names both classes, so the two indexes can be
/// compared: what one caller sees and the other does not is exactly the narrowing.
fn wide_grant() -> Vec<PolicySpec> {
    vec![policy(
        r#"contextSpaceRef: ovzdusie
assigner: did:web:banskabystrica.sk
assignee: { kind: role, id: public }
operations: [queryEntity, retrieveEntity]
information:
  - entities:
      - type: AirQualityObserved
      - type: InternalIncident
"#,
    )]
}

/// A second public endpoint over the same model with the wider grant.
const WIDE: &str = "w8k3zq6nxv2htb5rjs9cyd4gpm";

fn app_of(realm: &common::Realm) -> axum::Router {
    router(Arc::new(
        Gateway::new(
            Broker::new("http://127.0.0.1:1"),
            Box::new(PolicyPdp),
            "banskabystrica.sk",
        )
        .serve([
            endpoint(OPEN, Audience::Public, narrow_grant()),
            endpoint(WIDE, Audience::Public, wide_grant()),
            endpoint(CLOSED, Audience::Organization, narrow_grant()),
        ])
        .authenticate(
            Arc::new(realm.verifier()),
            ServiceAccounts::new(),
            Some("https://bb.example.sk".to_owned()),
        ),
    ))
}

async fn get(slug: &str) -> (StatusCode, Value) {
    let response = app_of(&common::Realm::new())
        .oneshot(
            Request::builder()
                .uri(format!("/api/endpoint/{slug}/openapi.json"))
                .body(Body::empty())
                .expect("a request"),
        )
        .await
        .expect("the gateway answers");
    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), 512 * 1024)
        .await
        .expect("a readable body");
    (status, serde_json::from_slice(&body).unwrap_or(Value::Null))
}

fn oas31() -> jsonschema::Validator {
    let schema: Value = serde_json::from_str(include_str!("fixtures/oas-3.1-2022-10-07.json"))
        .expect("the OAS 3.1 schema");
    jsonschema::validator_for(&schema).expect("the OAS 3.1 schema compiles")
}

#[tokio::test]
async fn the_document_is_valid_openapi_3_1_and_describes_only_what_the_caller_may_read() {
    let (status, document) = get(OPEN).await;
    assert_eq!(status, StatusCode::OK);
    let validator = oas31();
    let errors: Vec<String> = validator
        .iter_errors(&document)
        .map(|e| e.to_string())
        .collect();
    assert!(errors.is_empty(), "{errors:?}");
    assert!(
        !validator.is_valid(&json!({ "openapi": "3.1.0" })),
        "the validator refuses a document without info"
    );
    assert_eq!(document["openapi"], "3.1.0");
    assert_eq!(
        document["servers"][0]["url"],
        format!("/api/endpoint/{OPEN}")
    );
    let types =
        &document["paths"]["/ngsi-ld/v1/entities"]["get"]["parameters"][0]["schema"]["enum"];
    assert_eq!(types, &json!(["AirQualityObserved"]));
    let text = document.to_string();
    for hidden in ["InternalIncident", "internalNote", "severity"] {
        assert!(!text.contains(hidden), "{hidden} is in the document");
    }
    let schema = &document["components"]["schemas"]["AirQualityObserved"];
    assert!(schema["properties"]["pm10"].is_object(), "{schema}");
    assert_eq!(
        document["paths"]["/ngsi-ld/v1/entities/{entityId}"]["get"]["responses"]["200"]["content"]
            ["application/ld+json"]["schema"]["oneOf"][0]["$ref"],
        "#/components/schemas/AirQualityObserved"
    );
}

#[tokio::test]
async fn a_wider_grant_describes_both_types() {
    let (status, document) = get(WIDE).await;
    assert_eq!(status, StatusCode::OK);
    let types =
        &document["paths"]["/ngsi-ld/v1/entities"]["get"]["parameters"][0]["schema"]["enum"];
    assert_eq!(types, &json!(["AirQualityObserved", "InternalIncident"]));
    assert!(document["components"]["schemas"]["InternalIncident"].is_object());
    assert!(oas31().is_valid(&document));
}

#[tokio::test]
async fn a_closed_endpoint_refuses_the_anonymous_caller_and_an_unknown_slug_is_404() {
    let (status, _) = get(CLOSED).await;
    assert!(
        matches!(
            status,
            StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN | StatusCode::NOT_FOUND
        ),
        "{status}"
    );
    let (status, _) = get("nosuchslugnosuchslugnosuch").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}
