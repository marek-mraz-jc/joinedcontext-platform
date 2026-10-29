//! T-5322: an MCP tool call reaches the project's Activity as one OTLP log record, through the
//! hub, a named server and the Endpoint's own URL, with no argument, value or token in it.
//!
//! Alone in its binary: the activity tally is the process's own.

#[path = "../common/mod.rs"]
mod common;

use axum::body::{Body, Bytes};
use axum::http::{Method, Request, StatusCode};
use context_gateway::activity;
use context_gateway::app::{router, Gateway};
use context_gateway::auth::accounts::ServiceAccounts;
use context_gateway::mcp::server::McpServer;
use context_gateway::pdp::PolicyPdp;
use context_gateway::proxy::Broker;
use context_gateway::resolver::Endpoint;
use jc_core::kinds::{Audience, PolicySpec, Representation};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use tower::ServiceExt;

const SLUG: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbb";
/// An argument value the record must never carry.
const SECRET_FILTER: &str = "pm10>4711";

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

/// An OTLP/HTTP logs receiver that keeps every body it is posted.
async fn receiver() -> (String, Arc<Mutex<Vec<Value>>>) {
    let bodies: Arc<Mutex<Vec<Value>>> = Arc::default();
    let kept = Arc::clone(&bodies);
    let app = axum::Router::new().route(
        "/v1/logs",
        axum::routing::post(move |body: Bytes| {
            let kept = Arc::clone(&kept);
            async move {
                let value: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
                kept.lock().expect("the bodies").push(value);
                "{}"
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("a port");
    let address = listener.local_addr().expect("its address");
    tokio::spawn(async move { axum::serve(listener, app).await });
    (format!("http://{address}/v1/logs"), bodies)
}

fn call(uri: &str, token: Option<&str>, arguments: Value) -> Request<Body> {
    let mut request = Request::builder()
        .method(Method::POST)
        .uri(uri)
        .header("content-type", "application/json");
    if let Some(token) = token {
        request = request.header("authorization", format!("Bearer {token}"));
    }
    request
        .body(Body::from(
            json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {
                "name": "query_entities",
                "arguments": arguments,
            }})
            .to_string(),
        ))
        .expect("a request")
}

fn named(record: &Value) -> BTreeMap<String, Value> {
    let mut named = BTreeMap::new();
    for attribute in record["attributes"].as_array().into_iter().flatten() {
        let key = attribute["key"].as_str().unwrap_or_default().to_owned();
        let value = &attribute["value"];
        let value = value
            .get("stringValue")
            .or_else(|| value.get("intValue"))
            .cloned()
            .unwrap_or(Value::Null);
        named.insert(key, value);
    }
    named
}

#[tokio::test]
async fn a_tool_call_reaches_the_collector_as_one_record_without_its_arguments() {
    let realm = common::Realm::new();
    let broker = common::BrokerStub::start(vec![json!([]), json!([]), json!([])]).await;
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
    // The reload names the space behind the tenant segment.
    activity::name_spaces(&BTreeMap::from([(
        "ovzdusie".to_owned(),
        "air-quality".to_owned(),
    )]));
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
    let arguments = json!({ "type": "AirQualityObserved", "q": SECRET_FILTER });
    let mut hub = arguments.clone();
    hub["endpoint"] = json!(SLUG);
    for (uri, arguments) in [
        ("/api/mcp".to_owned(), hub),
        ("/api/mcp/ovzdusie/air".to_owned(), arguments.clone()),
        (format!("/api/endpoint/{SLUG}/mcp"), arguments),
    ] {
        let response = app
            .clone()
            .oneshot(call(&uri, Some(&token), arguments))
            .await
            .expect("the gateway answers");
        assert_eq!(response.status(), StatusCode::OK, "{uri}");
    }

    let (url, bodies) = receiver().await;
    let client = reqwest::Client::new();
    activity::flush(&client, &url, false).await;
    let posted = bodies.lock().expect("the bodies").clone();
    assert_eq!(posted.len(), 1, "one post: {posted:?}");
    let text = posted[0].to_string();
    for absent in [SECRET_FILTER, "AirQualityObserved", token.as_str(), "0f5a"] {
        assert!(
            !text.contains(absent),
            "{absent} reached the record: {text}"
        );
    }
    let records = posted[0]["resourceLogs"][0]["scopeLogs"][0]["logRecords"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let rows: Vec<BTreeMap<String, Value>> = records.iter().map(named).collect();
    let calls: u64 = rows
        .iter()
        .map(|row| {
            row["count"]
                .as_str()
                .unwrap_or("0")
                .parse::<u64>()
                .unwrap_or(0)
        })
        .sum();
    assert_eq!(calls, 3, "three calls: {rows:?}");
    for row in &rows {
        assert_eq!(row["kind"], json!("mcp.tool"));
        assert_eq!(row["source"], json!("gateway"));
        assert_eq!(row["project"], json!("ovzdusie"));
        assert_eq!(row["space"], json!("air-quality"));
        assert_eq!(row["endpoint"], json!(SLUG));
        assert_eq!(row["tool"], json!("query_entities"));
        let summary = row["summary"].as_str().unwrap_or_default();
        assert!(
            summary.starts_with(&format!("user:jana called query_entities on {SLUG}")),
            "{summary}"
        );
    }

    // An anonymous caller's calls wait for the minute's post, then go as one row; a collector
    // that is away drops them without holding anything.
    // (An anonymous caller reaches no Endpoint of this audience: recorded here directly.)
    let anonymous = context_gateway::auth::subject::Subject::default();
    for _ in 0..2 {
        activity::record(
            &endpoint(),
            &anonymous,
            "query_entities",
            activity::Outcome::Answered,
        );
    }
    activity::flush(&client, &url, false).await;
    assert_eq!(
        bodies.lock().expect("the bodies").len(),
        1,
        "held for the minute"
    );
    activity::flush(&client, &url, true).await;
    let posted = bodies.lock().expect("the bodies").clone();
    assert_eq!(posted.len(), 2);
    let records = posted[1]["resourceLogs"][0]["scopeLogs"][0]["logRecords"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let anonymous_rows: Vec<BTreeMap<String, Value>> = records.iter().map(named).collect();
    assert!(
        anonymous_rows.iter().all(|row| row["summary"]
            .as_str()
            .unwrap_or_default()
            .starts_with("anonymous called")),
        "{anonymous_rows:?}"
    );
    let anonymous_calls: u64 = anonymous_rows
        .iter()
        .map(|row| {
            row["count"]
                .as_str()
                .unwrap_or("0")
                .parse::<u64>()
                .unwrap_or(0)
        })
        .sum();
    assert_eq!(anonymous_calls, 2);
    activity::record(
        &endpoint(),
        &anonymous,
        "get_entity",
        activity::Outcome::Answered,
    );
    activity::flush(&client, "http://127.0.0.1:9/v1/logs", true).await;
    assert!(activity::take(true).is_empty(), "a refused post is dropped");
}
