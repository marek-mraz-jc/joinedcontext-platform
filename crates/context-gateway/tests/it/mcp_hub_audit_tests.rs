//! ADR-N-025 section 3, the audit row: every hub call's audit line names the Endpoint slug, the
//! tool and the subject (T-2490, EP-87).
//!
//! It reads its own lines through `common::Log`: the binary's one subscriber, routed per thread
//! (a scoped subscriber would race the callsite interest the other tests cache from their threads).

use crate::common;

use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use context_gateway::app::{router, Gateway};
use context_gateway::auth::accounts::ServiceAccounts;
use context_gateway::mcp::server::McpServer;
use context_gateway::pdp::PolicyPdp;
use context_gateway::proxy::Broker;
use context_gateway::resolver::Endpoint;
use jc_core::kinds::{Audience, PolicySpec, Representation};
use serde_json::json;
use std::sync::Arc;
use tower::ServiceExt;

const SLUG: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbb";

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
        base_path: format!("/api/endpoint/{SLUG}"),
        models: Vec::new(),
        policy_names: Vec::new(),
        manifest_name: String::new(),
        policies: vec![grant],
    }
}

#[tokio::test]
async fn the_audit_line_names_the_endpoint_the_tool_and_the_subject() {
    let captured = common::Log::capture();

    let realm = common::Realm::new();
    let broker = common::BrokerStub::start(vec![json!([])]).await;
    let gateway = Arc::new(
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
    );
    gateway.replace_servers(vec![McpServer {
        project: "ovzdusie".to_owned(),
        name: "air".to_owned(),
        title: None,
        description: None,
        members: vec![SLUG.to_owned()],
        select: None,
        endpoints: None,
        audience: Audience::Organization,
        allowed_projects: Vec::new(),
        knowledge_sources: Vec::new(),
    }]);
    let app = router(Arc::clone(&gateway));
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
    let response = app
        .clone()
        .oneshot(request)
        .await
        .expect("the gateway answers");
    assert_eq!(response.status(), StatusCode::OK);

    let log = captured.text();
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
    // The hop to the Endpoint's view follows, as for a call to its URL (T-3808).
    assert!(
        log.lines()
            .any(|line| line.contains("forwarded to the broker's view") && line.contains(SLUG)),
        "{log}"
    );
    assert!(!log.contains(&token), "a token reached the log");
    // ADR-N-043 §2.8: a named server's member call writes the same line, with the server beside it.
    let fanned = Request::builder()
        .method(Method::POST)
        .uri("/api/mcp/ovzdusie/air")
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {token}"))
        .body(Body::from(
            json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": {
                "name": "query_entities",
                "arguments": { "type": "AirQualityObserved" },
            }})
            .to_string(),
        ))
        .expect("a request");
    let response = app.oneshot(fanned).await.expect("the gateway answers");
    assert_eq!(response.status(), StatusCode::OK);
    let log = captured.text();
    let line = log
        .lines()
        .find(|line| line.contains("hub call") && line.contains("door=\"server\""))
        .unwrap_or_else(|| panic!("no server audit line in:\n{log}"));
    for field in [
        format!("slug={SLUG}"),
        "tool=query_entities".to_owned(),
        "principal=user:jana".to_owned(),
        "server=\"/api/mcp/ovzdusie/air\"".to_owned(),
    ] {
        assert!(line.contains(&field), "{field} missing from: {line}");
    }
}
