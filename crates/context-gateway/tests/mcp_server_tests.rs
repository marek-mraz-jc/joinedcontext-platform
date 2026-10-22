//! Named MCP servers at `/api/mcp/{project}/{name}`: the hub's catalogue narrowed to the members
//! a `kind: McpServer` names, a read without `endpoint` asking each member apart (T-3155,
//! ADR-N-043, EP-92…EP-96).
//!
//! One test per row of the ADR's security analysis (section 3) and per audience rule of §2.2.
//! The server adds no right: each member call is the member's own, decided by its PDP with the
//! caller's token.

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
use jc_core::kinds::{Audience, PolicySpec, RateLimits, Representation};
use serde_json::{json, Value};
use std::sync::Arc;
use tower::ServiceExt;

const HOST: &str = "https://city.example";
/// A member granted to the steward.
const AIR: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaa";
/// A member granted to the steward.
const BIKES: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbb";
/// A member that admits the steward and grants them nothing.
const CLOSED: &str = "cccccccccccccccccccccccccc";
/// Granted to the steward and no member of the server.
const OUTSIDE: &str = "dddddddddddddddddddddddddd";
/// A public member, read by anybody.
const OPEN: &str = "eeeeeeeeeeeeeeeeeeeeeeeeee";

const SERVER: &str = "/api/mcp/ovzdusie/mobility";
const PUBLIC_SERVER: &str = "/api/mcp/ovzdusie/open";
const LISTED_SERVER: &str = "/api/mcp/ovzdusie/listed";

const ENTITY: &str = "urn:ngsi-ld:AirQualityObserved:banskabystrica.sk:ovzdusie:st-1";

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

fn endpoint(slug: &str, policies: Vec<PolicySpec>) -> Endpoint {
    Endpoint {
        declared_types: None,
        roles: Default::default(),
        slug: slug.to_owned(),
        title: Default::default(),
        description: Default::default(),
        space: "ovzdusie".to_owned(),
        project: "ovzdusie".to_owned(),
        audience: Audience::Organization,
        allowed_projects: Vec::new(),
        representations: vec![Representation::NgsiLd, Representation::Mcp],
        rate_limit: None,
        creates: None,
        file_limits: None,
        hidden_attributes: Default::default(),
        projection: None,
        view_mapping: None,
        catalog: None,
        deprecation: None,
        feeds: Vec::new(),
        base_path: format!("/api/endpoint/{slug}"),
        models: Vec::new(),
        policy_names: Vec::new(),
        manifest_name: String::new(),
        policies,
    }
}

fn server(name: &str, members: &[&str], audience: Audience) -> McpServer {
    McpServer {
        project: "ovzdusie".to_owned(),
        name: name.to_owned(),
        title: Some("Mobility".to_owned()),
        description: Some("The city's mobility data.".to_owned()),
        members: members.iter().map(|slug| (*slug).to_owned()).collect(),
        select: None,
        endpoints: None,
        audience,
        allowed_projects: Vec::new(),
        knowledge_sources: Vec::new(),
    }
}

/// The gateway: three members and one Endpoint outside the server; `limited` gives BIKES a
/// bucket of one request a minute.
fn app(broker: &str, realm: &common::Realm, limited: bool) -> Router {
    let mut bikes = endpoint(BIKES, vec![grant("steward", "queryEntity")]);
    if limited {
        bikes.rate_limit = Some(RateLimits {
            requests_per_minute: 1,
            burst: Some(1),
        });
    }
    let mut open = endpoint(OPEN, vec![grant("public", "queryEntity")]);
    open.audience = Audience::Public;
    let gateway = Gateway::new(
        Broker::new(broker),
        Box::new(PolicyPdp),
        "banskabystrica.sk",
    )
    .serve(vec![
        endpoint(AIR, vec![grant("steward", "queryEntity, upsertBatch")]),
        bikes,
        endpoint(CLOSED, vec![grant("auditor", "queryEntity")]),
        endpoint(OUTSIDE, vec![grant("steward", "queryEntity")]),
        open,
    ])
    .authenticate(
        Arc::new(realm.verifier()),
        ServiceAccounts::new(),
        Some(HOST.to_owned()),
    );
    let mut listed = server("listed", &[AIR], Audience::ProjectList);
    listed.allowed_projects = vec!["doprava".to_owned()];
    gateway.replace_servers(vec![
        server(
            "mobility",
            &[BIKES, CLOSED, AIR, OPEN],
            Audience::Organization,
        ),
        server("open", &[OPEN], Audience::Public),
        listed,
    ]);
    router(Arc::new(gateway))
}

/// A steward of the `ovzdusie` project, for `aud`, in `group`.
fn token_in(realm: &common::Realm, aud: Value, group: &str) -> String {
    realm.mint(&json!({
        "iss": common::ISSUER,
        "sub": "0f5a",
        "aud": aud,
        "scope": format!("openid endpoint:{AIR}"),
        "preferred_username": "jana",
        "groups": [group],
        "realm_access": { "roles": ["steward"] },
        "exp": common::in_seconds(300),
        "iat": common::in_seconds(-10),
    }))
}

fn token(realm: &common::Realm, aud: Value) -> String {
    token_in(realm, aud, "/ovzdusie")
}

fn post(path: &str, token: Option<&str>, body: Value) -> Request<Body> {
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

async fn send(app: Router, request: Request<Body>) -> (StatusCode, Value) {
    let (status, _, body) = raw(app, request).await;
    (status, serde_json::from_str(&body).unwrap_or(Value::Null))
}

fn every_station() -> Value {
    call("query_entities", json!({ "type": ["AirQualityObserved"] }))
}

fn slugs(entries: &Value) -> Vec<String> {
    entries
        .as_array()
        .unwrap_or_else(|| panic!("a list: {entries}"))
        .iter()
        .map(|entry| entry["endpoint"].as_str().expect("a slug").to_owned())
        .collect()
}

/// Section 3, rows 1 and 3: the server answers each readable member; a member the caller may not
/// read is silent everywhere, and an Endpoint outside the server is never visited.
#[tokio::test]
async fn the_server_answers_a_subset_of_the_members_own_urls_and_a_refused_member_is_silent() {
    let realm = common::Realm::new();
    let broker = common::BrokerStub::start(vec![json!([
        { "id": ENTITY, "type": "AirQualityObserved" }
    ])])
    .await;
    let app = app(&broker.url, &realm, false);
    let edge = token(&realm, json!("context-gateway"));

    let (status, answer) = send(app.clone(), post(SERVER, Some(&edge), every_station())).await;
    assert_eq!(status, StatusCode::OK, "{answer}");
    let structured = &answer["result"]["structuredContent"];
    assert_eq!(
        slugs(&structured["results"]),
        [BIKES, AIR, OPEN],
        "{answer}"
    );
    assert!(structured.get("failed").is_none(), "{answer}");

    // Each member's own URL is the broker's view since T-3647; the server forwards there with
    // T-3808, which brings back the byte-for-byte comparison with it.
    for entry in structured["results"].as_array().expect("results") {
        assert!(entry["entities"].is_array(), "{entry}");
    }

    let (_, listed) = send(
        app.clone(),
        post(SERVER, Some(&edge), call("list_endpoints", json!({}))),
    )
    .await;
    let names: Vec<&str> = listed["result"]["structuredContent"]["endpoints"]
        .as_array()
        .expect("a list")
        .iter()
        .map(|entry| entry["slug"].as_str().expect("a slug"))
        .collect();
    assert_eq!(names, [BIKES, AIR, OPEN]);

    // Named, CLOSED and OUTSIDE answer the bytes of a slug nobody serves (SP-20).
    let named = |slug: &str| {
        call(
            "query_entities",
            json!({ "endpoint": slug, "type": ["AirQualityObserved"] }),
        )
    };
    let unknown = raw(
        app.clone(),
        post(SERVER, Some(&edge), named("zzzzzzzzzzzzzzzzzzzzzzzzzy")),
    )
    .await;
    for silent in [CLOSED, OUTSIDE] {
        let answer = raw(app.clone(), post(SERVER, Some(&edge), named(silent))).await;
        assert_eq!(answer.2, unknown.2, "{silent}");
    }
    for body in [&listed.to_string(), &answer.to_string()] {
        assert!(!body.contains(CLOSED) && !body.contains(OUTSIDE), "{body}");
    }
}

/// Section 3, row 4: the same URN in two members stays two entities, each in its member's entry
/// with its Endpoint and space; nothing is merged across members.
#[tokio::test]
async fn the_same_urn_in_two_members_stays_two_entities() {
    let realm = common::Realm::new();
    let broker = common::BrokerStub::start(vec![json!([
        { "id": ENTITY, "type": "AirQualityObserved" }
    ])])
    .await;
    let app = app(&broker.url, &realm, false);
    let edge = token(&realm, json!("context-gateway"));

    let (_, answer) = send(app, post(SERVER, Some(&edge), every_station())).await;
    let results = answer["result"]["structuredContent"]["results"]
        .as_array()
        .expect("results")
        .clone();
    let holding: Vec<(&str, &str)> = results
        .iter()
        .filter(|entry| {
            entry["entities"]
                .as_array()
                .is_some_and(|entities| entities.iter().any(|e| e["id"] == ENTITY))
        })
        .map(|entry| {
            (
                entry["endpoint"].as_str().expect("a slug"),
                entry["space"].as_str().expect("a space"),
            )
        })
        .collect();
    assert_eq!(
        holding,
        [(BIKES, "ovzdusie"), (AIR, "ovzdusie"), (OPEN, "ovzdusie")]
    );
}

/// Section 3, rows 6 and 8: a member that cannot answer is a `failed` entry beside the members
/// that did, and a fan-out spends each member's own bucket.
#[tokio::test]
async fn a_member_that_cannot_answer_makes_a_partial_answer_and_each_bucket_is_spent() {
    let realm = common::Realm::new();
    let broker = common::BrokerStub::start(vec![json!([])]).await;
    let app = app(&broker.url, &realm, true);
    let edge = token(&realm, json!("context-gateway"));

    let (_, first) = send(app.clone(), post(SERVER, Some(&edge), every_station())).await;
    assert!(
        first["result"]["structuredContent"]
            .get("partial")
            .is_none(),
        "{first}"
    );
    let (_, second) = send(app, post(SERVER, Some(&edge), every_station())).await;
    let structured = &second["result"]["structuredContent"];
    assert_eq!(structured["partial"], json!(true), "{second}");
    assert_eq!(slugs(&structured["failed"]), [BIKES]);
    assert_eq!(slugs(&structured["results"]), [AIR, OPEN]);
    assert_eq!(second["result"]["isError"], json!(false));
}

/// Section 3, row 5: a token for one Endpoint, for the hub or for another server is refused at a
/// server; the server's own token reaches its members and is no key to a member's own URL.
#[tokio::test]
async fn only_the_servers_own_token_or_the_edge_opens_it() {
    let realm = common::Realm::new();
    let broker = common::BrokerStub::start(vec![json!([])]).await;
    let app = app(&broker.url, &realm, false);

    for refused in [json!(AIR), json!("mcp-hub"), json!("mcp-ovzdusie-open")] {
        let token = token(&realm, refused.clone());
        let (status, _, _) = raw(app.clone(), post(SERVER, Some(&token), every_station())).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{refused}");
    }

    let own = token(&realm, json!("mcp-ovzdusie-mobility"));
    let (status, answer) = send(app.clone(), post(SERVER, Some(&own), every_station())).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        slugs(&answer["result"]["structuredContent"]["results"]),
        [BIKES, AIR, OPEN],
        "{answer}"
    );
    let url = token(&realm, json!(format!("{HOST}{SERVER}")));
    let (status, _) = send(app.clone(), post(SERVER, Some(&url), every_station())).await;
    assert_eq!(status, StatusCode::OK);

    let (status, _, _) = raw(
        app,
        post(
            &format!("/api/endpoint/{AIR}/mcp"),
            Some(&own),
            every_station(),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

/// §2.2: a public server answers an anonymous caller with its public members; any other names
/// its resource metadata, which names the realm.
#[tokio::test]
async fn a_public_server_answers_nobody_and_another_says_where_to_get_a_token() {
    let realm = common::Realm::new();
    let broker = common::BrokerStub::start(vec![json!([])]).await;
    let app = app(&broker.url, &realm, false);

    let (status, answer) = send(app.clone(), post(PUBLIC_SERVER, None, every_station())).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        slugs(&answer["result"]["structuredContent"]["results"]),
        [OPEN]
    );

    let (status, headers, _) = raw(app.clone(), post(SERVER, None, every_station())).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let challenge = headers["www-authenticate"].to_str().expect("ascii");
    let metadata = format!("{SERVER}/.well-known/oauth-protected-resource");
    assert!(
        challenge.contains(&format!("{HOST}{metadata}")),
        "{challenge}"
    );

    let (status, document) = send(
        app.clone(),
        Request::builder()
            .uri(&metadata)
            .body(Body::empty())
            .expect("a request"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(document["resource"], json!(format!("{HOST}{SERVER}")));
    assert_eq!(document["authorization_servers"], json!([common::ISSUER]));

    let (status, _) = send(
        app,
        post("/api/mcp/ovzdusie/nothing", None, every_station()),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

/// EP-93: a `project-list` server admits its own project and those it names, nobody else.
#[tokio::test]
async fn a_project_list_server_admits_only_its_projects() {
    let realm = common::Realm::new();
    let broker = common::BrokerStub::start(vec![json!([])]).await;
    let app = app(&broker.url, &realm, false);
    let edge = |group| token_in(&realm, json!("context-gateway"), group);

    let (status, _) = send(
        app.clone(),
        post(LISTED_SERVER, Some(&edge("/ovzdusie")), every_station()),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = send(
        app,
        post(LISTED_SERVER, Some(&edge("/skolstvo")), every_station()),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

/// §2.3, §2.5: the catalogue lists the readable members in the manifest's order; a read may leave
/// `endpoint` out, a write may not; `initialize` speaks with the server's own title.
#[tokio::test]
async fn reads_may_fan_out_and_writes_name_their_endpoint() {
    let realm = common::Realm::new();
    let broker = common::BrokerStub::start(vec![json!([])]).await;
    let app = app(&broker.url, &realm, false);
    let edge = token(&realm, json!("context-gateway"));

    let (_, tools) = send(
        app.clone(),
        post(
            SERVER,
            Some(&edge),
            json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list" }),
        ),
    )
    .await;
    let tool = |name: &str| {
        tools["result"]["tools"]
            .as_array()
            .expect("tools")
            .iter()
            .find(|tool| tool["name"] == name)
            .unwrap_or_else(|| panic!("{name} is listed: {tools}"))
            .clone()
    };
    let query = tool("query_entities");
    // A string, never an enum of the members: the list is list_endpoints' (T-4491).
    assert_eq!(
        query["inputSchema"]["properties"]["endpoint"]["type"],
        "string"
    );
    assert!(query["inputSchema"]["properties"]["endpoint"]
        .get("enum")
        .is_none());
    assert!(!query["inputSchema"]["required"]
        .as_array()
        .is_some_and(|required| required.contains(&json!("endpoint"))));
    assert!(tool("batch_upsert")["inputSchema"]["required"]
        .as_array()
        .expect("required")
        .contains(&json!("endpoint")));

    let (_, write) = send(
        app.clone(),
        post(
            SERVER,
            Some(&edge),
            call(
                "batch_upsert",
                json!({ "entities": [{ "id": ENTITY, "type": "AirQualityObserved" }] }),
            ),
        ),
    )
    .await;
    assert_eq!(write["error"]["code"], json!(-32602), "{write}");
    assert!(broker.hops().is_empty(), "a write without endpoint ran");

    let (_, hello) = send(
        app,
        post(
            SERVER,
            Some(&edge),
            json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {} }),
        ),
    )
    .await;
    assert_eq!(hello["result"]["serverInfo"]["title"], json!("Mobility"));
    assert!(hello["result"]["instructions"]
        .as_str()
        .expect("instructions")
        .starts_with("The city's mobility data."));
}

/// §2.4: a cursor object asks only the members it names, each from its own offset.
#[tokio::test]
async fn a_cursor_object_asks_only_the_members_it_names() {
    let realm = common::Realm::new();
    let broker = common::BrokerStub::start(vec![json!([])]).await;
    let app = app(&broker.url, &realm, false);
    let edge = token(&realm, json!("context-gateway"));

    let (_, answer) = send(
        app,
        post(
            SERVER,
            Some(&edge),
            call(
                "query_entities",
                json!({ "type": ["AirQualityObserved"], "cursor": { AIR: "q20" } }),
            ),
        ),
    )
    .await;
    assert_eq!(
        slugs(&answer["result"]["structuredContent"]["results"]),
        [AIR],
        "{answer}"
    );
    let hops = broker.hops();
    assert_eq!(hops.len(), 1);
    assert_eq!(hops[0].view, AIR);
    let sent: Value = serde_json::from_slice(&hops[0].body).expect("a JSON-RPC message");
    assert_eq!(
        sent["params"]["arguments"]["cursor"],
        json!("q20"),
        "{sent}"
    );
}

/// ADR-N-043 §5: the gateway loads each `McpServer` beside the Endpoints, its members resolved to
/// their slugs in the manifest's order; a member that names no Endpoint is left out, and a
/// manifest that breaks MF-53 is not served.
#[test]
fn the_store_loads_each_server_with_its_members_slugs() {
    let endpoint = |name: &str, slug: &str| {
        format!(
            "apiVersion: joinedcontext.com/v1alpha1\nkind: Endpoint\nmetadata:\n  name: {name}\n  \
             namespace: ovzdusie\nspec:\n  contextSpaceRef: ovzdusie\n  slug: {slug}\n  \
             audience: organization\n  enabledRepresentations: [\"ngsi-ld\", \"mcp\"]\n"
        )
    };
    let server = |name: &str, members: &str| {
        format!(
            "apiVersion: joinedcontext.com/v1alpha1\nkind: McpServer\nmetadata:\n  name: {name}\n  \
             namespace: ovzdusie\n  title: Mobility\nspec:\n  audience: organization\n  \
             members: [{members}]\n"
        )
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock after the epoch")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("gw-mcp-server-{now}"));
    std::fs::create_dir_all(&dir).expect("create the repository");
    for (file, body) in [
        (
            "space.yaml",
            "apiVersion: joinedcontext.com/v1alpha1\nkind: ContextSpace\nmetadata:\n  name: ovzdusie\n  \
             namespace: ovzdusie\nspec:\n  isSandbox: false\n"
                .to_owned(),
        ),
        ("air.yaml", endpoint("air", AIR)),
        ("bikes.yaml", endpoint("bikes", BIKES)),
        (
            "mobility.yaml",
            server(
                "mobility",
                "{ kind: Endpoint, name: bikes }, { kind: Endpoint, name: gone }, { kind: Endpoint, name: air }",
            ),
        ),
        (
            "twice.yaml",
            server(
                "twice",
                "{ kind: Endpoint, name: air }, { kind: Endpoint, name: air }",
            ),
        ),
    ] {
        std::fs::write(dir.join(file), body).expect("write a manifest");
    }

    let servers = context_gateway::store::load(&dir)
        .expect("the repository loads")
        .6;
    std::fs::remove_dir_all(&dir).expect("clean up");
    assert_eq!(servers.len(), 1, "{servers:?}");
    assert_eq!(servers[0].name, "mobility");
    assert_eq!(servers[0].title.as_deref(), Some("Mobility"));
    assert_eq!(servers[0].members, [BIKES, AIR]);
    assert_eq!(servers[0].path(), "/api/mcp/ovzdusie/mobility");
}

/// One hop to a member's view, as the broker's MCP surface received it.
struct ViewHop {
    view: String,
    headers: axum::http::HeaderMap,
    message: Value,
}

/// The broker's MCP surfaces of the members' views (T-3808): every view answers a page of one
/// entity, but `refusing`'s view answers the tool error a view without the grant gives.
async fn views(refusing: &'static str) -> (String, Arc<std::sync::Mutex<Vec<ViewHop>>>) {
    let hops: Arc<std::sync::Mutex<Vec<ViewHop>>> = Arc::default();
    let recorder = Arc::clone(&hops);
    let stub = Router::new().fallback(move |request: Request<Body>| {
        let recorder = Arc::clone(&recorder);
        async move {
            let (parts, body) = request.into_parts();
            let message: Value = serde_json::from_slice(
                &axum::body::to_bytes(body, usize::MAX).await.expect("a body"),
            )
            .unwrap_or(Value::Null);
            let view = parts
                .uri
                .path()
                .strip_prefix("/x/views/")
                .and_then(|rest| rest.strip_suffix("/mcp"))
                .unwrap_or_default()
                .to_owned();
            let result = if view == refusing {
                json!({ "isError": true, "content": [{ "type": "text", "text": "this view grants no read of AirQualityObserved" }] })
            } else {
                let page = json!({ "entities": [{ "id": ENTITY, "type": "AirQualityObserved" }], "nextCursor": "opaque-1" });
                json!({ "isError": false, "content": [{ "type": "text", "text": page.to_string() }], "structuredContent": page })
            };
            let answer = json!({ "jsonrpc": "2.0", "id": message["id"], "result": result });
            recorder.lock().expect("the hop log").push(ViewHop {
                view,
                headers: parts.headers,
                message,
            });
            axum::Json(answer)
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("a free port");
    let url = format!("http://{}", listener.local_addr().expect("an address"));
    tokio::spawn(async move {
        let _ = axum::serve(listener, stub).await;
    });
    (url, hops)
}

/// T-3808: a read without `endpoint` asks each readable member's own view in the broker's form,
/// the caller pinned and no token sent; a view that refuses is a `failed` entry with the
/// broker's sentence, and a member the caller cannot read is never asked.
#[tokio::test]
async fn a_fan_out_asks_each_members_view_and_a_refusing_view_is_a_failed_member() {
    let realm = common::Realm::new();
    let (url, hops) = views(BIKES).await;
    let app = app(&url, &realm, false);
    let edge = token(&realm, json!("context-gateway"));

    let (_, answer) = send(app, post(SERVER, Some(&edge), every_station())).await;
    let structured = &answer["result"]["structuredContent"];
    assert_eq!(slugs(&structured["results"]), [AIR, OPEN], "{answer}");
    assert_eq!(slugs(&structured["failed"]), [BIKES]);
    assert_eq!(
        structured["failed"][0]["reason"],
        json!("this view grants no read of AirQualityObserved")
    );
    assert_eq!(structured["partial"], json!(true));
    assert_eq!(
        structured["nextCursor"],
        json!({ AIR: "opaque-1", OPEN: "opaque-1" })
    );

    let hops = hops.lock().expect("the hop log");
    let mut asked: Vec<&str> = hops.iter().map(|hop| hop.view.as_str()).collect();
    asked.sort_unstable();
    assert_eq!(
        asked,
        [AIR, BIKES, OPEN],
        "CLOSED grants nothing and is never asked"
    );
    for hop in hops.iter() {
        assert_eq!(hop.headers["ngsild-tenant"], "ovzdusie");
        assert!(
            hop.headers.contains_key("x-jc-subject"),
            "the caller is pinned"
        );
        assert!(
            !hop.headers.contains_key("authorization"),
            "a token reached the broker"
        );
        assert_eq!(hop.message["params"]["name"], json!("query_entities"));
        assert_eq!(
            hop.message["params"]["arguments"],
            json!({ "type": ["AirQualityObserved"] }),
            "the broker's form, no routing argument"
        );
    }
}

// --- T-4530: a server's `select`, `org/all` and the fan-out bound (ADR-N-043 §7, EP-120…EP-122) ---

/// The gateway over `endpoints` with `servers` alone.
fn serving(
    broker: &str,
    realm: &common::Realm,
    endpoints: Vec<Endpoint>,
    servers: Vec<McpServer>,
) -> Router {
    let gateway = Gateway::new(
        Broker::new(broker),
        Box::new(PolicyPdp),
        "banskabystrica.sk",
    )
    .serve(endpoints)
    .authenticate(
        Arc::new(realm.verifier()),
        ServiceAccounts::new(),
        Some(HOST.to_owned()),
    );
    gateway.replace_servers(servers);
    router(Arc::new(gateway))
}

/// The steward's five Endpoints of `app`: two granted, one granting nothing, one outside any
/// member list, one public.
fn five() -> Vec<Endpoint> {
    let mut open = endpoint(OPEN, vec![grant("public", "queryEntity")]);
    open.audience = Audience::Public;
    vec![
        endpoint(AIR, vec![grant("steward", "queryEntity")]),
        endpoint(BIKES, vec![grant("steward", "queryEntity")]),
        endpoint(CLOSED, vec![grant("auditor", "queryEntity")]),
        endpoint(OUTSIDE, vec![grant("steward", "queryEntity")]),
        open,
    ]
}

fn selecting(name: &str, select: Value, audience: Audience) -> McpServer {
    McpServer {
        select: Some(serde_json::from_value(select).expect("a select")),
        ..server(name, &[], audience)
    }
}

async fn listed(app: &Router, path: &str, token: Option<&str>) -> (Vec<String>, String) {
    let (status, _, body) = raw(
        app.clone(),
        post(path, token, call("list_endpoints", json!({}))),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let answer: Value = serde_json::from_str(&body).expect("json");
    let names = answer["result"]["structuredContent"]["endpoints"]
        .as_array()
        .unwrap_or_else(|| panic!("a list: {answer}"))
        .iter()
        .map(|entry| entry["slug"].as_str().expect("a slug").to_owned())
        .collect();
    (names, body)
}

/// EP-120: a selector reaches the Endpoints the caller reads that it matches, never one the
/// caller cannot read, and says nothing of the rest; `accessLevel` narrows it further.
#[tokio::test]
async fn a_selector_reaches_only_what_the_caller_reads() {
    let realm = common::Realm::new();
    let broker = common::BrokerStub::start(vec![json!([])]).await;
    let app = serving(
        &broker.url,
        &realm,
        five(),
        vec![
            selecting(
                "picked",
                json!({ "projects": ["ovzdusie"] }),
                Audience::Organization,
            ),
            selecting(
                "open-only",
                json!({ "accessLevel": ["public"] }),
                Audience::Organization,
            ),
            selecting(
                "elsewhere",
                json!({ "projects": ["doprava"] }),
                Audience::Organization,
            ),
        ],
    );
    let edge = token(&realm, json!("context-gateway"));

    let (names, body) = listed(&app, "/api/mcp/ovzdusie/picked", Some(&edge)).await;
    assert_eq!(names, [AIR, BIKES, OUTSIDE, OPEN]);
    assert!(!body.contains(CLOSED), "{body}");
    // Each selected Endpoint is one the steward reads at its own URL: the selector added none.
    for slug in &names {
        let own = format!("/api/endpoint/{slug}/mcp");
        let (status, _) = send(
            app.clone(),
            post(&own, Some(&edge), call("list_types", json!({}))),
        )
        .await;
        assert_ne!(status, StatusCode::FORBIDDEN, "{slug}");
    }

    let (names, _) = listed(&app, "/api/mcp/ovzdusie/open-only", Some(&edge)).await;
    assert_eq!(names, [OPEN]);
    let (names, _) = listed(&app, "/api/mcp/ovzdusie/elsewhere", Some(&edge)).await;
    assert!(names.is_empty(), "{names:?}");
}

/// EP-120: a public server selects public Endpoints only, for a caller with a token too.
#[tokio::test]
async fn a_public_server_selects_public_endpoints_only() {
    let realm = common::Realm::new();
    let broker = common::BrokerStub::start(vec![json!([])]).await;
    let app = serving(
        &broker.url,
        &realm,
        five(),
        vec![selecting("everything", json!({}), Audience::Public)],
    );
    let edge = token(&realm, json!("context-gateway"));
    for caller in [None, Some(edge.as_str())] {
        let (names, body) = listed(&app, "/api/mcp/ovzdusie/everything", caller).await;
        assert_eq!(names, [OPEN], "{body}");
        for hidden in [AIR, BIKES, CLOSED, OUTSIDE] {
            assert!(!body.contains(hidden), "{body}");
        }
    }
}

/// EP-121, EP-122: `org/all` is served without a manifest to an organization member and to
/// nobody without a token; a read without `endpoint` over more than ten Endpoints is refused,
/// pointing to list_endpoints and the catalogue, and one naming its Endpoint still answers.
#[tokio::test]
async fn org_all_is_served_and_a_fan_out_over_ten_is_refused() {
    let realm = common::Realm::new();
    let broker = common::BrokerStub::start(vec![json!([])]).await;
    let eleven: Vec<Endpoint> = (0..11)
        .map(|i| {
            let mut open = endpoint(
                &format!("{:a<26}", format!("f{i:02}")),
                vec![grant("steward", "queryEntity")],
            );
            open.audience = Audience::Organization;
            open
        })
        .collect();
    let first = eleven[0].slug.clone();
    let app = serving(&broker.url, &realm, eleven, Vec::new());
    let edge = token(&realm, json!("context-gateway"));

    let (names, _) = listed(&app, "/api/mcp/org/all", Some(&edge)).await;
    assert_eq!(names.len(), 11);
    let (status, _) = send(
        app.clone(),
        post("/api/mcp/org/all", None, call("list_endpoints", json!({}))),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    let (_, answer) = send(
        app.clone(),
        post("/api/mcp/org/all", Some(&edge), every_station()),
    )
    .await;
    assert_eq!(answer["result"]["isError"], true, "{answer}");
    let text = answer["result"]["content"][0]["text"]
        .as_str()
        .expect("a sentence");
    assert!(
        text.contains("11 Endpoints")
            && text.contains("list_endpoints")
            && text.contains("`catalog`"),
        "{text}"
    );

    let one = call(
        "query_entities",
        json!({ "endpoint": first, "type": ["AirQualityObserved"] }),
    );
    let (_, answer) = send(app.clone(), post("/api/mcp/org/all", Some(&edge), one)).await;
    assert_ne!(answer["result"]["isError"], true, "{answer}");
    // Another organization server keeps its own name: org/all is the fallback, not a prefix.
    let (status, _) = send(
        app,
        post(
            "/api/mcp/org/everything",
            Some(&edge),
            call("list_endpoints", json!({})),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

/// EP-121: resolving a selector over 10,000 Endpoints, timed for the task (T-4530):
/// `cargo test --release -p context-gateway --test mcp_server_tests ten_thousand -- --ignored --nocapture`.
#[tokio::test]
#[ignore = "a timing, run by hand in release"]
async fn ten_thousand_endpoints_resolve_a_selector() {
    let realm = common::Realm::new();
    let broker = common::BrokerStub::start(vec![json!([])]).await;
    let endpoints: Vec<Endpoint> = (0..10_000)
        .map(|i| {
            let mut one = endpoint(&format!("{i:a>26}"), vec![grant("steward", "queryEntity")]);
            one.space = format!("space-{}", i % 100);
            one
        })
        .collect();
    let app = serving(
        &broker.url,
        &realm,
        endpoints,
        vec![selecting(
            "tenth",
            json!({ "spaces": (0..10).map(|i| format!("ovzdusie/space-{i}")).collect::<Vec<_>>() }),
            Audience::Organization,
        )],
    );
    let edge = token(&realm, json!("context-gateway"));
    for (path, want) in [
        ("/api/mcp/org/all", 10_000),
        ("/api/mcp/ovzdusie/tenth", 1_000),
    ] {
        let mut took = Vec::new();
        for _ in 0..21 {
            let started = std::time::Instant::now();
            let (_, body) = listed(&app, path, Some(&edge)).await;
            took.push(started.elapsed());
            let answer: Value = serde_json::from_str(&body).expect("json");
            assert_eq!(answer["result"]["structuredContent"]["total"], want);
        }
        took.sort();
        println!(
            "{path}: list_endpoints p50 {:?}, p95 {:?}",
            took[10], took[19]
        );
    }
}

// --- T-4538: a person's own connectors at /api/mcp/me/{name} (EP-123, UI-98) ---

/// The Portal's internal read: each `sub`'s list, `down` answering 503, every call counted.
struct Portal {
    url: String,
    down: Arc<std::sync::atomic::AtomicBool>,
    calls: Arc<std::sync::atomic::AtomicUsize>,
}

async fn portal(lists: Value) -> Portal {
    use std::sync::atomic::Ordering;
    let down = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let (is_down, counted) = (Arc::clone(&down), Arc::clone(&calls));
    let app = Router::new().route(
        "/internal/connectors/{sub}",
        axum::routing::get(
            move |axum::extract::Path(sub): axum::extract::Path<String>| {
                let (lists, is_down, counted) =
                    (lists.clone(), Arc::clone(&is_down), Arc::clone(&counted));
                async move {
                    counted.fetch_add(1, Ordering::SeqCst);
                    if is_down.load(Ordering::SeqCst) {
                        return (StatusCode::SERVICE_UNAVAILABLE, axum::Json(json!({})))
                            .into_response();
                    }
                    let connectors = lists.get(&sub).cloned().unwrap_or_else(|| json!([]));
                    axum::Json(json!({ "connectors": connectors })).into_response()
                }
            },
        ),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("a port");
    let address = listener.local_addr().expect("an address");
    tokio::spawn(async move { axum::serve(listener, app).await });
    Portal {
        url: format!("http://{address}/internal/connectors"),
        down,
        calls,
    }
}
use axum::response::IntoResponse;

/// The gateway over `five()` reading connectors from `portal`, held `keep`.
fn personal(
    broker: &str,
    realm: &common::Realm,
    portal: &Portal,
    keep: std::time::Duration,
) -> Router {
    let gateway = Gateway::new(
        Broker::new(broker),
        Box::new(PolicyPdp),
        "banskabystrica.sk",
    )
    .serve(five())
    .authenticate(
        Arc::new(realm.verifier()),
        ServiceAccounts::new(),
        Some(HOST.to_owned()),
    )
    .read_personal_connectors(context_gateway::mcp::personal::Personal::new(
        portal.url.clone(),
        None,
        keep,
    ));
    router(Arc::new(gateway))
}

/// The steward `sub` with the edge audience.
fn person(realm: &common::Realm, sub: &str) -> String {
    realm.mint(&json!({
        "iss": common::ISSUER,
        "sub": sub,
        "aud": "context-gateway",
        "preferred_username": sub,
        "groups": ["/ovzdusie"],
        "realm_access": { "roles": ["steward"] },
        "exp": common::in_seconds(300),
        "iat": common::in_seconds(-10),
    }))
}

/// EP-123: a person's connector serves their favourites as far as they read them; another
/// person at the same address gets the bytes of an unknown server, and nothing names whose it is.
#[tokio::test]
async fn a_personal_connector_answers_its_owner_alone() {
    let realm = common::Realm::new();
    let broker = common::BrokerStub::start(vec![json!([])]).await;
    let portal = portal(json!({
        "ana": [{ "name": "my-air", "select": { "endpoints": [AIR, CLOSED, OPEN] } }],
        "bob": [{ "name": "other", "select": {} }],
    }))
    .await;
    let app = personal(
        &broker.url,
        &realm,
        &portal,
        std::time::Duration::from_secs(60),
    );
    let (ana, bob) = (person(&realm, "ana"), person(&realm, "bob"));

    let (names, body) = listed(&app, "/api/mcp/me/my-air", Some(&ana)).await;
    assert_eq!(
        names,
        [AIR, OPEN],
        "a favourite the person cannot read is out: {body}"
    );
    assert!(!body.contains(CLOSED) && !body.contains(BIKES), "{body}");

    let (status, _, theirs) = raw(
        app.clone(),
        post(
            "/api/mcp/me/my-air",
            Some(&bob),
            call("list_endpoints", json!({})),
        ),
    )
    .await;
    let (_, _, nobodys) = raw(
        app.clone(),
        post(
            "/api/mcp/me/nothing-here",
            Some(&bob),
            call("list_endpoints", json!({})),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(
        theirs, nobodys,
        "another person's connector answers as an unknown server"
    );

    // Without a token: a challenge naming the address's metadata, which says nothing of whose.
    let (status, headers, _) = raw(
        app.clone(),
        post(
            "/api/mcp/me/my-air",
            None,
            call("list_endpoints", json!({})),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let challenge = headers
        .get("www-authenticate")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    assert!(
        challenge.contains("/api/mcp/me/my-air/.well-known/oauth-protected-resource"),
        "{challenge}"
    );
    for name in ["my-air", "nothing-here"] {
        let request = Request::builder()
            .uri(format!(
                "/api/mcp/me/{name}/.well-known/oauth-protected-resource"
            ))
            .body(Body::empty())
            .expect("a request");
        let (status, metadata) = send(app.clone(), request).await;
        assert_eq!(status, StatusCode::OK, "{name}");
        assert_eq!(metadata["resource"], format!("{HOST}/api/mcp/me/{name}"));
    }
}

/// EP-123: a list is held at most its hold, a Portal that cannot be read answers 503 and never
/// an older list.
#[tokio::test]
async fn a_held_list_expires_and_an_unreadable_portal_is_a_503() {
    use std::sync::atomic::Ordering;
    let realm = common::Realm::new();
    let broker = common::BrokerStub::start(vec![json!([])]).await;
    let portal =
        portal(json!({ "ana": [{ "name": "my-air", "select": { "endpoints": [AIR] } }] })).await;
    let app = personal(
        &broker.url,
        &realm,
        &portal,
        std::time::Duration::from_millis(200),
    );
    let ana = person(&realm, "ana");

    let (names, _) = listed(&app, "/api/mcp/me/my-air", Some(&ana)).await;
    assert_eq!(names, [AIR]);
    let (names, _) = listed(&app, "/api/mcp/me/my-air", Some(&ana)).await;
    assert_eq!(names, [AIR]);
    assert_eq!(
        portal.calls.load(Ordering::SeqCst),
        1,
        "the second call is held"
    );

    portal.down.store(true, Ordering::SeqCst);
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    let (status, problem) = send(
        app.clone(),
        post(
            "/api/mcp/me/my-air",
            Some(&ana),
            call("list_endpoints", json!({})),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{problem}");
    assert_eq!(
        problem["type"], "https://joinedcontext.com/errors/service-unavailable",
        "a catalogued type (API/00 §4)"
    );
    assert_eq!(portal.calls.load(Ordering::SeqCst), 2);
}
