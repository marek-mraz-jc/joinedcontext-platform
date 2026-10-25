//! A query the broker's database cancelled at its statement timeout (T-5616).
//!
//! Antares answers it as a `500` InternalError naming `database statement timeout` (CIM 009
//! 5.5.2). Asked again, the same query times out again, so the gateway answers `504
//! query-timeout` with a hint to ask for less, on the NGSI-LD surface and behind a download,
//! instead of a `broker-failure` that says to try again in a minute. Any other `500` stays one.

use axum::body::Body;
use axum::http::{Request as HttpRequest, StatusCode};
use axum::response::IntoResponse;
use axum::routing::any;
use axum::Router;
use context_gateway::app::{router, Gateway};
use context_gateway::pdp::PolicyPdp;
use context_gateway::proxy::Broker;
use context_gateway::resolver::Endpoint;
use jc_core::kinds::{Audience, Representation};
use serde_json::{json, Value};
use std::collections::BTreeSet;
use std::sync::Arc;
use tower::ServiceExt;

const SLUG: &str = "mluyob4nz52lok3ssk7pgn5vwt";
const PUBLIC_READ: &str = r#"contextSpaceRef: ovzdusie
assigner: did:web:banskabystrica.sk
assignee: { kind: role, id: public }
operations: [queryEntity, retrieveEntity, queryTemporal, retrieveTemporal]
"#;

fn endpoint() -> Endpoint {
    Endpoint {
        declared_types: None,
        roles: Default::default(),
        slug: SLUG.to_owned(),
        title: std::collections::BTreeMap::new(),
        description: std::collections::BTreeMap::new(),
        space: "ovzdusie".to_owned(),
        project: "ovzdusie".to_owned(),
        audience: Audience::Public,
        allowed_projects: Vec::new(),
        representations: vec![
            Representation::NgsiLd,
            Representation::Csv,
            Representation::OgcFeatures,
            Representation::Sta,
        ],
        rate_limit: None,
        creates: None,
        file_limits: None,
        hidden_attributes: BTreeSet::new(),
        projection: None,
        base_path: format!("/api/endpoint/{SLUG}"),
        models: Vec::new(),
        view_mapping: None,
        catalog: None,
        deprecation: None,
        feeds: Vec::new(),
        calendars: Vec::new(),
        map_defaults: Default::default(),
        policy_names: Vec::new(),
        manifest_name: String::new(),
        policies: vec![serde_norway::from_str(PUBLIC_READ).expect("the policy parses")],
    }
}

/// A broker that answers every request `500` with the InternalError whose detail is `detail`.
async fn broker(detail: &'static str) -> String {
    let app = Router::new().fallback(any(move || async move {
        let mut answer = axum::Json(json!({
            "type": "https://uri.etsi.org/ngsi-ld/errors/InternalError",
            "title": "Internal error",
            "detail": detail,
        }))
        .into_response();
        *answer.status_mut() = StatusCode::INTERNAL_SERVER_ERROR;
        answer
    }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("a free port");
    let port = listener.local_addr().expect("an address").port();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    format!("http://127.0.0.1:{port}")
}

async fn ask(broker: &str, path: &str) -> (StatusCode, Value) {
    let app = router(Arc::new(
        Gateway::new(
            Broker::new(broker),
            Box::new(PolicyPdp),
            "banskabystrica.sk",
        )
        .serve([endpoint()]),
    ));
    let request = HttpRequest::builder()
        .uri(format!("/api/endpoint/{SLUG}{path}"))
        .body(Body::empty())
        .expect("a request");
    let response = app.oneshot(request).await.expect("the gateway answers");
    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .expect("a readable body");
    let problem = serde_json::from_slice(&body)
        .unwrap_or_else(|_| panic!("a problem document: {}", String::from_utf8_lossy(&body)));
    (status, problem)
}

const PATHS: [&str; 2] = [
    "/ngsi-ld/v1/temporal/entities?type=AirQualityObserved&timerel=after&timeAt=2026-10-24T00:00:00Z&lastN=10",
    "/file.csv?type=AirQualityObserved",
];

#[tokio::test]
async fn a_statement_timeout_is_a_gateway_timeout_that_says_to_ask_for_less() {
    let broker = broker("database statement timeout").await;
    for path in PATHS {
        let (status, problem) = ask(&broker, path).await;
        assert_eq!(status, StatusCode::GATEWAY_TIMEOUT, "{path}: {problem}");
        let text = problem.to_string();
        assert!(text.contains("query-timeout"), "{path}: {problem}");
        assert!(
            problem["hint"]
                .as_str()
                .is_some_and(|hint| hint.contains("shorter time window")),
            "{path}: the hint says what to narrow: {problem}"
        );
        assert!(!text.contains("broker-failure"), "{path}: {problem}");
    }
}

#[tokio::test]
async fn any_other_broker_failure_stays_a_broker_failure() {
    let broker = broker("database error").await;
    for path in PATHS {
        let (status, problem) = ask(&broker, path).await;
        assert_eq!(
            status,
            StatusCode::INTERNAL_SERVER_ERROR,
            "{path}: {problem}"
        );
        assert!(
            problem.to_string().contains("broker-failure"),
            "{path}: {problem}"
        );
    }
}
