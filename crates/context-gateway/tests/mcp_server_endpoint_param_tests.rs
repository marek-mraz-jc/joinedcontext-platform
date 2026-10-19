//! What a caller can do with a named server's `endpoint` argument and tool list (EP-141,
//! ADR-N-043 §9, T-4750): a public server reads only whoever calls, `endpoint` is a name inside
//! the caller's reach that is never fetched, no tool takes a URL, a filter stays one broker
//! parameter, and no answer names a cluster-internal host.

mod common;

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
use serde_json::{json, Value};
use std::sync::Arc;
use tower::ServiceExt;

const HOST: &str = "https://city.example";
/// The public server's one member: anybody reads it, and its Policy grants writes too.
const OPEN: &str = "eeeeeeeeeeeeeeeeeeeeeeeeee";
/// Public and readable, but no member of the public server.
const OUTSIDE: &str = "dddddddddddddddddddddddddd";
/// An organization Endpoint, a member of the organization's server only.
const CLOSED: &str = "cccccccccccccccccccccccccc";

const PUBLIC_SERVER: &str = "/api/mcp/ovzdusie/open";
const ORG_SERVER: &str = "/api/mcp/ovzdusie/mobility";

const ENTITY: &str = "urn:ngsi-ld:AirQualityObserved:banskabystrica.sk:ovzdusie:st-1";
const WRITES: &[&str] = &[
    "create_entity",
    "batch_upsert",
    "batch_update",
    "batch_delete",
    "update_attributes",
    "append_attributes",
    "merge_entity",
    "replace_attribute",
    "update_attribute",
    "delete_attribute",
    "delete_entity",
    "create_subscription",
];

fn grant(role: &str, operations: &str) -> PolicySpec {
    serde_norway::from_str(&format!(
        "contextSpaceRef: ovzdusie\n\
         assigner: did:web:banskabystrica.sk\n\
         assignee: {{ kind: role, id: {role} }}\n\
         operations: [{operations}]\n\
         information:\n  \
         - entities:\n      \
         - type: AirQualityObserved\n"
    ))
    .expect("the policy spec parses")
}

fn endpoint(slug: &str, audience: Audience, policies: Vec<PolicySpec>) -> Endpoint {
    Endpoint {
        declared_types: None,
        roles: Default::default(),
        slug: slug.to_owned(),
        title: [("en".to_owned(), format!("Air {slug}"))].into(),
        description: Default::default(),
        space: format!("space-{}", &slug[..4]),
        project: "ovzdusie".to_owned(),
        audience,
        allowed_projects: Vec::new(),
        representations: vec![Representation::NgsiLd, Representation::Mcp],
        rate_limit: None,
        creates: None,
        file_limits: None,
        hidden_attributes: Default::default(),
        projection: None,
        view_mapping: None,
        catalog: None,
        base_path: format!("/api/endpoint/{slug}"),
        models: Vec::new(),
        policy_names: Vec::new(),
        policies,
    }
}

fn server(name: &str, members: &[&str], audience: Audience) -> McpServer {
    McpServer {
        project: "ovzdusie".to_owned(),
        name: name.to_owned(),
        title: Some("Air".to_owned()),
        description: Some("The city's air quality.".to_owned()),
        members: members.iter().map(|slug| (*slug).to_owned()).collect(),
        select: None,
        endpoints: None,
        audience,
        allowed_projects: Vec::new(),
        knowledge_sources: Vec::new(),
    }
}

fn app(broker: &str, realm: &common::Realm) -> Router {
    let writes = "queryEntity, retrieveEntity, upsertBatch, createEntity, deleteEntity";
    let gateway = Gateway::new(
        Broker::new(broker),
        Box::new(PolicyPdp),
        "banskabystrica.sk",
    )
    .serve(vec![
        endpoint(
            OPEN,
            Audience::Public,
            vec![grant("public", writes), grant("steward", writes)],
        ),
        endpoint(
            OUTSIDE,
            Audience::Public,
            vec![grant("public", "queryEntity")],
        ),
        endpoint(
            CLOSED,
            Audience::Organization,
            vec![grant("steward", writes)],
        ),
    ])
    .authenticate(
        Arc::new(realm.verifier()),
        ServiceAccounts::new(),
        Some(HOST.to_owned()),
    );
    gateway.replace_servers(vec![
        server("open", &[OPEN], Audience::Public),
        server("mobility", &[OPEN, CLOSED], Audience::Organization),
    ]);
    router(Arc::new(gateway))
}

/// A steward of the project, with the edge's audience, which every named server admits.
fn steward(realm: &common::Realm) -> String {
    realm.mint(&json!({
        "iss": common::ISSUER,
        "sub": "0f5a",
        "aud": "context-gateway",
        "scope": "openid",
        "preferred_username": "jana",
        "groups": ["/ovzdusie"],
        "realm_access": { "roles": ["steward"] },
        "exp": common::in_seconds(300),
        "iat": common::in_seconds(-10),
    }))
}

fn post(path: &str, token: Option<&str>, body: &Value) -> Request<Body> {
    let mut builder = Request::builder()
        .method(Method::POST)
        .uri(path)
        .header("content-type", "application/json");
    if let Some(token) = token {
        builder = builder.header("authorization", format!("Bearer {token}"));
    }
    builder
        .body(Body::from(body.to_string()))
        .expect("a request")
}

fn call(name: &str, arguments: Value) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": { "name": name, "arguments": arguments },
    })
}

fn list() -> Value {
    json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list" })
}

async fn raw(app: &Router, request: Request<Body>) -> (StatusCode, String) {
    let response = app
        .clone()
        .oneshot(request)
        .await
        .expect("the gateway answers");
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 4 * 1024 * 1024)
        .await
        .expect("a body");
    (status, String::from_utf8_lossy(&bytes).into_owned())
}

async fn send(app: &Router, request: Request<Body>) -> Value {
    let (status, body) = raw(app, request).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    serde_json::from_str(&body).unwrap_or_else(|_| panic!("JSON: {body}"))
}

fn names(tools: &Value) -> Vec<String> {
    tools["result"]["tools"]
        .as_array()
        .unwrap_or_else(|| panic!("a tool list: {tools}"))
        .iter()
        .map(|tool| tool["name"].as_str().expect("a name").to_owned())
        .collect()
}

/// EP-141, §9.1: a public server offers and runs no write, with or without a token, although its
/// member grants writes; the organization's server still offers them to the same steward.
#[tokio::test]
async fn a_public_server_reads_only_whoever_calls() {
    let realm = common::Realm::new();
    let broker = common::BrokerStub::start(vec![json!([])]).await;
    let app = app(&broker.url, &realm);
    let token = steward(&realm);

    for caller in [None, Some(token.as_str())] {
        let listed = names(&send(&app, post(PUBLIC_SERVER, caller, &list())).await);
        assert!(listed.contains(&"query_entities".to_owned()), "{listed:?}");
        for write in WRITES {
            assert!(
                !listed.contains(&(*write).to_owned()),
                "{write} listed to {caller:?}"
            );
        }
        for write in ["batch_upsert", "create_entity", "delete_entity"] {
            let answer = send(
                &app,
                post(
                    PUBLIC_SERVER,
                    caller,
                    &call(
                        write,
                        json!({ "endpoint": OPEN, "entities": [{ "id": ENTITY, "type": "AirQualityObserved" }], "entity": { "id": ENTITY, "type": "AirQualityObserved" }, "entityId": ENTITY }),
                    ),
                ),
            )
            .await;
            assert_eq!(answer["error"]["code"], json!(-32602), "{write}: {answer}");
            let message = answer["error"]["message"].as_str().expect("a message");
            assert!(message.contains("reads only"), "{message}");
        }
    }
    assert!(
        broker.hops().is_empty(),
        "a write reached the broker: {:?}",
        broker.hops()
    );

    let listed = names(&send(&app, post(ORG_SERVER, Some(&token), &list())).await);
    assert!(listed.contains(&"batch_upsert".to_owned()), "{listed:?}");
}

/// EP-141, §9.2: `endpoint` is a name inside the reach. An address, a path, an encoded slash, an
/// IP, a slug outside the server, an Endpoint the caller may not read and a megabyte of text all
/// answer the sentence a name nobody has answers, and none reaches the broker.
#[tokio::test]
async fn endpoint_is_a_name_inside_the_reach_and_never_fetched() {
    let realm = common::Realm::new();
    let broker = common::BrokerStub::start(vec![json!([])]).await;
    let app = app(&broker.url, &realm);
    let miss = |named: &str| {
        call(
            "query_entities",
            json!({ "endpoint": named, "type": ["AirQualityObserved"] }),
        )
    };
    let unknown = "zzzzzzzzzzzzzzzzzzzzzzzzzz";
    let expected = send(&app, post(PUBLIC_SERVER, None, &miss(unknown))).await;
    assert!(expected["error"].is_object(), "{expected}");

    let hostile = [
        OUTSIDE.to_owned(),
        CLOSED.to_owned(),
        "https://evil.example/ngsi-ld/v1/entities".to_owned(),
        "http://169.254.169.254/latest/meta-data/".to_owned(),
        "http://127.0.0.1:8080/".to_owned(),
        "//evil.example".to_owned(),
        "file:///etc/passwd".to_owned(),
        format!("../{OUTSIDE}"),
        format!("{OPEN}/../{OUTSIDE}"),
        format!("{OPEN}%2F..%2F{OUTSIDE}"),
        format!("ovzdusie/{OUTSIDE}"),
        "10.0.0.1".to_owned(),
        "antares.antares.svc.cluster.local".to_owned(),
        "\u{0000}\r\nHost: evil.example".to_owned(),
        String::new(),
    ];
    // A slug-shaped miss and any other miss take two sentences (the other echoes the name and the
    // closest names of the reach); within a shape the answer never tells an outside or closed
    // Endpoint from no Endpoint at all.
    let other = "no-such-name";
    let reference = send(&app, post(PUBLIC_SERVER, None, &miss(other))).await;
    let reference = reference["error"]["message"]
        .as_str()
        .expect("a message")
        .to_owned();
    for named in &hostile {
        let answer = send(&app, post(PUBLIC_SERVER, None, &miss(named))).await;
        if named.len() == 26 && named.bytes().all(|b| b.is_ascii_alphanumeric()) {
            assert_eq!(
                answer, expected,
                "{named:?} told more than a slug nobody has"
            );
        } else {
            assert_eq!(
                answer["error"]["code"],
                json!(-32602),
                "{named:?}: {answer}"
            );
            assert_eq!(
                answer["error"]["message"],
                json!(reference.replace(other, named)),
                "{named:?} told more than a name nobody has"
            );
        }
    }

    let huge = "a".repeat(1 << 20);
    let (status, body) = raw(&app, post(PUBLIC_SERVER, None, &miss(&huge))).await;
    assert!(!status.is_server_error(), "{status}");
    assert!(!body.contains("panicked"), "{body}");
    assert!(
        broker.hops().is_empty(),
        "a miss reached the broker: {:?}",
        broker.hops()
    );
}

/// Every string property of every tool's input schema, with its path.
fn properties(schema: &Value, path: &str, out: &mut Vec<String>) {
    if let Some(map) = schema.get("properties").and_then(Value::as_object) {
        for (name, inner) in map {
            out.push(format!("{path}.{name}"));
            properties(inner, &format!("{path}.{name}"), out);
        }
    }
    for key in ["items", "additionalProperties"] {
        if let Some(inner) = schema.get(key) {
            properties(inner, &format!("{path}[]"), out);
        }
    }
    for key in ["anyOf", "oneOf", "allOf"] {
        for inner in schema
            .get(key)
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            properties(inner, path, out);
        }
    }
}

/// EP-141, §9.3: no tool of the hub or of a server, to anybody, takes an address to fetch.
#[tokio::test]
async fn no_tool_takes_a_url() {
    let realm = common::Realm::new();
    let broker = common::BrokerStub::start(vec![json!([])]).await;
    let app = app(&broker.url, &realm);
    let token = steward(&realm);
    for (path, caller) in [
        (PUBLIC_SERVER, None),
        (PUBLIC_SERVER, Some(token.as_str())),
        (ORG_SERVER, Some(token.as_str())),
        ("/api/mcp", None),
    ] {
        let tools = send(&app, post(path, caller, &list())).await;
        for tool in tools["result"]["tools"].as_array().expect("tools") {
            let mut found = Vec::new();
            properties(&tool["inputSchema"], "", &mut found);
            for property in found {
                let last = property
                    .rsplit('.')
                    .next()
                    .unwrap_or_default()
                    .to_lowercase();
                assert!(
                    ![
                        "url",
                        "uri",
                        "href",
                        "link",
                        "address",
                        "host",
                        "endpointurl",
                        "callback"
                    ]
                    .contains(&last.as_str()),
                    "{path} {}: {property}",
                    tool["name"]
                );
            }
        }
    }
}

/// A filter is one parameter of one broker request: an injected `&`, `;`, quote or SQL never
/// adds a parameter, changes the path or the tenant, or reaches the broker unparsed.
#[tokio::test]
async fn a_filter_stays_one_broker_parameter() {
    let realm = common::Realm::new();
    let broker = common::BrokerStub::start(vec![json!([])]).await;
    let app = app(&broker.url, &realm);
    let filters = [
        "pm10>40&type=Secret",
        "pm10>40;name==\"x\" OR 1=1 --",
        "name=='; DROP TABLE entities; --'",
        "pm10>40|(((((((((((((((((((((((((((((((((x",
        "name~=\".*\"&local=true&tenant=other",
    ];
    for q in filters {
        let answer = send(
            &app,
            post(
                PUBLIC_SERVER,
                None,
                &call(
                    "query_entities",
                    json!({ "endpoint": OPEN, "type": ["AirQualityObserved"], "q": q }),
                ),
            ),
        )
        .await;
        assert!(!answer.to_string().contains("panicked"), "{answer}");
    }
    // Each call is one JSON-RPC message to the member's own MCP under its pinned tenant, the
    // filter one JSON string in it, as the caller wrote it: nothing concatenated into a path.
    let hops = broker.hops();
    assert_eq!(hops.len(), filters.len(), "{hops:?}");
    for (hop, q) in hops.iter().zip(filters) {
        assert_eq!(hop.path, "/mcp", "{hop:?}");
        assert_eq!(hop.view, OPEN);
        assert_eq!(hop.tenant, "space-eeee");
        assert!(hop.query.is_empty() && !hop.forged, "{hop:?}");
        let sent: Value = serde_json::from_slice(&hop.body).expect("a JSON-RPC message");
        let arguments = &sent["params"]["arguments"];
        assert_eq!(arguments["q"], json!(q));
        assert_eq!(arguments["type"], json!(["AirQualityObserved"]));
        assert!(arguments.get("endpoint").is_none(), "{arguments}");
    }
}

/// EP-141, §9.3: a broker the gateway cannot reach is a tool error that names no cluster host.
#[tokio::test]
async fn an_unreachable_broker_is_an_error_that_names_no_internal_host() {
    let realm = common::Realm::new();
    let app = app("http://broker.antares.svc.cluster.local:1", &realm);
    let answer = send(
        &app,
        post(
            PUBLIC_SERVER,
            None,
            &call(
                "query_entities",
                json!({ "endpoint": OPEN, "type": ["AirQualityObserved"] }),
            ),
        ),
    )
    .await;
    let text = answer.to_string();
    assert!(
        answer["result"]["isError"] == json!(true) || answer["error"].is_object(),
        "{text}"
    );
    for internal in ["svc.cluster.local", "antares", ":1/"] {
        assert!(!text.contains(internal), "{internal} in {text}");
    }
}
