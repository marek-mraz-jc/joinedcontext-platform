//! The hub's `endpoint` argument is a string, not an enum of every slug, and takes an Endpoint by
//! its slug, its title or its space (T-4491, ADR-N-025 2.5). `tools/list` is the same size for
//! 111 Endpoints and for 10,000; a name resolves only among the Endpoints the caller may read,
//! and a miss answers the closest of those.

mod common;

use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use axum::Router;
use context_gateway::app::{router, Gateway};
use context_gateway::auth::accounts::ServiceAccounts;
use context_gateway::pdp::PolicyPdp;
use context_gateway::proxy::Broker;
use context_gateway::resolver::Endpoint;
use jc_core::kinds::{Audience, PolicySpec, Representation};
use serde_json::{json, Value};
use std::sync::Arc;
use tower::ServiceExt;

const AIR: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaa";
const INNER: &str = "iiiiiiiiiiiiiiiiiiiiiiiiii";

fn grant(role: &str) -> PolicySpec {
    serde_norway::from_str(&format!(
        "contextSpaceRef: mesto\n\
         assigner: did:web:banskabystrica.sk\n\
         assignee: {{ kind: role, id: {role} }}\n\
         operations: [retrieveOps]\n"
    ))
    .expect("the policy spec parses")
}

fn endpoint(slug: &str, title: &str, space: &str, audience: Audience, role: &str) -> Endpoint {
    Endpoint {
        declared_types: None,
        roles: Default::default(),
        slug: slug.to_owned(),
        title: [("en".to_owned(), title.to_owned())].into_iter().collect(),
        description: Default::default(),
        space: space.to_owned(),
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
        deprecation: None,
        feeds: Vec::new(),
        calendars: Vec::new(),
        map_defaults: Default::default(),
        base_path: format!("/api/endpoint/{slug}"),
        models: Vec::new(),
        policy_names: Vec::new(),
        manifest_name: format!("{space}-data"),
        policies: vec![grant(role)],
    }
}

/// A 26-letter base32 slug of `n`, distinct per `n`.
fn slug(n: usize) -> String {
    const ALPHABET: &[u8; 32] = b"abcdefghijklmnopqrstuvwxyz234567";
    let mut digits = [b'q'; 26];
    let mut rest = n;
    for digit in digits.iter_mut().rev().take(4) {
        *digit = ALPHABET[rest % 32];
        rest /= 32;
    }
    String::from_utf8(digits.to_vec()).expect("ascii")
}

fn app(broker: &str, endpoints: Vec<Endpoint>) -> Router {
    let realm = common::Realm::new();
    router(Arc::new(
        Gateway::new(
            Broker::new(broker),
            Box::new(PolicyPdp),
            "banskabystrica.sk",
        )
        .serve(endpoints)
        .authenticate(
            Arc::new(realm.verifier()),
            ServiceAccounts::new(),
            Some("https://city.example".to_owned()),
        ),
    ))
}

/// The city's three Endpoints: two public, one for the organization only.
fn city(broker: &str) -> Router {
    app(
        broker,
        vec![
            endpoint(AIR, "Air quality", "ovzdusie", Audience::Public, "public"),
            endpoint(
                "bbbbbbbbbbbbbbbbbbbbbbbbbb",
                "Bike stations",
                "doprava",
                Audience::Public,
                "public",
            ),
            endpoint(
                INNER,
                "Internal registry",
                "register",
                Audience::Organization,
                "steward",
            ),
        ],
    )
}

async fn rpc(app: Router, body: Value) -> (StatusCode, String) {
    let request = Request::builder()
        .method(Method::POST)
        .uri("/api/mcp")
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .expect("a request");
    let response = app.oneshot(request).await.expect("the gateway answers");
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 64 * 1024 * 1024)
        .await
        .expect("a body");
    (status, String::from_utf8_lossy(&bytes).into_owned())
}

fn query(endpoint: &str) -> Value {
    json!({
        "jsonrpc": "2.0", "id": 1, "method": "tools/call",
        "params": { "name": "query_entities", "arguments": { "endpoint": endpoint, "type": "AirQualityObserved" } },
    })
}

#[tokio::test]
async fn tools_list_is_the_same_bytes_for_one_endpoint_and_for_10000() {
    let list = json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list" });
    let mut sizes = Vec::new();
    let mut bodies = Vec::new();
    for count in [1, 10_000] {
        let endpoints = (0..count)
            .map(|n| {
                endpoint(
                    &slug(n),
                    &format!("Dataset {n}"),
                    "mesto",
                    Audience::Public,
                    "public",
                )
            })
            .collect();
        let (status, body) = rpc(app("http://127.0.0.1:1", endpoints), list.clone()).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(!body.contains(&slug(0)), "no slug is listed");
        sizes.push(body.len());
        bodies.push(body);
    }
    eprintln!("tools/list bytes: {sizes:?}");
    // Byte for byte the same tools, whatever the number of Endpoints (T-4748).
    assert_eq!(bodies[0], bodies[1]);
    // The six read tools of this grant measured 12,162 bytes once the enum went (73,369 for the
    // fourteen tools of dev with it): a ceiling that only comes down, to 12,000 with T-4491's
    // schema trim measured by the agent benchmark. With get_endpoint and search (T-4492, T-4493)
    // and no copy of each description in its outputSchema it measured 12,831; with the `endpoint`
    // sentence and the output members' meanings said once in the instructions, 11,546 (T-4506).
    // The broker's `summarize` (T-4680, the seventh read tool) adds 1,918 bytes: 13,814, so the
    // ceiling for this tool set is 13,900, and it again only comes down (T-4725). Each tool's
    // human title, at the top and in its annotations as a connector directory requires (T-4732),
    // adds 426 bytes: 14,240. `openWorldHint` as an explicit boolean on each tool, which the
    // directories also require (T-4732), adds 198: 14,438. get_endpoint's `detail` and `type`
    // (T-4748), which keep its default answer short instead of every attribute of every type,
    // add 104: 14,542, so the ceiling is 14,600, and it only comes down.
    // query_entities' limit default and maximum (T-4804), in its schema and one note sentence
    // that no longer repeats the cursor's nextCursor, add 57: 14,599 (T-4812).
    // describe_access does not repeat `liveUpdates` (T-5045): initialize says it (T-5148).
    assert!(sizes[0] < 14_600, "{sizes:?}");
}

#[tokio::test]
async fn a_call_by_title_or_space_is_the_call_by_slug() {
    let broker = common::BrokerStub::start(vec![json!([]), json!([]), json!([]), json!([])]).await;
    let mut answers = Vec::new();
    // T-4748: the readable {project}/{name} too.
    for name in [AIR, "air quality", " Ovzdusie ", "mesto/ovzdusie-data"] {
        let (status, body) = rpc(city(&broker.url), query(name)).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        answers.push(body);
    }
    assert_eq!(answers[0], answers[1]);
    assert_eq!(answers[0], answers[2]);
    assert_eq!(answers[0], answers[3]);
    let hops = broker.hops.lock().expect("hops");
    assert_eq!(hops.len(), 4);
    assert!(hops.iter().all(|hop| hop.tenant == hops[0].tenant));
}

#[tokio::test]
async fn an_unknown_name_answers_the_closest_endpoints_the_caller_may_read() {
    let (status, body) = rpc(city("http://127.0.0.1:1"), query("air qualty")).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(
        body.contains("closest: aaaaaaaaaaaaaaaaaaaaaaaaaa (Air quality)"),
        "{body}"
    );
    assert!(!body.contains(INNER), "{body}");
}

#[tokio::test]
async fn an_endpoint_the_caller_may_not_read_is_not_found_by_its_name_either() {
    // Without a token the organization's Endpoint is out of reach: its exact title resolves to
    // nothing, and the suggestions name only public ones.
    let (status, body) = rpc(city("http://127.0.0.1:1"), query("Internal registry")).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body.contains("no Endpoint of yours"), "{body}");
    let (_, by_name) = rpc(city("http://127.0.0.1:1"), query("mesto/register-data")).await;
    assert!(
        by_name.contains("no Endpoint of yours") && !by_name.contains(INNER),
        "{by_name}"
    );
    assert!(
        !body.contains(INNER) && !body.contains("register"),
        "{body}"
    );
    // Its slug is the login prompt, the bytes an unknown slug gets (SP-20).
    let (inner, inner_body) = rpc(city("http://127.0.0.1:1"), query(INNER)).await;
    let (unknown, unknown_body) = rpc(
        city("http://127.0.0.1:1"),
        query("zzzzzzzzzzzzzzzzzzzzzzzzzy"),
    )
    .await;
    assert_eq!((inner, &inner_body), (unknown, &unknown_body));
    assert_eq!(inner, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn a_name_two_endpoints_share_names_both_and_resolves_neither() {
    let broker = common::BrokerStub::start(vec![json!([])]).await;
    let app = app(
        &broker.url,
        vec![
            endpoint(AIR, "Air quality", "ovzdusie", Audience::Public, "public"),
            endpoint(
                "cccccccccccccccccccccccccc",
                "Air quality",
                "ovzdusie-2",
                Audience::Public,
                "public",
            ),
        ],
    );
    let (status, body) = rpc(app, query("Air quality")).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(
        body.contains(AIR) && body.contains("cccccccccccccccccccccccccc"),
        "{body}"
    );
    assert!(broker.hops.lock().expect("hops").is_empty());
}
