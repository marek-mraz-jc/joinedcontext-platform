//! A broker that fails a member call leaves its status and the answer's length in the log, never
//! the answer: a 5xx body can carry the entities it was serving, personal data included.
//!
//! It reads its own lines through `common::Log`: the binary's one subscriber, routed per thread
//! (a scoped subscriber would race the callsite interest the other tests cache from their threads).

use crate::common;

use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use axum::Router;
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
async fn a_failed_member_call_logs_no_broker_answer() {
    let captured = common::Log::capture();

    let stub = Router::new().fallback(|| async {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            axum::Json(json!({ "entities": [{ "id": "urn:ngsi-ld:Person:1",
                "email": { "type": "Property", "value": "jana.novakova@example.org" } }] })),
        )
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("a free port");
    let url = format!("http://{}", listener.local_addr().expect("an address"));
    tokio::spawn(async move {
        let _ = axum::serve(listener, stub).await;
    });

    let realm = common::Realm::new();
    let gateway = Arc::new(
        Gateway::new(Broker::new(&url), Box::new(PolicyPdp), "banskabystrica.sk")
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
    let response = router(Arc::clone(&gateway))
        .oneshot(request)
        .await
        .expect("the gateway answers");
    assert_eq!(response.status(), StatusCode::OK);

    let log = captured.text();
    let line = log
        .lines()
        .find(|line| line.contains("the broker failed a member call"))
        .unwrap_or_else(|| panic!("no failure line in:\n{log}"));
    assert!(line.contains("status=500"), "{line}");
    assert!(line.contains("bytes="), "{line}");
    assert!(
        !log.contains("jana.novakova"),
        "the broker's answer reached the log:\n{log}"
    );
}
