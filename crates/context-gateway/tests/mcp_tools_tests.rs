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
        declared_types: None,
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
        creates: None,
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
        policy_names: Vec::new(),
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

/// AG-32: a phone with no token is told where its authorization server is.
#[tokio::test]
async fn an_unauthenticated_call_names_the_resource_metadata_and_a_public_one_does_not() {
    let realm = common::Realm::new();
    // The broker's MCP surface lists one tool for the public view (T-3647).
    let broker = common::mcp_broker(json!([{ "name": "query_entities" }])).await;

    let (status, _, headers) = send(
        app(&broker, &realm),
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
        app(&broker, &realm),
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
        app(&broker, &realm),
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
