//! A caller without a token reads, and only reads, on every MCP surface (T-4168).
//!
//! The hub, a named server, an Endpoint's MCP and a Context Space's MCP list nothing but
//! `ANONYMOUS_TOOLS` to such a caller and refuse every other tool before the broker sees it,
//! even where the Endpoint's Policy grants the public role a write; a token holder keeps what the
//! Policy grants. One per-address bucket spans all four surfaces, keyed by the address the edge
//! appended, so a spoofed `X-Forwarded-For` entry and a second surface open no new allowance.

mod common;

use axum::body::Body;
use axum::http::{header::AUTHORIZATION, Method, Request, StatusCode};
use axum::Router;
use context_gateway::app::{router, Gateway};
use context_gateway::auth::accounts::ServiceAccounts;
use context_gateway::mcp::server::McpServer;
use context_gateway::pdp::PolicyPdp;
use context_gateway::proxy::Broker;
use context_gateway::resolver::{Endpoint, Model, Space};
use jc_core::kinds::{Audience, PolicySpec, RateLimits, Representation};
use serde_json::{json, Value};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use tower::ServiceExt;

const HOST: &str = "https://city.example";
/// Public, and its Policy grants the public role reads and every subscription write.
const OPEN: &str = "oooooooooooooooooooooooooo";
/// A Context Space anyone may use, with the same grant.
const SPACE: &str = "ovzdusie";
const WRITES: &str = "queryEntity, retrieveEntity, createSubscription, updateSubscription, \
                      deleteSubscription";

fn grant(space: &str, role: &str) -> PolicySpec {
    serde_norway::from_str(&format!(
        "contextSpaceRef: {space}\n\
         assigner: did:web:banskabystrica.sk\n\
         assignee: {{ kind: role, id: {role} }}\n\
         operations: [{WRITES}]\n\
         information:\n  \
         - entities:\n      \
         - type: AirQualityObserved\n"
    ))
    .expect("the policy spec parses")
}

fn endpoint(slug: &str, space: &str, base_path: String) -> Endpoint {
    Endpoint {
        declared_types: None,
        roles: Default::default(),
        slug: slug.to_owned(),
        title: [("en".to_owned(), "Air quality".to_owned())]
            .into_iter()
            .collect(),
        description: Default::default(),
        space: space.to_owned(),
        project: "mesto".to_owned(),
        audience: Audience::Public,
        allowed_projects: Vec::new(),
        representations: vec![Representation::NgsiLd, Representation::Mcp],
        rate_limit: None,
        creates: None,
        file_limits: None,
        hidden_attributes: Default::default(),
        projection: None,
        view_mapping: None,
        catalog: None,
        base_path,
        models: vec![Model {
            name: "mesto".to_owned(),
            version: "1.0.0".to_owned(),
            major: 1,
            classes: vec!["AirQualityObserved".to_owned()],
            json_schema: Some(json!({
                "$defs": { "AirQualityObserved": { "type": "object", "properties": { "id": {} } } }
            }).into()),
            context: None,
        }],
        policy_names: Vec::new(),
        manifest_name: String::new(),
        policies: vec![grant(space, "public"), grant(space, "steward")],
    }
}

/// A broker whose view MCP lists a read and three writes, and counts the messages it is sent.
async fn broker() -> (String, Arc<AtomicUsize>) {
    let seen = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&seen);
    let tools = json!([
        // Annotated as the broker's view annotates a read (Antares antares-mcp, dev 2026-10-19).
        { "name": "query_entities", "inputSchema": { "type": "object" },
          "annotations": { "readOnlyHint": true, "openWorldHint": false } },
        { "name": "create_subscription", "inputSchema": { "type": "object" } },
        { "name": "update_subscription", "inputSchema": { "type": "object" } },
        { "name": "a_tool_the_broker_added_later", "inputSchema": { "type": "object" } },
    ]);
    let stub = axum::Router::new().fallback(move |request: axum::extract::Request| {
        let tools = tools.clone();
        let counter = Arc::clone(&counter);
        async move {
            if request.uri().path().ends_with("/mcp") {
                counter.fetch_add(1, Ordering::SeqCst);
            }
            axum::Json(json!({
                "jsonrpc": "2.0", "id": 1, "result": { "tools": tools, "resources": [] }
            }))
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("a free port");
    let url = format!("http://{}", listener.local_addr().expect("an address"));
    tokio::spawn(async move {
        let _ = axum::serve(listener, stub).await;
    });
    (url, seen)
}

fn gateway(broker: &str, realm: &common::Realm) -> Gateway {
    let space = Space {
        endpoint: Arc::new(endpoint(SPACE, SPACE, format!("/cs/{SPACE}"))),
        title: Default::default(),
        description: Default::default(),
        is_sandbox: false,
        default_locale: Some("sk".to_owned()),
    };
    let gateway = Gateway::new(
        Broker::new(broker),
        Box::new(PolicyPdp),
        "banskabystrica.sk",
    )
    .serve([endpoint(OPEN, "mesto", format!("/api/endpoint/{OPEN}"))])
    .serve_spaces([space])
    .authenticate(
        Arc::new(realm.verifier()),
        ServiceAccounts::new(),
        Some(HOST.to_owned()),
    );
    gateway.replace_servers(vec![McpServer {
        project: "mesto".to_owned(),
        name: "open".to_owned(),
        title: Some("Open data".to_owned()),
        description: None,
        members: vec![OPEN.to_owned()],
        select: None,
        endpoints: None,
        audience: Audience::Public,
        allowed_projects: Vec::new(),
        knowledge_sources: Vec::new(),
    }]);
    gateway
}

/// The surfaces a caller without a token reaches, each with the arguments a call names there.
fn surfaces() -> [(String, Value); 4] {
    let routed = json!({ "endpoint": OPEN, "subscription": { "type": "Subscription" } });
    let direct = json!({ "subscription": { "type": "Subscription" } });
    [
        ("/api/mcp".to_owned(), routed.clone()),
        ("/api/mcp/mesto/open".to_owned(), routed),
        (format!("/api/endpoint/{OPEN}/mcp"), direct.clone()),
        (format!("/cs/{SPACE}/mcp"), direct),
    ]
}

fn rpc(path: &str, token: Option<&str>, address: Option<&str>, body: &Value) -> Request<Body> {
    let mut builder = Request::builder()
        .method(Method::POST)
        .uri(path)
        .header("content-type", "application/json");
    if let Some(token) = token {
        builder = builder.header(AUTHORIZATION, format!("Bearer {token}"));
    }
    if let Some(address) = address {
        builder = builder.header("x-forwarded-for", address);
    }
    builder
        .body(Body::from(body.to_string()))
        .expect("a request")
}

fn list() -> Value {
    json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list" })
}

fn call(name: &str, arguments: &Value) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "tools/call",
        "params": { "name": name, "arguments": arguments },
    })
}

async fn send(app: &Router, request: Request<Body>) -> (StatusCode, Value) {
    let response = app
        .clone()
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

fn names(answer: &Value) -> Vec<String> {
    answer["result"]["tools"]
        .as_array()
        .unwrap_or_else(|| panic!("no tool list: {answer}"))
        .iter()
        .filter_map(|tool| tool["name"].as_str().map(str::to_owned))
        .collect()
}

#[tokio::test]
async fn without_a_token_no_surface_lists_a_write_or_an_unnamed_tool() {
    let realm = common::Realm::new();
    let (url, _) = broker().await;
    let app = router(Arc::new(gateway(&url, &realm)));
    for (path, _) in surfaces() {
        let (status, answer) = send(&app, rpc(&path, None, None, &list())).await;
        assert_eq!(status, StatusCode::OK, "{path}: {answer}");
        let listed = names(&answer);
        assert!(
            listed.iter().any(|name| name == "query_entities"),
            "{path}: {listed:?}"
        );
        for hidden in [
            "create_subscription",
            "update_subscription",
            "delete_subscription",
            "a_tool_the_broker_added_later",
        ] {
            assert!(
                !listed.iter().any(|name| name == hidden),
                "{path} lists {hidden}"
            );
        }
    }
}

/// T-4732: on every surface, the broker's tools included, each listed tool has a title of at
/// most 64 characters, the same in its annotations, and no two share one.
#[tokio::test]
async fn every_surface_lists_each_tool_with_its_own_title() {
    let realm = common::Realm::new();
    let (url, _) = broker().await;
    let app = router(Arc::new(gateway(&url, &realm)));
    for (path, _) in surfaces() {
        let (status, answer) = send(&app, rpc(&path, None, None, &list())).await;
        assert_eq!(status, StatusCode::OK, "{path}: {answer}");
        let mut titles = std::collections::HashSet::new();
        for tool in answer["result"]["tools"].as_array().into_iter().flatten() {
            let title = tool["title"].as_str().unwrap_or_default();
            assert!(
                !title.trim().is_empty() && title.chars().count() <= 64,
                "{path}: {tool}"
            );
            assert_eq!(
                tool["annotations"]["title"], tool["title"],
                "{path}: {tool}"
            );
            assert!(
                titles.insert(title.to_owned()),
                "{path}: two tools titled {title}"
            );
            for hint in ["readOnlyHint", "destructiveHint", "openWorldHint"] {
                assert!(
                    tool["annotations"][hint].is_boolean(),
                    "{path}: {hint} of {tool}"
                );
            }
        }
        assert!(titles.contains("Query entities"), "{path}: {titles:?}");
    }
}

#[tokio::test]
async fn without_a_token_a_write_is_refused_on_every_surface_before_the_broker() {
    let realm = common::Realm::new();
    let (url, seen) = broker().await;
    let app = router(Arc::new(gateway(&url, &realm)));
    for (path, arguments) in surfaces() {
        for name in [
            "create_subscription",
            "delete_subscription",
            "a_tool_the_broker_added_later",
        ] {
            let (status, answer) =
                send(&app, rpc(&path, None, None, &call(name, &arguments))).await;
            assert_eq!(status, StatusCode::OK, "{path} {name}: {answer}");
            assert_eq!(
                answer["error"]["code"],
                json!(-32602),
                "{path} {name}: {answer}"
            );
            let message = answer["error"]["message"].as_str().unwrap_or_default();
            assert!(message.contains(&format!("`{name}`")), "{path}: {message}");
            assert!(answer.get("result").is_none(), "{path} {name}: {answer}");
        }
    }
    assert_eq!(
        seen.load(Ordering::SeqCst),
        0,
        "a refused write reached the broker"
    );
}

#[tokio::test]
async fn a_token_holder_still_sees_the_writes_the_policy_grants() {
    let realm = common::Realm::new();
    let (url, _) = broker().await;
    let app = router(Arc::new(gateway(&url, &realm)));
    let token = realm.mint(&json!({
        "iss": common::ISSUER,
        "sub": "5e1d",
        "aud": "mcp-hub",
        "scope": "openid endpoint:*",
        "azp": "mcp-hub",
        "preferred_username": "jana",
        "groups": ["/mesto"],
        "realm_access": { "roles": ["steward"] },
        "exp": common::in_seconds(300),
        "iat": common::in_seconds(-10),
    }));
    let (status, answer) = send(&app, rpc("/api/mcp", Some(&token), None, &list())).await;
    assert_eq!(status, StatusCode::OK, "{answer}");
    let listed = names(&answer);
    for granted in [
        "create_subscription",
        "update_subscription",
        "delete_subscription",
    ] {
        assert!(
            listed.iter().any(|name| name == granted),
            "{granted}: {listed:?}"
        );
    }
}

#[tokio::test]
async fn one_address_without_a_token_has_one_allowance_over_every_surface() {
    let realm = common::Realm::new();
    let (url, _) = broker().await;
    let app = router(Arc::new(gateway(&url, &realm).limit_anonymous_mcp_to(
        RateLimits {
            requests_per_minute: 4,
            burst: Some(4),
        },
    )));
    // One call per surface spends the four; the fifth, on any surface, is 429.
    for (path, _) in surfaces() {
        let (status, answer) = send(&app, rpc(&path, None, Some("203.0.113.9"), &list())).await;
        assert_eq!(status, StatusCode::OK, "{path}: {answer}");
    }
    let (status, _) = send(&app, rpc("/api/mcp", None, Some("203.0.113.9"), &list())).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);

    // A client that writes its own X-Forwarded-For only prepends: the edge's entry is still last.
    let spoofed = "198.51.100.1, 203.0.113.9";
    let (status, _) = send(
        &app,
        rpc(&format!("/cs/{SPACE}/mcp"), None, Some(spoofed), &list()),
    )
    .await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);

    // Another address has its own allowance.
    let (status, answer) = send(&app, rpc("/api/mcp", None, Some("203.0.113.10"), &list())).await;
    assert_eq!(status, StatusCode::OK, "{answer}");
}

#[tokio::test]
async fn an_in_cluster_caller_without_the_edge_header_spends_no_anonymous_allowance() {
    let realm = common::Realm::new();
    let (url, _) = broker().await;
    let app = router(Arc::new(gateway(&url, &realm).limit_anonymous_mcp_to(
        RateLimits {
            requests_per_minute: 1,
            burst: Some(1),
        },
    )));
    let path = format!("/api/endpoint/{OPEN}/mcp");
    for _ in 0..3 {
        let (status, answer) = send(&app, rpc(&path, None, None, &list())).await;
        assert_eq!(status, StatusCode::OK, "{answer}");
    }
}

/// A broker whose view MCP answers `tools/list` with a body the gateway cannot parse.
async fn unreadable_broker(content_type: &'static str) -> String {
    let stub = axum::Router::new().fallback(move || async move {
        (
            [("content-type", content_type)],
            "<html>not a tool list</html>",
        )
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("a free port");
    let url = format!("http://{}", listener.local_addr().expect("an address"));
    tokio::spawn(async move {
        let _ = axum::serve(listener, stub).await;
    });
    url
}

#[tokio::test]
async fn without_a_token_a_tool_list_the_gateway_cannot_read_is_refused_not_passed_on() {
    let realm = common::Realm::new();
    // Not JSON by content type, and JSON by content type but not by body: both fail closed.
    for content_type in ["text/html", "application/json"] {
        let url = unreadable_broker(content_type).await;
        let app = router(Arc::new(gateway(&url, &realm)));
        for path in [
            format!("/api/endpoint/{OPEN}/mcp"),
            format!("/cs/{SPACE}/mcp"),
        ] {
            let (status, answer) = send(&app, rpc(&path, None, None, &list())).await;
            assert_eq!(status, StatusCode::OK, "{content_type} {path}: {answer}");
            assert_eq!(
                answer["error"]["code"],
                json!(-32603),
                "{content_type} {path}: {answer}"
            );
            assert!(
                answer.get("result").is_none(),
                "{content_type} {path}: {answer}"
            );
        }
    }
}

/// T-4191: every tool a named server lists to a caller without a token answers a call, never
/// `unknown tool`: a client offered a tool that never answers loses a turn on it.
#[tokio::test]
async fn without_a_token_every_tool_a_named_server_lists_answers_a_call() {
    let realm = common::Realm::new();
    let (url, _) = broker().await;
    let app = router(Arc::new(gateway(&url, &realm)));
    let path = "/api/mcp/mesto/open";
    let (_, listed) = send(&app, rpc(path, None, None, &list())).await;
    let listed = names(&listed);
    assert!(
        listed.iter().any(|name| name == "get_attribute_value"),
        "{listed:?}"
    );
    let arguments = json!({
        "endpoint": OPEN,
        "id": "urn:ngsi-ld:AirQualityObserved:sk001",
        "attrId": "pm10",
        "type": ["AirQualityObserved"],
    });
    for name in &listed {
        let (status, answer) = send(&app, rpc(path, None, None, &call(name, &arguments))).await;
        assert_eq!(status, StatusCode::OK, "{name}: {answer}");
        assert!(
            answer.get("error").is_none() && answer.get("result").is_some(),
            "{name} is listed but not answered: {answer}"
        );
    }
}

/// T-4191: an Endpoint's own MCP answers a caller without a token's tool list at once, as the
/// named server in front of it does.
#[tokio::test]
async fn without_a_token_an_endpoints_own_tool_list_answers_at_once() {
    let realm = common::Realm::new();
    let (url, _) = broker().await;
    let app = router(Arc::new(gateway(&url, &realm)));
    let path = format!("/api/endpoint/{OPEN}/mcp");
    let (status, answer) = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        send(&app, rpc(&path, None, None, &list())),
    )
    .await
    .expect("the tool list answers within 5 s");
    assert_eq!(status, StatusCode::OK, "{answer}");
    assert!(names(&answer).iter().any(|name| name == "query_entities"));
}
