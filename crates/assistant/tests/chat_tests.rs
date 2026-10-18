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
use assistant::chat::{router, ChatState, VISITOR_HEADER};
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
/// The region's public Endpoint, `bbsk/kraj` (T-4177).
const REGION_SLUG: &str = "regionabcdefghijklmnopqrstuvwxy";
/// An Endpoint of a project no deployment's scope names.
const ELSEWHERE_SLUG: &str = "elsewhereabcdefghijklmnopqrstuv";
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
            mcp_server: None,
            allowed_origins: vec![ORIGIN.into()],
            rate_limit: Some(RateLimit {
                requests_per_minute: 100,
                per_client_per_minute: per_client,
                per_client_per_hour: 600,
                per_client_per_day: 6_000,
                per_client_per_month: 6_000,
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
            answer: None,
            feedback: true,
            widget: None,
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
            {"name": "query_entities", "description": "Reads air quality stations.", "inputSchema": {"type": "object", "properties": {"type": {"type": "array", "items": {"type": "string"}}}}},
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
    let public = |slug: &str| EndpointRef {
        slug: slug.into(),
        audience: "public".into(),
    };
    snapshot.endpoints = BTreeMap::from([
        (("bbsk".into(), "kraj".into()), public(REGION_SLUG)),
        (
            ("bbsk".into(), "spisy".into()),
            EndpointRef {
                slug: HIDDEN_SLUG.into(),
                audience: "organization".into(),
            },
        ),
        (("helsinki".into(), "kraj".into()), public(ELSEWHERE_SLUG)),
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
        organization: "hronov".into(),
    });
    World {
        admin,
        pool,
        name,
        realm,
        proxy,
        gateway,
        functions,
        app: with_visitor_tokens(state),
        questions,
    }
}

/// The chat route behind a stand-in widget (T-4653): a question without `X-JC-Visitor` gets one
/// token per client address, earned from the service's own signer at no difficulty, so each
/// address the tests name is one visitor, as it was before tokens.
fn with_visitor_tokens(state: Arc<ChatState>) -> axum::Router {
    let minted = Arc::clone(&state);
    let tokens: Arc<std::sync::Mutex<BTreeMap<String, String>>> = Arc::default();
    router(state).layer(axum::middleware::map_request(
        move |mut request: Request<Body>| {
            let (state, tokens) = (Arc::clone(&minted), Arc::clone(&tokens));
            async move {
                let path = request.uri().path().to_owned();
                let public_id = path
                    .strip_prefix("/api/v1/d/")
                    .and_then(|rest| rest.strip_suffix("/chat"));
                if let Some(public_id) = public_id {
                    if !request.headers().contains_key(VISITOR_HEADER) {
                        let address = request
                            .headers()
                            .get("x-forwarded-for")
                            .and_then(|v| v.to_str().ok())
                            .unwrap_or_default()
                            .to_owned();
                        let key = format!("{public_id}\u{0}{address}");
                        let known = tokens.lock().expect("the tokens").get(&key).cloned();
                        let token = match known {
                            Some(token) => token,
                            None => {
                                let now = unix_now();
                                let limits = state.limits.lock().await;
                                let challenge = limits.signer.challenge(public_id, 0, now);
                                let (token, _) = limits
                                    .signer
                                    .redeem(public_id, &challenge, "0", now)
                                    .expect("a proof of no bits");
                                tokens
                                    .lock()
                                    .expect("the tokens")
                                    .insert(key, token.clone());
                                token
                            }
                        };
                        request
                            .headers_mut()
                            .insert(VISITOR_HEADER, token.parse().expect("a header value"));
                    }
                }
                request
            }
        },
    ))
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_secs())
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

/// The first words of the router's and the judge's rules: the routed loop calls them side by
/// side (T-4293), so their replies are matched by what they are told, not by their order.
const ROUTER: &str = "You prepare the searches";
const JUDGE: &str = "You check whether the passages";

/// The routed loop's script: each router, judge and answer reply once, in order of its kind.
async fn script_routed(
    proxy: &MockServer,
    routers: Vec<ResponseTemplate>,
    judges: Vec<ResponseTemplate>,
    answers: Vec<ResponseTemplate>,
) {
    for (rules, replies, first) in [(ROUTER, routers, 1), (JUDGE, judges, 1)] {
        for (i, reply) in replies.into_iter().enumerate() {
            Mock::given(method("POST"))
                .and(path("/v1/llm/chat/completions"))
                .and(body_string_contains(rules))
                .respond_with(reply)
                .up_to_n_times(1)
                .with_priority(u8::try_from(first + i).expect("few"))
                .mount(proxy)
                .await;
        }
    }
    for (i, reply) in answers.into_iter().enumerate() {
        Mock::given(method("POST"))
            .and(path("/v1/llm/chat/completions"))
            .respond_with(reply)
            .up_to_n_times(1)
            .with_priority(u8::try_from(100 + i).expect("few"))
            .mount(proxy)
            .await;
    }
}

/// The model calls told `rules`, in the order they arrived; `None` for the answer calls.
fn calls_of<'a>(
    calls: &'a [(axum::http::HeaderMap, Value)],
    rules: Option<&str>,
) -> Vec<&'a Value> {
    let told = |body: &Value| {
        let system = body["messages"][0]["content"][0]["text"]
            .as_str()
            .unwrap_or_default();
        [ROUTER, JUDGE].into_iter().find(|r| system.starts_with(r))
    };
    calls
        .iter()
        .map(|(_, body)| body)
        .filter(|body| told(body) == rules)
        .collect()
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
async fn recorded_air(gateway: &MockServer, slug: &str) {
    let mcp = || Mock::given(method("POST")).and(path(format!("/api/endpoint/{slug}/mcp")));
    mcp().and(body_string_contains("tools/list"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"jsonrpc": "2.0", "id": 1, "result": {"tools": [
            {"name": "list_types", "description": "Lists the entity types.", "inputSchema": {"type": "object"}},
            {"name": "query_entities", "description": "Reads entities.", "inputSchema": {"type": "object", "properties": {"type": {"type": "array", "items": {"type": "string"}, "minItems": 1}, "limit": {"type": "integer", "maximum": 1000}}}},
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
    recorded_air(&w.gateway, SLUG).await;
    script_routed(&w.proxy, vec![
        completion(json!({"role": "assistant", "content": "{\"route\": \"data\", \"queries\": [\"NO2\"], \"data\": [{\"endpoint\": \"ovzdusie\", \"type\": \"AirQualityObserved\"}, {\"endpoint\": \"interne\", \"type\": \"Record\"}]}"}), 400),
    ], vec![], vec![
        completion(json!({"role": "assistant", "content": "NO2 na Námestí SNP je 21 [1]."}), 930),
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
            "stage",
            "stage",
            "tool",
            "tool",
            "stage",
            "tool",
            "tool",
            "data",
            "stage",
            "answer",
            "citations",
            "done"
        ],
        "{events:?}"
    );
    // T-4339: the question's moment, the server's, in UTC to the second.
    let at = events[0].1["at"].as_str().unwrap_or_default().to_owned();
    assert!(
        at.len() == 20 && at.ends_with('Z') && at.as_bytes()[10] == b'T',
        "{at}"
    );
    // The question itself is searched while the router call runs (T-4293).
    assert_eq!(
        events[2].1,
        json!({"stage": "searching", "topic": "Aké je NO2 na Námestí SNP?"})
    );
    assert_eq!(
        events[7].1,
        json!({"name": "query_entities", "endpoint": "ovzdusie", "status": "done"})
    );
    assert_eq!(
        events[8].1,
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
        events[11].1,
        json!([{"n": 1, "tool": "query_entities", "endpoint": "ovzdusie"}])
    );
    let done = &events[12].1;
    assert_eq!(
        (&done["tokens"], &done["modelCalls"], &done["cached"]),
        (&json!(1330), &json!(2), &json!(false))
    );
    let stages = done["stagesMs"].as_object().expect("stagesMs");
    for stage in ["route", "search", "answer"] {
        assert!(stages[stage].as_u64().is_some(), "{done}");
    }
    // The station's passage tops both rankings: the first round is not judged (T-4360).
    assert!(stages.get("judge").is_none(), "{done}");

    let calls = model_calls(&w.proxy).await;
    assert_eq!(
        calls.len(),
        2,
        "a covered first round is not judged, no round for the rows"
    );
    let router = calls_of(&calls, Some(ROUTER))[0]["messages"][1]["content"]
        .as_str()
        .unwrap_or_default();
    assert!(
        router.contains("- ovzdusie: AirQualityObserved"),
        "{router}"
    );
    assert!(!router.contains("interne"), "{router}");
    let answering = calls_of(&calls, None)[0]["messages"][2]["content"]
        .as_str()
        .unwrap_or_default();
    let given: Value = serde_json::from_str(answering.split('\n').next().unwrap_or_default())
        .expect("the data as JSON");
    assert_eq!(
        given["passages"],
        json!([]),
        "rows that came back answer alone"
    );
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
        // T-4339: every call is told the moment the conversation event names, beside the
        // question and never in the cached rules.
        let told = format!("{}Z).\nThe question:\n", &at[..16]);
        assert!(
            body["messages"][1]["content"]
                .as_str()
                .is_some_and(|asked| asked.starts_with("Now: ") || asked.contains("\nNow: "))
                && body["messages"][1]["content"]
                    .as_str()
                    .is_some_and(|asked| asked.contains(&told)),
            "{body}"
        );
        assert!(!body["messages"][0].to_string().contains(&at[..16]));
    }
    // The router is offered no tool; the answer call the read-only ones, never the write
    // (T-4294).
    assert!(calls_of(&calls, Some(ROUTER))[0].get("tools").is_none());
    assert_eq!(
        offered_tools(calls_of(&calls, None)[0]),
        ["ovzdusie__list_types", "ovzdusie__query_entities"]
    );
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
    script_routed(&w.proxy, vec![
        completion(json!({"role": "assistant", "content": "{\"route\": \"data\", \"data\": [{\"endpoint\": \"ovzdusie\", \"type\": \"AirQualityObserved\"}]}"}), 400),
    ], vec![], vec![
        completion(json!({"role": "assistant", "content": "21 [1]."}), 930),
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
    let slow = std::time::Duration::from_millis(300);
    script_routed(
        &w.proxy,
        vec![completion(json!({"role": "assistant", "content": "{\"route\": \"rag\", \"queries\": [\"knižnica\"]}"}), 100).set_delay(slow)],
        vec![completion(json!({"role": "assistant", "content": "{\"answers\": true}"}), 100).set_delay(slow)],
        vec![completion(
            json!({"role": "assistant", "content": "Do 18:00 [1]."}),
            100,
        )],
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
    // The router and the first round's judgement were with the model; no answer call followed.
    let calls = model_calls(&w.proxy).await;
    assert!(
        calls_of(&calls, None).is_empty(),
        "no call after the visitor left"
    );
    let mut tx = assistant::project_scope(&w.pool, "hronov")
        .await
        .expect("scope");
    let (tokens,): (i64,) = sqlx::query_as("SELECT tokens FROM conversations")
        .fetch_one(&mut *tx)
        .await
        .expect("one");
    assert_eq!(
        tokens,
        100 * i64::try_from(calls.len()).expect("few"),
        "every call that was sent is counted"
    );
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
    // 3,700: the rules grew by the clock line's (T-4339).
    let mut long = looped(3_700);
    if let Some(budget) = long.spec.budget.as_mut() {
        budget.question_chars = 4_000;
    }
    let w = world("chatbudget", long).await;
    // The estimate of the first call alone is over a 3,700-token conversation.
    let (_, events, _) = answer_of(&w.app, ask_looped(json!({"message": "x".repeat(4000)}))).await;
    let error = events
        .iter()
        .find(|(n, _)| n == "error")
        .expect("an error")
        .1
        .clone();
    assert_eq!(error["status"], 429);
    // T-4192: the visitor reads it in Slovak, like the chat's other sentences.
    assert_eq!(
        error["detail"],
        "Tento rozhovor už minul svoj podiel asistenta. Ak sa chcete pýtať ďalej, začnite nový rozhovor."
    );
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
    // T-4286: a day the proxy refuses is answered as search, never as a refusal.
    let (_, events, _) = answer_of(&w.app, ask_looped(json!({"message": "Ahoj"}))).await;
    assert!(
        events.iter().all(|(n, _)| n != "error"),
        "no refusal: {events:?}"
    );
    let answer = events
        .iter()
        .find(|(n, _)| n == "answer")
        .expect("a search answer")
        .1["text"]
        .as_str()
        .unwrap_or_default()
        .to_owned();
    assert!(answer.contains("denný rozpočet"), "{answer}");

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
    assert_eq!(error["title"], "Model Unavailable");
    assert_eq!(
        error["detail"],
        "Model asistenta neodpovedal. Skúste to o minútu znova."
    );
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
    script_routed(
        &w.proxy,
        vec![
            completion(json!({"role": "assistant", "content": "{\"route\": \"rag\", \"queries\": [\"prvá\"]}"}), 60),
            completion(json!({"role": "assistant", "content": "{\"route\": \"rag\", \"queries\": [\"druhá\"]}"}), 60),
        ],
        vec![
            completion(json!({"role": "assistant", "content": "{\"queries\": []}"}), 20),
            completion(json!({"role": "assistant", "content": "{\"queries\": []}"}), 20),
        ],
        vec![streamed(&["Prvá."], 15, 5), streamed(&["Druhá."], 60, 10)],
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
    let calls = model_calls(&w.proxy).await;
    assert_eq!(calls.len(), 6, "a router, a judgement and an answer each");
    let history = calls_of(&calls, Some(ROUTER))[1]["messages"][1]["content"]
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
                tool_call("c1", "ovzdusie__query_entities", json!({"type": ["Event"]})),
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
        title: Some("Hronov <i>".into()),
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
    // T-4283: the header's name rides in the same attribute, escaped the same way.
    assert!(
        html.contains("&quot;title&quot;:&quot;Hronov &lt;i&gt;&quot;"),
        "{html}"
    );
    assert!(!html.contains("<i>"));
    // A visitor gets no picker: the page names no connector, the assistant uses them all (T-4253).
    assert!(
        !html.contains("ovzdusie"),
        "a public page names no connector: {html}"
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
    // T-4204: the launcher leaves the host page room at its end, and on a phone the open panel
    // takes the whole screen with the page behind it inert.
    assert!(launcher.contains("var(--jc-assistant-inset, 96px)"));
    assert!(launcher.contains("(max-width:640px)") && launcher.contains("(max-width: 640px)"));
    assert!(launcher.contains(".inert = true"));
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
    let (status, headers, body) = get(&w.app, "/d/hronov-obcania/widget").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    // T-4241: it answers the bytes of an id that never existed, so nothing tells the two apart,
    // and its policy frames nothing and runs nothing.
    let (_, unknown_headers, unknown_body) = get(&w.app, "/d/nikdy-nebolo/widget").await;
    assert_eq!(body, unknown_body);
    assert_eq!(headers, unknown_headers);
    assert_eq!(
        headers
            .get("content-security-policy")
            .and_then(|v| v.to_str().ok()),
        Some("default-src 'none'; base-uri 'none'; form-action 'none'")
    );
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
/// its router model (JSON), searches the service runs, a follow-up call that reads the start of
/// the passages found and says they answer (T-4259), and one streamed answer call on its answer model with no tools, its output held to
/// `outputTokensPerAnswer`. Only retrieved passages are cited, the `done` event sums every call's
/// usage, and the same first question again comes from the cache without a model call.
#[tokio::test]
async fn a_sources_only_question_is_routed_searched_and_streamed_once() {
    let mut sources_only = deployment(Channel::Public, 50_000, 10);
    sources_only.spec.connectors.clear();
    sources_only.spec.model = Some(jc_core::kinds::ModelChoice {
        router: Some("test/router".into()),
        answer: Some("test/answer".into()),
        provider: Some(jc_core::kinds::ProviderRouting {
            order: Some(vec!["cerebras".into()]),
            sort: Some(jc_core::kinds::ProviderSort::Latency),
            allow_fallbacks: Some(false),
        }),
        reasoning: Some(jc_core::kinds::StageReasoning {
            router: Some(jc_core::kinds::ReasoningEffort::None),
            judge: None,
            answer: Some(jc_core::kinds::ReasoningEffort::Medium),
        }),
    });
    let w = world("chatrouted", sources_only).await;
    script_routed(
        &w.proxy,
        vec![completion(
            json!({"role": "assistant", "content": "{\"route\": \"rag\", \"queries\": [\"knižnica otváracie hodiny\"], \"followups\": [\"Kde je knižnica?\", \"Koľko stojí preukaz?\"]}"}),
            130,
        )],
        vec![],
        vec![streamed(&["Knižnica je otvorená ", "v pondelok až piatok od 9:00 do 18:00 [1]", " [7]."], 900, 40)],
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
            "stage",
            "stage",
            "tool",
            "tool",
            "stage",
            "tool",
            "tool",
            "stage",
            "delta",
            "delta",
            "delta",
            "answer",
            "citations",
            "followups",
            "done"
        ],
        "{events:?}"
    );
    // The progress line's steps, each search with its query (T-4155): the question as asked,
    // judged while the router ran, then the router's query; no further judgement (T-4293).
    let stages: Vec<(&str, Option<&str>)> = events
        .iter()
        .filter(|(n, _)| n == "stage")
        .map(|(_, d)| (d["stage"].as_str().unwrap_or_default(), d["topic"].as_str()))
        .collect();
    assert_eq!(
        stages,
        [
            ("understanding", None),
            ("searching", Some("Dokedy je otvorená knižnica?")),
            ("searching", Some("knižnica otváracie hodiny")),
            ("answering", None)
        ]
    );
    // T-4174: the router's follow-up questions, from the call it made anyway.
    assert_eq!(
        events[14].1["questions"],
        json!(["Kde je knižnica?", "Koľko stojí preukaz?"])
    );
    let deltas: String = events
        .iter()
        .filter(|(n, _)| n == "delta")
        .filter_map(|(_, d)| d["text"].as_str())
        .collect();
    // The streamed text is the model's; the answer shown drops [7], which names no source
    // (T-4366), and the drop is counted in the answer's timings.
    let answer = events[12].1["text"].as_str().expect("an answer");
    assert_eq!(answer, deltas.trim().replace(" [7]", ""));
    let mut tx = assistant::project_scope(&w.pool, "hronov")
        .await
        .expect("scope");
    let dangling: Vec<i32> = sqlx::query_scalar("SELECT dangling_markers FROM answer_timings")
        .fetch_all(&mut *tx)
        .await
        .expect("timings");
    tx.rollback().await.expect("rollback");
    assert_eq!(dangling, [1]);

    let calls = model_calls(&w.proxy).await;
    assert_eq!(calls.len(), 2);
    assert!(
        calls_of(&calls, Some(JUDGE)).is_empty(),
        "the library's passage tops both rankings: nothing to judge (T-4360)"
    );
    let (router, answering) = (calls_of(&calls, Some(ROUTER))[0], calls_of(&calls, None)[0]);
    assert_eq!(router["model"], "test/router");
    // T-4292: the deployment's providers ride on every call, in OpenRouter's own field names;
    // a sort beside the order too (T-4360).
    let pinned = json!({"order": ["cerebras"], "sort": "latency", "allow_fallbacks": false});
    assert!(
        calls.iter().all(|(_, body)| body["provider"] == pinned),
        "{calls:?}"
    );
    // T-4360: each stage asks with its own reasoning effort, `none` as OpenRouter turns it off.
    assert_eq!(router["reasoning"], json!({"enabled": false}));
    assert_eq!(answering["reasoning"], json!({"effort": "medium"}));
    assert_eq!(router["response_format"]["type"], "json_object");
    assert!(router.get("tools").is_none() && router.get("stream").is_none());
    // The router hears the sources' language.
    let routed = router["messages"][1]["content"]
        .as_str()
        .expect("the router's question");
    assert!(
        routed.contains("The sources are written in: sk."),
        "{routed}"
    );
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
    let citations = events[13].1.as_array().expect("citations");
    assert_eq!(citations.len(), 1, "{citations:?}");
    assert_eq!(citations[0]["n"], 1);
    assert_eq!(citations[0]["url"], first);

    let done = &events[15].1;
    assert_eq!(done["tokensIn"], 120 + 900);
    assert_eq!(done["tokensOut"], 10 + 40);
    assert_eq!(done["tokens"], 130 + 940);
    assert_eq!(done["modelCalls"], 2);
    assert_eq!(done["judgeSkipped"], true);
    assert_eq!(done["providers"], json!({}), "no reply named its provider");
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
        2,
        "the cache made a model call"
    );
    let names: Vec<&str> = again.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(names, ["conversation", "answer", "citations", "done"]);
    assert_eq!(again[1].1["text"], answer);
    assert_eq!(again[2].1, events[13].1);
    assert_eq!(again[3].1["cached"], true);
    assert_eq!(again[3].1["tokensIn"], 0);
    assert_eq!(again[3].1["modelCalls"], 0);
    // The skip is counted, so the eval shows how often the judgement is saved (T-4360).
    let mut tx = assistant::project_scope(&w.pool, "hronov")
        .await
        .expect("scope");
    let skipped: Vec<bool> =
        sqlx::query_scalar("SELECT judge_skipped FROM answer_timings WHERE NOT cached")
            .fetch_all(&mut *tx)
            .await
            .expect("timings");
    tx.rollback().await.expect("rollback");
    assert_eq!(skipped, [true]);
    db::drop_database(w.admin, w.pool, &w.name).await;
}

/// A row of `answer_timings`: deployment, cached, model calls, then the first token, the total
/// and the route, search, judge and answer stages in milliseconds, whether the judgement was
/// skipped.
type Timing = (
    String,
    bool,
    i32,
    Option<i64>,
    i64,
    Option<i64>,
    Option<i64>,
    Option<i64>,
    Option<i64>,
    bool,
);

/// T-4293: the question is searched and its passages judged while the router call is with the
/// model, so a slow router and a slow judgement cost the time of one, and each answer leaves one
/// row of stage times with no text in it. The question shares no word with the passages, so only
/// the meaning ranking finds them and they are judged (T-4360).
#[tokio::test]
async fn the_first_round_is_judged_while_the_router_runs_and_its_times_are_kept() {
    let mut sources_only = deployment(Channel::Public, 50_000, 10);
    sources_only.spec.connectors.clear();
    let w = world("chatparallel", sources_only).await;
    let slow = std::time::Duration::from_millis(700);
    // Each reply names the provider OpenRouter served it on (T-4360).
    let served = |content: &str, provider: &str| {
        ResponseTemplate::new(200).set_body_json(json!({
            "provider": provider,
            "choices": [{"message": {"role": "assistant", "content": content}}],
            "usage": {"prompt_tokens": 40, "completion_tokens": 10, "total_tokens": 50,
                "completion_tokens_details": {"reasoning_tokens": 6}}
        }))
    };
    let answer = format!(
        "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
        json!({"provider": "DeepInfra", "choices": [{"delta": {"content": "Od 9:00 do 18:00 [1]."}}]}),
        json!({"provider": "DeepInfra", "choices": [], "usage": {"prompt_tokens": 300, "completion_tokens": 10, "total_tokens": 310, "completion_tokens_details": {"reasoning_tokens": 0}}})
    );
    script_routed(
        &w.proxy,
        vec![served(
            "{\"route\": \"rag\", \"queries\": [\"knižnica\"]}",
            "Google Vertex",
        )
        .set_delay(slow)],
        vec![served("{\"answers\": true}", "Groq").set_delay(slow)],
        vec![ResponseTemplate::new(200)
            .insert_header("content-type", "text/event-stream")
            .set_body_string(answer)],
    )
    .await;
    let (_, events, _) = answer_of(
        &w.app,
        ask(json!({"message": "Kedy si môžem požičať knihy?"}), None),
    )
    .await;
    let done = &events.last().expect("done").1;
    let stages = &done["stagesMs"];
    let (route, judge, total) = (
        stages["route"].as_u64().expect("route"),
        stages["judge"].as_u64().expect("judge"),
        done["totalMs"].as_u64().expect("totalMs"),
    );
    assert!(route >= 700 && judge >= 700, "{done}");
    assert!(
        total < 1_400,
        "the router and the judgement ran one after the other: {done}"
    );
    assert!(
        stages["search"].as_u64().is_some() && stages["answer"].as_u64().is_some(),
        "{done}"
    );
    assert_eq!(done["modelCalls"], 3);
    // T-4360: the provider that served each stage and the reasoning tokens it spent.
    assert_eq!(done["judgeSkipped"], false);
    assert_eq!(
        done["providers"],
        json!({"route": "Google Vertex", "judge": "Groq", "answer": "DeepInfra"})
    );
    assert_eq!(
        done["reasoningTokens"],
        json!({"route": 6, "judge": 6, "answer": 0})
    );
    // The judgement reads the passages' text as data, from this deployment's sources only.
    let calls = model_calls(&w.proxy).await;
    // T-4360: a deployment without `model` asks for OpenRouter's fastest provider, with fallbacks,
    // and every stage reasons at `low`.
    assert!(
        calls.iter().all(|(_, body)| body["provider"]
            == json!({"sort": "latency", "allow_fallbacks": true})
            && body["reasoning"] == json!({"effort": "low"})),
        "{calls:?}"
    );
    let judged = calls_of(&calls, Some(JUDGE))[0]["messages"][1]["content"]
        .as_str()
        .expect("passages");
    assert!(
        judged.contains("Knižnica je otvorená v pondelok"),
        "{judged}"
    );
    assert!(
        !judged.contains("Platové tabuľky"),
        "another source's passage reached the model"
    );

    let mut tx = assistant::project_scope(&w.pool, "hronov")
        .await
        .expect("scope");
    let rows: Vec<Timing> = sqlx::query_as(
        "SELECT deployment, cached, model_calls, first_token_ms, total_ms, route_ms, search_ms, \
             judge_ms, answer_ms, judge_skipped FROM answer_timings",
    )
    .fetch_all(&mut *tx)
    .await
    .expect("timings");
    // The provider OpenRouter served each stage on, as its reply named it (T-4360).
    type Served = (
        Option<String>,
        Option<String>,
        Option<String>,
        Option<i32>,
        Option<i32>,
        Option<i32>,
    );
    let providers: Served = sqlx::query_as(
        "SELECT router_provider, judge_provider, answer_provider, route_reasoning_tokens, \
         judge_reasoning_tokens, answer_reasoning_tokens FROM answer_timings",
    )
    .fetch_one(&mut *tx)
    .await
    .expect("providers");
    tx.rollback().await.expect("rollback");
    assert_eq!(
        providers,
        (
            Some("Google Vertex".into()),
            Some("Groq".into()),
            Some("DeepInfra".into()),
            Some(6),
            Some(6),
            Some(0)
        )
    );
    assert_eq!(rows.len(), 1, "one row per answer");
    let row = &rows[0];
    assert_eq!((row.0.as_str(), row.1, row.2), ("obcania", false, 3));
    assert_eq!(row.5, i64::try_from(route).ok());
    assert_eq!(row.7, i64::try_from(judge).ok());
    assert!(!row.9, "the judgement ran: {row:?}");
    assert!(
        row.3.is_some() && row.6.is_some() && row.8.is_some(),
        "{row:?}"
    );
    // Times, counts and provider names only: no column a question or an answer could sit in.
    let mut columns: Vec<String> = sqlx::query_scalar(
        "SELECT column_name::text FROM information_schema.columns WHERE table_name = 'answer_timings'",
    )
    .fetch_all(&w.pool)
    .await
    .expect("columns");
    columns.sort();
    assert_eq!(
        columns,
        [
            "answer_ms",
            "answer_provider",
            "answer_reasoning_tokens",
            "at",
            "cached",
            "dangling_markers",
            "deployment",
            "first_token_ms",
            "judge_ms",
            "judge_provider",
            "judge_reasoning_tokens",
            "judge_skipped",
            "model_calls",
            "no_answer",
            "project",
            "route_ms",
            "route_reasoning_tokens",
            "router_provider",
            "search_ms",
            "total_ms"
        ]
    );
    db::drop_database(w.admin, w.pool, &w.name).await;
}

/// T-4259: an English question over Slovak pages searches in Slovak too; a follow-up that says
/// the passages do not answer runs one more round with its own queries, and one that says they do
/// ends the rounds before the answer call. Every search shows on the progress line.
#[tokio::test]
async fn an_english_question_over_slovak_pages_searches_in_slovak_and_a_no_runs_one_more_round() {
    let mut sources_only = deployment(Channel::Public, 50_000, 10);
    sources_only.spec.connectors.clear();
    let w = world("chatroutedlang", sources_only).await;
    script_routed(
        &w.proxy,
        vec![completion(
            json!({"role": "assistant", "content": "{\"route\": \"rag\", \"queries\": [\"library opening hours\", \"knižnica otváracie hodiny\"]}"}),
            120,
        )],
        vec![
            completion(
                json!({"role": "assistant", "content": "{\"answers\": false, \"missing\": \"where the stations are\", \"queries\": [\"meracie stanice ovzdušia\"]}"}),
                90,
            ),
            completion(json!({"role": "assistant", "content": "{\"answers\": true}"}), 60),
        ],
        vec![streamed(&["The library is open on weekdays from 9:00 to 18:00 [1]."], 900, 30)],
    )
    .await;
    let (status, events, _) = answer_of(
        &w.app,
        ask(json!({"message": "When is the library open?"}), None),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let searched: Vec<&str> = events
        .iter()
        .filter(|(n, d)| n == "stage" && d["stage"] == "searching")
        .filter_map(|(_, d)| d["topic"].as_str())
        .collect();
    assert_eq!(
        searched,
        [
            "When is the library open?",
            "library opening hours",
            "knižnica otváracie hodiny",
            "meracie stanice ovzdušia"
        ],
        "{events:?}"
    );
    let calls = model_calls(&w.proxy).await;
    // Router, two verdicts, the answer: the first round's "no" sent its query into the router's
    // round, and the "yes" after it ended the rounds.
    assert_eq!(calls.len(), 4, "{calls:?}");
    let second = calls_of(&calls, Some(JUDGE))[1]["messages"][1]["content"]
        .as_str()
        .expect("passages");
    assert!(second.contains("meracie stanice ovzdušia"), "{second}");
    assert!(
        second.contains("Meracie stanice kvality ovzdušia"),
        "{second}"
    );
    assert_eq!(calls_of(&calls, None)[0]["stream"], true);
    assert!(events.iter().any(|(n, _)| n == "answer"));
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
        ask(json!({"message": "Kde sa kontroluje vzduch?"}), None),
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
    // The question shares no word with the passages, so its round is judged (T-4360): two router
    // calls (the second a follow-up whose reply is no verdict, so the rounds end: T-4259), then the
    // answer call that failed.
    assert_eq!(model_calls(&w.proxy).await.len(), 3);
    let (_, again, _) = answer_of(
        &w.app,
        ask(json!({"message": "Kde sa kontroluje vzduch?"}), None),
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
    let (status, first, _) = answer_of(&w.app, ask(json!({"message": "Ahoj"}), Some(ORIGIN))).await;
    assert_eq!(status, StatusCode::OK);
    // T-4178: the answer reports what its calls cost, for the eval's cost per answer.
    let done = &first.last().expect("a done event").1;
    assert_eq!(done["costUsd"], json!(0.012), "{first:?}");
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
    assert_eq!(events[3].1["costUsd"], json!(0.0), "no call, no cost");
    assert_eq!(
        model_calls(&w.proxy).await.len(),
        before,
        "no model call past the cap"
    );
    db::drop_database(w.admin, w.pool, &w.name).await;
}

/// T-4286: at 08:25Z on 2026-10-16 every visitor of the public assistant read "used up today's
/// answers": the proxy's `tokensPerDay` refused the router call. Such a day now answers as
/// search with the pages as links, like the cost cap, and nobody reads a refusal.
#[tokio::test]
async fn a_day_the_proxy_refuses_answers_with_links_not_a_refusal() {
    let w = world("chatproxyday", deployment(Channel::Public, 1_000_000, 10)).await;
    Mock::given(method("POST"))
        .and(path("/v1/llm/chat/completions"))
        .respond_with(
            ResponseTemplate::new(429).set_body_json(json!({"title": "Daily Budget Spent"})),
        )
        .mount(&w.proxy)
        .await;
    let (status, events, _) = answer_of(
        &w.app,
        ask(
            json!({"message": "Dokedy je otvorená knižnica?"}),
            Some(ORIGIN),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(events.iter().all(|(n, _)| n != "error"), "{events:?}");
    let answer = events
        .iter()
        .find(|(n, _)| n == "answer")
        .expect("an answer")
        .1["text"]
        .as_str()
        .unwrap_or_default()
        .to_owned();
    assert!(answer.contains("Tu sú stránky"), "{answer}");
    assert!(!answer.contains("vyčerpané"), "{answer}");
    let citations = &events
        .iter()
        .find(|(n, _)| n == "citations")
        .expect("citations")
        .1;
    assert!(
        citations.as_array().is_some_and(|c| !c.is_empty()),
        "the library's page as a link: {events:?}"
    );
    assert_eq!(events.last().map(|(n, _)| n.as_str()), Some("done"));
    db::drop_database(w.admin, w.pool, &w.name).await;
}

/// T-4286: a visitor past `perClientPerMonth` gets links and the day it resets, without a model
/// call, while another visitor is still answered by the model.
#[tokio::test]
async fn a_visitor_past_their_month_gets_links_and_others_are_answered() {
    let mut monthly = deployment(Channel::Public, 1_000_000, 10);
    if let Some(rate) = monthly.spec.rate_limit.as_mut() {
        rate.per_client_per_month = 1;
    }
    let w = world("chatmonth", monthly).await;
    let answered = || {
        ResponseTemplate::new(200).set_body_json(json!({
            "choices": [{"message": {"role": "assistant", "content": "Áno."}}],
            "usage": {"prompt_tokens": 40, "completion_tokens": 10, "total_tokens": 50}
        }))
    };
    script(
        &w.proxy,
        vec![answered(), answered(), answered(), answered()],
    )
    .await;
    let from =
        |address: &str, message: &str| ask_from(json!({"message": message}), Some(ORIGIN), address);
    // Whatever the model makes of it, the first question of the month is the model's.
    let month_said = |events: &[(String, Value)]| {
        events.iter().any(|(n, d)| {
            n == "answer"
                && d["text"]
                    .as_str()
                    .is_some_and(|t| t.contains("Tento mesiac"))
        })
    };
    let (status, first, _) = answer_of(&w.app, from("203.0.113.7", "Ahoj")).await;
    assert_eq!(status, StatusCode::OK);
    assert!(!month_said(&first), "{first:?}");
    let calls = model_calls(&w.proxy).await.len();
    assert!(calls > 0, "the first question asks the model");

    let (status, second, _) =
        answer_of(&w.app, from("203.0.113.7", "Dokedy je otvorená knižnica?")).await;
    assert_eq!(status, StatusCode::OK, "never a refusal");
    let answer = second
        .iter()
        .find(|(n, _)| n == "answer")
        .expect("an answer")
        .1["text"]
        .as_str()
        .unwrap_or_default()
        .to_owned();
    assert!(answer.contains("Tento mesiac"), "{answer}");
    assert!(answer.contains("znova tak odpovedá od 1. "), "{answer}");
    assert_eq!(second.last().expect("done").1["modelCalls"], 0);
    assert_eq!(model_calls(&w.proxy).await.len(), calls, "no model call");

    let (_, other, _) = answer_of(&w.app, from("203.0.113.8", "Ahoj")).await;
    assert!(!month_said(&other), "{other:?}");
    assert!(
        model_calls(&w.proxy).await.len() > calls,
        "another visitor is answered by the model"
    );
    db::drop_database(w.admin, w.pool, &w.name).await;
}

/// T-4178: a provider that reports no cost leaves `costUsd` out of `done`, so a sum that misses
/// a call never reads as the answer's price.
#[tokio::test]
async fn an_answer_whose_cost_was_not_reported_has_no_cost_in_done() {
    let w = world("chatnocost", deployment(Channel::Public, 1_000_000, 10)).await;
    script(
        &w.proxy,
        vec![ResponseTemplate::new(200).set_body_json(json!({
            "choices": [{"message": {"role": "assistant", "content": "Áno."}}],
            "usage": {"prompt_tokens": 40, "completion_tokens": 10, "total_tokens": 50}
        }))],
    )
    .await;
    let (status, events, _) =
        answer_of(&w.app, ask(json!({"message": "Ahoj"}), Some(ORIGIN))).await;
    assert_eq!(status, StatusCode::OK);
    let (name, done) = events.last().expect("a done event");
    assert_eq!(name, "done");
    assert_eq!(done["tokensIn"], 40, "{events:?}");
    assert!(done.get("costUsd").is_none(), "{done}");
    db::drop_database(w.admin, w.pool, &w.name).await;
}

fn verdict(body: Value, origin: Option<&str>, client: &str) -> Request<Body> {
    let mut request = Request::builder()
        .method("POST")
        .uri("/api/v1/d/hronov-obcania/feedback")
        .header("content-type", "application/json")
        .header("x-forwarded-for", format!("{client}, 10.0.0.1"));
    if let Some(origin) = origin {
        request = request.header("origin", origin);
    }
    request
        .body(Body::from(body.to_string()))
        .expect("a request")
}

/// T-4174, API/05 §1.8: a verdict on an answer of a current conversation is stored with its turn
/// and note alone and a second one replaces it; an unknown conversation, a turn not asked, a
/// foreign origin and a secret in the note are refused, and the route has the chat's per-client
/// limit in a window of its own.
#[tokio::test]
async fn a_verdict_on_an_answer_is_stored_without_who_sent_it() {
    let w = world("chatfeedback", deployment(Channel::Public, 1_000_000, 3)).await;
    script(
        &w.proxy,
        vec![completion(json!({"role": "assistant", "content": "Áno."}), 50); 2],
    )
    .await;
    let (_, events, _) = answer_of(&w.app, ask(json!({"message": "a"}), Some(ORIGIN))).await;
    let id = events[0].1["id"].as_str().expect("an id").to_owned();
    let send = |body: Value, origin: Option<&'static str>, client: &'static str| {
        let app = w.app.clone();
        async move {
            let response = app
                .oneshot(verdict(body, origin, client))
                .await
                .expect("an answer");
            let status = response.status();
            let headers = response.headers().clone();
            let bytes = axum::body::to_bytes(response.into_body(), 1 << 16)
                .await
                .expect("a body");
            (
                status,
                serde_json::from_slice::<Value>(&bytes).unwrap_or(Value::Null),
                headers,
            )
        }
    };
    let (status, _, headers) = send(
        json!({"conversation": id, "turn": 1, "verdict": "up"}),
        Some(ORIGIN),
        "198.51.100.20",
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(
        headers
            .get("access-control-allow-origin")
            .and_then(|v| v.to_str().ok()),
        Some(ORIGIN)
    );
    let (status, _, _) = send(
        json!({"conversation": id, "turn": 1, "verdict": "down", "note": "Chýbali otváracie hodiny."}),
        Some(ORIGIN),
        "198.51.100.20",
    )
    .await;
    assert_eq!(
        status,
        StatusCode::NO_CONTENT,
        "a second verdict replaces the first"
    );

    for (body, origin, status) in [
        (
            json!({"conversation": id, "turn": 2, "verdict": "up"}),
            Some(ORIGIN),
            StatusCode::BAD_REQUEST,
        ),
        (
            json!({"conversation": "6f1c0e9e-3b1a-4d7e-9b51-2c4f8f2a7c11", "turn": 1, "verdict": "up"}),
            Some(ORIGIN),
            StatusCode::BAD_REQUEST,
        ),
        (
            json!({"conversation": "nie", "turn": 1, "verdict": "up"}),
            Some(ORIGIN),
            StatusCode::BAD_REQUEST,
        ),
        (
            json!({"conversation": id, "turn": 1, "verdict": "up", "who": "me"}),
            Some(ORIGIN),
            StatusCode::BAD_REQUEST,
        ),
        (
            json!({"conversation": id, "turn": 1, "verdict": "down", "note": "ghp_abc123def456ghi789jkl012mno345"}),
            Some(ORIGIN),
            StatusCode::BAD_REQUEST,
        ),
        (
            json!({"conversation": id, "turn": 1, "verdict": "up"}),
            Some("https://evil.example"),
            StatusCode::FORBIDDEN,
        ),
        // Started from the allowed origin, so not this caller's without it.
        (
            json!({"conversation": id, "turn": 1, "verdict": "up"}),
            None,
            StatusCode::BAD_REQUEST,
        ),
    ] {
        assert_eq!(
            send(body.clone(), origin, "198.51.100.21").await.0,
            status,
            "{body}"
        );
    }

    let mut tx = assistant::project_scope(&w.pool, "hronov")
        .await
        .expect("scope");
    let rows: Vec<(String, i32, String, Option<String>)> =
        sqlx::query_as("SELECT deployment, turn, verdict, note FROM feedback")
            .fetch_all(&mut *tx)
            .await
            .expect("rows");
    let columns: Vec<String> = sqlx::query_scalar(
        "SELECT column_name::text FROM information_schema.columns WHERE table_name = 'feedback' ORDER BY 1",
    )
    .fetch_all(&mut *tx)
    .await
    .expect("columns");
    tx.rollback().await.expect("rollback");
    assert_eq!(
        rows,
        [(
            "obcania".to_owned(),
            1,
            "down".to_owned(),
            Some("Chýbali otváracie hodiny.".to_owned())
        )]
    );
    assert_eq!(
        columns,
        [
            "conversation",
            "created_at",
            "deployment",
            "note",
            "project",
            "turn",
            "verdict"
        ],
        "no address, cookie or other personal data"
    );

    // Three a minute from one client, in a window of its own: the chat is still open to them.
    for _ in 0..3 {
        let _ = send(
            json!({"conversation": id, "turn": 1, "verdict": "up"}),
            Some(ORIGIN),
            "198.51.100.22",
        )
        .await;
    }
    let (status, _, headers) = send(
        json!({"conversation": id, "turn": 1, "verdict": "up"}),
        Some(ORIGIN),
        "198.51.100.22",
    )
    .await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    assert!(headers.contains_key("retry-after"));
    let (status, _, _) = answer_of(
        &w.app,
        ask_from(json!({"message": "b"}), Some(ORIGIN), "198.51.100.22"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    db::drop_database(w.admin, w.pool, &w.name).await;
}

/// T-4177, MF-52: a public deployment reads another project's public Endpoint named
/// `{project}/{endpoint}` when its scope lists the project; that project's organization Endpoint
/// and a project outside the scope are never asked, whatever the router names.
#[tokio::test]
async fn a_scoped_project_s_public_endpoint_is_read_and_nothing_else_of_it() {
    let mut d = deployment(Channel::Public, 50_000, 10);
    d.spec.scope = Some(jc_core::kinds::AssistantScope {
        projects: vec!["bbsk".into()],
    });
    let tools = vec!["list_types".to_owned(), "query_entities".to_owned()];
    d.spec.connectors = ["bbsk/kraj", "bbsk/spisy", "helsinki/kraj"]
        .into_iter()
        .map(|endpoint| Connector {
            endpoint: endpoint.into(),
            tools: tools.clone(),
            timeout_seconds: 5,
        })
        .collect();
    let w = world("chatscope", d).await;
    for slug in [REGION_SLUG, HIDDEN_SLUG, ELSEWHERE_SLUG] {
        recorded_air(&w.gateway, slug).await;
    }
    script(&w.proxy, vec![
        completion(json!({"role": "assistant", "content": "{\"route\": \"data\", \"data\": [{\"endpoint\": \"helsinki/kraj\", \"type\": \"AirQualityObserved\"}, {\"endpoint\": \"bbsk/spisy\", \"type\": \"AirQualityObserved\"}, {\"endpoint\": \"bbsk/kraj\", \"type\": \"AirQualityObserved\"}]}"}), 400),
        streamed(&["NO2 je 21 [1]."], 900, 30),
    ]).await;
    let (status, events, _) = answer_of(
        &w.app,
        ask(json!({"message": "Aké je NO2 v kraji?"}), Some(ORIGIN)),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let data: Vec<&Value> = events
        .iter()
        .filter(|(n, _)| n == "data")
        .map(|(_, d)| d)
        .collect();
    assert_eq!(data.len(), 1, "{events:?}");
    assert_eq!(data[0]["endpoint"], "bbsk/kraj");
    assert_eq!(
        data[0]["download"],
        format!("https://data.hronov.example/api/endpoint/{REGION_SLUG}/ngsi-ld/v1/entities?type=AirQualityObserved")
    );
    let router = model_calls(&w.proxy).await[0].1["messages"][1]["content"]
        .as_str()
        .unwrap_or_default()
        .to_owned();
    assert!(
        router.contains("- bbsk/kraj: AirQualityObserved"),
        "{router}"
    );
    assert!(
        !router.contains("helsinki") && !router.contains("spisy"),
        "{router}"
    );
    let asked: Vec<String> = w
        .gateway
        .received_requests()
        .await
        .unwrap_or_default()
        .iter()
        .map(|r| r.url.path().to_owned())
        .collect();
    assert!(asked.iter().any(|p| p.contains(REGION_SLUG)), "{asked:?}");
    assert!(
        asked
            .iter()
            .all(|p| !p.contains(HIDDEN_SLUG) && !p.contains(ELSEWHERE_SLUG)),
        "{asked:?}"
    );
    db::drop_database(w.admin, w.pool, &w.name).await;
}

/// T-4611: one Endpoint's `list_types` hangs for 30 s and another's fails. Routing asks both at
/// once, gives each three seconds, answers with what answered and tells the answer call which
/// sources did not answer; a second question within the minute asks neither again.
#[tokio::test]
async fn a_hanging_and_a_failing_endpoint_cost_the_question_three_seconds_once() {
    let mut d = deployment(Channel::Public, 50_000, 10);
    d.spec.scope = Some(jc_core::kinds::AssistantScope {
        projects: vec!["bbsk".into()],
    });
    let tools = vec!["list_types".to_owned(), "query_entities".to_owned()];
    d.spec.connectors = ["ovzdusie", "bbsk/kraj"]
        .into_iter()
        .map(|endpoint| Connector {
            endpoint: endpoint.into(),
            tools: tools.clone(),
            timeout_seconds: 20,
        })
        .collect();
    let w = world("chatslowtypes", d).await;
    for (slug, listed) in [
        (
            SLUG,
            ResponseTemplate::new(200)
                .set_body_json(json!({"jsonrpc": "2.0", "id": 1, "result": {"content": [{"type": "text", "text": "{}"}]}}))
                .set_delay(std::time::Duration::from_secs(30)),
        ),
        (REGION_SLUG, ResponseTemplate::new(500)),
    ] {
        let mcp = || Mock::given(method("POST")).and(path(format!("/api/endpoint/{slug}/mcp")));
        mcp().and(body_string_contains("tools/list"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"jsonrpc": "2.0", "id": 1, "result": {"tools": [
                {"name": "list_types", "description": "Lists the entity types.", "inputSchema": {"type": "object"}},
                {"name": "query_entities", "description": "Reads entities.", "inputSchema": {"type": "object"}}
            ]}})))
            .with_priority(1)
            .mount(&w.gateway)
            .await;
        mcp()
            .and(body_string_contains("\"list_types\""))
            .respond_with(listed)
            .with_priority(1)
            .mount(&w.gateway)
            .await;
    }
    let route = || {
        completion(
            json!({"role": "assistant", "content": "{\"route\": \"search\", \"queries\": [\"knižnica\"]}"}),
            300,
        )
    };
    let answer = || streamed(&["Knižnica je otvorená do 18:00 [1]."], 300, 10);
    script_routed(&w.proxy, vec![route()], vec![], vec![answer()]).await;

    let at = std::time::Instant::now();
    let (status, events, _) = answer_of(
        &w.app,
        ask(
            json!({"message": "Dokedy je otvorená knižnica?"}),
            Some(ORIGIN),
        ),
    )
    .await;
    let took = at.elapsed();
    assert_eq!(status, StatusCode::OK);
    assert!(
        events.iter().any(|(n, _)| n == "answer"),
        "answered with what answered: {events:?}"
    );
    assert!(took < std::time::Duration::from_secs(5), "took {took:?}");

    let calls = model_calls(&w.proxy).await;
    let rules = calls_of(&calls, None)[0]["messages"][0].to_string();
    assert!(
        rules.contains("did not answer in time")
            && rules.contains("ovzdusie")
            && rules.contains("bbsk/kraj"),
        "{rules}"
    );
    let router = calls_of(&calls, Some(ROUTER))[0]["messages"][1]["content"]
        .as_str()
        .unwrap_or_default()
        .to_owned();
    assert!(
        !router.contains("- ovzdusie:") && !router.contains("- bbsk/kraj:"),
        "{router}"
    );

    let listed = || async {
        w.gateway
            .received_requests()
            .await
            .unwrap_or_default()
            .iter()
            .filter(|r| String::from_utf8_lossy(&r.body).contains("\"list_types\""))
            .count()
    };
    assert_eq!(listed().await, 2, "each asked once");

    // Within the minute both are skipped at once.
    script_routed(&w.proxy, vec![route()], vec![], vec![answer()]).await;
    let at = std::time::Instant::now();
    let (status, _, _) =
        answer_of(&w.app, ask(json!({"message": "A v sobotu?"}), Some(ORIGIN))).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        at.elapsed() < std::time::Duration::from_secs(2),
        "took {:?}",
        at.elapsed()
    );
    assert_eq!(
        listed().await,
        2,
        "a failed Endpoint is not asked again within the minute"
    );
    db::drop_database(w.admin, w.pool, &w.name).await;
}

/// The gateway's `tools/call` bodies of `tool`, in order.
async fn tool_calls_of(gateway: &MockServer, tool: &str) -> Vec<Value> {
    gateway
        .received_requests()
        .await
        .unwrap_or_default()
        .iter()
        .filter_map(|r| serde_json::from_slice::<Value>(&r.body).ok())
        .filter(|b| b["method"] == "tools/call" && b["params"]["name"] == tool)
        .collect()
}

/// T-4294: after the router's read the model filters once more and then answers on its own; a
/// tool it makes up is refused to it and never sent, the write the manifest names is never
/// offered, and both reads are tables the answer can cite.
#[tokio::test]
async fn a_data_question_reads_again_until_the_model_answers() {
    let mut d = deployment(Channel::Public, 80_000, 10);
    d.spec.connectors[0].tools = vec![
        "list_types".into(),
        "query_entities".into(),
        "delete_entity".into(),
    ];
    let w = world("chatloop", d).await;
    recorded_air(&w.gateway, SLUG).await;
    script_routed(&w.proxy, vec![
        completion(json!({"role": "assistant", "content": "{\"route\": \"data\", \"data\": [{\"endpoint\": \"ovzdusie\", \"type\": \"AirQualityObserved\"}]}"}), 400),
    ], vec![
        completion(json!({"role": "assistant", "content": "{\"answers\": false}"}), 50),
    ], vec![
        completion(json!({"role": "assistant", "content": null, "tool_calls": [
            {"id": "c1", "type": "function", "function": {"name": "ovzdusie__query_entities", "arguments": "{\"type\":[\"AirQualityObserved\"],\"limit\":5}"}},
            {"id": "c2", "type": "function", "function": {"name": "ovzdusie__delete_entity", "arguments": "{}"}}
        ]}), 1_000),
        completion(json!({"role": "assistant", "content": "Najvyššie NO2 má Železničná stanica, 34 [2]."}), 1_200),
    ]).await;
    let (status, events, _) = answer_of(
        &w.app,
        ask(json!({"message": "Kde je NO2 najvyššie?"}), Some(ORIGIN)),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let named = |name: &str| {
        events
            .iter()
            .filter(|(n, _)| n == name)
            .map(|(_, d)| d.clone())
            .collect::<Vec<_>>()
    };
    assert_eq!(named("data").len(), 2, "{events:?}");
    assert_eq!(
        named("stage")
            .iter()
            .map(|s| s["stage"].as_str().unwrap_or_default().to_owned())
            .collect::<Vec<_>>(),
        [
            "understanding",
            "searching",
            "reading",
            "answering",
            "reading"
        ]
    );
    assert_eq!(
        named("answer")[0]["text"],
        "Najvyššie NO2 má Železničná stanica, 34 [2]."
    );
    assert_eq!(
        named("citations")[0],
        json!([{"n": 2, "tool": "query_entities", "endpoint": "ovzdusie"}])
    );
    assert_eq!(named("done")[0]["modelCalls"], 4);

    let all = model_calls(&w.proxy).await;
    let calls = calls_of(&all, None);
    assert_eq!(
        offered_tools(calls[0]),
        ["ovzdusie__list_types", "ovzdusie__query_entities"]
    );
    let last = calls[1]["messages"].as_array().cloned().unwrap_or_default();
    let tools: Vec<&Value> = last.iter().filter(|m| m["role"] == "tool").collect();
    assert_eq!(tools.len(), 2);
    let read: Value = serde_json::from_str(tools[0]["content"].as_str().unwrap_or_default())
        .expect("a result as JSON");
    assert_eq!(
        (&read["n"], &read["endpoint"]),
        (&json!(2), &json!("ovzdusie"))
    );
    assert_eq!(
        tools[1]["content"],
        "There is no tool of that name. Use only the tools you were given."
    );
    assert!(
        calls[1].get("tool_choice").is_none(),
        "the model was still free to read"
    );
    assert!(calls[0]["messages"][0]["content"][0]["text"]
        .as_str()
        .unwrap_or_default()
        .contains("At most 10 tool calls"));

    let reads = tool_calls_of(&w.gateway, "query_entities").await;
    assert_eq!(reads.len(), 2);
    assert_eq!(
        reads[1]["params"]["arguments"],
        json!({"type": ["AirQualityObserved"], "limit": 5})
    );
    assert!(tool_calls_of(&w.gateway, "delete_entity").await.is_empty());
    db::drop_database(w.admin, w.pool, &w.name).await;
}

/// T-4294: the router's read and nine of the model's make ten tool calls, the most a question
/// makes; a call repeated with the same arguments is answered from the question's own result and
/// still counts; past the tenth the calls are refused, and the last model call offers no tool
/// and is told to answer from what was read.
#[tokio::test]
async fn the_tenth_tool_call_is_the_last_and_the_answer_comes_from_what_was_read() {
    let mut d = deployment(Channel::Public, 200_000, 10);
    d.spec.connectors[0].tools = vec!["list_types".into(), "query_entities".into()];
    let w = world("chatten", d).await;
    recorded_air(&w.gateway, SLUG).await;
    let reply = |limits: &[u32]| {
        let calls: Vec<Value> = limits
            .iter()
            .map(|n| json!({"id": format!("c{n}"), "type": "function", "function": {"name": "ovzdusie__query_entities", "arguments": format!("{{\"type\":[\"AirQualityObserved\"],\"limit\":{n}}}")}}))
            .collect();
        completion(
            json!({"role": "assistant", "content": null, "tool_calls": calls}),
            1_000,
        )
    };
    script_routed(&w.proxy, vec![
        completion(json!({"role": "assistant", "content": "{\"route\": \"data\", \"data\": [{\"endpoint\": \"ovzdusie\", \"type\": \"AirQualityObserved\"}]}"}), 400),
    ], vec![
        completion(json!({"role": "assistant", "content": "{\"answers\": false}"}), 50),
    ], vec![
        reply(&[1, 2, 3, 4]),
        reply(&[5, 6, 7, 1]),
        reply(&[8, 9, 10, 11]),
        completion(json!({"role": "assistant", "content": "Dve stanice merajú NO2 [1]."}), 1_000),
    ]).await;
    let (_, events, _) = answer_of(
        &w.app,
        ask(json!({"message": "Ako sa mení NO2?"}), Some(ORIGIN)),
    )
    .await;
    assert!(
        events
            .iter()
            .any(|(n, d)| n == "answer" && d["text"] == "Dve stanice merajú NO2 [1]."),
        "{events:?}"
    );
    // 1 (the router's) + 4 + 3 new + 1: the repeat was not sent, the eleventh call not either.
    assert_eq!(tool_calls_of(&w.gateway, "query_entities").await.len(), 9);
    let calls = model_calls(&w.proxy).await;
    assert_eq!(calls.len(), 6, "router, judge and four loop calls");
    let last = &calls[5].1;
    assert_eq!(last["tool_choice"], "none");
    let messages = last["messages"].as_array().cloned().unwrap_or_default();
    assert!(messages
        .last()
        .and_then(|m| m["content"].as_str())
        .unwrap_or_default()
        .starts_with("No more tool calls"));
    let refused = messages
        .iter()
        .filter(|m| {
            m["role"] == "tool"
                && m["content"]
                    .as_str()
                    .unwrap_or_default()
                    .contains("used up")
        })
        .count();
    assert_eq!(refused, 3, "calls 11 to 13 were refused");
    let repeated = messages
        .iter()
        .filter(|m| m["role"] == "tool" && m["tool_call_id"] == "c1")
        .count();
    assert_eq!(repeated, 2, "both calls of limit 1 were answered");
    db::drop_database(w.admin, w.pool, &w.name).await;
}

/// T-4281, MF-52: `feedback: false` takes no verdict, and `answer.routes: rag` connects no
/// Endpoint: the gateway is never asked, while the default `both` asks it.
#[tokio::test]
async fn feedback_off_takes_no_verdict_and_rag_never_reaches_the_gateway() {
    let mut rag = deployment(Channel::Public, 1_000_000, 10);
    rag.spec.feedback = false;
    rag.spec.answer = Some(jc_core::kinds::AnswerSpec {
        routes: jc_core::kinds::Routes::Rag,
        ..Default::default()
    });
    for (test, deployment, reached) in [
        ("chatrag", rag, false),
        ("chatboth", deployment(Channel::Public, 1_000_000, 10), true),
    ] {
        let w = world(test, deployment).await;
        script(
            &w.proxy,
            vec![completion(json!({"role": "assistant", "content": "Áno."}), 50); 4],
        )
        .await;
        let (status, events, _) = answer_of(
            &w.app,
            ask(json!({"message": "Kedy je zber?"}), Some(ORIGIN)),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{test}");
        let asked = w.gateway.received_requests().await.unwrap_or_default();
        assert_eq!(!asked.is_empty(), reached, "{test}: {asked:?}");
        if !reached {
            let id = events[0].1["id"].as_str().expect("an id").to_owned();
            let response = w
                .app
                .clone()
                .oneshot(verdict(
                    json!({"conversation": id, "turn": 1, "verdict": "up"}),
                    Some(ORIGIN),
                    "198.51.100.30",
                ))
                .await
                .expect("an answer");
            assert_eq!(response.status(), StatusCode::NOT_FOUND);
        }
        db::drop_database(w.admin, w.pool, &w.name).await;
    }
}

/// T-4294: `answer.maxDataSteps` bounds the loop instead of the default ten: with 2, the router's
/// read and one of the model's are all, and the next model call is told to answer.
#[tokio::test]
async fn a_deployments_max_data_steps_ends_its_loop() {
    let mut d = deployment(Channel::Public, 200_000, 10);
    d.spec.connectors[0].tools = vec!["list_types".into(), "query_entities".into()];
    d.spec.answer = Some(jc_core::kinds::AnswerSpec {
        max_data_steps: Some(2),
        ..Default::default()
    });
    let w = world("chatsteps", d).await;
    recorded_air(&w.gateway, SLUG).await;
    let call = |n: u32| json!({"id": format!("c{n}"), "type": "function", "function": {"name": "ovzdusie__query_entities", "arguments": format!("{{\"type\":[\"AirQualityObserved\"],\"limit\":{n}}}")}});
    script_routed(&w.proxy, vec![
        completion(json!({"role": "assistant", "content": "{\"route\": \"data\", \"data\": [{\"endpoint\": \"ovzdusie\", \"type\": \"AirQualityObserved\"}]}"}), 400),
    ], vec![
        completion(json!({"role": "assistant", "content": "{\"answers\": false}"}), 50),
    ], vec![
        completion(json!({"role": "assistant", "content": null, "tool_calls": [call(1), call(2)]}), 1_000),
        completion(json!({"role": "assistant", "content": "NO2 je 21 [1]."}), 1_000),
    ]).await;
    let (_, events, _) = answer_of(
        &w.app,
        ask(json!({"message": "Ako sa mení NO2?"}), Some(ORIGIN)),
    )
    .await;
    assert!(
        events
            .iter()
            .any(|(n, d)| n == "answer" && d["text"] == "NO2 je 21 [1]."),
        "{events:?}"
    );
    assert_eq!(tool_calls_of(&w.gateway, "query_entities").await.len(), 2);
    let all = model_calls(&w.proxy).await;
    let answers = calls_of(&all, None);
    assert_eq!(answers.len(), 2);
    assert_eq!(answers[1]["tool_choice"], "none");
    db::drop_database(w.admin, w.pool, &w.name).await;
}

/// A streamed answer that ends with `reason` as its `finish_reason` (T-4370).
fn stopped(pieces: &[&str], reason: &str) -> ResponseTemplate {
    let mut body = String::new();
    for piece in pieces {
        body.push_str(&format!(
            "data: {}\n\n",
            json!({"choices": [{"delta": {"content": piece}}]})
        ));
    }
    body.push_str(&format!(
        "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
        json!({"choices": [{"delta": {}, "finish_reason": reason}]}),
        json!({"choices": [], "usage": {"prompt_tokens": 900, "completion_tokens": 2, "total_tokens": 902}})
    ));
    ResponseTemplate::new(200)
        .insert_header("content-type", "text/event-stream")
        .set_body_string(body)
}

/// T-4370: on a public deployment a connector whose tools cannot be listed shows no step; a
/// provider's safety stop reaches the visitor as "Declined" and is not cached, so the same
/// question asks the model again; an answer call with no text is "No Answer" and is counted.
#[tokio::test]
async fn a_cut_refusal_is_declined_not_cached_and_an_empty_answer_is_counted() {
    let w = world("chatdeclined", deployment(Channel::Public, 50_000, 10)).await;
    Mock::given(method("POST"))
        .and(path(format!("/api/endpoint/{SLUG}/mcp")))
        .respond_with(ResponseTemplate::new(500))
        .mount(&w.gateway)
        .await;
    let rag = || {
        completion(
            json!({"role": "assistant", "content": "{\"route\": \"rag\", \"queries\": [\"knižnica\"]}"}),
            130,
        )
    };
    script_routed(
        &w.proxy,
        vec![rag(), rag()],
        vec![],
        vec![
            stopped(&["I cannot"], "content_filter"),
            stopped(&[], "stop"),
        ],
    )
    .await;
    let question = || ask(json!({"message": "Dokedy je otvorená knižnica?"}), None);
    let (_, events, _) = answer_of(&w.app, question()).await;
    assert!(
        events
            .iter()
            .all(|(n, d)| n != "tool" || d["name"] != "tools/list"),
        "no failed listing step: {events:?}"
    );
    let error = &events
        .iter()
        .find(|(n, _)| n == "error")
        .expect("an error")
        .1;
    assert_eq!(
        (error["status"].clone(), error["title"].clone()),
        (json!(422), json!("Declined"))
    );
    assert!(
        events.iter().all(|(n, _)| n != "answer"),
        "the cut text is never the answer"
    );

    let (_, events, _) = answer_of(&w.app, question()).await;
    let error = &events
        .iter()
        .find(|(n, _)| n == "error")
        .expect("an error")
        .1;
    assert_eq!(
        error["title"], "No Answer",
        "asked again, not replayed: {events:?}"
    );
    assert_eq!(calls_of(&model_calls(&w.proxy).await, None).len(), 2);

    let mut tx = assistant::project_scope(&w.pool, "hronov")
        .await
        .expect("scope");
    let mut unanswered: Vec<bool> = sqlx::query_scalar("SELECT no_answer FROM answer_timings")
        .fetch_all(&mut *tx)
        .await
        .expect("timings");
    tx.rollback().await.expect("rollback");
    unanswered.sort();
    assert_eq!(unanswered, [false, true]);
    db::drop_database(w.admin, w.pool, &w.name).await;
}

/// The named server `hronov/air` lends the public Endpoint and the organization's one; it lists
/// for whoever asks, the gateway deciding what each caller reads (EP-120).
async fn named_server(gateway: &MockServer) {
    Mock::given(method("POST"))
        .and(path("/api/mcp/hronov/air"))
        .and(body_string_contains("list_endpoints"))
        .respond_with(ResponseTemplate::new(200).set_body_json(
            json!({"jsonrpc": "2.0", "id": 1, "result": {
                "content": [{"type": "text", "text": "2 Endpoints"}],
                "structuredContent": {"total": 2, "endpoints": [
                    {"slug": SLUG, "access": "public"},
                    {"slug": HIDDEN_SLUG, "access": "internal"}
                ]}
            }}),
        ))
        .mount(gateway)
        .await;
}

/// MF-56 (T-4535): an internal deployment with only `mcpServer` answers a data question from the
/// Endpoints its server lends the person: the server is asked with the person's token, the
/// public Endpoint without one, the organization's with it, and the read is a cited tool call.
#[tokio::test]
async fn an_internal_deployment_reads_its_named_servers_endpoints_as_the_person() {
    let mut d = internal_deployment(true);
    d.spec.connectors = Vec::new();
    d.spec.mcp_server = Some("hronov/air".into());
    let w = world("chatserverinternal", d).await;
    named_server(&w.gateway).await;
    hidden_endpoint(&w.gateway).await;
    script(
        &w.proxy,
        vec![
            completion(
                tool_call("c1", &format!("{HIDDEN_SLUG}__query_entities"), json!({})),
                500,
            ),
            completion(
                json!({"role": "assistant", "content": "Otvorené sú 3 spisy [1]."}),
                600,
            ),
        ],
    )
    .await;
    let (status, events, _) = answer_of(
        &w.app,
        ask_in_portal(
            json!({"message": "Koľko je otvorených spisov?"}),
            Some(PORTAL_TOKEN),
            Some("jana"),
            Some(PERSON_TOKEN),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{events:?}");
    assert!(
        events
            .iter()
            .any(|(n, d)| n == "tool" && d["endpoint"] == HIDDEN_SLUG && d["status"] == "done"),
        "{events:?}"
    );
    let tools = offered_tools(&model_calls(&w.proxy).await[0].1);
    for slug in [SLUG, HIDDEN_SLUG] {
        assert!(
            tools.contains(&format!("{slug}__query_entities")),
            "{tools:?}"
        );
    }
    for request in w.gateway.received_requests().await.unwrap_or_default() {
        let bearer = request
            .headers
            .get("authorization")
            .map(|v| v.to_str().unwrap_or_default().to_owned());
        if request.url.path().contains(SLUG) {
            assert_eq!(bearer, None, "a public Endpoint is called without a token");
        } else {
            assert_eq!(
                bearer.as_deref(),
                Some(format!("Bearer {PERSON_TOKEN}").as_str()),
                "{}",
                request.url
            );
        }
    }
    db::drop_database(w.admin, w.pool, &w.name).await;
}

/// MF-56 (T-4535): a public deployment asks its named server as anyone and reaches only what the
/// server lends a stranger: an Endpoint it lists as internal is never called, and no token
/// leaves, even when the Portal brings the person's.
#[tokio::test]
async fn a_public_deployment_reads_its_named_server_as_anyone() {
    let mut d = deployment(Channel::Public, 50_000, 10);
    d.spec.connectors = Vec::new();
    d.spec.mcp_server = Some("hronov/air".into());
    let w = world("chatserverpublic", d).await;
    named_server(&w.gateway).await;
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
            json!({"message": "Aké je ovzdušie?"}),
            Some(PORTAL_TOKEN),
            Some("jana"),
            Some(PERSON_TOKEN),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{events:?}");
    let requests = w.gateway.received_requests().await.unwrap_or_default();
    assert!(
        requests
            .iter()
            .any(|r| r.url.path() == "/api/mcp/hronov/air"),
        "the server was not asked"
    );
    assert!(
        requests.iter().any(|r| r.url.path().contains(SLUG)),
        "the public Endpoint was not connected"
    );
    for request in &requests {
        assert!(!request.url.path().contains(HIDDEN_SLUG), "{}", request.url);
        assert!(
            request.headers.get("authorization").is_none(),
            "{}",
            request.url
        );
    }
    db::drop_database(w.admin, w.pool, &w.name).await;
}

/// T-4653, API/05 §1.2.1: a question needs a current visitor token; a challenge and its proof earn
/// one, a wrong proof earns nothing, and two tokens behind one address are two visitors with a
/// minute each.
#[tokio::test]
async fn a_visitor_token_is_earned_by_a_proof_and_counts_one_visitor() {
    use sha2::{Digest, Sha256};
    let mut published = deployment(Channel::Public, 50_000, 2);
    if let Some(rate) = published.spec.rate_limit.as_mut() {
        rate.per_client_per_month = 4;
    }
    let w = world("chatvisitor", published).await;
    for token in [
        "",
        "a.b.c",
        "0123456789abcdef0123456789abcdef.4102444800.00",
    ] {
        let mut request = ask(json!({"message": "a"}), Some(ORIGIN));
        request
            .headers_mut()
            .insert(VISITOR_HEADER, token.parse().expect("a header value"));
        let (status, body, headers) = answer_of(&w.app, request).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{token:?}");
        assert!(body[0].1["detail"]
            .as_str()
            .unwrap_or_default()
            .contains("Obnovte stránku"));
        assert_eq!(
            headers
                .get("access-control-allow-origin")
                .and_then(|v| v.to_str().ok()),
            Some(ORIGIN),
            "the widget reads the refusal"
        );
    }
    assert!(model_calls(&w.proxy).await.is_empty(), "nothing spent");

    let send = |request: Request<Body>| {
        let app = w.app.clone();
        async move {
            let response = app.oneshot(request).await.expect("an answer");
            let (status, headers) = (response.status(), response.headers().clone());
            let bytes = axum::body::to_bytes(response.into_body(), 1 << 16)
                .await
                .expect("a body");
            (
                status,
                headers,
                serde_json::from_slice::<Value>(&bytes).unwrap_or(Value::Null),
            )
        }
    };
    let earn = || async {
        let (status, headers, challenge) = send(
            Request::get("/api/v1/d/hronov-obcania/challenge")
                .header("origin", ORIGIN)
                .body(Body::empty())
                .expect("a request"),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            headers.get("cache-control").and_then(|v| v.to_str().ok()),
            Some("no-store")
        );
        assert_eq!(challenge["difficulty"], 14);
        let text = challenge["challenge"]
            .as_str()
            .expect("a challenge")
            .to_owned();
        let bits = |nonce: &u64| {
            let hash = Sha256::digest(format!("{text}:{nonce}"));
            let mut bits = 0;
            for byte in hash {
                bits += byte.leading_zeros();
                if byte != 0 {
                    break;
                }
            }
            bits
        };
        let wrong = (0u64..).find(|n| bits(n) == 0).expect("a miss");
        let proof = |nonce: u64| {
            Request::post("/api/v1/d/hronov-obcania/token")
                .header("origin", ORIGIN)
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({"challenge": text, "nonce": nonce.to_string()}).to_string(),
                ))
                .expect("a request")
        };
        assert_eq!(send(proof(wrong)).await.0, StatusCode::BAD_REQUEST);
        let nonce = (0u64..).find(|n| bits(n) >= 14).expect("a nonce");
        let (status, _, token) = send(proof(nonce)).await;
        assert_eq!(status, StatusCode::OK);
        token["token"].as_str().expect("a token").to_owned()
    };
    let (first, second, third) = (earn().await, earn().await, earn().await);

    script(
        &w.proxy,
        vec![completion(json!({"role": "assistant", "content": "Áno."}), 50); 4],
    )
    .await;
    let with = |token: &str| {
        let mut request = ask_from(json!({"message": "a"}), Some(ORIGIN), "198.51.100.40");
        request
            .headers_mut()
            .insert(VISITOR_HEADER, token.parse().expect("a header value"));
        request
    };
    for _ in 0..2 {
        assert_eq!(answer_of(&w.app, with(&first)).await.0, StatusCode::OK);
    }
    assert_eq!(
        answer_of(&w.app, with(&first)).await.0,
        StatusCode::TOO_MANY_REQUESTS,
        "the first visitor's minute is spent"
    );
    assert_eq!(
        answer_of(&w.app, with(&second)).await.0,
        StatusCode::OK,
        "a second visitor behind the same address has their own minute"
    );
    // The month is the address's (integrator review): the fourth question of the address spends
    // it, and a token minted after is answered as search, with no model call.
    assert_eq!(answer_of(&w.app, with(&second)).await.0, StatusCode::OK);
    assert_eq!(model_calls(&w.proxy).await.len(), 4);
    let (status, body, _) = answer_of(&w.app, with(&third)).await;
    assert_eq!(status, StatusCode::OK, "{body:?}");
    assert_eq!(
        model_calls(&w.proxy).await.len(),
        4,
        "a fresh token buys no fresh month"
    );

    // The preflight lets the widget send its token.
    let preflight = w
        .app
        .clone()
        .oneshot(
            Request::builder()
                .method("OPTIONS")
                .uri("/api/v1/d/hronov-obcania/token")
                .header("origin", ORIGIN)
                .body(Body::empty())
                .expect("a request"),
        )
        .await
        .expect("an answer");
    assert!(preflight
        .headers()
        .get("access-control-allow-headers")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.contains("x-jc-visitor")));
    db::drop_database(w.admin, w.pool, &w.name).await;
}
