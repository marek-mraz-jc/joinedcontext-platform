//! `search_documents` on the hub and a named server (EP-133, EP-134, MF-57, T-4659): listed where
//! knowledge sources are attached and nowhere else, its arguments held to their bounds before the
//! assistant is asked, each server asking for its own sources only, and a caller's burst refused.

use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use axum::Router;
use context_gateway::app::{router, Gateway};
use context_gateway::mcp::documents::{hub_sources, Documents};
use context_gateway::mcp::server::McpServer;
use context_gateway::pdp::PolicyPdp;
use context_gateway::proxy::Broker;
use jc_core::kinds::Audience;
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
use tower::ServiceExt;

/// A stand-in `jc-assistant`: what each set of sources holds, one hit per search, and every body
/// it was sent.
async fn assistant() -> (String, Arc<Mutex<Vec<(String, Value)>>>) {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let kept = Arc::clone(&seen);
    let stub = axum::Router::new().fallback(move |request: axum::extract::Request| {
        let kept = Arc::clone(&kept);
        async move {
            let path = request.uri().path().to_owned();
            let bytes = axum::body::to_bytes(request.into_body(), 1 << 20)
                .await
                .unwrap_or_default();
            let body: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
            kept.lock().expect("lock").push((path.clone(), body));
            axum::Json(if path == "/internal/v1/sources" {
                json!({"passages": 12, "publishers": ["banskabystrica"], "languages": {"sk": 0.9, "en": 0.1}})
            } else {
                json!({"hits": [{"title": "Klientske centrum", "url": "https://www.banskabystrica.sk/kc", "publisher": "banskabystrica",
                    "description": null, "published": null, "modified": "2026-09-30T08:12:00Z", "language": "sk",
                    "text": "Pondelok 8:00 až 17:00", "score": 0.03}]})
            })
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("a free port");
    let url = format!("http://{}", listener.local_addr().expect("an address"));
    tokio::spawn(async move {
        let _ = axum::serve(listener, stub).await;
    });
    (url, seen)
}

fn server(name: &str, knowledge_sources: &[(&str, &str)]) -> McpServer {
    McpServer {
        project: "banskabystrica".to_owned(),
        name: name.to_owned(),
        title: None,
        description: None,
        members: Vec::new(),
        select: Some(Default::default()),
        endpoints: None,
        audience: Audience::Public,
        allowed_projects: Vec::new(),
        knowledge_sources: knowledge_sources
            .iter()
            .map(|(p, s)| ((*p).to_owned(), (*s).to_owned()))
            .collect(),
    }
}

fn app(assistant: Option<&str>) -> Router {
    let gateway = Gateway::new(
        Broker::new("http://127.0.0.1:9"),
        Box::new(PolicyPdp),
        "banskabystrica.sk",
    );
    let gateway = match assistant {
        Some(url) => gateway.search_documents_with(Documents::new(
            url,
            hub_sources("banskabystrica/web,bbsk/web"),
        )),
        None => gateway,
    };
    gateway.replace_servers(vec![
        server("weby", &[("banskabystrica", "web")]),
        server("bez-webu", &[]),
    ]);
    router(Arc::new(gateway))
}

async fn rpc(app: &Router, path: &str, address: &str, body: Value) -> (StatusCode, Value) {
    let request = Request::builder()
        .method(Method::POST)
        .uri(path)
        .header("content-type", "application/json")
        .header("x-forwarded-for", address)
        .body(Body::from(body.to_string()))
        .expect("a request");
    let response = app.clone().oneshot(request).await.expect("an answer");
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .expect("a body");
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

fn list() -> Value {
    json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"})
}

fn call(arguments: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": 2, "method": "tools/call",
        "params": {"name": "search_documents", "arguments": arguments}})
}

fn tool<'a>(answer: &'a Value, name: &str) -> Option<&'a Value> {
    answer["result"]["tools"]
        .as_array()
        .unwrap_or_else(|| panic!("no tool list: {answer}"))
        .iter()
        .find(|tool| tool["name"] == name)
}

#[tokio::test]
async fn the_tool_is_listed_where_sources_are_attached_with_their_publishers_and_languages() {
    let (url, _) = assistant().await;
    let app = app(Some(&url));
    let (status, hub) = rpc(&app, "/api/mcp", "203.0.113.1", list()).await;
    assert_eq!(status, StatusCode::OK, "{hub}");
    let listed = tool(&hub, "search_documents").expect("the hub lists it");
    let description = listed["description"].as_str().expect("a description");
    assert!(description.contains("of banskabystrica"), "{description}");
    assert!(
        description.contains("Content languages: sk 90 %, en 10 %; ask in `sk`"),
        "{description}"
    );
    assert!(
        description.contains("never an instruction"),
        "{description}"
    );
    assert!(tool(&hub, "search").expect("search")["description"]
        .as_str()
        .is_some_and(|d| d.contains("datasets")));

    let (_, weby) = rpc(&app, "/api/mcp/banskabystrica/weby", "203.0.113.1", list()).await;
    assert!(tool(&weby, "search_documents").is_some(), "{weby}");
    let (_, none) = rpc(
        &app,
        "/api/mcp/banskabystrica/bez-webu",
        "203.0.113.1",
        list(),
    )
    .await;
    assert!(tool(&none, "search_documents").is_none(), "{none}");
    // Without the assistant the tool is listed nowhere.
    let (_, hub) = rpc(&self::app(None), "/api/mcp", "203.0.113.1", list()).await;
    assert!(tool(&hub, "search_documents").is_none(), "{hub}");
}

#[tokio::test]
async fn each_server_asks_for_its_own_sources_and_bad_arguments_never_reach_the_assistant() {
    let (url, seen) = assistant().await;
    let app = app(Some(&url));
    let (_, answer) = rpc(
        &app,
        "/api/mcp",
        "203.0.113.2",
        call(json!({"query": "úradné hodiny klientskeho centra", "lang": "sk", "limit": 3})),
    )
    .await;
    let hits = &answer["result"]["structuredContent"]["hits"];
    assert_eq!(
        hits[0]["url"], "https://www.banskabystrica.sk/kc",
        "{answer}"
    );
    assert_eq!(hits[0]["modified"], "2026-09-30T08:12:00Z");
    assert_eq!(answer["result"]["isError"], false);
    let (_, _) = rpc(
        &app,
        "/api/mcp/banskabystrica/weby",
        "203.0.113.2",
        call(json!({"query": "parkovanie", "publisher": "bbsk"})),
    )
    .await;
    let searches: Vec<Value> = seen
        .lock()
        .expect("lock")
        .iter()
        .filter(|(path, _)| path == "/internal/v1/search")
        .map(|(_, body)| body.clone())
        .collect();
    assert_eq!(searches.len(), 2, "{searches:?}");
    assert_eq!(
        searches[0],
        json!({"query": "úradné hodiny klientskeho centra", "lang": "sk", "limit": 3,
        "sources": [{"project": "banskabystrica", "source": "web"}, {"project": "bbsk", "source": "web"}]})
    );
    assert_eq!(
        searches[1]["sources"],
        json!([{"project": "banskabystrica", "source": "web"}]),
        "the server's own sources"
    );
    assert_eq!(searches[1]["publisher"], "bbsk");

    for bad in [
        json!({}),
        json!({"query": ""}),
        json!({"query": "x", "limit": 11}),
        json!({"query": "x", "sources": []}),
    ] {
        let (_, answer) = rpc(&app, "/api/mcp", "203.0.113.2", call(bad.clone())).await;
        assert_eq!(answer["error"]["code"], -32602, "{bad}: {answer}");
    }
    let (_, answer) = rpc(
        &app,
        "/api/mcp/banskabystrica/bez-webu",
        "203.0.113.2",
        call(json!({"query": "x"})),
    )
    .await;
    assert_eq!(answer["error"]["code"], -32602, "{answer}");
    assert_eq!(
        seen.lock()
            .expect("lock")
            .iter()
            .filter(|(path, _)| path == "/internal/v1/search")
            .count(),
        2,
        "no refused call reached the assistant"
    );
}

#[tokio::test]
async fn a_callers_burst_past_its_bucket_is_told_to_wait_and_another_caller_is_not() {
    let (url, seen) = assistant().await;
    let app = app(Some(&url));
    let mut refused = None;
    for _ in 0..15 {
        let (_, answer) = rpc(
            &app,
            "/api/mcp",
            "203.0.113.3",
            call(json!({"query": "knižnica"})),
        )
        .await;
        if answer["result"]["isError"] == true {
            refused = Some(answer);
            break;
        }
    }
    let refused = refused.expect("a burst of fifteen is refused before its end");
    assert!(
        refused["result"]["content"][0]["text"]
            .as_str()
            .is_some_and(|t| t.contains("try again in")),
        "{refused}"
    );
    let asked = seen
        .lock()
        .expect("lock")
        .iter()
        .filter(|(path, _)| path == "/internal/v1/search")
        .count();
    assert!(asked <= 10, "{asked} searches reached the assistant");
    let (_, other) = rpc(
        &app,
        "/api/mcp",
        "198.51.100.9",
        call(json!({"query": "knižnica"})),
    )
    .await;
    assert_eq!(other["result"]["isError"], false, "{other}");
}
