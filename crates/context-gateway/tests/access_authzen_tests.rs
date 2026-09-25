//! The access surface, which answers "what may I do here" without the caller probing for
//! it — and without ever answering "what is here" (T-0163, EP-55…EP-60, R20).

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

fn policy(yaml: &str) -> PolicySpec {
    serde_norway::from_str(yaml).expect("the policy spec parses")
}

/// The public grant of DEMO step 4, plus a prohibition and a grant for somebody else.
fn endpoint() -> Endpoint {
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
        catalog: None,
        policies: vec![
            policy(
                r#"contextSpaceRef: ovzdusie
assigner: did:web:banskabystrica.sk
assignee: { kind: role, id: public }
operations: [queryEntity, retrieveEntity, queryTemporal]
information:
  - entities:
      - type: AirQualityObserved
        idPattern: "^urn:ngsi-ld:AirQualityObserved:banskabystrica\\.sk:ovzdusie:.*$"
    propertyNames: [pm10, pm25, location]
q: "pm10>=0"
scopeQ: "/geo/SK/BB"
"#,
            ),
            // A grant for somebody else: the anonymous caller must not learn that this
            // type exists at all (EP-59, R20).
            policy(
                r#"contextSpaceRef: ovzdusie
assigner: did:web:banskabystrica.sk
assignee: { kind: role, id: steward }
operations: [createEntity, updateAttrs]
information:
  - entities:
      - type: InternalIncident
    propertyNames: [severity, reportedBy]
"#,
            ),
            // A prohibition that takes back one operation from the public role.
            policy(
                r#"contextSpaceRef: ovzdusie
effect: prohibition
assigner: did:web:banskabystrica.sk
assignee: { kind: role, id: public }
operations: [queryTemporal]
information:
  - entities:
      - type: AirQualityObserved
"#,
            ),
        ],
    }
}

fn app() -> axum::Router {
    router(Arc::new(
        Gateway::new(
            Broker::new("http://127.0.0.1:1"),
            Box::new(PolicyPdp),
            "banskabystrica.sk",
        )
        .serve([endpoint()]),
    ))
}

async fn call(request: Request<Body>) -> (StatusCode, Value) {
    let response = app().oneshot(request).await.expect("the gateway answers");
    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), 256 * 1024)
        .await
        .expect("a readable body");
    let json = serde_json::from_slice(&body).unwrap_or(Value::Null);
    (status, json)
}

fn get(path: &str) -> Request<Body> {
    Request::builder()
        .uri(path)
        .body(Body::empty())
        .expect("a request")
}

/// EP-55: the document is the caller's own grants, in the shape the docs promise.
#[tokio::test]
async fn the_access_document_lists_the_callers_grants() {
    let (status, document) = call(get(&format!("/api/endpoint/{SLUG}/access"))).await;
    assert_eq!(status, StatusCode::OK);

    assert_eq!(
        document["subject"],
        json!({ "type": "role", "id": "public" })
    );
    assert_eq!(document["resource"]["type"], json!("endpoint"));
    assert_eq!(document["resource"]["id"], json!(SLUG));
    assert_eq!(document["resource"]["space"], json!("ovzdusie"));

    let permissions = document["permissions"].as_array().expect("permissions");
    assert_eq!(permissions.len(), 1, "one granted type");
    let grant = &permissions[0];
    assert_eq!(grant["resource"]["type"], json!("AirQualityObserved"));
    assert_eq!(
        grant["actions"],
        json!(["queryEntity", "queryTemporal", "retrieveEntity"])
    );
    assert_eq!(grant["attributes"], json!(["location", "pm10", "pm25"]));
    assert_eq!(grant["constraints"]["q"], json!("pm10>=0"));
    assert_eq!(grant["constraints"]["scopeQ"], json!("/geo/SK/BB"));
    assert!(grant["resource"]["idPatterns"][0].as_str().is_some());
    // This Endpoint sets no rate, so the document claims none (EP-56).
    assert_eq!(document.get("limits"), None);
}

/// EP-56: the Endpoint's own rate belongs in the document. A caller that learns its limit
/// only by being refused has to probe for it, which is the shape of request this surface
/// exists to remove.
#[tokio::test]
async fn the_document_names_the_rate_the_endpoint_holds_the_caller_to() {
    let mut limited = endpoint();
    limited.rate_limit = Some(jc_core::kinds::RateLimits {
        requests_per_minute: 600,
        burst: Some(50),
    });
    let document = context_gateway::handlers::access::permissions(
        &context_gateway::pdp::evaluator::Subject::default(),
        &limited,
        chrono::Utc::now(),
    );
    assert_eq!(document["limits"]["requestsPerMinute"], json!(600));
    assert_eq!(document["limits"]["burst"], json!(50));
}

/// EP-59, R20: a type only somebody else may touch is absent, not listed as denied. The
/// document must not become a directory of what exists.
#[tokio::test]
async fn a_type_the_caller_may_not_touch_is_absent_from_the_document() {
    let (_, document) = call(get(&format!("/api/endpoint/{SLUG}/access"))).await;
    let rendered = document.to_string();

    assert!(!rendered.contains("InternalIncident"), "{rendered}");
    assert!(!rendered.contains("severity"));
    assert!(!rendered.contains("steward"));
}

/// GW8: a prohibition that applies to the caller is in the document, so a client can see
/// why an action it thought it had is refused.
#[tokio::test]
async fn a_prohibition_that_applies_to_the_caller_is_reported() {
    let (_, document) = call(get(&format!("/api/endpoint/{SLUG}/access"))).await;
    let prohibitions = document["prohibitions"].as_array().expect("prohibitions");

    assert_eq!(prohibitions.len(), 1);
    assert_eq!(
        prohibitions[0]["resource"]["type"],
        json!("AirQualityObserved")
    );
    assert_eq!(prohibitions[0]["actions"], json!(["queryTemporal"]));
    // No property whitelist means the prohibition reaches the whole type.
    assert_eq!(prohibitions[0]["attributes"], json!("*"));
}

/// R51: the dry run answers one prospective request, and a refusal explains nothing (GW6).
#[tokio::test]
async fn the_check_endpoint_answers_one_prospective_request() {
    let permitted = Request::builder()
        .method("POST")
        .uri(format!("/api/endpoint/{SLUG}/access/check"))
        .header("content-type", "application/json")
        .body(Body::from(
            json!({
                "subject": { "id": "anonymous" },
                "action": { "name": "queryEntity" },
                "resource": { "type": "AirQualityObserved" }
            })
            .to_string(),
        ))
        .expect("a request");
    let (status, answer) = call(permitted).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(answer["decision"], json!(true));
    assert_eq!(answer["context"]["reason"], json!("policy_grant_matched"));

    for refused in [
        json!({ "action": { "name": "deleteEntity" }, "resource": { "type": "AirQualityObserved" } }),
        json!({ "action": { "name": "queryEntity" }, "resource": { "type": "InternalIncident" } }),
        // Prohibited, even though a permission names the same operation (GW8).
        json!({ "action": { "name": "queryTemporal" }, "resource": { "type": "AirQualityObserved" } }),
    ] {
        let request = Request::builder()
            .method("POST")
            .uri(format!("/api/endpoint/{SLUG}/access/check"))
            .header("content-type", "application/json")
            .body(Body::from(refused.to_string()))
            .expect("a request");
        let (status, answer) = call(request).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(answer["decision"], json!(false), "{refused}");
        assert!(
            answer.get("context").is_none(),
            "a refusal explains nothing"
        );
    }
}

/// An unknown slug answers the same 404 here as everywhere else (EP-03), and a body that
/// is not a check is a bad request.
#[tokio::test]
async fn the_access_surface_refuses_the_same_way_the_data_surface_does() {
    let (status, _) = call(get("/api/endpoint/nosuchslugnosuchslugnosuchsl/access")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let malformed = Request::builder()
        .method("POST")
        .uri(format!("/api/endpoint/{SLUG}/access/check"))
        .body(Body::from("{}"))
        .expect("a request");
    let (status, _) = call(malformed).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

/// EP-57, EP-58: a caller that names a representation gets that one, never JSON wearing
/// somebody else's media type. The documents themselves are asserted in their own suites
/// (`access_odrl_tests`, `access_ucast_tests`); what this one holds is the negotiation.
#[tokio::test]
async fn a_named_representation_is_the_one_that_comes_back() {
    for (accept, media_type) in [
        ("application/odrl+json", "application/odrl+json"),
        ("text/turtle", "text/turtle"),
        (
            "application/vnd.joinedcontext.grant-ast+json",
            "application/vnd.joinedcontext.grant-ast+json",
        ),
        ("application/json", "application/json"),
        ("*/*", "application/json"),
        // Read in the caller's order of preference, not ours.
        ("text/turtle, application/json", "text/turtle"),
        // A type this surface does not serve falls through to the default document rather
        // than to a 406: the access surface always has an answer, and a refusal here tells
        // a caller nothing it could act on.
        ("text/csv", "application/json"),
    ] {
        let request = Request::builder()
            .uri(format!("/api/endpoint/{SLUG}/access"))
            .header("accept", accept)
            .body(Body::empty())
            .expect("a request");
        let response = app().oneshot(request).await.expect("the gateway answers");
        assert_eq!(response.status(), StatusCode::OK, "for {accept}");
        assert_eq!(
            response
                .headers()
                .get(axum::http::header::CONTENT_TYPE)
                .and_then(|value| value.to_str().ok()),
            Some(media_type),
            "for {accept}"
        );
    }
}
