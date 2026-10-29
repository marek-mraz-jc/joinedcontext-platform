//! The hub and the named servers advertise the broker's own page default and maximum (T-4804):
//! `limit` and `lastN` in the tool schemas and the read tools' descriptions carry what the
//! broker's `/q/health` publishes, read once at start.

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
use jc_core::kinds::{Audience, PolicySpec, Representation};
use serde_json::{json, Value};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use tower::ServiceExt;

const HOST: &str = "https://city.example";
/// Public, and its Policy grants the public role the entity and history reads.
const OPEN: &str = "oooooooooooooooooooooooooo";
/// A Context Space anyone may use, with the same grant.
const SPACE: &str = "ovzdusie";
const READS: &str = "queryEntity, retrieveEntity, queryTemporal, retrieveTemporal";

fn grant(space: &str, role: &str) -> PolicySpec {
    serde_norway::from_str(&format!(
        "contextSpaceRef: {space}\n\
         assigner: did:web:banskabystrica.sk\n\
         assignee: {{ kind: role, id: {role} }}\n\
         operations: [{READS}]\n\
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
        deprecation: None,
        feeds: Vec::new(),
        calendars: Vec::new(),
        map_defaults: Default::default(),
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
            if request.uri().path() == "/q/health" {
                return axum::Json(json!({ "status": "UP", "limits": {
                    "maxLimit": 700, "defaultLimit": 70,
                    "temporalInstanceLimit": 800, "temporalDefaultInstances": 80,
                }}));
            }
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

/// A fake broker publishes 70/700 Entities and 80/800 values: the hub and the named server
/// list `limit` and `lastN` with those defaults and maxima in their schemas.
#[tokio::test]
async fn the_hub_and_a_named_server_advertise_the_broker_s_limits() {
    let realm = common::Realm::new();
    let (url, _) = broker().await;
    let limits = context_gateway::mcp::limits::read(&Broker::new(url.as_str()))
        .await
        .expect("the fake broker's health");
    assert_eq!((limits.default_limit, limits.max_limit), (70, 700));
    // `follow` keeps them for every list from now on, as the binary does at start
    context_gateway::mcp::limits::follow(Broker::new(url.as_str())).await;
    let app = router(Arc::new(gateway(&url, &realm)));
    for path in ["/api/mcp", "/api/mcp/mesto/open"] {
        let (status, answer) = send(&app, rpc(path, None, None, &list())).await;
        assert_eq!(status, StatusCode::OK, "{path}: {answer}");
        let tool = |name: &str| {
            answer["result"]["tools"]
                .as_array()
                .and_then(|tools| tools.iter().find(|tool| tool["name"] == name))
                .cloned()
                .unwrap_or_else(|| panic!("{path} lists no {name}: {answer}"))
        };
        let query = tool("query_entities");
        let limit = &query["inputSchema"]["properties"]["limit"];
        assert_eq!(
            (&limit["default"], &limit["maximum"]),
            (&json!(70), &json!(700)),
            "{path}"
        );
        // Said once, by the schema: the description repeats none of it (T-6058).
        let said = query["description"].as_str().unwrap_or_default();
        assert!(!said.contains("700"), "{said}");
        let history = tool("query_temporal");
        let last_n = &history["inputSchema"]["properties"]["lastN"];
        assert_eq!(
            (&last_n["default"], &last_n["maximum"]),
            (&json!(80), &json!(800)),
            "{path}"
        );
        let said = history["description"].as_str().unwrap_or_default();
        assert!(!said.contains("800"), "{said}");
    }
}
