//! T-0335, T-0336: the Data MCP covers the CIM 009 read surface, validates what it is
//! given, and can be reached from a phone (AG-05, AG-07, AG-29…AG-32, EP-52, EP-60).

mod common;

use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use axum::Router;
use context_gateway::app::{router, Gateway};
use context_gateway::auth::accounts::ServiceAccounts;
use context_gateway::pdp::PolicyPdp;
use context_gateway::proxy::Broker;
use context_gateway::resolver::{Endpoint, Model, Space};
use jc_core::kinds::{Audience, PolicySpec, Representation};
use serde_json::{json, Value};
use std::sync::Arc;
use tower::ServiceExt;

const PUBLIC: &str = "k4y7pq2mzt6vhx3nbwrs5cjd8f";
const PRIVATE: &str = "zt4qm7ge2xdv6ksb3ncf5arw2y";
const HOST: &str = "https://city.example";
const ENTITY: &str = "urn:ngsi-ld:AirQualityObserved:banskabystrica.sk:ovzdusie:st-1";

fn policy(yaml: &str) -> PolicySpec {
    serde_norway::from_str(yaml).expect("the policy spec parses")
}

/// Everything the anonymous caller may do on the demo space: read entities, list the
/// types and the attributes, read history, and query in batch.
fn public_grant() -> PolicySpec {
    policy(
        r#"contextSpaceRef: ovzdusie
assigner: did:web:banskabystrica.sk
assignee: { kind: role, id: public }
operations:
  - queryEntity
  - retrieveEntity
  - retrieveEntityTypes
  - retrieveEntityTypeDetails
  - retrieveAttrTypes
  - queryTemporal
  - retrieveTemporal
  - queryBatch
information:
  - entities:
      - type: AirQualityObserved
    propertyNames: [pm10, pm25, location]
"#,
    )
}

fn endpoint(slug: &str, audience: Audience) -> Endpoint {
    Endpoint {
        roles: Default::default(),
        slug: slug.to_owned(),
        title: std::collections::BTreeMap::new(),
        description: std::collections::BTreeMap::new(),
        space: "ovzdusie".to_owned(),
        project: "ovzdusie".to_owned(),
        audience,
        allowed_projects: Vec::new(),
        representations: vec![Representation::NgsiLd, Representation::Mcp],
        rate_limit: None,
        file_limits: None,
        hidden_attributes: Default::default(),
        projection: None,
        view_mapping: None,
        catalog: None,
        base_path: format!("/api/endpoint/{slug}"),
        models: vec![Model {
            name: "air-quality".to_owned(),
            version: "1.0.0".to_owned(),
            major: 1,
            classes: vec!["AirQualityObserved".to_owned()],
            json_schema: None,
            context: None,
        }],
        policies: vec![public_grant()],
    }
}

/// The canonical space surface, which runs the same façade under `/cs/{space}` (SP-14).
fn space() -> Space {
    let mut record = endpoint("ovzdusie", Audience::Public);
    record.base_path = "/cs/ovzdusie".to_owned();
    Space {
        endpoint: Arc::new(record),
        title: Default::default(),
        description: Default::default(),
        is_sandbox: false,
        default_locale: None,
    }
}

fn gateway(broker: &str, realm: &common::Realm) -> Arc<Gateway> {
    Arc::new(
        Gateway::new(
            Broker::new(broker),
            Box::new(PolicyPdp),
            "banskabystrica.sk",
        )
        .serve([
            endpoint(PUBLIC, Audience::Public),
            endpoint(PRIVATE, Audience::Organization),
        ])
        .serve_spaces([space()])
        .authenticate(
            Arc::new(realm.verifier()),
            ServiceAccounts::new(),
            Some(HOST.to_owned()),
        ),
    )
}

fn app(broker: &str, realm: &common::Realm) -> Router {
    router(gateway(broker, realm))
}

fn message(path: &str, token: Option<&str>, body: Value) -> Request<Body> {
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

async fn send(app: Router, request: Request<Body>) -> (StatusCode, Value, axum::http::HeaderMap) {
    let response = app.oneshot(request).await.expect("the gateway answers");
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .expect("a body");
    let payload = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, payload, headers)
}

/// AG-29: every CIM 009 read the endpoint serves has a tool, and the list is the grant.
#[tokio::test]
async fn the_catalogue_covers_the_read_surface_and_annotates_every_tool() {
    let realm = common::Realm::new();
    let (_, answer, _) = send(
        app("http://127.0.0.1:1", &realm),
        message(
            &format!("/api/endpoint/{PUBLIC}/mcp"),
            None,
            json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list" }),
        ),
    )
    .await;

    let tools = answer["result"]["tools"].as_array().expect("a tool list");
    let mut names: Vec<&str> = tools
        .iter()
        .map(|tool| tool["name"].as_str().unwrap_or_default())
        .collect();
    names.sort_unstable();
    assert_eq!(
        names,
        vec![
            "batch_query",
            // The POST form of a query is the same operation as its GET, so the same grant
            // shows both (AG-84). `get_type` and `get_attribute` are not here: `GET /types/{type}`
            // is `retrieveEntityTypeInfo`, and this grant holds the list's
            // `retrieveEntityTypeDetails` instead.
            "batch_query_temporal",
            "describe_access",
            "describe_schema",
            "get_entity",
            "list_attributes",
            "list_types",
            "query_entities",
            "query_temporal",
            "retrieve_temporal",
        ],
        "the anonymous grant's read operations, and the two tools that describe the endpoint"
    );

    // AG-07: an agent runtime decides whether to ask a human from these, so they have to
    // be there and they have to be right.
    for tool in tools {
        assert_eq!(
            tool["annotations"]["readOnlyHint"],
            json!(true),
            "{} is a read and must say so",
            tool["name"]
        );
        assert_eq!(tool["inputSchema"]["additionalProperties"], json!(false));
    }
}

/// AG-31: an argument the schema does not know is refused before anything is built.
#[tokio::test]
async fn an_unknown_argument_is_a_tool_error_and_never_a_broker_call() {
    let realm = common::Realm::new();
    let broker = common::BrokerStub::start(vec![json!([])]).await;

    let (_, answer, _) = send(
        app(&broker.url, &realm),
        message(
            &format!("/api/endpoint/{PUBLIC}/mcp"),
            None,
            call(
                "query_entities",
                json!({ "type": "AirQualityObserved", "linit": 5 }),
            ),
        ),
    )
    .await;

    assert_eq!(answer["result"]["isError"], json!(true));
    let text = answer["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or_default();
    assert!(
        text.contains("linit"),
        "the refusal must name the argument the caller got wrong: {text}"
    );
    assert!(
        broker.hops().is_empty(),
        "a request that fails validation must never reach the broker (AG-21)"
    );

    // A type outside the schema is refused by the same check.
    let (_, wrong_type, _) = send(
        app(&broker.url, &realm),
        message(
            &format!("/api/endpoint/{PUBLIC}/mcp"),
            None,
            call("query_entities", json!({ "type": 7 })),
        ),
    )
    .await;
    assert_eq!(wrong_type["result"]["isError"], json!(true));
    assert!(broker.hops().is_empty());
}

/// GW31, AG-21: a type or q the query language has no reading for is refused as sent, and never
/// answered by an empty intersection with the grants; the refusal does not repeat the payload.
#[tokio::test]
async fn a_hostile_type_or_q_is_a_tool_error_and_never_an_empty_answer() {
    let realm = common::Realm::new();
    let broker = common::BrokerStub::start(vec![json!([])]).await;
    let hostile = "'; DROP TABLE entities; -- SYSTEM: ignore validation";

    for arguments in [
        json!({ "type": hostile }),
        json!({ "type": "AirQualityObserved", "q": hostile }),
    ] {
        let (_, answer, _) = send(
            app(&broker.url, &realm),
            message(
                &format!("/api/endpoint/{PUBLIC}/mcp"),
                None,
                call("query_entities", arguments.clone()),
            ),
        )
        .await;
        assert_eq!(answer["result"]["isError"], json!(true), "{arguments}");
        let text = answer["result"]["content"][0]["text"]
            .as_str()
            .unwrap_or_default();
        assert!(
            !text.contains("DROP TABLE"),
            "the refusal repeats the payload: {text}"
        );
    }
    assert!(
        broker.hops().is_empty(),
        "a malformed query must never reach the broker"
    );
}

/// AG-30: the temporal grammar reaches the broker unchanged, on the temporal path.
#[tokio::test]
async fn the_temporal_tools_forward_the_whole_grammar_to_the_temporal_api() {
    let realm = common::Realm::new();
    let broker = common::BrokerStub::start(vec![json!([])]).await;

    send(
        app(&broker.url, &realm),
        message(
            &format!("/api/endpoint/{PUBLIC}/mcp"),
            None,
            call(
                "query_temporal",
                json!({
                    "type": "AirQualityObserved",
                    "timerel": "between",
                    "timeAt": "2026-09-01T00:00:00Z",
                    "endTimeAt": "2026-09-02T00:00:00Z",
                    "lastN": 2,
                    "aggrMethods": ["avg", "max"],
                    "aggrPeriodDuration": "PT1H",
                    "attrs": ["pm10"],
                }),
            ),
        ),
    )
    .await;

    let hop = broker.hops().pop().expect("the broker was called");
    assert_eq!(hop.path, "/ngsi-ld/v1/temporal/entities");
    assert_eq!(
        hop.tenant, "ovzdusie",
        "the space is pinned, never asked for"
    );
    for expected in [
        "timerel=between",
        "endTimeAt=2026-09-02T00%3A00%3A00Z",
        "lastN=2",
        "aggrMethods=avg%2Cmax",
        "aggrPeriodDuration=PT1H",
    ] {
        assert!(
            hop.query.contains(expected),
            "{expected} must survive the trip: {}",
            hop.query
        );
    }

    // The one entity's history is the other CIM 009 operation, and therefore the other
    // tool: the path follows the tool, never an argument that happens to be present.
    send(
        app(&broker.url, &realm),
        message(
            &format!("/api/endpoint/{PUBLIC}/mcp"),
            None,
            call(
                "retrieve_temporal",
                json!({ "id": ENTITY, "timerel": "after", "timeAt": "2026-09-01T00:00:00Z" }),
            ),
        ),
    )
    .await;
    let hop = broker.hops().pop().expect("the broker was called again");
    assert!(
        hop.path.starts_with("/ngsi-ld/v1/temporal/entities/urn"),
        "one entity's history is a path, not a filter: {}",
        hop.path
    );
}

/// AG-29: the type and attribute listings, and the batch query, are the operations they
/// say they are.
#[tokio::test]
async fn the_listing_and_batch_tools_make_the_requests_they_stand_for() {
    let realm = common::Realm::new();
    let broker = common::BrokerStub::start(vec![json!([])]).await;

    send(
        app(&broker.url, &realm),
        message(
            &format!("/api/endpoint/{PUBLIC}/mcp"),
            None,
            call("list_types", json!({ "details": true })),
        ),
    )
    .await;
    let hop = broker.hops().pop().expect("a hop");
    assert_eq!(hop.path, "/ngsi-ld/v1/types");
    assert!(
        hop.query.contains("details=true"),
        "the details flag is the difference between the two type operations: {}",
        hop.query
    );

    send(
        app(&broker.url, &realm),
        message(
            &format!("/api/endpoint/{PUBLIC}/mcp"),
            None,
            call("list_attributes", json!({})),
        ),
    )
    .await;
    assert_eq!(
        broker.hops().pop().expect("a hop").path,
        "/ngsi-ld/v1/attributes"
    );

    send(
        app(&broker.url, &realm),
        message(
            &format!("/api/endpoint/{PUBLIC}/mcp"),
            None,
            call("batch_query", json!({ "ids": [ENTITY], "attrs": ["pm10"] })),
        ),
    )
    .await;
    let hop = broker.hops().pop().expect("a hop");
    assert_eq!(hop.path, "/ngsi-ld/v1/entityOperations/query");
}

/// EP-52, EP-60: the resources are the types, the grant document and the schema, and one
/// naming another space is not a way to read it (AG-05).
#[tokio::test]
async fn resources_list_what_the_caller_may_read_and_never_another_space() {
    let realm = common::Realm::new();
    let broker = common::BrokerStub::start(vec![json!([{ "id": ENTITY }])]).await;

    let (_, listed, _) = send(
        app(&broker.url, &realm),
        message(
            &format!("/api/endpoint/{PUBLIC}/mcp"),
            None,
            json!({ "jsonrpc": "2.0", "id": 1, "method": "resources/list" }),
        ),
    )
    .await;
    let uris: Vec<&str> = listed["result"]["resources"]
        .as_array()
        .expect("a resource list")
        .iter()
        .map(|resource| resource["uri"].as_str().unwrap_or_default())
        .collect();
    assert!(uris.contains(&"ngsi-ld://ovzdusie/types/AirQualityObserved"));
    assert!(uris.contains(&format!("access://{PUBLIC}").as_str()));
    assert!(uris.iter().any(|uri| uri.starts_with("schema://")));

    // The grant document is answered from the endpoint itself, with no broker hop.
    let before = broker.hops().len();
    let (_, grants, _) = send(
        app(&broker.url, &realm),
        message(
            &format!("/api/endpoint/{PUBLIC}/mcp"),
            None,
            json!({
                "jsonrpc": "2.0",
                "id": 2,
                "method": "resources/read",
                "params": { "uri": format!("access://{PUBLIC}") },
            }),
        ),
    )
    .await;
    assert!(grants["result"]["contents"][0]["text"]
        .as_str()
        .unwrap_or_default()
        .contains("AirQualityObserved"));
    assert_eq!(broker.hops().len(), before, "the access document is local");

    // A URI for a space this endpoint does not serve is an unknown resource, which is
    // what a URI nobody defined is too (SP-20, R20).
    let (_, foreign, _) = send(
        app(&broker.url, &realm),
        message(
            &format!("/api/endpoint/{PUBLIC}/mcp"),
            None,
            json!({
                "jsonrpc": "2.0",
                "id": 3,
                "method": "resources/read",
                "params": { "uri": "ngsi-ld://doprava/types/Vehicle" },
            }),
        ),
    )
    .await;
    assert_eq!(foreign["error"]["code"], json!(-32602));
    assert_eq!(foreign["error"]["message"], json!("unknown resource"));
}

/// AG-08: a subscription outlives the conversation, so the operator has to have said yes.
#[tokio::test]
async fn creating_a_subscription_needs_the_operators_confirmation_first() {
    let realm = common::Realm::new();
    let broker = common::BrokerStub::start(vec![json!({})]).await;
    // The public grant does not include it, so it is not even advertised.
    let (_, listed, _) = send(
        app(&broker.url, &realm),
        message(
            &format!("/api/endpoint/{PUBLIC}/mcp"),
            None,
            json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list" }),
        ),
    )
    .await;
    let names: Vec<&str> = listed["result"]["tools"]
        .as_array()
        .expect("a tool list")
        .iter()
        .map(|tool| tool["name"].as_str().unwrap_or_default())
        .collect();
    assert!(!names.contains(&"create_subscription"));

    // Calling it by name answers as an undefined tool does, and reaches nothing.
    let (_, refused, _) = send(
        app(&broker.url, &realm),
        message(
            &format!("/api/endpoint/{PUBLIC}/mcp"),
            None,
            call("create_subscription", json!({ "subscription": {} })),
        ),
    )
    .await;
    assert_eq!(refused["error"]["message"], json!("unknown tool"));
    assert!(broker.hops().is_empty());
}

/// AG-32: a phone with no token is told where its authorization server is.
#[tokio::test]
async fn an_unauthenticated_call_names_the_resource_metadata_and_a_public_one_does_not() {
    let realm = common::Realm::new();
    let broker = common::BrokerStub::start(vec![json!([])]).await;

    let (status, _, headers) = send(
        app(&broker.url, &realm),
        message(
            &format!("/api/endpoint/{PRIVATE}/mcp"),
            None,
            json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list" }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let challenge = headers
        .get("www-authenticate")
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default();
    assert_eq!(
        challenge,
        format!(
            "Bearer resource_metadata=\"{HOST}/api/endpoint/{PRIVATE}/.well-known/oauth-protected-resource\""
        )
    );

    // An unknown slug answers exactly the same, so the challenge discloses nothing (R20).
    let (unknown_status, _, unknown_headers) = send(
        app(&broker.url, &realm),
        message(
            "/api/endpoint/aaaaaaaaaaaaaaaaaaaaaaaaaa/mcp",
            None,
            json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list" }),
        ),
    )
    .await;
    assert_eq!(unknown_status, StatusCode::UNAUTHORIZED);
    assert!(
        unknown_headers.get("www-authenticate").is_some(),
        "an endpoint that needs a login and one that does not exist answer alike"
    );

    // The public endpoint needs no token at all: a phone connects with one paste.
    let (public_status, answer, _) = send(
        app(&broker.url, &realm),
        message(
            &format!("/api/endpoint/{PUBLIC}/mcp"),
            None,
            json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list" }),
        ),
    )
    .await;
    assert_eq!(public_status, StatusCode::OK);
    assert!(answer["result"]["tools"]
        .as_array()
        .is_some_and(|tools| !tools.is_empty()));
}

/// AG-32: the protected-resource document is RFC 9728 shaped and names the realm.
#[tokio::test]
async fn the_protected_resource_metadata_names_the_realm_for_every_slug() {
    let realm = common::Realm::new();
    let request = Request::builder()
        .method(Method::GET)
        .uri(format!(
            "/api/endpoint/{PRIVATE}/.well-known/oauth-protected-resource"
        ))
        .body(Body::empty())
        .expect("a request");
    let (status, document, _) = send(app("http://127.0.0.1:1", &realm), request).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        document["resource"],
        json!(format!("{HOST}/api/endpoint/{PRIVATE}/mcp"))
    );
    assert!(document["authorization_servers"][0].as_str().is_some());
    assert_eq!(document["bearer_methods_supported"], json!(["header"]));
}

/// SP-14: the space's canonical surface runs the same façade as the endpoint's.
#[tokio::test]
async fn the_space_surface_serves_the_same_tools_as_the_endpoint() {
    let realm = common::Realm::new();
    let broker = common::BrokerStub::start(vec![json!([])]).await;

    let (status, answer, _) = send(
        app(&broker.url, &realm),
        message(
            "/cs/ovzdusie/mcp",
            None,
            json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list" }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let names: Vec<&str> = answer["result"]["tools"]
        .as_array()
        .expect("a tool list")
        .iter()
        .map(|tool| tool["name"].as_str().unwrap_or_default())
        .collect();
    assert!(names.contains(&"query_entities"));

    // And a tool call from the space surface reaches the broker pinned to that space.
    send(
        app(&broker.url, &realm),
        message(
            "/cs/ovzdusie/mcp",
            None,
            call("query_entities", json!({ "type": "AirQualityObserved" })),
        ),
    )
    .await;
    let hop = broker.hops().pop().expect("the broker was called");
    assert_eq!(hop.path, "/ngsi-ld/v1/entities");
    assert_eq!(hop.tenant, "ovzdusie");
}

/// EP-46, EP-52, AG-29: the agent reads every formalism the REST schema surface renders, and
/// `entityType` narrows the answer (T-0845).
#[tokio::test]
async fn describe_schema_renders_every_formalism_the_rest_surface_serves() {
    let realm = common::Realm::new();
    for (format, must_contain) in [
        ("shacl", "sh:NodeShape"),
        ("owl", "owl:Class"),
        ("rdf", "rdfs:Class"),
        ("linkml", "classes:"),
        ("markdown", "AirQualityObserved"),
    ] {
        let (_, answer, _) = send(
            app("http://127.0.0.1:1", &realm),
            message(
                &format!("/api/endpoint/{PUBLIC}/mcp"),
                None,
                call("describe_schema", json!({ "format": format })),
            ),
        )
        .await;
        let result = &answer["result"];
        assert_eq!(result["isError"], json!(false), "{format}: {answer}");
        assert_eq!(
            result["structuredContent"]["schema"]["format"],
            json!(format)
        );
        let document = result["structuredContent"]["schema"]["document"]
            .as_str()
            .unwrap_or_default();
        assert!(
            document.contains(must_contain),
            "{format} renders the model: {document}"
        );
    }
}

/// The two JSON formalisms and the summary keep answering as they did.
#[tokio::test]
async fn describe_schema_still_answers_the_summary_and_the_json_formalisms() {
    let realm = common::Realm::new();
    for (format, key) in [("summary", "models"), ("json-schema", "$defs")] {
        let (_, answer, _) = send(
            app("http://127.0.0.1:1", &realm),
            message(
                &format!("/api/endpoint/{PUBLIC}/mcp"),
                None,
                call("describe_schema", json!({ "format": format })),
            ),
        )
        .await;
        assert_eq!(answer["result"]["isError"], json!(false), "{answer}");
        assert!(
            answer["result"]["structuredContent"]["schema"]
                .get(key)
                .is_some(),
            "{format} answers the document itself: {answer}"
        );
    }
}

/// An `entityType` is accepted, and one the caller may not read is refused the way an unknown
/// name is: the schema never says which of the two it was (EP-47, SP-15).
#[tokio::test]
async fn describe_schema_narrows_to_one_entity_type_and_refuses_the_rest() {
    let realm = common::Realm::new();
    let (_, answer, _) = send(
        app("http://127.0.0.1:1", &realm),
        message(
            &format!("/api/endpoint/{PUBLIC}/mcp"),
            None,
            call(
                "describe_schema",
                json!({ "format": "json-schema", "entityType": "AirQualityObserved" }),
            ),
        ),
    )
    .await;
    assert_eq!(answer["result"]["isError"], json!(false), "{answer}");
    let defs = answer["result"]["structuredContent"]["schema"]["$defs"]
        .as_object()
        .expect("the definitions");
    assert!(defs.contains_key("AirQualityObserved"), "{answer}");

    let (_, refused, _) = send(
        app("http://127.0.0.1:1", &realm),
        message(
            &format!("/api/endpoint/{PUBLIC}/mcp"),
            None,
            call("describe_schema", json!({ "entityType": "SecretReading" })),
        ),
    )
    .await;
    assert_eq!(refused["result"]["isError"], json!(true), "{refused}");
    let text = refused["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or_default();
    assert!(text.contains("SecretReading"), "{refused}");
}

/// A format nobody renders is refused by the tool's own schema, before anything is built
/// (AG-31), and the refusal names the formalisms that are served.
#[tokio::test]
async fn describe_schema_refuses_a_formalism_nobody_renders() {
    let realm = common::Realm::new();
    let (_, answer, _) = send(
        app("http://127.0.0.1:1", &realm),
        message(
            &format!("/api/endpoint/{PUBLIC}/mcp"),
            None,
            call("describe_schema", json!({ "format": "protobuf" })),
        ),
    )
    .await;
    let said = serde_json::to_string(&answer).unwrap_or_default();
    assert_eq!(answer["result"]["isError"], json!(true), "{said}");
    assert!(
        said.contains("format") && said.contains("json-schema"),
        "the refusal names the parameter and the formalisms it does render: {said}"
    );
    assert!(
        !said.contains("protobuf"),
        "the refusal repeats what the caller sent: {said}"
    );
    assert_eq!(
        answer["result"]["structuredContent"]["schema"]["document"],
        Value::Null
    );
}

/// A grant that may create a subscription, held by a role no anonymous caller has.
fn steward_grant() -> PolicySpec {
    policy(
        r#"contextSpaceRef: ovzdusie
assigner: did:web:banskabystrica.sk
assignee: { kind: role, id: steward }
operations:
  - queryEntity
  - createSubscription
information:
  - entities:
      - type: AirQualityObserved
"#,
    )
}

/// A steward's token for the private endpoint.
fn steward(realm: &common::Realm) -> String {
    realm.mint(&json!({
        "iss": common::ISSUER,
        "sub": "0f5a",
        "aud": PRIVATE,
        "preferred_username": "jana",
        "realm_access": { "roles": ["steward"] },
        "exp": common::in_seconds(300),
        "iat": common::in_seconds(-10),
    }))
}

/// The same gateway across several calls, which is what a two-step confirmation needs: the
/// server minted the question and the same server reads the answer.
fn app_with_steward(broker: &str, realm: &common::Realm) -> Router {
    let mut private = endpoint(PRIVATE, Audience::Organization);
    private.policies = vec![public_grant(), steward_grant()];
    router(Arc::new(
        Gateway::new(
            Broker::new(broker),
            Box::new(PolicyPdp),
            "banskabystrica.sk",
        )
        .serve([endpoint(PUBLIC, Audience::Public), private])
        .serve_spaces([space()])
        .authenticate(
            Arc::new(realm.verifier()),
            ServiceAccounts::new(),
            Some(HOST.to_owned()),
        ),
    ))
}

/// AG-08, T-0849: a subscription outlives the conversation that made it, so the person
/// decides. The first call answers an elicitation and creates nothing; only the same call
/// carrying the server's own id reaches the broker.
#[tokio::test]
async fn a_subscription_is_created_only_after_the_person_answered_the_servers_question() {
    let realm = common::Realm::new();
    let broker = common::BrokerStub::start(vec![json!({})]).await;
    let app = app_with_steward(&broker.url, &realm);
    let token = steward(&realm);
    let arguments = json!({ "subscription": {
        "type": "Subscription",
        "entities": [{ "type": "AirQualityObserved" }],
        "notification": { "endpoint": { "uri": "https://example.org/hook" } }
    }});
    let path = format!("/api/endpoint/{PRIVATE}/mcp");

    // 1. The call the model makes alone: a question, and nothing created.
    let (_, asked, _) = send(
        app.clone(),
        message(
            &path,
            Some(&token),
            call("create_subscription", arguments.clone()),
        ),
    )
    .await;
    assert_eq!(
        asked["result"]["status"],
        json!("input_required"),
        "{asked}"
    );
    let elicitation = &asked["result"]["structuredContent"]["elicitation"];
    let elicitation_id = elicitation["elicitationId"]
        .as_str()
        .unwrap_or_else(|| panic!("the server minted no question: {asked}"))
        .to_owned();
    assert_eq!(elicitation["mode"], json!("form"), "{asked}");
    assert!(
        elicitation["schema"]["properties"]["action"].is_object(),
        "{asked}"
    );
    assert!(
        broker.hops().is_empty(),
        "the subscription was created before anybody answered"
    );

    // 2. An id the server never minted is not an answer, and still nothing is created.
    let (_, forged, _) = send(
        app.clone(),
        message(
            &path,
            Some(&token),
            json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {
                "name": "create_subscription", "arguments": arguments.clone(),
                "elicitation": { "elicitationId": "eli-0000000000000000", "action": "accept" }
            }}),
        ),
    )
    .await;
    assert_eq!(forged["result"]["isError"], json!(true), "{forged}");
    assert!(broker.hops().is_empty(), "a forged answer created it");

    // 3. The person's answer, carried by the client: the broker is asked exactly once.
    let (_, created, _) = send(
        app.clone(),
        message(
            &path,
            Some(&token),
            json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {
                "name": "create_subscription", "arguments": arguments.clone(),
                "elicitation": { "elicitationId": elicitation_id, "action": "accept" }
            }}),
        ),
    )
    .await;
    assert_eq!(created["result"]["isError"], json!(false), "{created}");
    assert_eq!(broker.hops().len(), 1, "one answer, one subscription");

    // 4. The same id is spent: it cannot create a second subscription.
    let (_, again, _) = send(
        app.clone(),
        message(
            &path,
            Some(&token),
            json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {
                "name": "create_subscription", "arguments": arguments,
                "elicitation": { "elicitationId": elicitation_id, "action": "accept" }
            }}),
        ),
    )
    .await;
    assert_eq!(again["result"]["isError"], json!(true), "{again}");
    assert_eq!(
        broker.hops().len(),
        1,
        "a spent answer created a second one"
    );
}

/// The person may say no, and a declined call creates nothing and says why.
#[tokio::test]
async fn a_declined_subscription_creates_nothing() {
    let realm = common::Realm::new();
    let broker = common::BrokerStub::start(vec![json!({})]).await;
    let app = app_with_steward(&broker.url, &realm);
    let token = steward(&realm);
    let arguments = json!({ "subscription": { "type": "Subscription" } });
    let path = format!("/api/endpoint/{PRIVATE}/mcp");

    let (_, asked, _) = send(
        app.clone(),
        message(
            &path,
            Some(&token),
            call("create_subscription", arguments.clone()),
        ),
    )
    .await;
    let elicitation_id = asked["result"]["structuredContent"]["elicitation"]["elicitationId"]
        .as_str()
        .unwrap_or_else(|| panic!("{asked}"))
        .to_owned();

    let (_, declined, _) = send(
        app.clone(),
        message(
            &path,
            Some(&token),
            json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {
                "name": "create_subscription", "arguments": arguments,
                "elicitation": { "elicitationId": elicitation_id, "action": "decline" }
            }}),
        ),
    )
    .await;
    assert_eq!(declined["result"]["isError"], json!(true), "{declined}");
    assert!(
        declined["result"]["content"][0]["text"]
            .as_str()
            .unwrap_or_default()
            .contains("declined"),
        "{declined}"
    );
    assert!(broker.hops().is_empty());
}

/// An answer to a question asked about other arguments is not an answer to this call: the
/// person saw a different subscription.
#[tokio::test]
async fn an_answer_does_not_carry_over_to_other_arguments() {
    let realm = common::Realm::new();
    let broker = common::BrokerStub::start(vec![json!({})]).await;
    let app = app_with_steward(&broker.url, &realm);
    let token = steward(&realm);
    let path = format!("/api/endpoint/{PRIVATE}/mcp");

    let (_, asked, _) = send(
        app.clone(),
        message(
            &path,
            Some(&token),
            call(
                "create_subscription",
                json!({ "subscription": { "entities": [{ "type": "AirQualityObserved" }] } }),
            ),
        ),
    )
    .await;
    let elicitation_id = asked["result"]["structuredContent"]["elicitation"]["elicitationId"]
        .as_str()
        .unwrap_or_else(|| panic!("{asked}"))
        .to_owned();

    let (_, swapped, _) = send(
        app.clone(),
        message(
            &path,
            Some(&token),
            json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {
                "name": "create_subscription",
                "arguments": { "subscription": { "entities": [{ "type": "Vehicle" }] } },
                "elicitation": { "elicitationId": elicitation_id, "action": "accept" }
            }}),
        ),
    )
    .await;
    assert_eq!(swapped["result"]["isError"], json!(true), "{swapped}");
    assert!(broker.hops().is_empty(), "the swapped call was carried out");
}

/// T-1039, R20: `describe_schema` narrowed to one entity type answers for a type the caller's
/// grant covers, and refuses one it does not without saying whether that type exists here — the
/// refusal reads "not an entity type this endpoint describes for you", the same answer an
/// ungranted type and an invented one both get.
#[tokio::test]
async fn describe_schema_narrows_to_a_type_the_grant_covers_and_tells_no_tales_about_the_rest() {
    let realm = common::Realm::new();

    let (_, described, _) = send(
        app("http://127.0.0.1:1", &realm),
        message(
            &format!("/api/endpoint/{PUBLIC}/mcp"),
            None,
            call(
                "describe_schema",
                json!({ "entityType": "AirQualityObserved" }),
            ),
        ),
    )
    .await;
    assert_ne!(
        described["result"]["isError"],
        json!(true),
        "a type the grant covers is described: {described}"
    );

    for unknown in ["Secret", "NoSuchTypeAnywhere"] {
        let (_, refused, _) = send(
            app("http://127.0.0.1:1", &realm),
            message(
                &format!("/api/endpoint/{PUBLIC}/mcp"),
                None,
                call("describe_schema", json!({ "entityType": unknown })),
            ),
        )
        .await;
        let said = format!("{refused}");
        assert!(
            said.contains("describes for you"),
            "{unknown} is refused without saying whether it exists: {refused}"
        );
    }
}

/// AG-84: every query parameter the REST read surface forwards is an argument of the tool that maps
/// to the same operation, under the same name — so this case sends the whole table at once and reads
/// the broker hop it became. The one deliberate difference is the shape: a JSON list here, a comma
/// string on the wire, and `cursor` is the REST `offset`.
#[tokio::test]
async fn every_read_parameter_of_the_table_reaches_the_broker_under_its_rest_name() {
    let realm = common::Realm::new();
    let broker = common::BrokerStub::start(vec![json!([])]).await;

    send(
        app(&broker.url, &realm),
        message(
            &format!("/api/endpoint/{PUBLIC}/mcp"),
            None,
            call(
                "query_entities",
                json!({
                    "type": ["AirQualityObserved", "WeatherObserved"],
                    "id": [ENTITY],
                    "idPattern": "^urn:ngsi-ld:AirQualityObserved:.*$",
                    "attrs": ["pm10", "pm25"],
                    "pick": ["id", "type", "pm10"],
                    "omit": ["scope"],
                    "q": "pm25>35",
                    "csf": "name==\"regional\"",
                    "georel": "near;maxDistance==2000",
                    "geometry": "Point",
                    "coordinates": "[19.15,48.73]",
                    "scopeQ": "/geo/SK/BB",
                    "lang": "sk",
                    "datasetId": ["urn:ngsi-ld:Dataset:one"],
                    "join": "inline",
                    "joinLevel": 2,
                    "containedBy": [ENTITY],
                    "local": true,
                    "count": true,
                    "limit": 25,
                    "cursor": 50,
                    "format": "concise",
                    "options": ["sysAttrs"],
                }),
            ),
        ),
    )
    .await;

    let hop = broker.hops().pop().expect("a hop");
    assert_eq!(hop.path, "/ngsi-ld/v1/entities");
    // Every name the REST surface knows, spelled as the REST surface spells it, and carrying the
    // value that was sent: these are the arguments the gateway forwards untouched.
    for expected in [
        "idPattern=%5Eurn%3Angsi-ld%3AAirQualityObserved%3A.%2A%24",
        "pick=id%2Ctype%2Cpm10",
        "omit=scope",
        "csf=name%3D%3D%22regional%22",
        "scopeQ=%2Fgeo%2FSK%2FBB",
        "lang=sk",
        "datasetId=urn%3Angsi-ld%3ADataset%3Aone",
        "join=inline",
        "joinLevel=2",
        "containedBy=urn%3Angsi-ld",
        "local=true",
        "count=true",
        "limit=25",
        // The one rename: `cursor` is the REST `offset`.
        "offset=50",
        "format=concise",
        "options=sysAttrs",
    ] {
        assert!(
            hop.query.contains(expected),
            "{expected} is not in the query the tool built: {}",
            hop.query
        );
    }
    assert!(
        !hop.query.contains("cursor="),
        "the MCP name for the offset reached the broker: {}",
        hop.query
    );

    // AG-85: `type`, `attrs`, `q` and the geo query are the four dimensions a grant narrows, so
    // what reaches the broker is the grant's answer to what was asked, never the argument as
    // written. The grant covers one type of the two asked for, so the other is simply not there —
    // an argument cannot widen a read by naming something the token does not hold.
    assert!(
        hop.query.contains("type=AirQualityObserved"),
        "the granted type is not selected on: {}",
        hop.query
    );
    assert!(
        !hop.query.contains("WeatherObserved"),
        "an ungranted type reached the broker: {}",
        hop.query
    );
    for expected in [
        "attrs=pm10%2Cpm25",
        "q=%28pm25%3E35%29",
        "georel=near%3BmaxDistance%3D%3D2000",
        "geometry=Point",
        "coordinates=%5B19.15%2C48.73%5D",
    ] {
        assert!(
            hop.query.contains(expected),
            "{expected} is not in the query the tool built: {}",
            hop.query
        );
    }
}

/// AG-84, AG-85: the POST form of a query is a way to send a long selector, not a narrower one. The
/// selectors travel in the body — `id` and `type` as the `entities` array CIM 009 asks for — and the
/// paging stays in the query string, which is where a REST client puts it too.
#[tokio::test]
async fn the_batch_query_carries_the_same_selectors_in_the_body_and_pages_in_the_query() {
    let realm = common::Realm::new();
    let broker = common::BrokerStub::start(vec![json!([])]).await;

    send(
        app(&broker.url, &realm),
        message(
            &format!("/api/endpoint/{PUBLIC}/mcp"),
            None,
            call(
                "batch_query",
                json!({
                    "id": [ENTITY],
                    "type": "AirQualityObserved",
                    "idPattern": "^urn:ngsi-ld:.*$",
                    "q": "pm25>35",
                    "attrs": ["pm10"],
                    "limit": 10,
                    "cursor": 20,
                    "count": true,
                }),
            ),
        ),
    )
    .await;

    let hop = broker.hops().pop().expect("a hop");
    assert_eq!(hop.path, "/ngsi-ld/v1/entityOperations/query");
    let body: Value = serde_json::from_slice(&hop.body).expect("a query body");
    let entities = body["entities"].as_array().expect("an entities array");
    assert!(
        entities.iter().any(|entity| entity["id"] == json!(ENTITY)),
        "the id list is not in the body: {body}"
    );
    assert!(
        entities
            .iter()
            .any(|entity| entity["type"] == json!("AirQualityObserved")),
        "the type selector is not in the body: {body}"
    );
    assert!(
        entities
            .iter()
            .any(|entity| entity["idPattern"].is_string()),
        "the pattern does not ride with a selector: {body}"
    );
    assert_eq!(body["q"], json!("pm25>35"), "{body}");
    assert_eq!(body["attrs"], json!(["pm10"]), "{body}");
    // Paging is the query string's, in both surfaces.
    assert!(body.get("limit").is_none(), "paging is in the body: {body}");
    for expected in ["limit=10", "offset=20", "count=true"] {
        assert!(
            hop.query.contains(expected),
            "{expected} is not in the query: {}",
            hop.query
        );
    }
}

/// AG-84: the reads the catalogue was missing each make the CIM 009 request they stand for, and the
/// POST form of the temporal query carries its grammar in the body.
///
/// The two registry item reads need their own grant: `GET /types/{type}` is
/// `retrieveEntityTypeInfo`, and a grant holding the list's `retrieveEntityTypeDetails` does not
/// open the item — which is why the anonymous catalogue above does not show them.
fn app_with_registry_reads(broker: &str, realm: &common::Realm) -> Router {
    let mut record = endpoint(PUBLIC, Audience::Public);
    record.policies = vec![policy(
        r#"contextSpaceRef: ovzdusie
assigner: did:web:banskabystrica.sk
assignee: { kind: role, id: public }
operations:
  - retrieveEntityTypeInfo
  - retrieveAttrTypeInfo
  - queryTemporal
information:
  - entities:
      - type: AirQualityObserved
    propertyNames: [pm10, pm25, location]
"#,
    )];
    router(Arc::new(
        Gateway::new(
            Broker::new(broker),
            Box::new(PolicyPdp),
            "banskabystrica.sk",
        )
        .serve([record])
        .authenticate(
            Arc::new(realm.verifier()),
            ServiceAccounts::new(),
            Some(HOST.to_owned()),
        ),
    ))
}

#[tokio::test]
async fn the_registry_reads_and_the_temporal_batch_make_the_requests_they_stand_for() {
    let realm = common::Realm::new();
    let broker = common::BrokerStub::start(vec![json!([])]).await;

    for (tool, arguments, path) in [
        (
            "get_type",
            json!({ "type": "AirQualityObserved" }),
            "/ngsi-ld/v1/types/AirQualityObserved",
        ),
        (
            "get_attribute",
            json!({ "attrId": "pm10" }),
            "/ngsi-ld/v1/attributes/pm10",
        ),
    ] {
        send(
            app_with_registry_reads(&broker.url, &realm),
            message(
                &format!("/api/endpoint/{PUBLIC}/mcp"),
                None,
                call(tool, arguments),
            ),
        )
        .await;
        let hops = broker.hops();
        assert_eq!(
            hops.last().map(|hop| hop.path.as_str()),
            Some(path),
            "{tool} asked for something else"
        );
    }

    send(
        app_with_registry_reads(&broker.url, &realm),
        message(
            &format!("/api/endpoint/{PUBLIC}/mcp"),
            None,
            call(
                "batch_query_temporal",
                json!({
                    "type": "AirQualityObserved",
                    "timerel": "between",
                    "timeAt": "2026-09-01T00:00:00Z",
                    "endTimeAt": "2026-09-02T00:00:00Z",
                    "aggrMethods": ["avg"],
                    "aggrPeriodDuration": "PT1H",
                    "limit": 5,
                }),
            ),
        ),
    )
    .await;
    let hop = broker.hops().pop().expect("a hop");
    assert_eq!(hop.path, "/ngsi-ld/v1/temporal/entityOperations/query");
    let body: Value = serde_json::from_slice(&hop.body).expect("a query body");
    assert_eq!(body["timerel"], json!("between"), "{body}");
    assert_eq!(body["aggrMethods"], json!(["avg"]), "{body}");
    assert!(hop.query.contains("limit=5"), "{}", hop.query);
}

/// AG-21, AG-84: a bound is a bound on every argument of the table. Each of these is refused before a
/// broker request exists, so the whole table is validated and not only the arguments that were there
/// before it.
#[tokio::test]
async fn an_argument_of_the_table_outside_its_bound_is_refused_before_the_broker() {
    let realm = common::Realm::new();
    let broker = common::BrokerStub::start(vec![json!([])]).await;
    let before = broker.hops().len();

    let refused = [
        (
            "a type selection that is a path",
            json!({ "type": "../../etc/passwd" }),
        ),
        (
            "a type list past fifty",
            json!({ "type": (0..51).map(|n| format!("Type{n}")).collect::<Vec<_>>() }),
        ),
        ("an id that is not a URN", json!({ "id": ["not-a-urn"] })),
        (
            "an id list past a hundred",
            json!({ "id": (0..101).map(|n| format!("urn:ngsi-ld:T:{n}")).collect::<Vec<_>>() }),
        ),
        (
            "a pattern past its length",
            json!({ "type": "T", "idPattern": "x".repeat(257) }),
        ),
        (
            "a join nobody serves",
            json!({ "type": "T", "join": "sideways" }),
        ),
        (
            "a join level past three",
            json!({ "type": "T", "joinLevel": 4 }),
        ),
        (
            "a join level of zero",
            json!({ "type": "T", "joinLevel": 0 }),
        ),
        ("a cursor below zero", json!({ "type": "T", "cursor": -1 })),
        (
            "a limit past a thousand",
            json!({ "type": "T", "limit": 1001 }),
        ),
        (
            "a format nobody serves",
            json!({ "type": "T", "format": "yaml" }),
        ),
        (
            "an option nobody serves",
            json!({ "type": "T", "options": ["everything"] }),
        ),
        (
            "a containedBy that is not a URN",
            json!({ "type": "T", "containedBy": ["../x"] }),
        ),
        ("no selector at all", json!({ "limit": 5 })),
        ("a pattern on its own", json!({ "idPattern": "^urn:.*$" })),
        (
            "an argument nobody published",
            json!({ "type": "T", "offset": 5 }),
        ),
    ];
    for (what, arguments) in refused {
        let (_, answer, _) = send(
            app(&broker.url, &realm),
            message(
                &format!("/api/endpoint/{PUBLIC}/mcp"),
                None,
                call("query_entities", arguments),
            ),
        )
        .await;
        let refused_here =
            answer["result"]["isError"] == json!(true) || answer.get("error").is_some();
        assert!(refused_here, "{what} was accepted: {answer}");
    }
    assert_eq!(
        broker.hops().len(),
        before,
        "an argument outside its bound reached the broker"
    );
}
