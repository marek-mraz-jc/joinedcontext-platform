//! The chat route end to end (T-3055, AG-98…AG-108): a stub realm hands out the service's
//! token, a stub `jc-agent-proxy` plays the model, a stub MCP surface plays a public Endpoint,
//! and the passages come from the real store and embedder (see `common`).

#[path = "common/corpus.rs"]
mod corpus;
#[path = "common/db.rs"]
mod db;
#[path = "common/model.rs"]
mod model;

use std::collections::BTreeMap;
use std::sync::{Arc, RwLock};

use assistant::chat::model::{Model, ModelConfig};
use assistant::chat::{router, ChatState};
use assistant::embed::{embed_missing, Embedder};
use assistant::worker::{Deployment, EndpointRef, Snapshot};
use axum::body::Body;
use axum::http::{Request, StatusCode};
use jc_core::kinds::assistant::{AssistantDeploymentSpec, Budget, Channel, Connector, RateLimit};
use serde_json::{json, Value};
use sqlx::PgPool;
use tower::ServiceExt;
use wiremock::matchers::{body_string_contains, header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const ORIGIN: &str = "https://www.hronov.example";
const SLUG: &str = "abcdefghijklmnopqrstuvwxyz234567";
const HIDDEN_SLUG: &str = "zyxwvutsrqponmlkjihgfedcba765432";
const PORTAL_TOKEN: &str = "the-portals-token";
const PERSON_TOKEN: &str = "the-persons-own-token";

struct World {
    admin: PgPool,
    pool: PgPool,
    name: String,
    realm: MockServer,
    proxy: MockServer,
    gateway: MockServer,
    functions: MockServer,
    app: axum::Router,
    /// The service's question permits, to fill in a test of the limit.
    questions: Arc<tokio::sync::Semaphore>,
}

fn deployment(channel: Channel, per_conversation: u64, per_client: u32) -> Deployment {
    Deployment {
        project: "hronov".into(),
        name: "obcania".into(),
        spec: AssistantDeploymentSpec {
            public_id: "hronov-obcania".into(),
            channel,
            system_prompt: Some("Odpovedaj stručne.".into()),
            sources: vec!["web".into()],
            connectors: vec![
                Connector {
                    endpoint: "ovzdusie".into(),
                    tools: vec!["query_entities".into()],
                    timeout_seconds: 5,
                },
                Connector {
                    endpoint: "interne".into(),
                    tools: vec!["query_entities".into()],
                    timeout_seconds: 5,
                },
            ],
            allowed_origins: vec![ORIGIN.into()],
            rate_limit: Some(RateLimit {
                requests_per_minute: 100,
                per_client_per_minute: per_client,
                per_client_per_hour: 600,
                per_client_per_day: 6_000,
                streams_per_client: 2,
            }),
            budget: Some(Budget {
                tokens_per_day: 1_000_000,
                tokens_per_conversation: per_conversation,
                cost_per_day_usd: 3.0,
                output_tokens_per_answer: 700,
                messages_per_conversation: 30,
                question_chars: 1_000,
            }),
            theme: None,
            languages: vec!["sk".into()],
            sandbox: false,
            model: None,
            suggestions: Default::default(),
            disclaimer: Default::default(),
            enabled: true,
            scope: None,
        },
    }
}

async fn world(test: &str, deployment: Deployment) -> World {
    let embedder = Embedder::load(&model::model_dir(), 1).expect("the pinned model loads");
    let (admin, pool, name) = db::database(test).await;
    corpus::pages(
        &pool,
        "hronov",
        "web",
        &[
            (
                "https://hronov.example/ovzdusie",
                "sk",
                "# Ovzdušie v Hronove\n\nMeracie stanice kvality ovzdušia sú na Námestí SNP a pri železničnej stanici.",
            ),
            (
                "https://hronov.example/kniznica",
                "sk",
                "Knižnica je otvorená v pondelok až piatok od 9:00 do 18:00.",
            ),
        ],
    )
    .await;
    corpus::pages(
        &pool,
        "hronov",
        "intranet",
        &[(
            "https://intranet.hronov.example/platy",
            "sk",
            "Platové tabuľky zamestnancov úradu.",
        )],
    )
    .await;
    embed_missing(&pool, &embedder, "hronov", 100)
        .await
        .expect("embedded");

    let realm = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/token"))
        .and(body_string_contains("client_credentials"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"access_token": "svc-token", "expires_in": 300})),
        )
        .mount(&realm)
        .await;
    Mock::given(method("POST")).and(path("/token/introspect")).and(body_string_contains(format!("token={PORTAL_TOKEN}")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"active": true, "azp": "portal-api", "username": "service-account-portal-api", "aud": ["jc-assistant"]})))
        .mount(&realm).await;
    Mock::given(method("POST"))
        .and(path("/token/introspect"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"active": false})))
        .with_priority(10)
        .mount(&realm)
        .await;
    let proxy = MockServer::start().await;
    let gateway = MockServer::start().await;
    Mock::given(method("POST")).and(path(format!("/api/endpoint/{SLUG}/mcp"))).and(body_string_contains("tools/list"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"jsonrpc": "2.0", "id": 1, "result": {"tools": [
            {"name": "query_entities", "description": "Reads air quality stations.", "inputSchema": {"type": "object", "properties": {"type": {"type": "string"}}}},
            {"name": "delete_everything", "description": "Not allowed.", "inputSchema": {"type": "object"}}
        ]}})))
        .mount(&gateway).await;
    Mock::given(method("POST")).and(path(format!("/api/endpoint/{SLUG}/mcp"))).and(body_string_contains("tools/call"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"jsonrpc": "2.0", "id": 1, "result": {
            "content": [{"type": "text", "text": "[{\"id\":\"urn:ngsi-ld:AirQualityObserved:snp\",\"no2\":21}]"}]
        }})))
        .mount(&gateway).await;

    let mut snapshot = Snapshot {
        deployments: vec![deployment],
        ..Snapshot::default()
    };
    snapshot.endpoints = BTreeMap::from([
        (
            ("hronov".into(), "ovzdusie".into()),
            EndpointRef {
                slug: SLUG.into(),
                audience: "public".into(),
            },
        ),
        (
            ("hronov".into(), "interne".into()),
            EndpointRef {
                slug: HIDDEN_SLUG.into(),
                audience: "organization".into(),
            },
        ),
    ]);
    let functions = MockServer::start().await;
    let http = reqwest::Client::new();
    let questions = Arc::new(tokio::sync::Semaphore::new(assistant::chat::MAX_QUESTIONS));
    let state = Arc::new(ChatState {
        pool: pool.clone(),
        embedder,
        model: Model::new(
            ModelConfig {
                proxy: proxy.uri(),
                token_url: format!("{}/token", realm.uri()),
                client_id: "jc-assistant".into(),
                credential: jc_core::ClientCredential::Secret(jc_core::Secret::new("s3cret")),
                model: "test/model".into(),
            },
            http.clone(),
        ),
        http,
        gateway: gateway.uri(),
        functions: Some(functions.uri()),
        portal_client: "portal-api".into(),
        public_origin: Some("https://assistant.example".into()),
        snapshot: RwLock::new(Arc::new(snapshot)),
        limits: tokio::sync::Mutex::default(),
        questions: Arc::clone(&questions),
        answers: Default::default(),
        endpoint_base: url::Url::parse("https://data.hronov.example/").ok(),
    });
    World {
        admin,
        pool,
        name,
        realm,
        proxy,
        gateway,
        functions,
        app: router(state),
        questions,
    }
}

fn completion(message: Value, tokens: u64) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(json!({
        "choices": [{"message": message}],
        "usage": {"prompt_tokens": tokens - 10, "completion_tokens": 10, "total_tokens": tokens}
    }))
}

fn tool_call(id: &str, name: &str, arguments: Value) -> Value {
    json!({"role": "assistant", "content": null, "tool_calls": [
        {"id": id, "type": "function", "function": {"name": name, "arguments": arguments.to_string()}}
    ]})
}

/// The model's script: each completion once, in order.
async fn script(proxy: &MockServer, answers: Vec<ResponseTemplate>) {
    for (i, answer) in answers.into_iter().enumerate() {
        Mock::given(method("POST"))
            .and(path("/v1/llm/chat/completions"))
            .respond_with(answer)
            .up_to_n_times(1)
            .with_priority(u8::try_from(i + 1).expect("few"))
            .mount(proxy)
            .await;
    }
}

fn ask(body: Value, origin: Option<&str>) -> Request<Body> {
    ask_from(body, origin, "203.0.113.7")
}

fn ask_from(body: Value, origin: Option<&str>, client: &str) -> Request<Body> {
    let mut request = Request::builder()
        .method("POST")
        .uri("/api/v1/d/hronov-obcania/chat")
        .header("content-type", "application/json")
        .header("x-forwarded-for", format!("{client}, 10.0.0.1"));
    if let Some(origin) = origin {
        request = request.header("origin", origin);
    }
    request
        .body(Body::from(body.to_string()))
        .expect("a request")
}

/// The events of an answer, as (name, data).
async fn answer_of(
    app: &axum::Router,
    request: Request<Body>,
) -> (StatusCode, Vec<(String, Value)>, axum::http::HeaderMap) {
    let response = app.clone().oneshot(request).await.expect("an answer");
    let (status, headers) = (response.status(), response.headers().clone());
    let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .expect("a body");
    let text = String::from_utf8_lossy(&bytes);
    let mut found = Vec::new();
    for block in text.split("\n\n") {
        let name = block
            .lines()
            .find_map(|l| l.strip_prefix("event: "))
            .unwrap_or_default();
        let data = block
            .lines()
            .find_map(|l| l.strip_prefix("data: "))
            .unwrap_or_default();
        if !name.is_empty() {
            found.push((
                name.to_owned(),
                serde_json::from_str(data).unwrap_or(Value::Null),
            ));
        }
    }
    if found.is_empty() {
        found.push((
            "body".into(),
            serde_json::from_str(&text).unwrap_or(Value::Null),
        ));
    }
    (status, found, headers)
}

async fn model_calls(proxy: &MockServer) -> Vec<(axum::http::HeaderMap, Value)> {
    proxy
        .received_requests()
        .await
        .unwrap_or_default()
        .into_iter()
        .map(|r| {
            let mut headers = axum::http::HeaderMap::new();
            for (k, v) in r.headers.iter() {
                if let (Ok(k), Ok(v)) = (
                    axum::http::HeaderName::from_bytes(k.as_str().as_bytes()),
                    axum::http::HeaderValue::from_bytes(v.as_bytes()),
                ) {
                    headers.insert(k, v);
                }
            }
            (
                headers,
                serde_json::from_slice(&r.body).unwrap_or(Value::Null),
            )
        })
        .collect()
}

/// The recorded answers of the public Endpoint's `list_types` and `query_entities` (T-4140).
async fn recorded_air(gateway: &MockServer) {
    let mcp = || Mock::given(method("POST")).and(path(format!("/api/endpoint/{SLUG}/mcp")));
    mcp().and(body_string_contains("tools/list"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"jsonrpc": "2.0", "id": 1, "result": {"tools": [
            {"name": "list_types", "description": "Lists the entity types.", "inputSchema": {"type": "object"}},
            {"name": "query_entities", "description": "Reads entities.", "inputSchema": {"type": "object", "properties": {"type": {"type": "string"}, "limit": {"type": "integer", "maximum": 1000}}}},
            {"name": "delete_entity", "description": "Writes.", "inputSchema": {"type": "object"}}
        ]}})))
        .with_priority(1).mount(gateway).await;
    let types = json!({"entityTypes": {"id": "urn:ngsi-ld:EntityTypeList:1", "type": "EntityTypeList", "typeList": ["AirQualityObserved"]}});
    mcp()
        .and(body_string_contains("\"list_types\""))
        .respond_with(ResponseTemplate::new(200).set_body_json(
            json!({"jsonrpc": "2.0", "id": 1, "result": {
                "content": [{"type": "text", "text": types.to_string()}]
            }}),
        ))
        .with_priority(1)
        .mount(gateway)
        .await;
    let entities = json!({"entities": [
        {"id": "urn:ngsi-ld:AirQualityObserved:snp", "type": "AirQualityObserved",
         "name": {"type": "Property", "value": "Námestie SNP"},
         "no2": {"type": "Property", "value": 21, "observedAt": "2026-10-15T18:00:00Z"},
         "location": {"type": "GeoProperty", "value": {"type": "Point", "coordinates": [19.15, 48.74]}}},
        {"id": "urn:ngsi-ld:AirQualityObserved:stanica", "type": "AirQualityObserved",
         "name": {"type": "Property", "value": "Železničná stanica"},
         "no2": {"type": "Property", "value": 34, "observedAt": "2026-10-15T18:00:00Z"}}
    ]});
    mcp()
        .and(body_string_contains("\"query_entities\""))
        .respond_with(ResponseTemplate::new(200).set_body_json(
            json!({"jsonrpc": "2.0", "id": 1, "result": {
                "content": [{"type": "text", "text": entities.to_string()}]
            }}),
        ))
        .with_priority(1)
        .mount(gateway)
        .await;
}

/// T-4140, AG-102, AG-105, AG-106, AG-108: a data question is routed to the public Endpoint's
/// live values: the router sees the types `list_types` gives, the service reads them with
/// `query_entities` alone (a write tool the manifest names is never offered or called), the
/// person gets the table with its download link, and the answer cites it without a search.
#[tokio::test]
async fn a_data_question_is_answered_from_the_endpoints_live_values() {
    let mut d = deployment(Channel::Public, 50_000, 10);
    d.spec.connectors[0].tools = vec![
        "list_types".into(),
        "query_entities".into(),
        "delete_entity".into(),
    ];
    let w = world("chatok", d).await;
    recorded_air(&w.gateway).await;
    script(&w.proxy, vec![
        completion(json!({"role": "assistant", "content": "{\"route\": \"data\", \"queries\": [\"NO2\"], \"data\": [{\"endpoint\": \"ovzdusie\", \"type\": \"AirQualityObserved\"}, {\"endpoint\": \"interne\", \"type\": \"Record\"}]}"}), 400),
        streamed(&["NO2 na Námestí SNP je 21 [1]."], 900, 30),
    ]).await;
    let (status, events, headers) = answer_of(
        &w.app,
        ask(
            json!({"message": "Aké je NO2 na Námestí SNP?"}),
            Some(ORIGIN),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        headers
            .get("access-control-allow-origin")
            .map(|v| v.to_str().unwrap_or_default()),
        Some(ORIGIN)
    );
    let names: Vec<&str> = events.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(
        names,
        [
            "conversation",
            "tool",
            "tool",
            "data",
            "delta",
            "answer",
            "citations",
            "done"
        ],
        "{events:?}"
    );
    assert_eq!(
        events[2].1,
        json!({"name": "query_entities", "endpoint": "ovzdusie", "status": "done"})
    );
    assert_eq!(
        events[3].1,
        json!({
            "endpoint": "ovzdusie",
            "type": "AirQualityObserved",
            "columns": ["name", "no2", "observedAt"],
            "rows": [
                ["Námestie SNP", 21, "2026-10-15T18:00:00Z"],
                ["Železničná stanica", 34, "2026-10-15T18:00:00Z"]
            ],
            "download": format!("https://data.hronov.example/api/endpoint/{SLUG}/ngsi-ld/v1/entities?type=AirQualityObserved")
        })
    );
    // No page of the sources gives this Endpoint's address: the live data stays unlinked.
    assert_eq!(
        events[6].1,
        json!([{"n": 1, "tool": "query_entities", "endpoint": "ovzdusie"}])
    );
    let done = &events[7].1;
    assert_eq!(
        (&done["tokens"], &done["modelCalls"], &done["cached"]),
        (&json!(1330), &json!(2), &json!(false))
    );

    let calls = model_calls(&w.proxy).await;
    assert_eq!(calls.len(), 2, "no search round, no follow-up");
    let router = calls[0].1["messages"][1]["content"]
        .as_str()
        .unwrap_or_default();
    assert!(
        router.contains("- ovzdusie: AirQualityObserved"),
        "{router}"
    );
    assert!(!router.contains("interne"), "{router}");
    let answering = calls[1].1["messages"][2]["content"]
        .as_str()
        .unwrap_or_default();
    let given: Value = serde_json::from_str(answering.split('\n').next().unwrap_or_default())
        .expect("the data as JSON");
    assert_eq!(given["passages"], json!([]));
    assert_eq!(given["data"][0]["n"], 1);
    assert_eq!(
        given["data"][0]["rows"][1],
        json!(["Železničná stanica", 34, "2026-10-15T18:00:00Z"])
    );
    for (headers, body) in &calls {
        assert_eq!(
            headers
                .get("authorization")
                .map(|v| v.to_str().unwrap_or_default()),
            Some("Bearer svc-token")
        );
        assert_eq!(
            headers
                .get("x-jc-assistant-deployment")
                .map(|v| v.to_str().unwrap_or_default()),
            Some("hronov/obcania")
        );
        assert_eq!(
            body["messages"][0]["content"][0]["cache_control"]["type"],
            "ephemeral"
        );
        assert!(body.get("tools").is_none());
    }
    let sent: Vec<String> = w
        .gateway
        .received_requests()
        .await
        .unwrap_or_default()
        .iter()
        .inspect(|r| {
            assert!(
                !r.url.path().contains(HIDDEN_SLUG),
                "the internal Endpoint was read"
            )
        })
        .map(|r| String::from_utf8_lossy(&r.body).into_owned())
        .filter(|b| b.contains("tools/call"))
        .collect();
    assert_eq!(sent.len(), 2, "{sent:?}");
    assert!(sent[1].contains("\"limit\":50") && sent[1].contains("AirQualityObserved"));
    assert!(sent.iter().all(|b| !b.contains("delete_entity")));

    // The conversation and the day were counted; no text was kept (AG-99).
    let mut tx = assistant::project_scope(&w.pool, "hronov")
        .await
        .expect("scope");
    let (tokens,): (i64,) = sqlx::query_as("SELECT tokens FROM conversations")
        .fetch_one(&mut *tx)
        .await
        .expect("one");
    tx.rollback().await.expect("rollback");
    assert_eq!(tokens, 1330);

    // Live values are read again, never answered from the cache; the types are.
    script(&w.proxy, vec![
        completion(json!({"role": "assistant", "content": "{\"route\": \"data\", \"data\": [{\"endpoint\": \"ovzdusie\", \"type\": \"AirQualityObserved\"}]}"}), 400),
        streamed(&["21 [1]."], 900, 30),
    ]).await;
    let (_, again, _) = answer_of(
        &w.app,
        ask(
            json!({"message": "Aké je NO2 na Námestí SNP?"}),
            Some(ORIGIN),
        ),
    )
    .await;
    assert_eq!(
        again.last().map(|(_, d)| d["cached"].clone()),
        Some(json!(false))
    );
    assert_eq!(model_calls(&w.proxy).await.len(), 4);
    let types_listed = w
        .gateway
        .received_requests()
        .await
        .unwrap_or_default()
        .iter()
        .filter(|r| String::from_utf8_lossy(&r.body).contains("\"list_types\""))
        .count();
    assert_eq!(types_listed, 1, "the types are kept for ten minutes");
    db::drop_database(w.admin, w.pool, &w.name).await;
}

/// AG-105, AG-107: a tool the model makes up is refused and never called; asking for the same
/// call twice ends the loop, and the last call offers no tools.
#[tokio::test]
async fn a_made_up_tool_is_never_called_and_a_repeated_call_ends_the_loop() {
    // A write tool the manifest names is still never offered (T-4140, read-only allowlist).
    let mut d = looped(50_000);
    d.spec.connectors[0].tools.push("delete_everything".into());
    let w = world("chatloop", d).await;
    script(
        &w.proxy,
        vec![
            completion(
                tool_call("c1", "ovzdusie__delete_everything", json!({})),
                100,
            ),
            completion(tool_call("c2", "search", json!({"query": "knižnica"})), 100),
            completion(tool_call("c3", "search", json!({"query": "knižnica"})), 100),
            completion(
                json!({"role": "assistant", "content": "Knižnica je otvorená do 18:00 [1]."}),
                100,
            ),
        ],
    )
    .await;
    let (_, events, _) = answer_of(
        &w.app,
        ask_looped(json!({"message": "Dokedy je otvorená knižnica?"})),
    )
    .await;
    assert_eq!(
        events
            .iter()
            .find(|(n, _)| n == "answer")
            .map(|(_, d)| d["text"].clone()),
        Some(json!("Knižnica je otvorená do 18:00 [1]."))
    );
    let calls = model_calls(&w.proxy).await;
    assert_eq!(calls.len(), 4);
    assert_eq!(
        offered_tools(&calls[0].1),
        ["search", "ovzdusie__query_entities"],
        "the made-up tool is never offered"
    );
    assert_eq!(
        calls[3].1["tool_choice"], "none",
        "after a repeat the model answers"
    );
    // T-3205: the last call is told in words to answer, which `tool_choice: none` alone is not.
    let last = calls[3].1["messages"]
        .as_array()
        .and_then(|m| m.last())
        .cloned()
        .unwrap_or_default();
    assert_eq!(last["role"], "user");
    assert!(
        last["content"]
            .as_str()
            .unwrap_or_default()
            .starts_with("No more searches"),
        "{last}"
    );
    assert!(calls[..3]
        .iter()
        .all(|(_, body)| !body.to_string().contains("No more searches")));
    let refused = calls[1].1["messages"][3]["content"]
        .as_str()
        .expect("refusal");
    assert!(refused.contains("no tool of that name"), "{refused}");
    assert!(w
        .gateway
        .received_requests()
        .await
        .unwrap_or_default()
        .iter()
        .all(|r| !String::from_utf8_lossy(&r.body).contains("tools/call")));
    db::drop_database(w.admin, w.pool, &w.name).await;
}

/// T-3314: a visitor who leaves stops the question at the model's next call; the call already
/// sent is let finish and counted.
#[tokio::test]
async fn a_question_stops_when_its_visitor_leaves() {
    let w = world("chatgone", deployment(Channel::Public, 50_000, 10)).await;
    script(
        &w.proxy,
        vec![
            completion(tool_call("c1", "search", json!({"query": "knižnica"})), 100)
                .set_delay(std::time::Duration::from_millis(300)),
            completion(
                json!({"role": "assistant", "content": "Do 18:00 [1]."}),
                100,
            ),
        ],
    )
    .await;
    let response = w
        .app
        .clone()
        .oneshot(ask(
            json!({"message": "Dokedy je otvorená knižnica?"}),
            None,
        ))
        .await
        .expect("an answer");
    assert_eq!(response.status(), StatusCode::OK);
    // The visitor leaves while the first call is with the model.
    for _ in 0..100 {
        if !model_calls(&w.proxy).await.is_empty() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    drop(response);
    tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
    assert_eq!(
        model_calls(&w.proxy).await.len(),
        1,
        "no call after the visitor left"
    );
    let mut tx = assistant::project_scope(&w.pool, "hronov")
        .await
        .expect("scope");
    let (tokens,): (i64,) = sqlx::query_as("SELECT tokens FROM conversations")
        .fetch_one(&mut *tx)
        .await
        .expect("one");
    assert_eq!(tokens, 100, "the call that was sent is counted");
    drop(tx);
    db::drop_database(w.admin, w.pool, &w.name).await;
}

/// T-3314, AG-99: a conversation past its 24 hours is deleted, in every project, and the
/// deletion reaches no current one, of any project.
#[tokio::test]
async fn expired_conversations_are_deleted_and_current_ones_kept() {
    let (admin, pool, name) = db::database("chatforget").await;
    // Seeded as the owner: the app role writes only its own project's rows.
    let url = std::env::var("JC_ASSISTANT_TEST_DATABASE_URL").expect("the test server");
    let options: sqlx::postgres::PgConnectOptions = url.parse().expect("parses");
    let owner = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .connect_with(options.database(&name))
        .await
        .expect("the owner");
    for (project, age) in [
        ("hronov", 25),
        ("hronov", 1),
        ("trnava", 30),
        ("trnava", 23),
    ] {
        sqlx::query(
            "INSERT INTO conversations (project, deployment, created_at) \
             VALUES ($1, 'obcania', now() - make_interval(hours => $2))",
        )
        .bind(project)
        .bind(age)
        .execute(&owner)
        .await
        .expect("seed");
    }
    assert_eq!(
        assistant::chat::forget_expired(&pool)
            .await
            .expect("deleted"),
        2
    );
    let left: Vec<(String, i64)> = sqlx::query_as(
        "SELECT project, extract(hour FROM now() - created_at)::bigint FROM conversations ORDER BY project",
    )
    .fetch_all(&owner)
    .await
    .expect("left");
    assert_eq!(left, vec![("hronov".into(), 1), ("trnava".into(), 23)]);
    assert_eq!(
        assistant::chat::forget_expired(&pool).await.expect("again"),
        0
    );
    owner.close().await;
    db::drop_database(admin, pool, &name).await;
}

/// T-3314: arguments that break the tool's schema are refused to the model with the place and
/// the rule, and nothing reaches the Endpoint.
#[tokio::test]
async fn arguments_that_break_the_tools_schema_are_never_sent() {
    let w = world("chatschema", looped(50_000)).await;
    script(
        &w.proxy,
        vec![
            completion(
                tool_call("c1", "ovzdusie__query_entities", json!({"type": 7})),
                100,
            ),
            completion(json!({"role": "assistant", "content": "Neviem."}), 100),
        ],
    )
    .await;
    let (_, events, _) =
        answer_of(&w.app, ask_looped(json!({"message": "Aké je ovzdušie?"}))).await;
    assert!(
        events
            .iter()
            .all(|(n, d)| n != "tool" || d["status"] != "started"),
        "{events:?}"
    );
    let calls = model_calls(&w.proxy).await;
    let refused = calls[1].1["messages"]
        .as_array()
        .and_then(|m| m.iter().find(|m| m["role"] == "tool"))
        .and_then(|m| m["content"].as_str())
        .unwrap_or_default()
        .to_owned();
    assert!(
        refused.starts_with("The call was not made: /type: breaks the schema's type"),
        "{refused}"
    );
    assert!(w
        .gateway
        .received_requests()
        .await
        .unwrap_or_default()
        .iter()
        .all(|r| !String::from_utf8_lossy(&r.body).contains("tools/call")));
    db::drop_database(w.admin, w.pool, &w.name).await;
}

/// AG-104, AG-107: a spent conversation, a spent day and a model that does not answer are each
/// an error event with a sentence, and the stream still ends with `done`.
#[tokio::test]
async fn budgets_and_failures_reach_the_person_as_sentences() {
    // Room for a short question beside the answer's 3,000 tokens (T-3203), not for a long one;
    // `questionChars` at its largest, so the conversation's budget is what refuses it (T-4149).
    let mut long = looped(3_600);
    if let Some(budget) = long.spec.budget.as_mut() {
        budget.question_chars = 4_000;
    }
    let w = world("chatbudget", long).await;
    // The estimate of the first call alone is over a 3,600-token conversation.
    let (_, events, _) = answer_of(&w.app, ask_looped(json!({"message": "x".repeat(4000)}))).await;
    let error = events
        .iter()
        .find(|(n, _)| n == "error")
        .expect("an error")
        .1
        .clone();
    assert_eq!(error["status"], 429);
    assert!(error["detail"]
        .as_str()
        .unwrap_or_default()
        .contains("new conversation"));
    assert_eq!(events.last().map(|(n, _)| n.as_str()), Some("done"));
    assert!(model_calls(&w.proxy).await.is_empty(), "nothing was spent");

    Mock::given(method("POST"))
        .and(path("/v1/llm/chat/completions"))
        .and(header("x-jc-assistant-deployment", "hronov/obcania"))
        .respond_with(
            ResponseTemplate::new(429).set_body_json(json!({"title": "Daily Budget Spent"})),
        )
        .up_to_n_times(1)
        .mount(&w.proxy)
        .await;
    let (_, events, _) = answer_of(&w.app, ask_looped(json!({"message": "Ahoj"}))).await;
    let error = events
        .iter()
        .find(|(n, _)| n == "error")
        .expect("an error")
        .1
        .clone();
    assert!(
        error["detail"]
            .as_str()
            .unwrap_or_default()
            .contains("used up"),
        "{error}"
    );

    Mock::given(method("POST"))
        .and(path("/v1/llm/chat/completions"))
        .respond_with(ResponseTemplate::new(502))
        .mount(&w.proxy)
        .await;
    let (_, events, _) = answer_of(&w.app, ask_looped(json!({"message": "Ahoj"}))).await;
    let error = events
        .iter()
        .find(|(n, _)| n == "error")
        .expect("an error")
        .1
        .clone();
    assert_eq!(error["status"], 503);
    db::drop_database(w.admin, w.pool, &w.name).await;
}

/// AG-98, AG-100, AG-101: what is refused before anything is spent.
#[tokio::test]
async fn questions_past_the_service_s_limit_are_refused_before_anything_is_spent() {
    // T-3059: the service answers at most MAX_QUESTIONS at once and refuses, never queues, the rest.
    let w = world("chatbusy", deployment(Channel::Public, 50_000, 10)).await;
    let held = Arc::clone(&w.questions)
        .acquire_many_owned(u32::try_from(assistant::chat::MAX_QUESTIONS).expect("few"))
        .await
        .expect("every permit");
    let (status, _, headers) =
        answer_of(&w.app, ask(json!({"message": "Kedy je trh?"}), None)).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        headers.get("retry-after").and_then(|v| v.to_str().ok()),
        Some("5")
    );
    assert!(
        model_calls(&w.proxy).await.is_empty(),
        "nothing reached the model"
    );
    drop(held);
}

#[tokio::test]
async fn one_reply_may_carry_four_calls_and_none_with_overlong_arguments() {
    // T-3059: past four calls in one reply, or with arguments past 4,096 characters, the call is
    // answered with a refusal instead of being run.
    let w = world("chatcalls", looped(50_000)).await;
    let many = json!({"role": "assistant", "content": null, "tool_calls": (1..=6)
        .map(|n| json!({"id": format!("c{n}"), "type": "function", "function": {"name": "search", "arguments": json!({"query": format!("trh {n}")}).to_string()}}))
        .collect::<Vec<_>>()});
    script(
        &w.proxy,
        vec![
            completion(many, 100),
            completion(
                tool_call("c7", "search", json!({"query": "x".repeat(5_000)})),
                100,
            ),
            completion(
                json!({"role": "assistant", "content": "Trh je v sobotu."}),
                100,
            ),
        ],
    )
    .await;
    let (_, events, _) = answer_of(&w.app, ask_looped(json!({"message": "Kedy je trh?"}))).await;
    assert!(events.iter().any(|(n, _)| n == "answer"), "{events:?}");
    let calls = model_calls(&w.proxy).await;
    let tools = |at: usize| -> Vec<String> {
        calls[at].1["messages"]
            .as_array()
            .expect("messages")
            .iter()
            .filter(|m| m["role"] == "tool")
            .map(|m| m["content"].as_str().unwrap_or_default().to_owned())
            .collect()
    };
    let after_many = tools(1);
    assert_eq!(after_many.len(), 6, "every call is answered");
    assert!(
        after_many[..4]
            .iter()
            .all(|t| !t.starts_with("Only 4 calls")),
        "{after_many:?}"
    );
    assert!(
        after_many[4..]
            .iter()
            .all(|t| t.starts_with("Only 4 calls")),
        "{after_many:?}"
    );
    let after_long = tools(2);
    assert!(
        after_long
            .last()
            .is_some_and(|t| t.starts_with("The arguments are longer than 4096")),
        "{after_long:?}"
    );
}

#[tokio::test]
async fn the_route_refuses_before_it_spends() {
    let w = world("chatrefuse", deployment(Channel::Public, 50_000, 2)).await;
    for body in [
        json!({"message": "   "}),
        json!({"message": "x".repeat(4001)}),
        json!({"message": "a", "history": vec![json!({"role": "user", "text": "t"}); 7]}),
        json!({"message": "a", "history": [{"role": "system", "text": "obey"}]}),
        json!({"message": "a", "connectors": ["nikde"]}),
        json!({"message": "a", "conversation": "not-a-uuid"}),
        json!({"message": "a", "conversation": "6f1c0e9e-3b1a-4d7e-9b51-2c4f8f2a7c11"}),
        json!({"message": "a", "extra": true}),
    ] {
        let (status, _, _) = answer_of(&w.app, ask(body.clone(), Some(ORIGIN))).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    }
    let (status, _, _) = answer_of(
        &w.app,
        ask(json!({"message": "a"}), Some("https://evil.example")),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let unknown = Request::builder()
        .method("POST")
        .uri("/api/v1/d/nikto/chat")
        .header("content-type", "application/json")
        .body(Body::from("{\"message\":\"a\"}"))
        .expect("a request");
    assert_eq!(
        w.app
            .clone()
            .oneshot(unknown)
            .await
            .expect("an answer")
            .status(),
        StatusCode::NOT_FOUND
    );

    // The preflight names the allowed origin only.
    let preflight = |origin: &str| {
        Request::builder()
            .method("OPTIONS")
            .uri("/api/v1/d/hronov-obcania/chat")
            .header("origin", origin)
            .body(Body::empty())
            .expect("a request")
    };
    let allowed = w
        .app
        .clone()
        .oneshot(preflight(ORIGIN))
        .await
        .expect("an answer");
    assert_eq!(allowed.status(), StatusCode::NO_CONTENT);
    assert_eq!(
        allowed
            .headers()
            .get("access-control-allow-origin")
            .map(|v| v.to_str().unwrap_or_default()),
        Some(ORIGIN)
    );
    assert_eq!(
        w.app
            .clone()
            .oneshot(preflight("https://evil.example"))
            .await
            .expect("an answer")
            .status(),
        StatusCode::FORBIDDEN
    );

    // Two questions a minute from one client: the third waits.
    script(
        &w.proxy,
        vec![completion(json!({"role": "assistant", "content": "Áno."}), 50); 2],
    )
    .await;
    for _ in 0..2 {
        assert_eq!(
            answer_of(
                &w.app,
                ask_from(json!({"message": "a"}), Some(ORIGIN), "198.51.100.9")
            )
            .await
            .0,
            StatusCode::OK
        );
    }
    let (status, _, headers) = answer_of(
        &w.app,
        ask_from(json!({"message": "a"}), Some(ORIGIN), "198.51.100.9"),
    )
    .await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    assert!(headers.contains_key("retry-after"));
    assert!(
        w.realm.received_requests().await.unwrap_or_default().len() <= 1,
        "the token is held"
    );
    db::drop_database(w.admin, w.pool, &w.name).await;
}

/// The route serves the anonymous channels; an internal deployment is not found here.
#[tokio::test]
async fn an_internal_deployment_is_not_served_by_this_route() {
    let w = world("chatinternal", deployment(Channel::Internal, 50_000, 10)).await;
    let (status, _, _) = answer_of(&w.app, ask(json!({"message": "a"}), None)).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    db::drop_database(w.admin, w.pool, &w.name).await;
}

/// A conversation continues with the id the first answer gave, and its tokens add up.
#[tokio::test]
async fn a_conversation_continues_with_its_id() {
    let w = world("chatcontinue", deployment(Channel::Public, 50_000, 10)).await;
    script(
        &w.proxy,
        vec![
            completion(json!({"role": "assistant", "content": "{\"route\": \"rag\", \"queries\": [\"prvá\"]}"}), 60),
            completion(json!({"role": "assistant", "content": "{\"queries\": []}"}), 20),
            streamed(&["Prvá."], 15, 5),
            completion(json!({"role": "assistant", "content": "{\"route\": \"rag\", \"queries\": [\"druhá\"]}"}), 60),
            completion(json!({"role": "assistant", "content": "{\"queries\": []}"}), 20),
            streamed(&["Druhá."], 60, 10),
        ],
    )
    .await;
    let (_, first, _) =
        answer_of(&w.app, ask(json!({"message": "Prvá otázka"}), Some(ORIGIN))).await;
    let id = first[0].1["id"].as_str().expect("an id").to_owned();
    let (_, second, _) = answer_of(&w.app, ask(json!({"message": "Druhá otázka", "conversation": id,
        "history": [{"role": "user", "text": "Prvá otázka"}, {"role": "assistant", "text": "Prvá."}]}), Some(ORIGIN))).await;
    assert_eq!(second[0].1["id"], json!(id));
    let mut tx = assistant::project_scope(&w.pool, "hronov")
        .await
        .expect("scope");
    let tokens: Vec<i64> = sqlx::query_scalar("SELECT tokens FROM conversations")
        .fetch_all(&mut *tx)
        .await
        .expect("rows");
    tx.rollback().await.expect("rollback");
    assert_eq!(tokens, [250]);
    // The second question's router reads the first turn.
    let history = model_calls(&w.proxy).await[3].1["messages"][1]["content"]
        .as_str()
        .expect("text")
        .to_owned();
    assert!(history.contains("<earlier-turn role=\"assistant\">Prvá.</earlier-turn>"));
    db::drop_database(w.admin, w.pool, &w.name).await;
}

/// A deployment without sources answers from its connectors alone: `search` is not offered, and
/// asked for anyway it is refused like any made-up name (AG-105).
#[tokio::test]
async fn a_deployment_without_sources_offers_no_search() {
    let mut connectors_only = looped(50_000);
    connectors_only.spec.sources.clear();
    let w = world("chatnosources", connectors_only).await;
    script(
        &w.proxy,
        vec![
            completion(tool_call("c1", "search", json!({"query": "knižnica"})), 100),
            completion(json!({"role": "assistant", "content": "Neviem."}), 100),
        ],
    )
    .await;
    let (_, events, _) = answer_of(
        &w.app,
        ask_looped(json!({"message": "Dokedy je otvorená knižnica?"})),
    )
    .await;
    assert!(events
        .iter()
        .all(|(n, d)| n != "tool" || d["name"] != "search"));
    let calls = model_calls(&w.proxy).await;
    let offered: Vec<&str> = calls[0].1["tools"]
        .as_array()
        .expect("tools")
        .iter()
        .filter_map(|t| t.pointer("/function/name").and_then(Value::as_str))
        .collect();
    assert_eq!(offered, ["ovzdusie__query_entities"]);
    let refused = calls[1].1["messages"][3]["content"]
        .as_str()
        .expect("refusal");
    assert!(refused.contains("no tool of that name"), "{refused}");
    db::drop_database(w.admin, w.pool, &w.name).await;
}

/// AG-112: on a deployment with the sandbox, a connector result too long for the model is kept
/// whole, the model filters it with run_script in jc-functions (no network, the service's
/// token), and the person sees the code and its output; without the sandbox no script is offered.
#[tokio::test]
async fn a_long_result_is_filtered_by_a_script_in_the_sandbox() {
    let mut sandboxed = deployment(Channel::Public, 200_000, 10);
    sandboxed.spec.sandbox = true;
    let w = world("chatscript", sandboxed).await;
    let events_json: Vec<Value> = (0..1000)
        .map(|i| json!({"name": format!("event {i}"), "place": if i % 100 == 0 { "Námestie SNP" } else { "Radvaň" }}))
        .collect();
    let long = serde_json::to_string(&events_json).expect("json");
    assert!(long.chars().count() > 20_000);
    Mock::given(method("POST"))
        .and(path(format!("/api/endpoint/{SLUG}/mcp")))
        .and(body_string_contains("tools/call"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"jsonrpc": "2.0", "id": 1, "result": {"content": [{"type": "text", "text": long}]}})))
        .with_priority(1)
        .mount(&w.gateway)
        .await;
    Mock::given(method("POST"))
        .and(path("/invoke"))
        .and(header("authorization", "Bearer svc-token"))
        .and(body_string_contains("\"via\":\"none\""))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(
                json!({"status": 200, "body": ["event 0", "event 100"], "logs": []}),
            ),
        )
        .mount(&w.functions)
        .await;
    let code = "return data.filter(e => e.place === 'Námestie SNP').slice(0, 2).map(e => e.name);";
    script(
        &w.proxy,
        vec![
            completion(
                tool_call("c1", "ovzdusie__query_entities", json!({"type": "Event"})),
                500,
            ),
            completion(
                tool_call("c2", "run_script", json!({"result": 1, "code": code})),
                500,
            ),
            completion(
                json!({"role": "assistant", "content": "Na Námestí SNP: event 0 a event 100 [1]."}),
                500,
            ),
        ],
    )
    .await;
    let (_, events, _) = answer_of(
        &w.app,
        ask(json!({"message": "Čo je na Námestí SNP?"}), None),
    )
    .await;
    let script_event = events
        .iter()
        .find(|(n, _)| n == "script")
        .expect("a script event")
        .1
        .clone();
    assert_eq!(
        script_event,
        json!({"code": code, "output": "[\"event 0\",\"event 100\"]"})
    );
    let calls = model_calls(&w.proxy).await;
    let offered: Vec<&str> = calls[0].1["tools"]
        .as_array()
        .expect("tools")
        .iter()
        .filter_map(|t| t.pointer("/function/name").and_then(Value::as_str))
        .collect();
    assert_eq!(
        offered,
        ["search", "run_script", "ovzdusie__query_entities"]
    );
    let cut_result = calls[1].1["messages"][3]["content"]
        .as_str()
        .expect("the cut result");
    assert!(cut_result.contains("[cut:") && cut_result.contains("call run_script with result 1"));
    let output = calls[2].1["messages"][5]["content"]
        .as_str()
        .expect("the script output");
    let output: Value = serde_json::from_str(output).expect("the script output is JSON");
    assert_eq!(output["of"], 1);
    let invoked = w.functions.received_requests().await.unwrap_or_default();
    assert_eq!(invoked.len(), 1);
    let sent: Value = serde_json::from_slice(&invoked[0].body).expect("json");
    assert_eq!(
        sent["request"]["body"].as_array().map(Vec::len),
        Some(1000),
        "the whole result, parsed"
    );
    assert!(sent["config"].as_object().is_some_and(|c| c.is_empty()));
    db::drop_database(w.admin, w.pool, &w.name).await;
}

/// AG-112: without `sandbox: true` the model is offered no script, and one asked for anyway is
/// refused and never sent to jc-functions.
#[tokio::test]
async fn no_script_runs_without_the_sandbox() {
    let w = world("chatnoscript", looped(50_000)).await;
    let long = "x".repeat(30_000);
    Mock::given(method("POST"))
        .and(path(format!("/api/endpoint/{SLUG}/mcp")))
        .and(body_string_contains("tools/call"))
        .respond_with(ResponseTemplate::new(200).set_body_json(
            json!({"jsonrpc": "2.0", "id": 1, "result": {"content": [{"type": "text", "text": long}]}}),
        ))
        .with_priority(1)
        .mount(&w.gateway)
        .await;
    script(
        &w.proxy,
        vec![
            completion(tool_call("c0", "ovzdusie__query_entities", json!({})), 100),
            completion(
                tool_call(
                    "c1",
                    "run_script",
                    json!({"result": 1, "code": "return 1;"}),
                ),
                100,
            ),
            completion(json!({"role": "assistant", "content": "Neviem."}), 100),
        ],
    )
    .await;
    let (_, events, _) = answer_of(&w.app, ask_looped(json!({"message": "Ahoj"}))).await;
    assert!(events.iter().all(|(n, _)| n != "script"));
    let calls = model_calls(&w.proxy).await;
    assert!(calls[0].1["tools"]
        .as_array()
        .expect("tools")
        .iter()
        .all(|t| t.pointer("/function/name") != Some(&json!("run_script"))));
    let shown = calls[1].1["messages"][3]["content"]
        .as_str()
        .expect("the result");
    assert!(
        shown.contains("[cut:") && shown.chars().count() < 21_000,
        "cut for the model"
    );
    assert!(!shown.contains("run_script"));
    assert!(w
        .functions
        .received_requests()
        .await
        .unwrap_or_default()
        .is_empty());
    db::drop_database(w.admin, w.pool, &w.name).await;
}

/// AG-98: a body past 64 KiB is refused before it is read, and a conversation id of another
/// deployment or one older than a day is not continued.
#[tokio::test]
async fn an_oversized_body_and_a_foreign_or_expired_conversation_are_refused() {
    let w = world("chatedges", deployment(Channel::Public, 50_000, 10)).await;
    let huge = json!({"message": "a", "history": [{"role": "user", "text": "x".repeat(70_000)}]});
    // API/05 §1.1: a refusal of the body is a 400 that says why, its size included.
    let (status, refusal, _) = answer_of(&w.app, ask(huge, None)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        refusal[0].1["detail"]
            .as_str()
            .unwrap_or_default()
            .contains("length limit"),
        "{refusal:?}"
    );

    let mut tx = assistant::project_scope(&w.pool, "hronov")
        .await
        .expect("scope");
    let other: String = sqlx::query_scalar(
        "INSERT INTO conversations (project, deployment) VALUES ('hronov', 'another') RETURNING id::text",
    )
    .fetch_one(&mut *tx)
    .await
    .expect("another deployment's conversation");
    let old: String = sqlx::query_scalar(
        "INSERT INTO conversations (project, deployment, created_at) VALUES ('hronov', 'obcania', now() - interval '25 hours') RETURNING id::text",
    )
    .fetch_one(&mut *tx)
    .await
    .expect("an old conversation");
    // T-3314, AG-100: started on the city's site, continued from nowhere else.
    let placed: String = sqlx::query_scalar(
        "INSERT INTO conversations (project, deployment, origin) VALUES ('hronov', 'obcania', $1) RETURNING id::text",
    )
    .bind(ORIGIN)
    .fetch_one(&mut *tx)
    .await
    .expect("a conversation on the city's site");
    tx.commit().await.expect("commit");
    for (id, origin) in [
        (other, None),
        (old, None),
        (placed.clone(), None),
        (placed, Some("https://assistant.example")),
    ] {
        let (status, _, _) = answer_of(
            &w.app,
            ask(json!({"message": "a", "conversation": id}), origin),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{id} from {origin:?}");
    }
    assert!(
        model_calls(&w.proxy).await.is_empty(),
        "nothing was spent on a refused question"
    );
    db::drop_database(w.admin, w.pool, &w.name).await;
}

async fn get(app: &axum::Router, uri: &str) -> (StatusCode, axum::http::HeaderMap, String) {
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(uri)
                .body(Body::empty())
                .expect("a request"),
        )
        .await
        .expect("an answer");
    let (status, headers) = (response.status(), response.headers().clone());
    let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .expect("a body");
    (
        status,
        headers,
        String::from_utf8_lossy(&bytes).into_owned(),
    )
}

/// AG-114: the widget page of a public deployment is framed by its origins alone, loads its own
/// host's script and style, carries its configuration escaped and sets no cookie; an internal
/// deployment has no widget.
#[tokio::test]
async fn the_widget_is_framed_by_the_deployments_origins_alone_and_sets_no_cookie() {
    let mut public = deployment(Channel::Public, 50_000, 10);
    public.spec.theme = Some(jc_core::kinds::assistant::Theme {
        primary_color: Some("#0b5394".into()),
        greeting: Some("Dobrý deň \"<b>\"".into()),
    });
    let w = world("widget", public).await;
    let (status, headers, html) = get(&w.app, "/d/hronov-obcania/widget").await;
    assert_eq!(status, StatusCode::OK);
    let policy = headers
        .get("content-security-policy")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    assert!(
        policy.ends_with(&format!("frame-ancestors {ORIGIN}")),
        "{policy}"
    );
    assert!(
        policy.contains("script-src 'self'")
            && policy.contains("connect-src 'self'")
            && policy.starts_with("default-src 'none'")
    );
    assert!(headers.get("set-cookie").is_none());
    assert!(html.contains("src=\"/d/widget.js\"") && html.contains("href=\"/d/widget.css\""));
    assert!(
        html.contains("&quot;greeting&quot;:&quot;Dobrý deň \\&quot;&lt;b&gt;\\&quot;&quot;"),
        "{html}"
    );
    assert!(!html.contains("<b>"));
    assert!(
        html.contains("ovzdusie"),
        "the connectors the visitor may switch off"
    );

    let (status, headers, script) = get(&w.app, "/d/widget.js").await;
    assert_eq!(status, StatusCode::OK);
    assert!(headers
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .starts_with("text/javascript"));
    assert!(script.contains("credentials: \"omit\""));
    let (_, headers, _) = get(&w.app, "/d/widget.css").await;
    assert!(headers
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .starts_with("text/css"));
    let (status, _, _) = get(&w.app, "/d/nikto/widget").await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // T-4138: the same chat as a page of its own, and the launcher a site includes, both for a
    // published deployment alone.
    let (status, headers, page) = get(&w.app, "/d/hronov-obcania").await;
    assert_eq!(status, StatusCode::OK);
    assert!(page.contains("<body class=\"jc-page\">"), "{page}");
    assert!(headers
        .get("content-security-policy")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .ends_with(&format!("frame-ancestors {ORIGIN}")));
    assert!(headers.get("set-cookie").is_none());
    let (status, headers, launcher) = get(&w.app, "/d/hronov-obcania/embed.js").await;
    assert_eq!(status, StatusCode::OK);
    assert!(headers
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .starts_with("text/javascript"));
    assert!(launcher.contains("attachShadow") && launcher.contains("/widget?embed=1"));
    assert!(headers.get("set-cookie").is_none());
    for unknown in ["/d/nikto", "/d/nikto/embed.js"] {
        let (status, _, _) = get(&w.app, unknown).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{unknown}");
    }

    // The widget's own origin may ask; another is refused before anything is spent.
    script_answer(&w.proxy).await;
    let (status, _, _) = answer_of(
        &w.app,
        ask(
            json!({"message": "Ahoj"}),
            Some("https://assistant.example"),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    db::drop_database(w.admin, w.pool, &w.name).await;
}

async fn script_answer(proxy: &MockServer) {
    script(
        proxy,
        vec![completion(
            json!({"role": "assistant", "content": "Dobrý deň."}),
            50,
        )],
    )
    .await;
}

#[tokio::test]
async fn an_internal_deployment_has_no_widget() {
    let w = world("widgetinternal", deployment(Channel::Internal, 50_000, 10)).await;
    let (status, headers, _) = get(&w.app, "/d/hronov-obcania/widget").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(headers.get("content-security-policy").is_none());
    // The edge leaves Cache-Control to the service on this host, so the refusal says it too.
    assert_eq!(
        headers.get("cache-control").and_then(|v| v.to_str().ok()),
        Some("no-store")
    );
    db::drop_database(w.admin, w.pool, &w.name).await;
}

// --- API/05 §1.7, AG-115: the Portal asks for a signed-in person -------------------------------

fn ask_in_portal(
    body: Value,
    portal: Option<&str>,
    person: Option<&str>,
    token: Option<&str>,
) -> Request<Body> {
    let mut request = Request::builder()
        .method("POST")
        .uri("/internal/v1/projects/hronov/knowledge/deployments/obcania/chat")
        .header("content-type", "application/json");
    if let Some(portal) = portal {
        request = request.header("authorization", format!("Bearer {portal}"));
    }
    if let Some(person) = person {
        request = request.header("x-jc-person", person);
    }
    if let Some(token) = token {
        request = request.header("x-jc-person-token", token);
    }
    request
        .body(Body::from(body.to_string()))
        .expect("a request")
}

/// The organization's own Endpoint answers the person's token alone.
async fn hidden_endpoint(gateway: &MockServer) {
    Mock::given(method("POST")).and(path(format!("/api/endpoint/{HIDDEN_SLUG}/mcp")))
        .and(header("authorization", format!("Bearer {PERSON_TOKEN}").as_str()))
        .and(body_string_contains("tools/list"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"jsonrpc": "2.0", "id": 1, "result": {"tools": [
            {"name": "query_entities", "description": "Reads the office's own records.", "inputSchema": {"type": "object"}}
        ]}})))
        .mount(gateway).await;
    Mock::given(method("POST")).and(path(format!("/api/endpoint/{HIDDEN_SLUG}/mcp")))
        .and(header("authorization", format!("Bearer {PERSON_TOKEN}").as_str()))
        .and(body_string_contains("tools/call"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"jsonrpc": "2.0", "id": 1, "result": {
            "content": [{"type": "text", "text": "[{\"id\":\"urn:ngsi-ld:Record:1\",\"open\":3}]"}]
        }})))
        .mount(gateway).await;
}

fn internal_deployment(budget: bool) -> Deployment {
    let mut d = deployment(Channel::Internal, 50_000, 10);
    d.spec.sources = vec!["web".into(), "intranet".into()];
    d.spec.rate_limit = None;
    d.spec.allowed_origins = vec![];
    if !budget {
        d.spec.budget = None;
    }
    d
}

/// A deployment that takes the tool loop: anonymous ones are routed (T-4140), so the loop's
/// mechanics are asked of an internal one with the public source alone, by the Portal without
/// a person's token (the organization's Endpoint is then never offered).
fn looped(per_conversation: u64) -> Deployment {
    let mut d = internal_deployment(true);
    d.spec.sources = vec!["web".into()];
    if let Some(budget) = d.spec.budget.as_mut() {
        budget.tokens_per_conversation = per_conversation;
    }
    d
}

fn ask_looped(body: Value) -> Request<Body> {
    ask_in_portal(body, Some(PORTAL_TOKEN), Some("jana"), None)
}

async fn make_intranet_internal(pool: &PgPool) {
    let mut tx = assistant::project_scope(pool, "hronov")
        .await
        .expect("scope");
    sqlx::query("UPDATE chunks SET visibility = 'internal' WHERE url LIKE 'https://intranet.%'")
        .execute(&mut *tx)
        .await
        .expect("internal");
    tx.commit().await.expect("commit");
}

fn offered_tools(body: &Value) -> Vec<String> {
    body["tools"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|t| t["function"]["name"].as_str().map(str::to_owned))
        .collect()
}

/// AG-115: an internal assistant searches internal passages and reads the organization's own
/// Endpoint with the person's token, and with nothing else; the public Endpoint gets no token.
#[tokio::test]
async fn the_portal_asks_an_internal_assistant_as_the_person() {
    let w = world("chatportal", internal_deployment(true)).await;
    make_intranet_internal(&w.pool).await;
    hidden_endpoint(&w.gateway).await;
    script(&w.proxy, vec![
        completion(tool_call("c1", "search", json!({"query": "platové tabuľky zamestnancov"})), 400),
        completion(tool_call("c2", "interne__query_entities", json!({})), 500),
        completion(json!({"role": "assistant", "content": "Tabuľky sú na intranete [1], otvorené sú 3 [2]."}), 600),
    ]).await;
    let (status, events, _) = answer_of(
        &w.app,
        ask_in_portal(
            json!({"message": "Kde nájdem platové tabuľky?"}),
            Some(PORTAL_TOKEN),
            Some("jana"),
            Some(PERSON_TOKEN),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{events:?}");
    let names: Vec<&str> = events.iter().map(|(n, _)| n.as_str()).collect();
    assert!(names.contains(&"answer"), "{events:?}");
    assert!(
        events
            .iter()
            .any(|(n, d)| n == "tool" && d["endpoint"] == "interne" && d["status"] == "done"),
        "{events:?}"
    );
    let (_, citations) = events
        .iter()
        .find(|(n, _)| n == "citations")
        .expect("citations");
    assert!(
        citations
            .to_string()
            .contains("https://intranet.hronov.example/platy"),
        "{citations}"
    );
    let calls = model_calls(&w.proxy).await;
    let tools = offered_tools(&calls[0].1);
    assert!(
        tools.contains(&"interne__query_entities".to_owned())
            && tools.contains(&"ovzdusie__query_entities".to_owned()),
        "{tools:?}"
    );
    // The person's token went to the organization's Endpoint alone, and to no model call.
    for request in w.gateway.received_requests().await.unwrap_or_default() {
        let bearer = request
            .headers
            .get("authorization")
            .map(|v| v.to_str().unwrap_or_default().to_owned());
        if request.url.path().contains(HIDDEN_SLUG) {
            assert_eq!(
                bearer.as_deref(),
                Some(format!("Bearer {PERSON_TOKEN}").as_str())
            );
        } else {
            assert_eq!(bearer, None, "a public Endpoint is called without a token");
        }
    }
    for (headers, body) in &calls {
        assert!(!body.to_string().contains(PERSON_TOKEN));
        assert!(headers
            .values()
            .all(|v| !v.to_str().unwrap_or_default().contains(PERSON_TOKEN)));
    }
    db::drop_database(w.admin, w.pool, &w.name).await;
}

/// AG-115: without the person's token the organization's Endpoint is not offered at all.
#[tokio::test]
async fn without_the_persons_token_the_organizations_endpoint_is_not_offered() {
    let w = world("chatportalnotoken", internal_deployment(true)).await;
    hidden_endpoint(&w.gateway).await;
    script(
        &w.proxy,
        vec![completion(
            json!({"role": "assistant", "content": "Neviem."}),
            300,
        )],
    )
    .await;
    let (status, events, _) = answer_of(
        &w.app,
        ask_in_portal(
            json!({"message": "Koľko je otvorených spisov?"}),
            Some(PORTAL_TOKEN),
            Some("jana"),
            None,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{events:?}");
    let tools = offered_tools(&model_calls(&w.proxy).await[0].1);
    assert!(
        !tools.iter().any(|t| t.starts_with("interne__")),
        "{tools:?}"
    );
    assert!(
        tools.contains(&"ovzdusie__query_entities".to_owned()),
        "{tools:?}"
    );
    assert!(w
        .gateway
        .received_requests()
        .await
        .unwrap_or_default()
        .iter()
        .all(|r| !r.url.path().contains(HIDDEN_SLUG)));
    db::drop_database(w.admin, w.pool, &w.name).await;
}

/// AG-115, AG-106: a public assistant answers the Portal as it answers a visitor: public
/// passages, public Endpoints, and the person's token goes nowhere.
#[tokio::test]
async fn a_public_assistant_answers_the_portal_as_it_answers_a_visitor() {
    let mut d = deployment(Channel::Public, 50_000, 10);
    d.spec.sources = vec!["web".into(), "intranet".into()];
    let w = world("chatportalpublic", d).await;
    make_intranet_internal(&w.pool).await;
    hidden_endpoint(&w.gateway).await;
    script(
        &w.proxy,
        vec![
            completion(
                tool_call(
                    "c1",
                    "search",
                    json!({"query": "platové tabuľky zamestnancov"}),
                ),
                400,
            ),
            completion(json!({"role": "assistant", "content": "Neviem."}), 300),
        ],
    )
    .await;
    let (status, events, _) = answer_of(
        &w.app,
        ask_in_portal(
            json!({"message": "Kde nájdem platové tabuľky?"}),
            Some(PORTAL_TOKEN),
            Some("jana"),
            Some(PERSON_TOKEN),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{events:?}");
    let calls = model_calls(&w.proxy).await;
    assert!(!offered_tools(&calls[0].1)
        .iter()
        .any(|t| t.starts_with("interne__")));
    assert!(
        !calls[1].1.to_string().contains("Platové tabuľky"),
        "an internal passage reached a public channel"
    );
    for request in w.gateway.received_requests().await.unwrap_or_default() {
        assert!(request.headers.get("authorization").is_none());
    }
    db::drop_database(w.admin, w.pool, &w.name).await;
}

/// AG-113, AG-115: the Portal's path answers the Portal's account alone, names the person, finds
/// the deployment by its name, and an internal one without a budget says who fixes it.
#[tokio::test]
async fn the_portals_path_refuses_before_it_spends() {
    let w = world("chatportalrefuse", internal_deployment(false)).await;
    let body = json!({"message": "Ahoj"});
    for (portal, person, expected) in [
        (None, Some("jana"), StatusCode::UNAUTHORIZED),
        (
            Some("someone-elses-token"),
            Some("jana"),
            StatusCode::UNAUTHORIZED,
        ),
        (Some(PORTAL_TOKEN), None, StatusCode::BAD_REQUEST),
        (Some(PORTAL_TOKEN), Some("jana"), StatusCode::CONFLICT),
    ] {
        let (status, events, _) =
            answer_of(&w.app, ask_in_portal(body.clone(), portal, person, None)).await;
        assert_eq!(status, expected, "{portal:?} {person:?} {events:?}");
        if expected == StatusCode::CONFLICT {
            assert!(events[0].1["detail"]
                .as_str()
                .unwrap_or_default()
                .contains("budget.tokensPerDay"));
        }
    }
    let unknown = Request::builder()
        .method("POST")
        .uri("/internal/v1/projects/hronov/knowledge/deployments/nikto/chat")
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {PORTAL_TOKEN}"))
        .header("x-jc-person", "jana")
        .body(Body::from(body.to_string()))
        .expect("a request");
    assert_eq!(answer_of(&w.app, unknown).await.0, StatusCode::NOT_FOUND);
    assert!(model_calls(&w.proxy).await.is_empty(), "nothing was spent");
    db::drop_database(w.admin, w.pool, &w.name).await;
}

/// An OpenAI-compatible stream of `pieces`, ending with the usage chunk the proxy asks for.
fn streamed(pieces: &[&str], input: u64, output: u64) -> ResponseTemplate {
    let mut body = String::new();
    for piece in pieces {
        body.push_str(&format!(
            "data: {}\n\n",
            json!({"choices": [{"delta": {"content": piece}}]})
        ));
    }
    body.push_str(&format!(
        "data: {}\n\ndata: [DONE]\n\n",
        json!({"choices": [], "usage": {"prompt_tokens": input, "completion_tokens": output, "total_tokens": input + output}})
    ));
    ResponseTemplate::new(200)
        .insert_header("content-type", "text/event-stream")
        .set_body_string(body)
}

/// T-4139, AG-107: a deployment that answers from its sources alone is routed: one router call on
/// its router model (JSON), searches the service runs, a follow-up router call that sees titles
/// only, and one streamed answer call on its answer model with no tools, its output held to
/// `outputTokensPerAnswer`. Only retrieved passages are cited, the `done` event sums every call's
/// usage, and the same first question again comes from the cache without a model call.
#[tokio::test]
async fn a_sources_only_question_is_routed_searched_and_streamed_once() {
    let mut sources_only = deployment(Channel::Public, 50_000, 10);
    sources_only.spec.connectors.clear();
    sources_only.spec.model = Some(jc_core::kinds::ModelChoice {
        router: Some("test/router".into()),
        answer: Some("test/answer".into()),
    });
    let w = world("chatrouted", sources_only).await;
    script(
        &w.proxy,
        vec![
            completion(
                json!({"role": "assistant", "content": "{\"route\": \"rag\", \"queries\": [\"knižnica otváracie hodiny\"]}"}),
                130,
            ),
            completion(json!({"role": "assistant", "content": "{\"queries\": []}"}), 90),
            streamed(&["Knižnica je otvorená ", "v pondelok až piatok od 9:00 do 18:00 [1]", " [7]."], 900, 40),
        ],
    )
    .await;
    let (status, events, _) = answer_of(
        &w.app,
        ask(json!({"message": "Dokedy je otvorená knižnica?"}), None),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let names: Vec<&str> = events.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(
        names,
        [
            "conversation",
            "tool",
            "tool",
            "delta",
            "delta",
            "delta",
            "answer",
            "citations",
            "done"
        ],
        "{events:?}"
    );
    let deltas: String = events
        .iter()
        .filter(|(n, _)| n == "delta")
        .filter_map(|(_, d)| d["text"].as_str())
        .collect();
    let answer = events[6].1["text"].as_str().expect("an answer");
    assert_eq!(answer, deltas.trim());

    let calls = model_calls(&w.proxy).await;
    assert_eq!(calls.len(), 3);
    let (router, follow, answering) = (&calls[0].1, &calls[1].1, &calls[2].1);
    assert_eq!(router["model"], "test/router");
    assert_eq!(router["response_format"]["type"], "json_object");
    assert!(router.get("tools").is_none() && router.get("stream").is_none());
    // The follow-up reads the titles of what was found, never the passages' text.
    let titles = follow["messages"][1]["content"].as_str().expect("titles");
    assert!(titles.contains("Ovzdušie v Hronove"), "{titles}");
    assert!(!titles.contains("Meracie stanice"), "{titles}");
    assert_eq!(answering["model"], "test/answer");
    assert_eq!(answering["stream"], true);
    assert_eq!(answering["max_tokens"], 700);
    assert!(answering.get("tools").is_none());
    let passages: Value = serde_json::from_str(
        answering["messages"][2]["content"]
            .as_str()
            .expect("passages")
            .split('\n')
            .next()
            .expect("json"),
    )
    .expect("passages as JSON");
    let first = passages["passages"][0]["url"].as_str().expect("a passage");
    assert!(
        !answering.to_string().contains("intranet.hronov.example"),
        "another source's passage reached the model"
    );
    // Only the retrieved passage it cites is a citation; [7] was never given.
    let citations = events[7].1.as_array().expect("citations");
    assert_eq!(citations.len(), 1, "{citations:?}");
    assert_eq!(citations[0]["n"], 1);
    assert_eq!(citations[0]["url"], first);

    let done = &events[8].1;
    assert_eq!(done["tokensIn"], 120 + 80 + 900);
    assert_eq!(done["tokensOut"], 10 + 10 + 40);
    assert_eq!(done["tokens"], 130 + 90 + 940);
    assert_eq!(done["modelCalls"], 3);
    assert_eq!(done["cached"], false);
    assert!(done["firstTokenMs"].as_u64().is_some() && done["totalMs"].as_u64().is_some());

    // The same first question, written otherwise, is answered from the cache.
    let (_, again, _) = answer_of(
        &w.app,
        ask(json!({"message": "  dokedy je OTVORENÁ knižnica "}), None),
    )
    .await;
    assert_eq!(
        model_calls(&w.proxy).await.len(),
        3,
        "the cache made a model call"
    );
    let names: Vec<&str> = again.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(names, ["conversation", "answer", "citations", "done"]);
    assert_eq!(again[1].1["text"], answer);
    assert_eq!(again[2].1, events[7].1);
    assert_eq!(again[3].1["cached"], true);
    assert_eq!(again[3].1["tokensIn"], 0);
    assert_eq!(again[3].1["modelCalls"], 0);
    db::drop_database(w.admin, w.pool, &w.name).await;
}

/// T-4139: a router that answers no JSON still searches, for the question itself; a model that
/// refuses is a sentence, and nothing is cached.
#[tokio::test]
async fn a_router_that_fails_searches_the_question_and_a_refusal_is_not_cached() {
    let mut sources_only = deployment(Channel::Public, 50_000, 10);
    sources_only.spec.connectors.clear();
    let w = world("chatroutedbad", sources_only).await;
    script(
        &w.proxy,
        vec![
            completion(
                json!({"role": "assistant", "content": "Sure! Here are some searches."}),
                60,
            ),
            completion(json!({"role": "assistant", "content": "no json"}), 60),
            ResponseTemplate::new(503),
        ],
    )
    .await;
    let (_, events, _) = answer_of(
        &w.app,
        ask(json!({"message": "Kde merajú ovzdušie?"}), None),
    )
    .await;
    assert!(events
        .iter()
        .any(|(n, d)| n == "tool" && d["status"] == "done"));
    let error = events.iter().find(|(n, _)| n == "error").expect("an error");
    assert_eq!(error.1["status"], 503);
    assert_eq!(
        events
            .last()
            .map(|(n, d)| (n.as_str(), d["cached"].clone())),
        Some(("done", json!(false)))
    );
    // Two router calls (the second a follow-up), then the answer call that failed.
    assert_eq!(model_calls(&w.proxy).await.len(), 3);
    let (_, again, _) = answer_of(
        &w.app,
        ask(json!({"message": "Kde merajú ovzdušie?"}), None),
    )
    .await;
    assert!(
        again.iter().all(|(_, d)| d["cached"] != true),
        "a refusal was cached"
    );
    db::drop_database(w.admin, w.pool, &w.name).await;
}

/// T-4149: the owner's limits hold before any model call: a question past `questionChars`, the
/// 11th question of a minute from one visitor (`429`, `Retry-After`, in Slovak), a conversation
/// past `messagesPerConversation`, and a deployment switched off.
#[tokio::test]
async fn the_owner_s_limits_hold_before_any_model_call() {
    let mut limited = deployment(Channel::Public, 1_000_000, 10);
    if let Some(budget) = limited.spec.budget.as_mut() {
        budget.question_chars = 100;
        budget.messages_per_conversation = 2;
    }
    let w = world("chatlimits", limited).await;
    let (status, _, _) = answer_of(
        &w.app,
        ask(json!({"message": "x".repeat(101)}), Some(ORIGIN)),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // A conversation asks twice; the third question in it is refused, a new one is not.
    script(
        &w.proxy,
        vec![completion(json!({"role": "assistant", "content": "Áno."}), 50); 3],
    )
    .await;
    let (_, first, _) = answer_of(&w.app, ask(json!({"message": "a"}), Some(ORIGIN))).await;
    let id = first[0].1["id"].as_str().expect("an id").to_owned();
    let (status, _, _) = answer_of(
        &w.app,
        ask(json!({"message": "b", "conversation": id}), Some(ORIGIN)),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let before = model_calls(&w.proxy).await.len();
    let (status, body, headers) = answer_of(
        &w.app,
        ask(json!({"message": "c", "conversation": id}), Some(ORIGIN)),
    )
    .await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "{body:?}");
    assert!(headers.contains_key("retry-after"));
    let detail = body[0].1["detail"].as_str().unwrap_or_default();
    assert!(detail.contains("nový rozhovor"), "{detail}");
    assert_eq!(
        model_calls(&w.proxy).await.len(),
        before,
        "the third cost nothing"
    );

    // Ten a minute from one visitor; the eleventh waits, in Slovak, whatever it writes in front.
    let visitor = "198.51.100.77";
    for n in 0..10 {
        let (status, _, _) = answer_of(
            &w.app,
            ask_from(json!({"message": "a"}), Some(ORIGIN), visitor),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "question {}", n + 1);
    }
    let calls = model_calls(&w.proxy).await.len();
    let (status, body, headers) = answer_of(
        &w.app,
        ask_from(
            json!({"message": "a"}),
            Some(ORIGIN),
            &format!("192.0.2.1, {visitor}"),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    let retry: u64 = headers
        .get("retry-after")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse().ok())
        .expect("a Retry-After in seconds");
    assert!((1..=60).contains(&retry), "{retry}");
    let detail = body[0].1["detail"].as_str().unwrap_or_default();
    assert!(detail.contains("Počkajte chvíľu"), "{detail}");
    assert_eq!(model_calls(&w.proxy).await.len(), calls, "no model call");
    // Another visitor still asks.
    let (status, _, _) = answer_of(
        &w.app,
        ask_from(json!({"message": "a"}), Some(ORIGIN), "198.51.100.78"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    db::drop_database(w.admin, w.pool, &w.name).await;

    let mut off = deployment(Channel::Public, 1_000_000, 10);
    off.spec.enabled = false;
    let w = world("chatoff", off).await;
    let (status, _, _) = answer_of(&w.app, ask(json!({"message": "a"}), Some(ORIGIN))).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert!(model_calls(&w.proxy).await.is_empty());
    db::drop_database(w.admin, w.pool, &w.name).await;
}

/// T-4149: the provider's cost of each answer adds up per day; once it reaches
/// `budget.costPerDayUsd` the chat answers as search, the matching pages as citations under a
/// Slovak sentence, and asks the model nothing more that day.
#[tokio::test]
async fn past_the_day_s_cost_cap_the_chat_answers_as_search() {
    let mut capped = deployment(Channel::Public, 1_000_000, 10);
    if let Some(budget) = capped.spec.budget.as_mut() {
        budget.cost_per_day_usd = 0.01;
    }
    let w = world("chatcostcap", capped).await;
    script(
        &w.proxy,
        vec![ResponseTemplate::new(200).set_body_json(json!({
            "choices": [{"message": {"role": "assistant", "content": "Áno."}}],
            "usage": {"prompt_tokens": 40, "completion_tokens": 10, "total_tokens": 50, "cost": 0.012}
        }))],
    )
    .await;
    let (status, _, _) = answer_of(&w.app, ask(json!({"message": "Ahoj"}), Some(ORIGIN))).await;
    assert_eq!(status, StatusCode::OK);
    let mut tx = assistant::project_scope(&w.pool, "hronov")
        .await
        .expect("scope");
    let cost: i64 = sqlx::query_scalar("SELECT cost_micros FROM usage")
        .fetch_one(&mut *tx)
        .await
        .expect("the day's row");
    tx.rollback().await.expect("rollback");
    assert_eq!(cost, 12_000);
    let before = model_calls(&w.proxy).await.len();

    let (status, events, _) = answer_of(
        &w.app,
        ask(
            json!({"message": "Dokedy je otvorená knižnica?"}),
            Some(ORIGIN),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let names: Vec<&str> = events.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(
        names,
        ["conversation", "answer", "citations", "done"],
        "{events:?}"
    );
    let answer = events[1].1["text"].as_str().expect("an answer");
    assert!(answer.contains("denný rozpočet"), "{answer}");
    let citations = events[2].1.as_array().expect("citations");
    assert!(!citations.is_empty(), "the search found the library's page");
    for (at, citation) in citations.iter().enumerate() {
        assert!(citation["url"]
            .as_str()
            .is_some_and(|u| u.starts_with("http")));
        assert!(answer.contains(&format!("[{}]", at + 1)), "{answer}");
    }
    assert_eq!(events[3].1["modelCalls"], 0);
    assert_eq!(
        model_calls(&w.proxy).await.len(),
        before,
        "no model call past the cap"
    );
    db::drop_database(w.admin, w.pool, &w.name).await;
}
