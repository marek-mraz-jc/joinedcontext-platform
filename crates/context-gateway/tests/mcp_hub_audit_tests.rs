//! ADR-N-025 section 3, the audit row: every hub call's audit line names the Endpoint slug, the
//! tool and the subject (T-2490, EP-87).
//!
//! Alone in its binary: it installs the process's subscriber, and a scoped one races the
//! callsite interest the other hub tests cache from their own threads.

mod common;

use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use context_gateway::app::{router, Gateway};
use context_gateway::auth::accounts::ServiceAccounts;
use context_gateway::pdp::PolicyPdp;
use context_gateway::proxy::Broker;
use context_gateway::resolver::Endpoint;
use jc_core::kinds::{Audience, PolicySpec, Representation};
use serde_json::json;
use std::sync::{Arc, Mutex};
use tower::ServiceExt;

const SLUG: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbb";

/// A writer the test reads back.
#[derive(Clone, Default)]
struct Captured(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for Captured {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().expect("the log").extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn endpoint() -> Endpoint {
    let grant: PolicySpec = serde_norway::from_str(
        "contextSpaceRef: ovzdusie\n\
         assigner: did:web:banskabystrica.sk\n\
         assignee: { kind: role, id: steward }\n\
         operations: [queryEntity]\n\
         information:\n  - entities:\n      - type: AirQualityObserved\n",
    )
    .expect("the policy spec parses");
    Endpoint {
        declared_types: None,
        roles: Default::default(),
        slug: SLUG.to_owned(),
        title: Default::default(),
        description: Default::default(),
        space: "ovzdusie".to_owned(),
        project: "ovzdusie".to_owned(),
        audience: Audience::Organization,
        allowed_projects: Vec::new(),
        representations: vec![Representation::NgsiLd, Representation::Mcp],
        rate_limit: None,
        file_limits: None,
        hidden_attributes: Default::default(),
        projection: None,
        view_mapping: None,
        catalog: None,
        base_path: format!("/api/endpoint/{SLUG}"),
        models: Vec::new(),
        policies: vec![grant],
    }
}

#[tokio::test]
async fn the_audit_line_names_the_endpoint_the_tool_and_the_subject() {
    let captured = Captured::default();
    let writer = captured.clone();
    tracing::subscriber::set_global_default(
        tracing_subscriber::fmt()
            .with_writer(move || writer.clone())
            .with_ansi(false)
            .finish(),
    )
    .expect("the only subscriber of this binary");

    let realm = common::Realm::new();
    let broker = common::BrokerStub::start(vec![json!([])]).await;
    let app = router(Arc::new(
        Gateway::new(
            Broker::new(&broker.url),
            Box::new(PolicyPdp),
            "banskabystrica.sk",
        )
        .serve([endpoint()])
        .authenticate(
            Arc::new(realm.verifier()),
            ServiceAccounts::new(),
            Some("https://city.example".to_owned()),
        ),
    ));
    let token = realm.mint(&json!({
        "iss": common::ISSUER,
        "sub": "0f5a",
        "aud": "context-gateway",
        "preferred_username": "jana",
        "groups": ["/ovzdusie"],
        "realm_access": { "roles": ["steward"] },
        "exp": common::in_seconds(300),
        "iat": common::in_seconds(-10),
    }));
    let request = Request::builder()
        .method(Method::POST)
        .uri("/api/mcp")
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {token}"))
        .body(Body::from(
            json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {
                "name": "query_entities",
                "arguments": { "endpoint": SLUG, "type": "AirQualityObserved" },
            }})
            .to_string(),
        ))
        .expect("a request");
    let response = app.oneshot(request).await.expect("the gateway answers");
    assert_eq!(response.status(), StatusCode::OK);

    let log = String::from_utf8(captured.0.lock().expect("the log").clone()).expect("utf-8");
    let line = log
        .lines()
        .find(|line| line.contains("hub call"))
        .unwrap_or_else(|| panic!("no hub audit line in:\n{log}"));
    for field in [
        format!("slug={SLUG}"),
        "tool=query_entities".to_owned(),
        "principal=user:jana".to_owned(),
        "door=\"hub\"".to_owned(),
    ] {
        assert!(line.contains(&field), "{field} missing from: {line}");
    }
    // The Endpoint's own decision line follows, as for a call to its URL.
    assert!(
        log.lines()
            .any(|line| line.contains("decision") && line.contains(SLUG)),
        "{log}"
    );
    assert!(!log.contains(&token), "a token reached the log");
}
