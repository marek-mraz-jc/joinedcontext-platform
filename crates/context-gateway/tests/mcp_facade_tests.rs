//! T-0166: the endpoint's own MCP instance (EP-24, EP-25, EP-26, AG-04, AG-05, SP-14…SP-20).

mod common;

use axum::body::Body;
use axum::extract::State;
use axum::http::{Method, Request, StatusCode};
use axum::routing::any;
use axum::Router;
use context_gateway::app::{router, Gateway};
use context_gateway::auth::accounts::ServiceAccounts;
use context_gateway::pdp::PolicyPdp;
use context_gateway::proxy::Broker;
use context_gateway::resolver::Endpoint;
use jc_core::kinds::{Audience, PolicySpec, Representation};
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
use tower::ServiceExt;

const SLUG: &str = "k4y7pq2mzt6vhx3nbwrs5cjd8f";
const NO_MCP: &str = "zt4qm7ge2xdv6ksb3ncf5arw2y";

fn policy(yaml: &str) -> PolicySpec {
    serde_norway::from_str(yaml).expect("the policy spec parses")
}

/// The demo endpoint: anonymous callers read air quality, a steward also writes it and
/// may list the types.
fn endpoint(slug: &str, representations: Vec<Representation>) -> Endpoint {
    Endpoint {
        declared_types: None,
        roles: Default::default(),
        slug: slug.to_owned(),
        title: std::collections::BTreeMap::new(),
        description: std::collections::BTreeMap::new(),
        space: "ovzdusie".to_owned(),
        project: "ovzdusie".to_owned(),
        audience: Audience::Public,
        allowed_projects: Vec::new(),
        representations,
        rate_limit: None,
        creates: None,
        file_limits: None,
        hidden_attributes: Default::default(),
        projection: None,
        base_path: format!("/api/endpoint/{slug}"),
        models: Vec::new(),
        view_mapping: None,
        catalog: None,
        deprecation: None,
        policy_names: Vec::new(),
        manifest_name: String::new(),
        policies: vec![
            policy(
                r#"contextSpaceRef: ovzdusie
assigner: did:web:banskabystrica.sk
assignee: { kind: role, id: public }
operations: [queryEntity, retrieveEntity]
information:
  - entities:
      - type: AirQualityObserved
    propertyNames: [pm10, pm25, location]
"#,
            ),
            policy(
                r#"contextSpaceRef: ovzdusie
assigner: did:web:banskabystrica.sk
assignee: { kind: role, id: steward }
operations: [queryEntity, retrieveEntity, retrieveEntityTypes, queryTemporal, upsertBatch]
information:
  - entities:
      - type: AirQualityObserved
"#,
            ),
        ],
    }
}

/// The gateway of every test: both endpoints, and the throwaway realm's verifier so a
/// token can be presented at all.
fn gateway(broker: &str, realm: &common::Realm) -> Arc<Gateway> {
    Arc::new(
        Gateway::new(
            Broker::new(broker),
            Box::new(PolicyPdp),
            "banskabystrica.sk",
        )
        .serve([
            endpoint(SLUG, vec![Representation::NgsiLd, Representation::Mcp]),
            endpoint(NO_MCP, vec![Representation::NgsiLd]),
        ])
        .authenticate(Arc::new(realm.verifier()), ServiceAccounts::new(), None),
    )
}

fn app(broker: &str, realm: &common::Realm) -> Router {
    router(gateway(broker, realm))
}

/// One JSON-RPC message, with an optional token.
fn message(slug: &str, token: Option<&str>, body: Value) -> Request<Body> {
    let mut builder = Request::builder()
        .method(Method::POST)
        .uri(format!("/api/endpoint/{slug}/mcp"))
        .header("content-type", "application/json");
    if let Some(token) = token {
        builder = builder.header("authorization", format!("Bearer {token}"));
    }
    builder
        .body(Body::from(body.to_string()))
        .expect("a request")
}

async fn send(app: Router, request: Request<Body>) -> (StatusCode, Value) {
    let response = app.oneshot(request).await.expect("the gateway answers");
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .expect("a body");
    let payload = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, payload)
}

/// The paths the broker was asked for.
type Log = Arc<Mutex<Vec<String>>>;

/// A broker that answers an empty result set and records the request it was given.
async fn stub_broker() -> (String, Log) {
    let log: Log = Arc::new(Mutex::new(Vec::new()));
    let recorder = {
        let log = Arc::clone(&log);
        Router::new().fallback(any(record)).with_state(log)
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("a free port");
    let address = listener.local_addr().expect("the bound address");
    tokio::spawn(async move {
        let _ = axum::serve(listener, recorder).await;
    });
    (format!("http://{address}"), log)
}

async fn record(
    State(log): State<Log>,
    request: Request<Body>,
) -> ([(&'static str, &'static str); 1], String) {
    // The access surface of a view is the engine's (T-3649): one public grant, as the engine
    // writes it for the anonymous caller.
    let access = request.uri().path().ends_with("/access");
    log.lock().expect("the log").push(request.uri().to_string());
    let body = if access {
        json!({
            "subject": { "type": "role", "id": "public" },
            "resource": { "type": "view", "id": SLUG },
            "permissions": [{ "resource": { "type": "AirQualityObserved" },
                              "actions": ["queryEntity"], "attributes": "*", "constraints": {} }],
            "prohibitions": []
        })
        .to_string()
    } else {
        "[]".to_owned()
    };
    ([("content-type", "application/json")], body)
}

#[tokio::test]
async fn a_notification_is_acknowledged_and_answered_with_nothing() {
    let realm = common::Realm::new();
    let response = app("http://127.0.0.1:1", &realm)
        .oneshot(message(
            SLUG,
            None,
            json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }),
        ))
        .await
        .expect("the gateway answers");

    assert_eq!(response.status(), StatusCode::ACCEPTED);
    let bytes = axum::body::to_bytes(response.into_body(), 1024)
        .await
        .expect("a body");
    assert!(bytes.is_empty(), "a notification has no answer");
}

/// EP-05: the surface exists only where the endpoint enables the representation.
#[tokio::test]
async fn the_mcp_surface_is_not_there_where_the_endpoint_does_not_enable_it() {
    let realm = common::Realm::new();
    let (status, _) = send(
        app("http://127.0.0.1:1", &realm),
        message(
            NO_MCP,
            None,
            json!({ "jsonrpc": "2.0", "id": 9, "method": "tools/list" }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn a_body_that_is_not_json_is_the_parse_error() {
    let realm = common::Realm::new();
    let broken = Request::builder()
        .method(Method::POST)
        .uri(format!("/api/endpoint/{SLUG}/mcp"))
        .header("content-type", "application/json")
        .body(Body::from("{ not json"))
        .expect("a request");
    let (status, answer) = send(app("http://127.0.0.1:1", &realm), broken).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(answer["error"]["code"], -32700);

    // A method and a request the gateway does not read itself are the broker's to answer
    // (T-3647, Antares `mcp_transport`).
}

/// EP-47, EP-55: `describe_access` reads the view's access surface the HTTP document reads too
/// (T-3649). The schema is the broker's LinkML surface (T-3645, T-3646, `mcp_schema_tests.rs`);
/// a narrowed call for a formalism the broker renders for no one class is refused before any
/// hop.
#[tokio::test]
async fn the_describing_tools_answer_from_the_endpoint_and_its_view() {
    let realm = common::Realm::new();
    let (broker, log) = stub_broker().await;
    let call = |id: u32, name: &str, arguments: Value| {
        json!({ "jsonrpc": "2.0", "id": id, "method": "tools/call", "params": {
            "name": name, "arguments": arguments,
        }})
    };

    let (_, access) = send(
        app(&broker, &realm),
        message(SLUG, None, call(11, "describe_access", json!({}))),
    )
    .await;
    assert_eq!(access["result"]["isError"], false);
    let permissions = &access["result"]["structuredContent"]["access"];
    assert_eq!(permissions["resource"]["id"], SLUG);
    assert_eq!(permissions["resource"]["space"], "ovzdusie");
    assert!(
        permissions["permissions"]
            .as_array()
            .expect("the caller's own grants")
            .len()
            == 1,
        "the anonymous caller sees the public grant and nobody else's (R20)"
    );

    let (_, shacl) = send(
        app(&broker, &realm),
        message(
            SLUG,
            None,
            call(
                13,
                "describe_schema",
                json!({ "format": "shacl", "entityType": "AirQualityObserved" }),
            ),
        ),
    )
    .await;
    assert_eq!(
        shacl["result"]["isError"], true,
        "a formalism no class description carries is named, never approximated"
    );

    // `describe_access` reads the view's access surface, the document `GET …/access` serves
    // (EP-60, T-3649); `describe_schema` asks the broker nothing.
    assert_eq!(
        *log.lock().expect("the log"),
        [format!("/x/views/{SLUG}/access")]
    );
}

/// T-0425, EP-60: `describe_access` speaks the three languages the HTTP access surface speaks,
/// and says the same thing in each; a word it does not know is refused, never defaulted.
#[tokio::test]
async fn describe_access_answers_each_format_with_the_document_the_http_surface_serves() {
    let realm = common::Realm::new();
    let (broker, _log) = stub_broker().await;
    let call = |id: u32, arguments: Value| {
        json!({ "jsonrpc": "2.0", "id": id, "method": "tools/call", "params": {
            "name": "describe_access", "arguments": arguments,
        }})
    };
    let over_http = |accept: &'static str| {
        let app = app(&broker, &realm);
        async move {
            let request = Request::builder()
                .uri(format!("/api/endpoint/{SLUG}/access"))
                .header("accept", accept)
                .body(Body::empty())
                .expect("a request");
            send(app, request).await.1
        }
    };

    for (id, format, accept) in [
        (21, "permissions", "application/json"),
        (22, "odrl", "application/odrl+json"),
        (
            23,
            "grant-ast",
            "application/vnd.joinedcontext.grant-ast+json",
        ),
    ] {
        let (_, answer) = send(
            app(&broker, &realm),
            message(SLUG, None, call(id, json!({ "format": format }))),
        )
        .await;
        assert_eq!(answer["result"]["isError"], false, "{format}: {answer}");
        assert_eq!(
            answer["result"]["structuredContent"]["access"],
            over_http(accept).await,
            "{format} over MCP is the {accept} document"
        );
    }

    let (_, unknown) = send(
        app(&broker, &realm),
        message(SLUG, None, call(24, json!({ "format": "xacml" }))),
    )
    .await;
    assert_eq!(
        unknown["result"]["isError"], true,
        "an unknown format is refused: {unknown}"
    );

    let (_, resource) = send(
        app(&broker, &realm),
        message(
            SLUG,
            None,
            json!({ "jsonrpc": "2.0", "id": 25, "method": "resources/read", "params": {
                "uri": format!("access://{SLUG}?format=odrl"),
            }}),
        ),
    )
    .await;
    let text = resource["result"]["contents"][0]["text"]
        .as_str()
        .map(|t| serde_json::from_str::<Value>(t).expect("json text"))
        .unwrap_or_else(|| resource["result"]["contents"][0].clone());
    assert_eq!(
        text,
        over_http("application/odrl+json").await,
        "the resource takes the same format"
    );
}
