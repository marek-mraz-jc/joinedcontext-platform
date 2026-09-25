//! T-0166: the endpoint's own MCP instance (EP-24, EP-25, EP-26, AG-04, AG-05, SP-14…SP-20).

mod common;

use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderMap, Method, Request, StatusCode};
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
const ENTITY: &str = "urn:ngsi-ld:AirQualityObserved:banskabystrica.sk:ovzdusie:st-1";

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
        file_limits: None,
        hidden_attributes: Default::default(),
        projection: None,
        base_path: format!("/api/endpoint/{slug}"),
        models: Vec::new(),
        view_mapping: None,
        catalog: None,
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

/// The tool names of a `tools/list` answer, sorted.
fn tool_names(answer: &Value) -> Vec<String> {
    let mut names: Vec<String> = answer["result"]["tools"]
        .as_array()
        .expect("a tool list")
        .iter()
        .map(|tool| tool["name"].as_str().unwrap_or_default().to_owned())
        .collect();
    names.sort();
    names
}

/// A steward's token: a human of the organization holding the `steward` realm role.
fn steward(realm: &common::Realm) -> String {
    realm.mint(&json!({
        "iss": common::ISSUER,
        "sub": "0f5a",
        "aud": SLUG,
        "preferred_username": "jana",
        "realm_access": { "roles": ["steward"] },
        "exp": common::in_seconds(300),
        "iat": common::in_seconds(-10),
    }))
}

/// What the broker was asked, so a test can see what came out of the enforcement path.
#[derive(Clone, Debug)]
struct Seen {
    method: String,
    uri: String,
    tenant: Option<String>,
    authorization: Option<String>,
}

type Log = Arc<Mutex<Vec<Seen>>>;

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
    let header = |headers: &HeaderMap, name: &str| {
        headers
            .get(name)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned)
    };
    log.lock().expect("the log").push(Seen {
        method: request.method().to_string(),
        uri: request.uri().to_string(),
        tenant: header(request.headers(), "ngsild-tenant"),
        authorization: header(request.headers(), "authorization"),
    });
    ([("content-type", "application/json")], "[]".to_owned())
}

#[tokio::test]
async fn the_handshake_names_the_protocol_the_server_and_the_one_space_it_serves() {
    let realm = common::Realm::new();
    let (_, answer) = send(
        app("http://127.0.0.1:1", &realm),
        message(
            SLUG,
            None,
            json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
                "protocolVersion": "2025-06-18",
                "clientInfo": { "name": "an agent", "version": "0" },
            }}),
        ),
    )
    .await;

    assert_eq!(answer["jsonrpc"], "2.0");
    assert_eq!(answer["id"], 1);
    assert_eq!(answer["result"]["protocolVersion"], "2025-06-18");
    assert_eq!(
        answer["result"]["serverInfo"]["name"],
        format!("joinedcontext-endpoint-{SLUG}")
    );
    assert_eq!(
        answer["result"]["capabilities"]["tools"]["listChanged"], false,
        "a stateless server has nobody to notify (SP-19)"
    );
    assert!(
        answer["result"]["instructions"]
            .as_str()
            .expect("instructions")
            .contains("ovzdusie"),
        "the agent is told which space its URNs live in"
    );
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

/// EP-25, SP-15: the list is rendered from the caller's grants, by the PDP that enforces
/// them, so discovery cannot advertise what a call would refuse.
#[tokio::test]
async fn the_tool_list_is_the_callers_grants_and_grows_with_the_token() {
    let realm = common::Realm::new();
    let token = steward(&realm);

    let (_, anonymous) = send(
        app("http://127.0.0.1:1", &realm),
        message(
            SLUG,
            None,
            json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list" }),
        ),
    )
    .await;
    assert_eq!(
        tool_names(&anonymous),
        vec![
            "describe_access",
            "describe_schema",
            "get_entity",
            "query_entities"
        ],
        "two reads, and the two tools that describe the endpoint to whoever it admits"
    );

    let (_, granted) = send(
        app("http://127.0.0.1:1", &realm),
        message(
            SLUG,
            Some(&token),
            json!({ "jsonrpc": "2.0", "id": 3, "method": "tools/list" }),
        ),
    )
    .await;
    assert_eq!(
        tool_names(&granted),
        vec![
            // `queryTemporal` is one operation and CIM 009 gives it two forms, so its holder sees
            // both tools: the GET and the POST that carries a long selector (AG-84).
            "batch_query_temporal",
            "describe_access",
            "describe_schema",
            "get_entity",
            "list_types",
            "query_entities",
            "query_temporal",
            "upsert_entity"
        ],
        "the steward writes and reads history; nobody's grant hides a tool from its holder"
    );
    assert!(
        granted["result"]["tools"][0]["inputSchema"]["type"] == "object",
        "every tool carries the schema an agent needs to call it (AG-04)"
    );
}

/// SP-20: a tool the caller may not use answers exactly what a tool nobody defined does,
/// so an MCP client cannot map the grants of a space by probing it.
#[tokio::test]
async fn an_ungranted_tool_is_indistinguishable_from_one_that_does_not_exist() {
    let realm = common::Realm::new();
    let call = |name: &str| {
        json!({ "jsonrpc": "2.0", "id": 4, "method": "tools/call", "params": {
            "name": name, "arguments": {},
        }})
    };

    let (_, ungranted) = send(
        app("http://127.0.0.1:1", &realm),
        message(SLUG, None, call("upsert_entity")),
    )
    .await;
    let (_, unknown) = send(
        app("http://127.0.0.1:1", &realm),
        message(SLUG, None, call("summon_everything")),
    )
    .await;

    assert_eq!(ungranted["error"], unknown["error"]);
    assert_eq!(ungranted["error"]["code"], -32602);
    assert_eq!(ungranted["error"]["message"], "unknown tool");
}

/// AG-05, SP-14: the space is the URL's, and an argument that would choose another one is
/// refused rather than ignored.
#[tokio::test]
async fn an_argument_that_would_aim_the_tool_at_another_space_is_refused() {
    let realm = common::Realm::new();
    for selector in ["space", "tenant", "contextSpace"] {
        let (_, answer) = send(
            app("http://127.0.0.1:1", &realm),
            message(
                SLUG,
                None,
                json!({ "jsonrpc": "2.0", "id": 5, "method": "tools/call", "params": {
                    "name": "query_entities",
                    "arguments": { "type": "AirQualityObserved", selector: "doprava" },
                }}),
            ),
        )
        .await;
        assert_eq!(answer["error"]["code"], -32602, "{selector} is refused");
        assert!(answer["error"]["message"]
            .as_str()
            .expect("a message")
            .contains(selector));
    }
}

/// EP-26, SP-16: the tool call becomes the NGSI-LD request it stands for, with the tenant
/// pinned by the gateway and the caller's own token carried to the broker.
#[tokio::test]
async fn a_tool_call_reaches_the_broker_pinned_to_the_space_and_carrying_the_callers_token() {
    let realm = common::Realm::new();
    let token = steward(&realm);
    let (broker, log) = stub_broker().await;

    let (_, answer) = send(
        app(&broker, &realm),
        message(
            SLUG,
            Some(&token),
            json!({ "jsonrpc": "2.0", "id": 6, "method": "tools/call", "params": {
                "name": "query_entities",
                "arguments": { "type": "AirQualityObserved", "limit": 5 },
            }}),
        ),
    )
    .await;
    assert_eq!(answer["result"]["isError"], false);
    // The structured half is an object naming what it carries, never the broker's bare
    // list: a client that reads it by name reads nothing from a list (T-0946).
    assert!(answer["result"]["structuredContent"]["entities"].is_array());

    let seen = log.lock().expect("the log").clone();
    assert_eq!(seen.len(), 1, "one tool call, one broker request");
    assert_eq!(seen[0].method, "GET");
    assert!(
        seen[0].uri.starts_with("/ngsi-ld/v1/entities?"),
        "got {}",
        seen[0].uri
    );
    assert!(seen[0].uri.contains("type=AirQualityObserved"));
    assert_eq!(
        seen[0].tenant.as_deref(),
        Some("ovzdusie"),
        "the space is pinned by the endpoint, never by the agent (EP-22)"
    );
    assert_eq!(
        seen[0].authorization.as_deref(),
        Some(format!("Bearer {token}").as_str()),
        "the caller's own token, not an identity the façade holds (EP-26)"
    );
}

/// An anonymous tool call is served, and nothing invents a token for it.
#[tokio::test]
async fn an_anonymous_tool_call_carries_no_token_at_all() {
    let realm = common::Realm::new();
    let (broker, log) = stub_broker().await;

    let (_, answer) = send(
        app(&broker, &realm),
        message(
            SLUG,
            None,
            json!({ "jsonrpc": "2.0", "id": 7, "method": "tools/call", "params": {
                "name": "get_entity",
                "arguments": { "id": ENTITY },
            }}),
        ),
    )
    .await;
    assert_eq!(answer["result"]["isError"], false);

    let seen = log.lock().expect("the log").clone();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].authorization, None);
    assert!(
        seen[0].uri.contains("urn%3Angsi-ld%3AAirQualityObserved"),
        "the URN survives the trip encoded, got {}",
        seen[0].uri
    );
}

/// SP-17: a refusal is a tool error an agent can read, never an empty result it would
/// report as "there is nothing there".
#[tokio::test]
async fn a_refusal_comes_back_as_a_tool_error_and_never_as_an_empty_answer() {
    let realm = common::Realm::new();
    let (broker, log) = stub_broker().await;

    let (_, answer) = send(
        app(&broker, &realm),
        message(
            SLUG,
            None,
            json!({ "jsonrpc": "2.0", "id": 8, "method": "tools/call", "params": {
                "name": "get_entity",
                "arguments": { "id": "urn:ngsi-ld:AirQualityObserved:banskabystrica.sk:doprava:st-1" },
            }}),
        ),
    )
    .await;

    assert_eq!(
        answer["result"]["isError"], true,
        "an entity of another space is refused, and the agent is told so"
    );
    assert!(answer["result"]["content"][0]["text"]
        .as_str()
        .expect("a text")
        .starts_with("400 "));
    assert!(
        log.lock().expect("the log").is_empty(),
        "a refusal never reaches the broker"
    );
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
async fn a_body_that_is_not_json_and_a_method_nobody_defined_answer_as_json_rpc_says() {
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

    let (_, unknown) = send(
        app("http://127.0.0.1:1", &realm),
        message(
            SLUG,
            None,
            json!({ "jsonrpc": "2.0", "id": 10, "method": "prompts/get" }),
        ),
    )
    .await;
    assert_eq!(unknown["error"]["code"], -32601);

    for invalid in [
        json!({ "jsonrpc": "1.0", "id": 11, "method": "ping" }),
        json!({ "id": 12, "method": "ping" }),
        json!({ "jsonrpc": "2.0", "id": 13 }),
    ] {
        let (_, answer) = send(
            app("http://127.0.0.1:1", &realm),
            message(SLUG, None, invalid.clone()),
        )
        .await;
        assert_eq!(answer["error"]["code"], -32600, "{invalid}");
        assert_eq!(answer["id"], invalid["id"], "the id is echoed");
    }
}

/// EP-47, EP-55: the two describing tools are the access and schema projections, answered
/// from the endpoint itself, so an agent can orient without a single broker round trip.
#[tokio::test]
async fn the_describing_tools_answer_from_the_endpoint_and_never_from_the_broker() {
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

    let (_, summary) = send(
        app(&broker, &realm),
        message(SLUG, None, call(12, "describe_schema", json!({}))),
    )
    .await;
    assert_eq!(
        summary["result"]["structuredContent"]["schema"]["endpoint"],
        SLUG
    );

    let (_, shacl) = send(
        app(&broker, &realm),
        message(
            SLUG,
            None,
            call(13, "describe_schema", json!({ "format": "shacl" })),
        ),
    )
    .await;
    assert_eq!(
        shacl["result"]["isError"], true,
        "a formalism the gateway cannot compile is named, never approximated"
    );

    assert!(
        log.lock().expect("the log").is_empty(),
        "neither tool asks the broker anything"
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

/// AG-21, T-0944: an argument that cannot denote an entity type is a bad request, not an
/// empty result. A caller who reads `[]` learns that the parameter was accepted.
#[tokio::test]
async fn a_type_argument_that_is_not_a_type_name_is_refused_by_name() {
    let realm = common::Realm::new();
    let (broker, log) = stub_broker().await;
    let hostile = [
        "path/../../../../etc/passwd%00trailing-null-byte-attempt",
        "../../secrets",
        "AirQualityObserved OR 1=1",
        "urn:ngsi-ld:AirQualityObserved:hel.fi:helsinki:x",
        "",
    ];
    for argument in hostile {
        let (_, answer) = send(
            app(&broker, &realm),
            message(
                SLUG,
                None,
                json!({ "jsonrpc": "2.0", "id": 31, "method": "tools/call", "params": {
                    "name": "query_entities",
                    "arguments": { "type": argument },
                }}),
            ),
        )
        .await;
        assert_eq!(
            answer["result"]["isError"], true,
            "`{argument}` was accepted as an entity type: {answer}"
        );
        let text = answer["result"]["content"][0]["text"]
            .as_str()
            .expect("a refusal an agent can read");
        assert!(
            text.contains("/type"),
            "the refusal does not name the parameter it refused: {text}"
        );
    }
    assert!(
        log.lock().expect("the log").is_empty(),
        "a refused argument still reached the broker"
    );
}

/// AG-21: an `id` that is not an NGSI-LD URN is refused the same way, and a legal type name
/// still goes through — the pattern refuses hostile values, not the catalogue.
#[tokio::test]
async fn an_id_argument_is_a_urn_and_a_plain_type_name_still_passes() {
    let realm = common::Realm::new();
    let token = steward(&realm);
    let (broker, log) = stub_broker().await;

    let (_, refused) = send(
        app(&broker, &realm),
        message(
            SLUG,
            Some(&token),
            json!({ "jsonrpc": "2.0", "id": 32, "method": "tools/call", "params": {
                "name": "get_entity",
                "arguments": { "id": "../../../etc/passwd" },
            }}),
        ),
    )
    .await;
    assert_eq!(refused["result"]["isError"], true, "{refused}");
    assert!(log.lock().expect("the log").is_empty());

    let (_, allowed) = send(
        app(&broker, &realm),
        message(
            SLUG,
            Some(&token),
            json!({ "jsonrpc": "2.0", "id": 33, "method": "tools/call", "params": {
                "name": "query_entities",
                "arguments": { "type": "AirQualityObserved", "limit": 1 },
            }}),
        ),
    )
    .await;
    assert_eq!(allowed["result"]["isError"], false, "{allowed}");
    assert_eq!(log.lock().expect("the log").len(), 1);
}

/// T-0946, AG-13: `structuredContent` is an object naming what it carries, and every tool
/// publishes the output schema that says so.
#[tokio::test]
async fn the_structured_half_of_a_result_is_an_object_the_output_schema_describes() {
    let realm = common::Realm::new();
    let token = steward(&realm);
    let (broker, _log) = stub_broker().await;

    let (_, listed) = send(
        app(&broker, &realm),
        message(
            SLUG,
            Some(&token),
            json!({ "jsonrpc": "2.0", "id": 34, "method": "tools/list" }),
        ),
    )
    .await;
    let tools = listed["result"]["tools"]
        .as_array()
        .expect("the catalogue")
        .clone();
    for tool in &tools {
        let schema = &tool["outputSchema"];
        assert_eq!(schema["type"], "object", "{}: {schema}", tool["name"]);
        let key = schema["required"][0]
            .as_str()
            .expect("the output schema names what the result carries");
        assert!(
            schema["properties"][key].is_object(),
            "{}: the required key is not a property of the schema",
            tool["name"]
        );
    }

    let (_, answer) = send(
        app(&broker, &realm),
        message(
            SLUG,
            Some(&token),
            json!({ "jsonrpc": "2.0", "id": 35, "method": "tools/call", "params": {
                "name": "query_entities",
                "arguments": { "type": "AirQualityObserved", "limit": 5 },
            }}),
        ),
    )
    .await;
    let structured = &answer["result"]["structuredContent"];
    assert!(
        structured.is_object(),
        "a bare list is read as nothing by a client that reads the structured half by name: {structured}"
    );
    assert!(structured["entities"].is_array(), "{structured}");
}

/// T-1006, AG-08: the person's answer gates every tool that changes something, not the
/// subscription alone. `upsert_entity` is annotated `destructiveHint: true`, so a model that
/// calls it without carrying an answer is asked first and nothing is written.
#[tokio::test]
async fn a_write_tool_asks_the_person_before_it_changes_anything() {
    let realm = common::Realm::new();
    let token = steward(&realm);
    let arguments = json!({ "entity": {
        "id": "urn:ngsi-ld:AirQualityObserved:banskabystrica.sk:ovzdusie:st-1",
        "type": "AirQualityObserved",
        "pm10": { "type": "Property", "value": 12.5 }
    }});

    let (_, asked) = send(
        app("http://127.0.0.1:1", &realm),
        message(
            SLUG,
            Some(&token),
            json!({ "jsonrpc": "2.0", "id": 7, "method": "tools/call", "params": {
                "name": "upsert_entity", "arguments": arguments,
            }}),
        ),
    )
    .await;

    assert_eq!(
        asked["result"]["status"],
        json!("input_required"),
        "a write runs on the person's word, not the model's: {asked}"
    );
    assert!(
        asked["result"]["structuredContent"]["elicitation"]["elicitationId"]
            .as_str()
            .is_some_and(|id| !id.is_empty()),
        "the question carries the server's own id, so the model cannot answer it: {asked}"
    );
    // The broker is unreachable in this test; reaching it at all would fail the call instead.
    assert_ne!(asked["result"]["isError"], json!(true), "{asked}");
}

/// T-0972: every argument a tool takes is bounded, and the bound is enforced by the schema
/// validator rather than only described. The broker parses whatever arrives, so an unbounded
/// filter or attribute list is a way to spend its memory through a tool the grant allows.
#[tokio::test]
async fn an_oversized_argument_is_refused_before_the_broker_sees_it() {
    let realm = common::Realm::new();
    let token = steward(&realm);
    let huge = "x".repeat(20_000);

    for (name, arguments) in [
        (
            "coordinates",
            json!({ "type": "AirQualityObserved", "coordinates": huge.clone() }),
        ),
        (
            "scopeQ",
            json!({ "type": "AirQualityObserved", "scopeQ": huge.clone() }),
        ),
        (
            "georel",
            json!({ "type": "AirQualityObserved", "georel": huge.clone() }),
        ),
        (
            "q",
            json!({ "type": "AirQualityObserved", "q": huge.clone() }),
        ),
        (
            "attrs",
            json!({
                "type": "AirQualityObserved",
                "attrs": (0..5_000).map(|n| format!("a{n}")).collect::<Vec<_>>()
            }),
        ),
    ] {
        let (_, answered) = send(
            app("http://127.0.0.1:1", &realm),
            message(
                SLUG,
                Some(&token),
                json!({ "jsonrpc": "2.0", "id": 8, "method": "tools/call", "params": {
                    "name": "query_entities", "arguments": arguments,
                }}),
            ),
        )
        .await;
        // The reason, not only the failure: an unreachable broker also answers isError, so
        // the assertion is that the schema refused it before any call was made.
        let said = format!("{answered}");
        assert!(
            said.contains("is longer than") || said.contains("has more than"),
            "{name} is bounded by the schema, not by the broker being away: {answered}"
        );
    }
}
