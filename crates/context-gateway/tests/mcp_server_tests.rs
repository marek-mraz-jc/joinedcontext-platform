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
        base_path: format!("/api/endpoint/{slug}"),
        models: Vec::new(),
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
        audience,
        allowed_projects: Vec::new(),
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
    .seal_subscribers_with(common::delivery_key())
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
    call("query_entities", json!({ "type": "AirQualityObserved" }))
}

fn slugs(entries: &Value) -> Vec<String> {
    entries
        .as_array()
        .unwrap_or_else(|| panic!("a list: {entries}"))
        .iter()
        .map(|entry| entry["endpoint"].as_str().expect("a slug").to_owned())
        .collect()
}

/// Section 3, rows 1 and 3: the server answers each readable member exactly what the member's own
/// URL answers the same caller; a member the caller may not read is silent everywhere, and an
/// Endpoint outside the server is never visited.
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

    for entry in structured["results"].as_array().expect("results") {
        let slug = entry["endpoint"].as_str().expect("a slug");
        let (_, direct) = send(
            app.clone(),
            post(
                &format!("/api/endpoint/{slug}/mcp"),
                Some(&edge),
                every_station(),
            ),
        )
        .await;
        assert_eq!(
            entry["entities"], direct["result"]["structuredContent"]["entities"],
            "{slug}"
        );
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
            json!({ "endpoint": slug, "type": "AirQualityObserved" }),
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
    assert_eq!(
        query["inputSchema"]["properties"]["endpoint"]["enum"],
        json!([BIKES, AIR, OPEN])
    );
    assert!(!query["inputSchema"]["required"]
        .as_array()
        .is_some_and(|required| required.contains(&json!("endpoint"))));
    assert!(tool("upsert_entity")["inputSchema"]["required"]
        .as_array()
        .expect("required")
        .contains(&json!("endpoint")));

    let (_, write) = send(
        app.clone(),
        post(
            SERVER,
            Some(&edge),
            call(
                "upsert_entity",
                json!({ "entity": { "id": ENTITY, "type": "AirQualityObserved" } }),
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
                json!({ "type": "AirQualityObserved", "cursor": { AIR: 20 } }),
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
    assert!(hops[0].query.contains("offset=20"), "{}", hops[0].query);
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
