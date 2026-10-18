//! An assistant's own MCP server, `/api/mcp/d/{publicId}` (T-4691, MF-52): exactly its
//! connectors' Endpoints and the reach of the named server it reads, `search_documents` only
//! when it has knowledge sources, read-only for every caller, and a public deployment never
//! serving an internal Endpoint.

mod common;

use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use axum::Router;
use context_gateway::app::{router, Gateway};
use context_gateway::auth::accounts::ServiceAccounts;
use context_gateway::mcp::documents::{hub_sources, Documents};
use context_gateway::pdp::PolicyPdp;
use context_gateway::proxy::Broker;
use context_gateway::resolver::Endpoint;
use jc_core::kinds::{Audience, PolicySpec, Representation};
use serde_json::{json, Value};
use std::sync::Arc;
use tower::ServiceExt;

const HOST: &str = "https://city.example";
const AIR: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaa";
const BIKES: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbb";
/// An organization Endpoint a public deployment names by mistake.
const STAFF: &str = "cccccccccccccccccccccccccc";
/// Public, and no connector of any deployment.
const OUTSIDE: &str = "dddddddddddddddddddddddddd";

/// The manifests: three Endpoints, a public deployment with two of them, the internal one and
/// a web source; one without sources; an internal one; a disabled one.
fn repository() -> std::path::PathBuf {
    let endpoint = |name: &str, slug: &str, audience: &str| {
        format!(
            "apiVersion: joinedcontext.com/v1alpha1\nkind: Endpoint\nmetadata:\n  name: {name}\n  \
             namespace: bbsk\nspec:\n  contextSpaceRef: data\n  slug: {slug}\n  \
             audience: {audience}\n  enabledRepresentations: [\"ngsi-ld\", \"mcp\"]\n"
        )
    };
    let deployment = |name: &str, extra: &str| {
        format!(
            "apiVersion: joinedcontext.com/v1alpha1\nkind: AssistantDeployment\nmetadata:\n  \
             name: {name}\n  namespace: bbsk\n  title: Dáta kraja\nspec:\n  publicId: bbsk-{name}\n\
             {extra}  allowedOrigins: [https://www.bbsk.sk]\n  \
             rateLimit: {{ requestsPerMinute: 60, perClientPerMinute: 10 }}\n  \
             budget: {{ tokensPerDay: 1000000, tokensPerConversation: 20000 }}\n"
        )
    };
    let connectors = "  connectors:\n    - { endpoint: air, tools: [query_entities] }\n    \
                      - { endpoint: bikes, tools: [query_entities] }\n    \
                      - { endpoint: staff, tools: [query_entities] }\n    \
                      - { endpoint: gone, tools: [query_entities] }\n";
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock after the epoch")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("gw-mcp-deployment-{now}"));
    std::fs::create_dir_all(&dir).expect("create the repository");
    for (file, body) in [
        (
            "space.yaml",
            "apiVersion: joinedcontext.com/v1alpha1\nkind: ContextSpace\nmetadata:\n  name: data\n  \
             namespace: bbsk\nspec:\n  isSandbox: false\n"
                .to_owned(),
        ),
        ("air.yaml", endpoint("air", AIR, "public")),
        ("bikes.yaml", endpoint("bikes", BIKES, "public")),
        ("staff.yaml", endpoint("staff", STAFF, "organization")),
        (
            "data.yaml",
            deployment(
                "data",
                &format!("  channel: public\n  sources: [web]\n{connectors}"),
            ),
        ),
        (
            "plain.yaml",
            deployment(
                "plain",
                "  channel: public\n  connectors:\n    - { endpoint: air, tools: [query_entities] }\n",
            ),
        ),
        (
            "internal.yaml",
            deployment(
                "internal",
                "  channel: internal\n  connectors:\n    - { endpoint: staff, tools: [query_entities] }\n",
            ),
        ),
        (
            "off.yaml",
            deployment(
                "off",
                "  channel: public\n  enabled: false\n  connectors:\n    - { endpoint: air, tools: [query_entities] }\n",
            ),
        ),
    ] {
        std::fs::write(dir.join(file), body).expect("write a manifest");
    }
    dir
}

#[test]
fn the_store_serves_each_enabled_deployment_with_its_connectors_and_sources() {
    let dir = repository();
    let servers = context_gateway::store::load(&dir)
        .expect("the repository loads")
        .6;
    std::fs::remove_dir_all(&dir).expect("clean up");
    let paths: Vec<String> = servers.iter().map(|s| s.path()).collect();
    assert_eq!(
        paths,
        [
            "/api/mcp/d/bbsk-data",
            "/api/mcp/d/bbsk-internal",
            "/api/mcp/d/bbsk-plain"
        ],
        "a disabled deployment serves no MCP"
    );
    let data = &servers[0];
    let mut members = vec![AIR, BIKES, STAFF];
    members.sort_unstable();
    assert_eq!(
        data.members, members,
        "the connector naming no Endpoint is left out"
    );
    assert_eq!(data.audience, Audience::Public);
    assert_eq!(data.title.as_deref(), Some("Dáta kraja"));
    assert_eq!(
        data.knowledge_sources,
        [("bbsk".to_owned(), "web".to_owned())]
    );
    assert_eq!(servers[1].audience, Audience::Organization);
    assert!(servers[2].knowledge_sources.is_empty());
}

/// A stand-in `jc-assistant` answering the sources summary and one search hit.
async fn assistant() -> String {
    let stub = Router::new().fallback(|request: axum::extract::Request| async move {
        axum::Json(if request.uri().path() == "/internal/v1/sources" {
            json!({"passages": 3, "publishers": ["bbsk"], "languages": {"sk": 1.0}})
        } else {
            json!({"hits": []})
        })
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("a free port");
    let url = format!("http://{}", listener.local_addr().expect("an address"));
    tokio::spawn(async move {
        let _ = axum::serve(listener, stub).await;
    });
    url
}

fn endpoint(slug: &str, audience: Audience, role: &str) -> Endpoint {
    let policy: PolicySpec = serde_norway::from_str(&format!(
        "contextSpaceRef: data\nassigner: did:web:bbsk.sk\nassignee: {{ kind: role, id: {role} }}\n\
         operations: [queryEntity, upsertBatch]\ninformation:\n  - entities:\n      - type: Event\n"
    ))
    .expect("the policy spec parses");
    Endpoint {
        declared_types: None,
        roles: Default::default(),
        slug: slug.to_owned(),
        title: Default::default(),
        description: Default::default(),
        space: "data".to_owned(),
        project: "bbsk".to_owned(),
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
        policies: vec![policy],
    }
}

/// The gateway with the deployments the store loads from [`repository`].
async fn app(realm: &common::Realm) -> Router {
    let dir = repository();
    let servers = context_gateway::store::load(&dir)
        .expect("the repository loads")
        .6;
    std::fs::remove_dir_all(&dir).expect("clean up");
    let gateway = Gateway::new(
        Broker::new("http://127.0.0.1:9"),
        Box::new(PolicyPdp),
        "bbsk.sk",
    )
    .serve(vec![
        endpoint(AIR, Audience::Public, "public"),
        endpoint(BIKES, Audience::Public, "public"),
        endpoint(STAFF, Audience::Organization, "steward"),
        endpoint(OUTSIDE, Audience::Public, "public"),
    ])
    .authenticate(
        Arc::new(realm.verifier()),
        ServiceAccounts::new(),
        Some(HOST.to_owned()),
    )
    .search_documents_with(Documents::new(&assistant().await, hub_sources("bbsk/web")));
    gateway.replace_servers(servers);
    router(Arc::new(gateway))
}

/// A steward of `bbsk` with a token for the edge.
fn steward(realm: &common::Realm) -> String {
    realm.mint(&json!({
        "iss": common::ISSUER,
        "sub": "0f5a",
        "aud": "context-gateway",
        "scope": "openid",
        "preferred_username": "jana",
        "groups": ["/bbsk"],
        "realm_access": { "roles": ["steward"] },
        "exp": common::in_seconds(300),
        "iat": common::in_seconds(-10),
    }))
}

async fn rpc(app: &Router, path: &str, token: Option<&str>, body: Value) -> (StatusCode, Value) {
    let mut request = Request::builder()
        .method(Method::POST)
        .uri(path)
        .header("content-type", "application/json")
        .header("x-forwarded-for", "203.0.113.7");
    if let Some(token) = token {
        request = request.header("authorization", format!("Bearer {token}"));
    }
    let request = request
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

fn call(name: &str, arguments: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": 2, "method": "tools/call",
        "params": {"name": name, "arguments": arguments}})
}

fn names(answer: &Value) -> Vec<String> {
    answer["result"]["tools"]
        .as_array()
        .unwrap_or_else(|| panic!("no tool list: {answer}"))
        .iter()
        .filter_map(|tool| tool["name"].as_str().map(str::to_owned))
        .collect()
}

/// The text of a tool answer, the JSON an MCP client reads.
fn text(answer: &Value) -> String {
    answer["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or_else(|| panic!("no tool text: {answer}"))
        .to_owned()
}

#[tokio::test]
async fn a_public_deployment_lists_its_public_endpoints_and_search_documents_only() {
    let realm = common::Realm::new();
    let app = app(&realm).await;
    let (status, tools) = rpc(&app, "/api/mcp/d/bbsk-data", None, list()).await;
    assert_eq!(status, StatusCode::OK, "{tools}");
    let tools = names(&tools);
    assert!(tools.contains(&"search_documents".to_owned()), "{tools:?}");
    let (_, plain) = rpc(&app, "/api/mcp/d/bbsk-plain", None, list()).await;
    assert!(
        !names(&plain).contains(&"search_documents".to_owned()),
        "a deployment without sources has no search_documents: {plain}"
    );

    let (_, listed) = rpc(
        &app,
        "/api/mcp/d/bbsk-data",
        None,
        call("list_endpoints", json!({})),
    )
    .await;
    let listed = text(&listed);
    assert!(listed.contains(AIR) && listed.contains(BIKES), "{listed}");
    assert!(
        !listed.contains(STAFF) && !listed.contains(OUTSIDE),
        "neither an internal connector nor an unnamed Endpoint: {listed}"
    );
}

#[tokio::test]
async fn a_signed_in_caller_gets_no_write_tool_and_no_internal_endpoint() {
    let realm = common::Realm::new();
    let app = app(&realm).await;
    let token = steward(&realm);
    let (status, tools) = rpc(&app, "/api/mcp/d/bbsk-data", Some(&token), list()).await;
    assert_eq!(status, StatusCode::OK, "{tools}");
    let (_, anonymous) = rpc(&app, "/api/mcp/d/bbsk-data", None, list()).await;
    assert_eq!(
        names(&tools),
        names(&anonymous),
        "a token opens no write tool on an assistant's server"
    );
    let (_, listed) = rpc(
        &app,
        "/api/mcp/d/bbsk-data",
        Some(&token),
        call("list_endpoints", json!({})),
    )
    .await;
    assert!(!text(&listed).contains(STAFF), "{listed}");
}

#[tokio::test]
async fn an_endpoint_outside_the_deployment_answers_as_an_unknown_one() {
    let realm = common::Realm::new();
    let app = app(&realm).await;
    let read = |slug: &str| {
        call(
            "query_entities",
            json!({ "endpoint": slug, "type": ["Event"] }),
        )
    };
    let (_, unknown) = rpc(
        &app,
        "/api/mcp/d/bbsk-data",
        None,
        read("zzzzzzzzzzzzzzzzzzzzzzzzzy"),
    )
    .await;
    for outside in [OUTSIDE, STAFF] {
        let (_, answer) = rpc(&app, "/api/mcp/d/bbsk-data", None, read(outside)).await;
        assert_eq!(
            answer
                .to_string()
                .replace(outside, "zzzzzzzzzzzzzzzzzzzzzzzzzy"),
            unknown.to_string(),
            "{outside}"
        );
    }
}

#[tokio::test]
async fn an_internal_deployment_asks_for_a_token_and_an_unknown_one_is_404() {
    let realm = common::Realm::new();
    let app = app(&realm).await;
    let (status, _) = rpc(&app, "/api/mcp/d/bbsk-internal", None, list()).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let (status, _) = rpc(&app, "/api/mcp/d/bbsk-off", None, list()).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "a disabled deployment");
    let (status, _) = rpc(&app, "/api/mcp/d/nobody", None, list()).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}
