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

struct World {
    admin: PgPool,
    pool: PgPool,
    name: String,
    realm: MockServer,
    proxy: MockServer,
    gateway: MockServer,
    functions: MockServer,
    app: axum::Router,
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
            }),
            budget: Some(Budget {
                tokens_per_day: 1_000_000,
                tokens_per_conversation: per_conversation,
            }),
            theme: None,
            languages: vec!["sk".into()],
            sandbox: false,
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
                "Meracie stanice kvality ovzdušia sú na Námestí SNP a pri železničnej stanici.",
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
    let state = Arc::new(ChatState {
        pool: pool.clone(),
        embedder,
        model: Model::new(
            ModelConfig {
                proxy: proxy.uri(),
                token_url: format!("{}/token", realm.uri()),
                client_id: "jc-assistant".into(),
                client_secret: "s3cret".into(),
                model: "test/model".into(),
            },
            http.clone(),
        ),
        http,
        gateway: gateway.uri(),
        functions: Some(functions.uri()),
        portal_client: "portal-api".into(),
        snapshot: RwLock::new(Arc::new(snapshot)),
        limits: tokio::sync::Mutex::default(),
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
            .and(path("/v1/llm/v1/chat/completions"))
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

/// AG-102, AG-105, AG-106, AG-108: the model searches, calls the public connector, and answers
/// with both cited; the internal Endpoint and the unlisted tool are never offered.
#[tokio::test]
async fn a_question_is_answered_from_a_passage_and_a_tool_with_both_cited() {
    let w = world("chatok", deployment(Channel::Public, 50_000, 10)).await;
    script(&w.proxy, vec![
        completion(tool_call("c1", "search", json!({"query": "meracie stanice ovzdušia"})), 400),
        completion(tool_call("c2", "ovzdusie__query_entities", json!({"type": "AirQualityObserved"})), 600),
        completion(json!({"role": "assistant", "content": "Stanice sú na Námestí SNP [1], NO2 je 21 [3]."}), 700),
    ]).await;
    let (status, events, headers) = answer_of(
        &w.app,
        ask(
            json!({"message": "Kde sú meracie stanice ovzdušia?"}),
            Some(ORIGIN),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        headers
            .get("x-accel-buffering")
            .map(|v| v.to_str().unwrap_or_default()),
        Some("no")
    );
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
            "tool",
            "tool",
            "answer",
            "citations",
            "done"
        ]
    );
    assert_eq!(events[1].1, json!({"name": "search", "status": "started"}));
    assert_eq!(
        events[4].1,
        json!({"name": "query_entities", "endpoint": "ovzdusie", "status": "done"})
    );
    assert_eq!(
        events[5].1["text"],
        "Stanice sú na Námestí SNP [1], NO2 je 21 [3]."
    );
    let citations = events[6].1.as_array().expect("citations");
    assert_eq!(
        citations.len(),
        2,
        "only the numbers the answer uses: {citations:?}"
    );
    assert_eq!(citations[0]["url"], "https://hronov.example/ovzdusie");
    assert_eq!(
        citations[1],
        json!({"n": 3, "tool": "query_entities", "endpoint": "ovzdusie"})
    );
    assert_eq!(events[7].1, json!({"tokens": 1700}));

    let calls = model_calls(&w.proxy).await;
    assert_eq!(calls.len(), 3);
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
            headers
                .get("x-jc-assistant-tokens-per-day")
                .map(|v| v.to_str().unwrap_or_default()),
            Some("1000000")
        );
        assert_eq!(
            body["messages"][0], calls[0].1["messages"][0],
            "one stable prefix (AG-108)"
        );
        assert_eq!(
            body["messages"][0]["content"][0]["cache_control"]["type"],
            "ephemeral"
        );
    }
    let offered: Vec<&str> = calls[0].1["tools"]
        .as_array()
        .expect("tools")
        .iter()
        .filter_map(|t| t.pointer("/function/name").and_then(Value::as_str))
        .collect();
    assert_eq!(offered, ["search", "ovzdusie__query_entities"]);
    let passages = calls[1].1["messages"][3]["content"]
        .as_str()
        .expect("the search result");
    assert!(passages.contains("<passage n=\"1\" url=\"https://hronov.example/ovzdusie\">"));
    assert!(
        !passages.contains("intranet"),
        "another source is never searched"
    );
    assert!(w
        .gateway
        .received_requests()
        .await
        .unwrap_or_default()
        .iter()
        .all(|r| !r.url.path().contains(HIDDEN_SLUG)));

    // The conversation and the day were counted; no text was kept (AG-99).
    let mut tx = assistant::project_scope(&w.pool, "hronov")
        .await
        .expect("scope");
    let (tokens,): (i64,) = sqlx::query_as("SELECT tokens FROM conversations")
        .fetch_one(&mut *tx)
        .await
        .expect("one");
    let (requests, input, output): (i64, i64, i64) =
        sqlx::query_as("SELECT requests, tokens_in, tokens_out FROM usage")
            .fetch_one(&mut *tx)
            .await
            .expect("usage");
    tx.rollback().await.expect("rollback");
    assert_eq!((tokens, requests, input, output), (1700, 1, 1670, 30));
    db::drop_database(w.admin, w.pool, &w.name).await;
}

/// AG-105, AG-107: a tool the model makes up is refused and never called; asking for the same
/// call twice ends the loop, and the last call offers no tools.
#[tokio::test]
async fn a_made_up_tool_is_never_called_and_a_repeated_call_ends_the_loop() {
    let w = world("chatloop", deployment(Channel::Public, 50_000, 10)).await;
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
        ask(
            json!({"message": "Dokedy je otvorená knižnica?", "connectors": []}),
            None,
        ),
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
    let offered = calls[0].1["tools"].as_array().expect("tools").len();
    assert_eq!(offered, 1, "connectors: [] leaves search alone");
    assert_eq!(
        calls[3].1["tool_choice"], "none",
        "after a repeat the model answers"
    );
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

/// AG-104, AG-107: a spent conversation, a spent day and a model that does not answer are each
/// an error event with a sentence, and the stream still ends with `done`.
#[tokio::test]
async fn budgets_and_failures_reach_the_person_as_sentences() {
    let w = world("chatbudget", deployment(Channel::Public, 2_000, 10)).await;
    // The estimate of the first call alone is over a 2,000-token conversation.
    let (_, events, _) = answer_of(&w.app, ask(json!({"message": "x".repeat(4000)}), None)).await;
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
        .and(path("/v1/llm/v1/chat/completions"))
        .and(header("x-jc-assistant-deployment", "hronov/obcania"))
        .respond_with(
            ResponseTemplate::new(429).set_body_json(json!({"title": "Daily Budget Spent"})),
        )
        .up_to_n_times(1)
        .mount(&w.proxy)
        .await;
    let (_, events, _) = answer_of(&w.app, ask(json!({"message": "Ahoj"}), None)).await;
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
        .and(path("/v1/llm/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(502))
        .mount(&w.proxy)
        .await;
    let (_, events, _) = answer_of(&w.app, ask(json!({"message": "Ahoj"}), None)).await;
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
            completion(json!({"role": "assistant", "content": "Prvá."}), 100),
            completion(json!({"role": "assistant", "content": "Druhá."}), 150),
        ],
    )
    .await;
    let (_, first, _) = answer_of(&w.app, ask(json!({"message": "Prvá otázka"}), None)).await;
    let id = first[0].1["id"].as_str().expect("an id").to_owned();
    let (_, second, _) = answer_of(&w.app, ask(json!({"message": "Druhá otázka", "conversation": id,
        "history": [{"role": "user", "text": "Prvá otázka"}, {"role": "assistant", "text": "Prvá."}]}), None)).await;
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
    let history = model_calls(&w.proxy).await[1].1["messages"][1]["content"]
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
    let mut connectors_only = deployment(Channel::Public, 50_000, 10);
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
        ask(json!({"message": "Dokedy je otvorená knižnica?"}), None),
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
    assert!(output.contains("<script-output of=\"1\">"));
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
    let w = world("chatnoscript", deployment(Channel::Public, 50_000, 10)).await;
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
    let (_, events, _) = answer_of(&w.app, ask(json!({"message": "Ahoj"}), None)).await;
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
