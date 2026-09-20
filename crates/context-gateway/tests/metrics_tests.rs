//! The gateway's Prometheus surface (T-0463, OPS-16, TS-22).
//!
//! `components/monitoring` has scraped `/metrics` on this service since T-0038 and got the
//! 404 fallback, which is why these tests exist: the interesting failure is not a wrong
//! number, it is a target that answers nothing while a dashboard reports it green.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use context_gateway::app::{router, Gateway};
use context_gateway::pdp::PolicyPdp;
use context_gateway::proxy::Broker;
use context_gateway::resolver::Endpoint;
use context_gateway::telemetry;
use jc_core::kinds::{Audience, PolicySpec, Representation};
use std::sync::Arc;
use tower::ServiceExt;

const SLUG: &str = "k4y7pq2mzt6vhx3nbwrs5cjd8f";

fn policy(yaml: &str) -> PolicySpec {
    serde_norway::from_str(yaml).expect("the policy spec parses")
}

fn endpoint() -> Endpoint {
    Endpoint {
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
        policies: vec![policy(
            r#"contextSpaceRef: ovzdusie
assigner: did:web:banskabystrica.sk
assignee: { kind: role, id: public }
operations: [queryEntity, retrieveEntity]
information:
  - entities:
      - type: AirQualityObserved
"#,
        )],
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

async fn call(path: &str) -> (StatusCode, String, String) {
    let response = app()
        .oneshot(
            Request::builder()
                .uri(path)
                .body(Body::empty())
                .expect("a request"),
        )
        .await
        .expect("the gateway answers");
    let status = response.status();
    let content_type = response
        .headers()
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    let body = axum::body::to_bytes(response.into_body(), 4 * 1024 * 1024)
        .await
        .expect("a readable body");
    (
        status,
        content_type,
        String::from_utf8_lossy(&body).into_owned(),
    )
}

#[tokio::test]
async fn the_scrape_target_answers_the_text_format_prometheus_reads() {
    call(&format!("/api/endpoint/{SLUG}")).await;
    let (status, content_type, body) = call("/metrics").await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(content_type, telemetry::TEXT_FORMAT);
    // The help text is what makes the surface readable without this module beside it.
    assert!(
        body.contains("# HELP jc_gateway_requests_total requests answered by the gateway surface"),
        "{body}"
    );
}

/// The series the edge cannot produce: APISIX counts statuses, not decisions (GW1, OPS-16).
#[tokio::test]
async fn a_policy_decision_is_counted_with_its_operation_and_verdict() {
    // Reaches the decision point and then fails to reach the broker, which is enough: the
    // decision is made before anything is forwarded.
    call(&format!(
        "/api/endpoint/{SLUG}/ngsi-ld/v1/entities?type=AirQualityObserved"
    ))
    .await;

    let (_, _, body) = call("/metrics").await;
    let counted: Vec<&str> = body
        .lines()
        .filter(|line| line.starts_with("jc_gateway_pdp_decisions_total{"))
        .collect();
    assert!(!counted.is_empty(), "no decision was counted:\n{body}");
    assert!(
        counted
            .iter()
            .any(|line| line.contains("operation=\"queryEntity\"")),
        "{counted:?}"
    );
}

#[tokio::test]
async fn a_request_through_the_surface_moves_the_counter() {
    // The endpoint record: one request, no broker, and a route the monitor's dashboard draws.
    let (status, _, _) = call(&format!("/api/endpoint/{SLUG}")).await;
    assert_eq!(status, StatusCode::OK);

    let (_, _, body) = call("/metrics").await;
    let counted: Vec<&str> = body
        .lines()
        .filter(|line| line.starts_with("jc_gateway_requests_total{"))
        .filter(|line| line.contains(&format!("endpoint=\"{SLUG}\"")))
        .collect();
    assert!(!counted.is_empty(), "nothing counted the request:\n{body}");
    for line in &counted {
        let value: f64 = line
            .rsplit(' ')
            .next()
            .and_then(|number| number.parse().ok())
            .unwrap_or_default();
        assert!(value >= 1.0, "{line}");
    }
}

/// A scrape must not count itself, or the request rate becomes a function of the scrape
/// interval and says nothing about the traffic.
#[tokio::test]
async fn the_scrape_does_not_count_as_traffic() {
    call("/metrics").await;
    let (_, _, body) = call("/metrics").await;

    assert!(
        !body.contains("route=\"/metrics\""),
        "the scrape counted itself:\n{body}"
    );
    assert!(!body.contains("route=\"/healthz\""), "{body}");
}

/// A duration has to arrive as a histogram, not as a summary: the dashboard reads latency as
/// `histogram_quantile` over these buckets, and a quantile computed inside one replica cannot
/// be combined with another replica's once the HPA has scaled the gateway (docs Deployment/05
/// §1, T-0464).
#[tokio::test]
async fn a_duration_is_exported_as_buckets_a_dashboard_can_sum() {
    call(&format!("/api/endpoint/{SLUG}")).await;

    let (_, _, body) = call("/metrics").await;
    assert!(
        body.contains("# TYPE jc_gateway_request_duration_seconds histogram"),
        "the duration is not a histogram:\n{body}"
    );
    assert!(
        body.lines().any(
            |line| line.starts_with("jc_gateway_request_duration_seconds_bucket{")
                && line.contains("le=\"0.005\"")
        ),
        "no bucket in the range the SLO is written in:\n{body}"
    );
    assert!(
        !body.contains("quantile=\""),
        "a summary quantile is exported and cannot be aggregated:\n{body}"
    );
}
