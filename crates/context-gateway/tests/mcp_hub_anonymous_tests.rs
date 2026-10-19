//! The hub as one connector for the whole portal (T-4114, ADR-N-025 §4b, EP-87, EP-88, EP-111).
//!
//! Without a token it answers over the public Endpoints alone; `endpoint:*` reaches every
//! Endpoint the person may read; `list_endpoints` filters and describes enough to pick one. No
//! internal title, type or count reaches a caller without a token.

mod common;

use axum::body::Body;
use axum::http::{header::AUTHORIZATION, Method, Request, StatusCode};
use axum::Router;
use context_gateway::app::{router, Gateway};
use context_gateway::auth::accounts::ServiceAccounts;
use context_gateway::pdp::PolicyPdp;
use context_gateway::proxy::Broker;
use context_gateway::resolver::{Endpoint, Model};
use jc_core::kinds::{Audience, PolicySpec, Representation};
use serde_json::{json, Value};
use std::sync::Arc;
use tower::ServiceExt;

const HOST: &str = "https://city.example";
/// Public: anybody reads its air quality, history included.
const OPEN: &str = "oooooooooooooooooooooooooo";
/// Internal: a steward of the organization reads its bikes.
const INNER: &str = "iiiiiiiiiiiiiiiiiiiiiiiiii";
const UNKNOWN: &str = "zzzzzzzzzzzzzzzzzzzzzzzzzy";

fn grant(role: &str, operations: &str, entity_type: &str) -> PolicySpec {
    serde_norway::from_str(&format!(
        "contextSpaceRef: mesto\n\
         assigner: did:web:banskabystrica.sk\n\
         assignee: {{ kind: role, id: {role} }}\n\
         operations: [{operations}]\n\
         information:\n  \
         - entities:\n      \
         - type: {entity_type}\n"
    ))
    .expect("the policy spec parses")
}

fn endpoint(
    slug: &str,
    title: &str,
    audience: Audience,
    class: &str,
    policy: PolicySpec,
) -> Endpoint {
    let schema = json!({
        "$defs": {
            class: {
                "type": "object",
                "properties": { "id": {}, "type": {}, "name": {}, "location": {} },
            }
        }
    });
    Endpoint {
        declared_types: None,
        roles: Default::default(),
        slug: slug.to_owned(),
        title: [("en".to_owned(), title.to_owned())].into_iter().collect(),
        description: [("sk".to_owned(), format!("Dáta: {title}"))]
            .into_iter()
            .collect(),
        space: "mesto".to_owned(),
        project: "mesto".to_owned(),
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
            name: "mesto".to_owned(),
            version: "1.0.0".to_owned(),
            major: 1,
            classes: vec![class.to_owned()],
            json_schema: Some(schema.into()),
            context: None,
        }],
        policy_names: Vec::new(),
        manifest_name: title.to_lowercase().replace(' ', "-"),
        policies: vec![policy],
    }
}

fn app(broker: &str, realm: &common::Realm) -> Router {
    let open = endpoint(
        OPEN,
        "Air quality",
        Audience::Public,
        "AirQualityObserved",
        grant("public", "queryEntity, queryTemporal", "AirQualityObserved"),
    );
    let inner = endpoint(
        INNER,
        "Bike stations",
        Audience::Organization,
        "BikeHireDockingStation",
        grant("steward", "queryEntity", "BikeHireDockingStation"),
    );
    router(Arc::new(
        Gateway::new(
            Broker::new(broker),
            Box::new(PolicyPdp),
            "banskabystrica.sk",
        )
        .serve([open, inner])
        .authenticate(
            Arc::new(realm.verifier()),
            ServiceAccounts::new(),
            Some(HOST.to_owned()),
        ),
    ))
}

/// A steward's hub token with `scope`.
fn hub_token(realm: &common::Realm, scope: &str) -> String {
    realm.mint(&json!({
        "iss": common::ISSUER,
        "sub": "5e1d",
        "aud": "mcp-hub",
        "scope": scope,
        "azp": "mcp-hub",
        "preferred_username": "jana",
        "groups": ["/mesto"],
        "realm_access": { "roles": ["steward"] },
        "exp": common::in_seconds(300),
        "iat": common::in_seconds(-10),
    }))
}

fn rpc(token: Option<&str>, body: Value) -> Request<Body> {
    let mut builder = Request::builder()
        .method(Method::POST)
        .uri("/api/mcp")
        .header("content-type", "application/json");
    if let Some(token) = token {
        builder = builder.header(AUTHORIZATION, format!("Bearer {token}"));
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

async fn raw(app: Router, request: Request<Body>) -> (StatusCode, axum::http::HeaderMap, String) {
    let response = app.oneshot(request).await.expect("the gateway answers");
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .expect("a body");
    (
        status,
        headers,
        String::from_utf8_lossy(&bytes).into_owned(),
    )
}

async fn listed(app: Router, token: Option<&str>, arguments: Value) -> Value {
    let (status, _, body) = raw(app, rpc(token, call("list_endpoints", arguments))).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let answer: Value = serde_json::from_str(&body).expect("JSON");
    answer["result"].clone()
}

fn slugs(result: &Value) -> Vec<&str> {
    result["structuredContent"]["endpoints"]
        .as_array()
        .unwrap_or_else(|| panic!("no list: {result}"))
        .iter()
        .filter_map(|row| row["slug"].as_str())
        .collect()
}

#[tokio::test]
async fn without_a_token_the_hub_lists_the_public_endpoints_and_nothing_internal() {
    let realm = common::Realm::new();
    let app = app("http://127.0.0.1:1", &realm);

    let result = listed(app.clone(), None, json!({})).await;
    assert_eq!(slugs(&result), [OPEN], "{result}");
    assert_eq!(result["structuredContent"]["total"], json!(1));
    let (status, _, tools) = raw(
        app.clone(),
        rpc(
            None,
            json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list" }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    // Neither the slug, the title nor the type of the internal Endpoint is in either answer.
    for text in [result.to_string(), tools] {
        for secret in [INNER, "Bike stations", "BikeHireDockingStation"] {
            assert!(!text.contains(secret), "{secret} leaked: {text}");
        }
    }
}

#[tokio::test]
async fn without_a_token_an_internal_slug_is_the_login_prompt_like_an_unknown_one() {
    let realm = common::Realm::new();
    let app = app("http://127.0.0.1:1", &realm);
    let mut answers = Vec::new();
    for slug in [INNER, UNKNOWN] {
        let message = call("query_entities", json!({ "endpoint": slug, "type": ["X"] }));
        let (status, headers, body) = raw(app.clone(), rpc(None, message)).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{slug}: {body}");
        assert_eq!(
            headers["www-authenticate"],
            format!(
                "Bearer resource_metadata=\"{HOST}/api/mcp/.well-known/oauth-protected-resource\""
            )
            .as_str()
        );
        answers.push(body);
    }
    assert_eq!(
        answers[0], answers[1],
        "an internal slug answers like an unknown one (SP-20)"
    );
}

/// T-4373: the hub's answer names the unit a person reads beside each UN/CEFACT code, as
/// Claude.ai otherwise reads `GQ` as "no unit".
#[tokio::test]
async fn the_hubs_tool_result_names_the_unit_beside_its_code() {
    let realm = common::Realm::new();
    let station = json!({ "id": "urn:ngsi-ld:AirQualityObserved:s1", "type": "AirQualityObserved",
        "pm10": { "type": "Property", "value": 8.9, "unitCode": "GQ" } });
    let broker = common::BrokerStub::start(vec![json!([station])]).await;
    let app = app(&broker.url, &realm);
    let message = call(
        "query_entities",
        json!({ "endpoint": OPEN, "type": ["AirQualityObserved"] }),
    );
    let (status, _, body) = raw(app, rpc(None, message)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let answer: Value = serde_json::from_str(&body).expect("json");
    let pm10 = &answer["result"]["structuredContent"]["entities"][0]["pm10"];
    assert_eq!(pm10["unit"], "µg/m³", "{body}");
    assert_eq!(pm10["unitCode"], "GQ");
}

#[tokio::test]
async fn without_a_token_a_public_endpoint_is_read_as_the_anonymous_caller() {
    let realm = common::Realm::new();
    let broker = common::BrokerStub::start(vec![json!([])]).await;
    let app = app(&broker.url, &realm);
    let message = call(
        "query_entities",
        json!({ "endpoint": OPEN, "type": ["AirQualityObserved"] }),
    );
    let (status, _, body) = raw(app, rpc(None, message)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let hops = broker.hops();
    assert_eq!(hops.len(), 1, "{body}");
    assert_eq!(hops[0].view, OPEN);
    assert!(
        hops[0].headers.get(AUTHORIZATION).is_none(),
        "no credential is invented"
    );
}

#[tokio::test]
async fn the_one_login_scope_adds_the_internal_endpoints_the_person_may_read() {
    let realm = common::Realm::new();
    let app = app("http://127.0.0.1:1", &realm);

    let everything = hub_token(&realm, "openid endpoint:*");
    let result = listed(app.clone(), Some(&everything), json!({})).await;
    assert_eq!(slugs(&result), [INNER, OPEN], "{result}");
    let rows = result["structuredContent"]["endpoints"]
        .as_array()
        .expect("rows");
    assert_eq!(rows[0]["access"], json!("internal"));
    assert_eq!(rows[1]["access"], json!("public"));

    // No scope picks nothing internal; the public Endpoints stay, as their own URLs answer.
    let none = hub_token(&realm, "openid");
    assert_eq!(
        slugs(&listed(app.clone(), Some(&none), json!({})).await),
        [OPEN]
    );
    // A narrower scope narrows.
    let one = hub_token(&realm, &format!("openid endpoint:{INNER}"));
    assert_eq!(
        slugs(&listed(app, Some(&one), json!({})).await),
        [INNER, OPEN]
    );
}

/// T-4492: a list row says what an agent picks by, in one line; get_endpoint answers the
/// whole record, URLs included, for the one it picked, by slug, title or space.
#[tokio::test]
async fn each_row_says_enough_to_pick_the_endpoint() {
    let realm = common::Realm::new();
    let app = app("http://127.0.0.1:1", &realm);
    let result = listed(app.clone(), None, json!({})).await;
    let row = &result["structuredContent"]["endpoints"][0];
    assert_eq!(row["types"], json!(["AirQualityObserved"]));
    assert_eq!(row["domains"], json!(["Environment"]));
    assert!(
        row.get("surfaces").is_none() && row.get("dcat").is_none(),
        "{row}"
    );
    let text = result["content"][0]["text"].as_str().expect("text");
    assert!(
        text.lines()
            .nth(1)
            .is_some_and(|line| line.starts_with(OPEN)),
        "{text}"
    );
    let (status, _, body) = raw(
        app.clone(),
        rpc(None, call("get_endpoint", json!({ "endpoint": OPEN }))),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let answer: Value = serde_json::from_str(&body).expect("JSON");
    let row = &answer["result"]["structuredContent"]["endpoint"];
    assert_eq!(row["slug"], json!(OPEN));
    // T-4748: short by default, the attributes of one type on request.
    assert!(row.get("attributes").is_none(), "{row}");
    assert_eq!(row["types"], json!(["AirQualityObserved"]));
    assert_eq!(row["temporal"], json!(true));
    assert_eq!(row["description"], json!({ "sk": "Dáta: Air quality" }));
    assert_eq!(row["dcat"], json!(format!("{HOST}/api/endpoint/{OPEN}/")));
    assert_eq!(
        row["schema"],
        json!(format!(
            "{HOST}/api/endpoint/{OPEN}/schema/v1/model.linkml.yaml"
        ))
    );
    let base = format!("{HOST}/api/endpoint/{OPEN}/schema");
    assert_eq!(
        row["schemas"],
        json!({
            "index": format!("{base}/index.json"),
            "linkml": format!("{base}/v1/model.linkml.yaml"),
            "shacl": format!("{base}/v1/model.shacl.ttl"),
            "jsonSchema": format!("{base}/v1/model.schema.json"),
            "context": format!("{base}/v1/context.jsonld"),
        })
    );
    let whole = answer["result"]["structuredContent"].to_string();
    assert!(whole.len() < 2_000, "{} bytes: {whole}", whole.len());

    // The readable {project}/{name} names it too, without case.
    let by_name = call_ok(app.clone(), json!({ "endpoint": "Mesto/air-quality" })).await;
    assert_eq!(by_name["endpoint"]["slug"], json!(OPEN));
    let detail = call_ok(
        app.clone(),
        json!({ "endpoint": OPEN, "detail": "attributes", "type": "AirQualityObserved" }),
    )
    .await;
    assert_eq!(
        detail["endpoint"]["attributes"],
        json!({ "AirQualityObserved": ["location", "name"] })
    );
    for (arguments, says) in [
        (
            json!({ "endpoint": OPEN, "detail": "attributes" }),
            "comes with one `type`",
        ),
        (
            json!({ "endpoint": OPEN, "type": "AirQualityObserved" }),
            "comes with one `type`",
        ),
        (
            json!({ "endpoint": OPEN, "detail": "attributes", "type": "Bike" }),
            "is no type of this Endpoint",
        ),
        (
            json!({ "endpoint": OPEN, "detail": "everything" }),
            "`detail` is",
        ),
        // The internal Endpoint is not found by its readable name without a token.
        (
            json!({ "endpoint": "mesto/bike-stations" }),
            "no Endpoint you may read",
        ),
    ] {
        let (_, _, body) = raw(
            app.clone(),
            rpc(None, call("get_endpoint", arguments.clone())),
        )
        .await;
        let answer: Value = serde_json::from_str(&body).expect("JSON");
        assert_eq!(
            answer["result"]["isError"],
            json!(true),
            "{arguments}: {body}"
        );
        assert!(body.contains(says), "{arguments}: {body}");
        assert!(!body.contains(INNER), "{arguments}: {body}");
    }
}

/// The structured half of a get_endpoint call that answered.
async fn call_ok(app: Router, arguments: Value) -> Value {
    let (status, _, body) = raw(app, rpc(None, call("get_endpoint", arguments.clone()))).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let answer: Value = serde_json::from_str(&body).expect("JSON");
    assert_ne!(
        answer["result"]["isError"],
        json!(true),
        "{arguments}: {body}"
    );
    answer["result"]["structuredContent"].clone()
}

#[tokio::test]
async fn list_endpoints_filters_and_an_empty_match_is_an_empty_list() {
    let realm = common::Realm::new();
    let app = app("http://127.0.0.1:1", &realm);
    let token = hub_token(&realm, "openid endpoint:*");
    for (arguments, expected) in [
        (json!({ "q": "BIKE" }), vec![INNER]),
        (json!({ "q": "dáta: air" }), vec![OPEN]),
        (json!({ "type": "AirQualityObserved" }), vec![OPEN]),
        (json!({ "attribute": "location" }), vec![INNER, OPEN]),
        (json!({ "access": "internal" }), vec![INNER]),
        (json!({ "space": "mesto", "access": "public" }), vec![OPEN]),
        (json!({ "q": "trams" }), vec![]),
    ] {
        let result = listed(app.clone(), Some(&token), arguments.clone()).await;
        assert_eq!(result["isError"], json!(false), "{arguments}: {result}");
        assert_eq!(slugs(&result), expected, "{arguments}");
        assert_eq!(
            result["structuredContent"]["total"],
            json!(expected.len()),
            "{arguments}"
        );
    }

    let first = listed(app.clone(), Some(&token), json!({ "limit": 1 })).await;
    assert_eq!(first["structuredContent"]["nextCursor"], json!("1"));
    let next = listed(
        app.clone(),
        Some(&token),
        json!({ "limit": 1, "cursor": "1" }),
    )
    .await;
    assert_eq!(slugs(&next), [OPEN]);

    for arguments in [
        json!({ "access": "secret" }),
        json!({ "cursor": "x" }),
        json!({ "type": 5 }),
        json!({ "q": "" }),
        json!({ "spaces": "mesto" }),
    ] {
        let result = listed(app.clone(), Some(&token), arguments.clone()).await;
        assert_eq!(result["isError"], json!(true), "{arguments}: {result}");
    }
}

#[tokio::test]
async fn the_rest_twin_answers_the_same_list_without_a_token() {
    let realm = common::Realm::new();
    let app = app("http://127.0.0.1:1", &realm);
    let get = |uri: &str| {
        Request::builder()
            .uri(uri)
            .body(Body::empty())
            .expect("a request")
    };
    let (status, _, body) = raw(app.clone(), get("/api/endpoints")).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let page: Value = serde_json::from_str(&body).expect("JSON");
    assert_eq!(page["total"], json!(1));
    assert_eq!(page["endpoints"][0]["slug"], json!(OPEN));
    assert!(!body.contains(INNER));
    // A filter is text, even one that looks like a number.
    let (status, _, body) = raw(app.clone(), get("/api/endpoints?q=2026")).await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let (status, _, metadata) =
        raw(app, get("/api/mcp/.well-known/oauth-protected-resource")).await;
    assert_eq!(status, StatusCode::OK);
    let metadata: Value = serde_json::from_str(&metadata).expect("JSON");
    assert_eq!(metadata["scopes_supported"], json!(["endpoint:*"]));
}

/// T-4258, EP-116: an agent given only the hub's address learns from it that it speaks NGSI-LD,
/// finds the Endpoint in the catalogue, reads its schema, and queries current values and history
/// with the standard's arguments, which reach the broker's view as the agent sent them.
#[tokio::test]
async fn an_agent_with_only_the_address_reads_ngsi_ld_from_catalogue_to_history() {
    let realm = common::Realm::new();
    let broker = common::BrokerStub::start(vec![json!([]), json!([])]).await;
    let app = app(&broker.url, &realm);
    let answer = |body: String| -> Value { serde_json::from_str(&body).expect("JSON") };

    let initialize = json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {} });
    let (status, _, body) = raw(app.clone(), rpc(None, initialize)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let instructions = answer(body)["result"]["instructions"]
        .as_str()
        .unwrap_or_default()
        .to_owned();
    for word in [
        "NGSI-LD",
        "@context",
        "idPattern",
        "timerel",
        "list_endpoints",
    ] {
        assert!(
            instructions.contains(word),
            "{word} is not in {instructions}"
        );
    }

    let list = json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list" });
    let (_, _, body) = raw(app.clone(), rpc(None, list)).await;
    let tools = answer(body);
    let names: Vec<&str> = tools["result"]["tools"]
        .as_array()
        .expect("a tool list")
        .iter()
        .filter_map(|tool| tool["name"].as_str())
        .collect();
    assert!(names.contains(&"query_temporal"), "{names:?}");
    for tool in tools["result"]["tools"].as_array().into_iter().flatten() {
        assert_eq!(tool["annotations"]["readOnlyHint"], true, "{tool}");
    }

    let found = listed(app.clone(), None, json!({ "q": "air" })).await;
    assert_eq!(slugs(&found), [OPEN], "{found}");

    let schema = call(
        "describe_schema",
        json!({ "endpoint": OPEN, "format": "shacl" }),
    );
    let (status, _, body) = raw(app.clone(), rpc(None, schema)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_ne!(answer(body.clone())["result"]["isError"], true, "{body}");

    let current = json!({
        "type": ["AirQualityObserved"],
        "idPattern": "^urn:ngsi-ld:AirQualityObserved:.*",
        "q": "no2>40",
        "format": "keyValues",
        "sysAttrs": true,
        "orderBy": "no2;desc",
    });
    let history = json!({
        "type": ["AirQualityObserved"],
        "attrs": ["no2"],
        "timerel": "between",
        "timeAt": "2026-10-12T00:00:00Z",
        "endTimeAt": "2026-10-13T00:00:00Z",
        "lastN": 5,
        "format": "temporalValues",
    });
    for (name, arguments) in [("query_entities", &current), ("query_temporal", &history)] {
        let mut sent = arguments.clone();
        sent["endpoint"] = json!(OPEN);
        let (status, _, body) = raw(app.clone(), rpc(None, call(name, sent))).await;
        assert_eq!(status, StatusCode::OK, "{name}: {body}");
        assert_ne!(
            answer(body.clone())["result"]["isError"],
            true,
            "{name}: {body}"
        );
    }

    let forwarded: Vec<Value> = broker
        .hops()
        .iter()
        .filter(|hop| hop.path == "/mcp")
        .map(|hop| serde_json::from_slice(&hop.body).expect("a JSON-RPC message"))
        .collect();
    assert_eq!(forwarded.len(), 2, "{forwarded:?}");
    assert_eq!(forwarded[0]["params"]["name"], "query_entities");
    assert_eq!(forwarded[0]["params"]["arguments"], current);
    assert_eq!(forwarded[1]["params"]["name"], "query_temporal");
    assert_eq!(forwarded[1]["params"]["arguments"], history);
}

fn answer(body: String) -> Value {
    serde_json::from_str(&body).expect("JSON")
}

/// The `tools/call` messages the broker's views received, in order.
fn forwarded(broker: &common::BrokerStub) -> Vec<Value> {
    broker
        .hops()
        .iter()
        .filter(|hop| hop.path == "/mcp")
        .map(|hop| serde_json::from_slice(&hop.body).expect("a JSON-RPC message"))
        .collect()
}

/// Two years of every AirQualityObserved's history, with no lastN.
fn years() -> Value {
    json!({
        "endpoint": OPEN,
        "type": ["AirQualityObserved"],
        "timerel": "between",
        "timeAt": "2024-10-01T00:00:00Z",
        "endTimeAt": "2026-10-01T00:00:00Z",
    })
}

/// T-4261: without a token a history read is bounded before the broker sees it: two years are
/// refused by name, a day goes on with lastN 100 filled in.
#[tokio::test]
async fn without_a_token_query_temporal_reads_a_bounded_window() {
    let realm = common::Realm::new();
    let broker = common::BrokerStub::start(vec![json!([])]).await;
    let app = app(&broker.url, &realm);

    let (status, _, body) = raw(app.clone(), rpc(None, call("query_temporal", years()))).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(answer(body.clone())["result"]["isError"], true, "{body}");
    assert!(body.contains("at most 31 days"), "{body}");
    assert!(forwarded(&broker).is_empty(), "nothing reached the broker");

    let mut day = years();
    day["timeAt"] = json!("2026-09-30T00:00:00Z");
    let (status, _, body) = raw(app, rpc(None, call("query_temporal", day))).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_ne!(answer(body.clone())["result"]["isError"], true, "{body}");
    let sent = forwarded(&broker);
    assert_eq!(sent.len(), 1, "{sent:?}");
    assert_eq!(sent[0]["params"]["arguments"]["lastN"], json!(100));
}

/// T-4261: a signed-in caller keeps the Endpoint Policy's own limits: the same two years go on
/// as asked, nothing filled in.
#[tokio::test]
async fn a_signed_in_caller_is_not_held_to_the_anonymous_bounds() {
    let realm = common::Realm::new();
    let broker = common::BrokerStub::start(vec![json!([])]).await;
    let app = app(&broker.url, &realm);
    let token = hub_token(&realm, "openid endpoint:*");
    let (status, _, body) = raw(app, rpc(Some(&token), call("query_temporal", years()))).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_ne!(answer(body.clone())["result"]["isError"], true, "{body}");
    let sent = forwarded(&broker);
    assert_eq!(sent.len(), 1, "{sent:?}");
    let mut asked = years();
    asked.as_object_mut().expect("an object").remove("endpoint");
    assert_eq!(sent[0]["params"]["arguments"], asked);
}
