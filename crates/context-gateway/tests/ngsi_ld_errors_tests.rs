//! The gateway's own refusals on the NGSI-LD surface, rendered the way CIM 009 wants them
//! (T-0272 clause 5.5.3, T-0273 clause 6.3.2, T-0271 clause 4.10).
//!
//! Every case here is refused before the broker is reached, so the broker is a closed port.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use context_gateway::app::{router, Gateway};
use context_gateway::pdp::PolicyPdp;
use context_gateway::proxy::Broker;
use context_gateway::resolver::Endpoint;
use jc_core::kinds::{Audience, PolicySpec, Representation};
use serde_json::{json, Value};
use std::sync::Arc;
use tower::ServiceExt;

const SLUG: &str = "k4y7pq2mzt6vhx3nbwrs5cjd8f";

fn endpoint() -> Endpoint {
    let grant: PolicySpec = serde_norway::from_str(
        r#"contextSpaceRef: ovzdusie
assigner: did:web:banskabystrica.sk
assignee: { kind: role, id: public }
operations: [queryEntity, retrieveEntity, createEntity]
information:
  - entities:
      - type: AirQualityObserved
    propertyNames: [pm10, pm25]
"#,
    )
    .expect("the policy parses");
    Endpoint {
        roles: Default::default(),
        slug: SLUG.to_owned(),
        title: std::collections::BTreeMap::new(),
        description: std::collections::BTreeMap::new(),
        space: "ovzdusie".to_owned(),
        project: "ovzdusie".to_owned(),
        audience: Audience::Public,
        allowed_projects: Vec::new(),
        representations: vec![Representation::NgsiLd],
        rate_limit: None,
        file_limits: None,
        hidden_attributes: Default::default(),
        projection: None,
        base_path: format!("/api/endpoint/{SLUG}"),
        models: Vec::new(),
        view_mapping: None,
        policies: vec![grant],
    }
}

async fn call(request: Request<Body>) -> (StatusCode, String, Value) {
    let app = router(Arc::new(
        Gateway::new(
            Broker::new("http://127.0.0.1:1"),
            Box::new(PolicyPdp),
            "banskabystrica.sk",
        )
        .serve([endpoint()]),
    ));
    let response = app.oneshot(request).await.expect("the gateway answers");
    let status = response.status();
    let content_type = response
        .headers()
        .get("content-type")
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    let body = axum::body::to_bytes(response.into_body(), 64 * 1024)
        .await
        .expect("a readable body");
    (
        status,
        content_type,
        serde_json::from_slice(&body).unwrap_or(Value::Null),
    )
}

fn post(path: &str, content_type: Option<&str>, body: &str) -> Request<Body> {
    let mut builder = Request::builder().method("POST").uri(path);
    if let Some(content_type) = content_type {
        builder = builder.header("content-type", content_type);
    }
    builder
        .body(Body::from(body.to_owned()))
        .expect("a request")
}

fn entities() -> String {
    format!("/api/endpoint/{SLUG}/ngsi-ld/v1/entities")
}

/// Clause 6.3.3: type, title and detail, as application/json, with the clause 5.5.2 type.
fn assert_ngsi_ld_error(status: StatusCode, content_type: &str, body: &Value, expected: u16) {
    assert_eq!(status.as_u16(), expected, "{body}");
    assert!(
        content_type.starts_with("application/json"),
        "clause 5.5.3: never application/problem+json on this surface, got {content_type}"
    );
    for term in ["type", "title", "detail"] {
        assert!(
            !body[term].is_null(),
            "clause 6.3.3: `{term}` is carried: {body}"
        );
    }
}

/// T-0272: a write-guard refusal is a 400 BadRequestData, and a refused operation is a
/// 403 — two statuses, so one hard-coded string cannot satisfy both.
#[tokio::test]
async fn gateway_refusals_are_ngsi_ld_errors() {
    let foreign = json!({
        "id": "urn:ngsi-ld:AirQualityObserved:other-city.sk:ovzdusie:s1",
        "type": "AirQualityObserved",
        "pm10": { "type": "Property", "value": 12 },
    });
    let (status, content_type, body) = call(post(
        &entities(),
        Some("application/ld+json"),
        &foreign.to_string(),
    ))
    .await;
    assert_ngsi_ld_error(status, &content_type, &body, 400);
    assert_eq!(
        body["type"],
        json!("https://uri.etsi.org/ngsi-ld/errors/BadRequestData")
    );

    // PATCH is nothing the public grant covers: refused before any body is read.
    let patch = Request::builder()
        .method("PATCH")
        .uri(format!(
            "{}/urn:ngsi-ld:AirQualityObserved:banskabystrica.sk:ovzdusie:s1/attrs",
            entities()
        ))
        .header("content-type", "application/json")
        .body(Body::from(r#"{"pm10":{"type":"Property","value":1}}"#))
        .expect("a request");
    let (status, content_type, body) = call(patch).await;
    assert_ngsi_ld_error(status, &content_type, &body, 403);
    assert!(
        !body["detail"]
            .as_str()
            .unwrap_or_default()
            .contains("policy"),
        "GW1: a refusal does not name the rule that refused it: {body}"
    );

    // A refused single-entity read stays a miss, and the miss is an ETSI one: the public
    // grant covers no temporal operation, so one entity's history is refused before the
    // broker, as a 404 (R20).
    let (status, content_type, body) = call(
        Request::builder()
            .uri(format!(
                "/api/endpoint/{SLUG}/ngsi-ld/v1/temporal/entities/urn:ngsi-ld:AirQualityObserved:banskabystrica.sk:ovzdusie:s1"
            ))
            .body(Body::empty())
            .expect("a request"),
    )
    .await;
    assert_ngsi_ld_error(status, &content_type, &body, 404);
    assert_eq!(
        body["type"],
        json!("https://uri.etsi.org/ngsi-ld/errors/ResourceNotFound")
    );
}

/// T-0273, clause 6.3.2: a payload in a media type the surface does not take is 415, and
/// the accepted ones still reach the write guard — so a gateway that answers 415 to
/// everything cannot pass.
#[tokio::test]
async fn an_unsupported_payload_media_type_is_415_and_the_accepted_ones_reach_the_guard() {
    for content_type in [Some("text/csv"), Some("application/xml"), None] {
        let (status, _, body) = call(post(&entities(), content_type, "id,type\n1,x")).await;
        assert_eq!(
            status,
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "{content_type:?}: {body}"
        );
        assert!(!body["detail"].is_null());
    }

    let foreign = json!({
        "id": "urn:ngsi-ld:AirQualityObserved:other-city.sk:ovzdusie:s1",
        "type": "AirQualityObserved",
    });
    for content_type in [
        "application/json",
        "application/ld+json",
        "application/ld+json; charset=utf-8",
    ] {
        let (status, _, body) =
            call(post(&entities(), Some(content_type), &foreign.to_string())).await;
        assert_eq!(
            status,
            StatusCode::BAD_REQUEST,
            "{content_type} passed the media-type check and met the write guard: {body}"
        );
    }

    // A write with no payload at all is not a payload in the wrong type.
    let delete = Request::builder()
        .method("DELETE")
        .uri(format!(
            "{}/urn:ngsi-ld:AirQualityObserved:banskabystrica.sk:ovzdusie:s1",
            entities()
        ))
        .body(Body::empty())
        .expect("a request");
    let (status, _, _) = call(delete).await;
    assert_ne!(status, StatusCode::UNSUPPORTED_MEDIA_TYPE);
}
