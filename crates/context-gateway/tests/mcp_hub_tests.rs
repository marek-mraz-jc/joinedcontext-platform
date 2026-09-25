//! The MCP hub at `/api/mcp`: one connector over several Endpoints, the Endpoint named on every
//! call, nothing remembered (T-2490, ADR-N-025, EP-87, EP-88).
//!
//! One test per row of the ADR's security analysis (section 3) and per audience rule of its
//! section 4; the audit row is `mcp_hub_audit_tests.rs`, alone in its binary because it installs
//! the process's subscriber. The hub adds no right: each call is the named Endpoint's own, decided by its PDP
//! with the caller's token.

mod common;

use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use axum::Router;
use context_gateway::app::{router, Gateway};
use context_gateway::auth::accounts::ServiceAccounts;
use context_gateway::pdp::PolicyPdp;
use context_gateway::proxy::Broker;
use context_gateway::resolver::{Endpoint, Model};
use jc_core::kinds::{Audience, PolicySpec, RateLimits, Representation};
use serde_json::{json, Value};
use std::sync::Arc;
use tower::ServiceExt;

const HOST: &str = "https://city.example";
/// Granted to the steward: reads and subscriptions.
const AIR: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaa";
/// Granted to the steward: reads only.
const BIKES: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbb";
/// Admits the steward and grants them nothing.
const CLOSED: &str = "cccccccccccccccccccccccccc";
/// Grants the steward, but only for its own project, which is not theirs.
const FOREIGN: &str = "dddddddddddddddddddddddddd";
/// Grants the steward, and has turned MCP off.
const QUIET: &str = "eeeeeeeeeeeeeeeeeeeeeeeeee";
/// Nothing serves this.
const UNKNOWN: &str = "zzzzzzzzzzzzzzzzzzzzzzzzzy";

fn policy(yaml: &str) -> PolicySpec {
    serde_norway::from_str(yaml).expect("the policy spec parses")
}

fn grant(role: &str, operations: &str) -> PolicySpec {
    policy(&format!(
        "contextSpaceRef: ovzdusie\n\
         assigner: did:web:banskabystrica.sk\n\
         assignee: {{ kind: role, id: {role} }}\n\
         operations: [{operations}]\n\
         information:\n  \
         - entities:\n      \
         - type: AirQualityObserved\n"
    ))
}

fn endpoint(slug: &str, policies: Vec<PolicySpec>) -> Endpoint {
    Endpoint {
        declared_types: None,
        roles: Default::default(),
        slug: slug.to_owned(),
        title: [("en".to_owned(), format!("Endpoint {}", &slug[..1]))]
            .into_iter()
            .collect(),
        description: Default::default(),
        space: "ovzdusie".to_owned(),
        project: "ovzdusie".to_owned(),
        audience: Audience::Organization,
        allowed_projects: Vec::new(),
        representations: vec![Representation::NgsiLd, Representation::Mcp],
        rate_limit: None,
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
        policies,
    }
}

fn endpoints() -> Vec<Endpoint> {
    let mut foreign = endpoint(FOREIGN, vec![grant("steward", "queryEntity")]);
    foreign.project = "doprava".to_owned();
    foreign.audience = Audience::ProjectList;
    let mut quiet = endpoint(QUIET, vec![grant("steward", "queryEntity")]);
    quiet.representations = vec![Representation::NgsiLd];
    vec![
        endpoint(
            AIR,
            vec![grant("steward", "queryEntity, createSubscription")],
        ),
        endpoint(BIKES, vec![grant("steward", "queryEntity")]),
        endpoint(CLOSED, vec![grant("auditor", "queryEntity")]),
        foreign,
        quiet,
    ]
}

fn app_limited(broker: &str, realm: &common::Realm, hub: Option<RateLimits>) -> Router {
    let mut gateway = Gateway::new(
        Broker::new(broker),
        Box::new(PolicyPdp),
        "banskabystrica.sk",
    )
    .serve(endpoints())
    .seal_subscribers_with(common::delivery_key())
    .authenticate(
        Arc::new(realm.verifier()),
        ServiceAccounts::new(),
        Some(HOST.to_owned()),
    );
    if let Some(limits) = hub {
        gateway = gateway.limit_hub_to(limits);
    }
    router(Arc::new(gateway))
}

fn app(broker: &str, realm: &common::Realm) -> Router {
    app_limited(broker, realm, None)
}

/// A steward of the `ovzdusie` project, for `aud`, with `scope`.
fn steward(realm: &common::Realm, sub: &str, aud: Value, scope: &str) -> String {
    realm.mint(&json!({
        "iss": common::ISSUER,
        "sub": sub,
        "aud": aud,
        "scope": scope,
        "azp": "mcp-hub",
        "preferred_username": format!("jana-{sub}"),
        "groups": ["/ovzdusie"],
        "realm_access": { "roles": ["steward"] },
        "exp": common::in_seconds(300),
        "iat": common::in_seconds(-10),
    }))
}

/// A person signed in at the edge: every Endpoint whose Policy grants them.
fn edge(realm: &common::Realm) -> String {
    steward(realm, "0f5a", json!("context-gateway"), "openid")
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

fn hub(token: Option<&str>, body: Value) -> Request<Body> {
    post("/api/mcp", token, body)
}

fn call(name: &str, arguments: Value) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": { "name": name, "arguments": arguments },
    })
}

fn list() -> Value {
    json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list" })
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

/// The slugs `list_endpoints` answers, and the `endpoint` enum of one tool.
async fn listed(app: Router, token: &str) -> (Vec<String>, Vec<String>) {
    let (_, answer) = send(
        app.clone(),
        hub(Some(token), call("list_endpoints", json!({}))),
    )
    .await;
    let slugs = answer["result"]["structuredContent"]["endpoints"]
        .as_array()
        .unwrap_or_else(|| panic!("no list: {answer}"))
        .iter()
        .map(|entry| entry["slug"].as_str().expect("a slug").to_owned())
        .collect();
    let (_, tools) = send(app, hub(Some(token), list())).await;
    let enumerated = tools["result"]["tools"]
        .as_array()
        .expect("tools")
        .iter()
        .find(|tool| tool["name"] == "query_entities")
        .map(|tool| {
            tool["inputSchema"]["properties"]["endpoint"]["enum"]
                .as_array()
                .expect("an enum")
                .iter()
                .map(|slug| slug.as_str().expect("a slug").to_owned())
                .collect()
        })
        .unwrap_or_default();
    (slugs, enumerated)
}

fn query(endpoint: &str) -> Value {
    call(
        "query_entities",
        json!({ "endpoint": endpoint, "type": "AirQualityObserved" }),
    )
}

/// Section 3, row 1: an Endpoint the token may not read is absent from the list and the enum,
/// and naming it answers the bytes an unknown slug gets, whatever the reason it is out: no
/// grant, not the caller's project, MCP turned off (SP-20).
#[tokio::test]
async fn an_endpoint_the_token_may_not_read_is_never_listed_and_answers_like_an_unknown_one() {
    let realm = common::Realm::new();
    let broker = common::BrokerStub::start(vec![json!([])]).await;
    let app = app(&broker.url, &realm);
    let token = edge(&realm);

    let (slugs, enumerated) = listed(app.clone(), &token).await;
    assert_eq!(slugs, [AIR, BIKES]);
    assert_eq!(enumerated, [AIR, BIKES]);

    let unknown = raw(app.clone(), hub(Some(&token), query(UNKNOWN))).await;
    assert_eq!(unknown.0, StatusCode::OK);
    for outside in [CLOSED, FOREIGN, QUIET] {
        let answer = raw(app.clone(), hub(Some(&token), query(outside))).await;
        assert_eq!(answer.2, unknown.2, "{outside}");
        assert!(!answer.2.contains(outside), "{}", answer.2);
    }
    assert!(
        broker.hops().is_empty(),
        "nothing outside the list reached the broker"
    );
}

/// Section 3, row 2: `endpoint` is one string. A list is invalid input, and no other argument
/// can carry a second slug or a space (AG-05, SP-14).
#[tokio::test]
async fn endpoint_is_one_string_and_no_other_argument_routes() {
    let realm = common::Realm::new();
    let broker = common::BrokerStub::start(vec![json!([])]).await;
    let app = app(&broker.url, &realm);
    let token = edge(&realm);

    for arguments in [
        json!({ "endpoint": [AIR, BIKES], "type": "AirQualityObserved" }),
        json!({ "endpoint": { "slug": AIR }, "type": "AirQualityObserved" }),
        json!({ "type": "AirQualityObserved" }),
        json!({ "endpoint": AIR, "slug": BIKES, "type": "AirQualityObserved" }),
        json!({ "endpoint": AIR, "space": "doprava", "type": "AirQualityObserved" }),
        json!({ "endpoint": AIR, "Tenant": "doprava", "type": "AirQualityObserved" }),
    ] {
        let (status, answer) = send(
            app.clone(),
            hub(Some(&token), call("query_entities", arguments.clone())),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            answer["error"]["code"],
            json!(-32602),
            "{arguments}: {answer}"
        );
    }

    let (_, tools) = send(app, hub(Some(&token), list())).await;
    let query = tools["result"]["tools"]
        .as_array()
        .expect("tools")
        .iter()
        .find(|tool| tool["name"] == "query_entities")
        .expect("listed")
        .clone();
    assert_eq!(
        query["inputSchema"]["properties"]["endpoint"]["type"],
        json!("string")
    );
    assert!(query["inputSchema"]["required"]
        .as_array()
        .expect("required")
        .contains(&json!("endpoint")));
    assert!(
        broker.hops().is_empty(),
        "a refused call reached the broker"
    );
}

/// Section 3, row 3: an elicitation asked through one Endpoint does not confirm a call on
/// another; on its own Endpoint it does, once.
#[tokio::test]
async fn an_elicitation_asked_on_one_endpoint_does_not_confirm_a_call_on_another() {
    let realm = common::Realm::new();
    let broker = common::BrokerStub::start(vec![json!({})]).await;
    let app = app(&broker.url, &realm);
    // A second Endpoint that grants subscriptions too, so only the binding can refuse.
    let token = edge(&realm);
    let subscription = json!({
        "type": "Subscription",
        "entities": [{ "type": "AirQualityObserved" }],
        "notification": { "endpoint": { "uri": "https://example.org/hook" } }
    });

    let (_, asked) = send(
        app.clone(),
        hub(
            Some(&token),
            call(
                "create_subscription",
                json!({ "endpoint": AIR, "subscription": subscription }),
            ),
        ),
    )
    .await;
    let elicitation_id = asked["result"]["structuredContent"]["elicitation"]["elicitationId"]
        .as_str()
        .unwrap_or_else(|| panic!("no question: {asked}"))
        .to_owned();

    let answered = |endpoint: &str| {
        json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": {
            "name": "create_subscription",
            "arguments": { "endpoint": endpoint, "subscription": subscription },
            "elicitation": { "elicitationId": elicitation_id, "action": "accept" },
        }})
    };

    // BIKES does not grant subscriptions: the answer is the SP-17 tool error, and the question
    // stays unanswered.
    let (_, elsewhere) = send(app.clone(), hub(Some(&token), answered(BIKES))).await;
    assert_eq!(elsewhere["result"]["isError"], json!(true), "{elsewhere}");
    assert!(
        broker.hops().is_empty(),
        "an answer given on BIKES created something"
    );

    let (_, created) = send(app.clone(), hub(Some(&token), answered(AIR))).await;
    assert_eq!(created["result"]["isError"], json!(false), "{created}");
    assert_eq!(broker.hops().len(), 1);

    let (_, again) = send(app, hub(Some(&token), answered(AIR))).await;
    assert_eq!(
        again["result"]["isError"],
        json!(true),
        "a spent answer was reused: {again}"
    );
    assert_eq!(broker.hops().len(), 1);
}

/// Section 3, row 3 again, where both Endpoints grant the write: the question is bound to the
/// Endpoint it was asked on, so the other Endpoint's façade does not know it.
#[tokio::test]
async fn a_question_is_bound_to_its_endpoint_even_where_both_grant_the_write() {
    let realm = common::Realm::new();
    let broker = common::BrokerStub::start(vec![json!({})]).await;
    let both = vec![
        endpoint(
            AIR,
            vec![grant("steward", "queryEntity, createSubscription")],
        ),
        endpoint(
            BIKES,
            vec![grant("steward", "queryEntity, createSubscription")],
        ),
    ];
    let app = router(Arc::new(
        Gateway::new(
            Broker::new(&broker.url),
            Box::new(PolicyPdp),
            "banskabystrica.sk",
        )
        .serve(both)
        .seal_subscribers_with(common::delivery_key())
        .authenticate(
            Arc::new(realm.verifier()),
            ServiceAccounts::new(),
            Some(HOST.to_owned()),
        ),
    ));
    let token = edge(&realm);
    let subscription = json!({
        "type": "Subscription",
        "entities": [{ "type": "AirQualityObserved" }],
        "notification": { "endpoint": { "uri": "https://example.org/hook" } }
    });
    let (_, asked) = send(
        app.clone(),
        hub(
            Some(&token),
            call(
                "create_subscription",
                json!({ "endpoint": AIR, "subscription": subscription }),
            ),
        ),
    )
    .await;
    let elicitation_id = asked["result"]["structuredContent"]["elicitation"]["elicitationId"]
        .as_str()
        .unwrap_or_else(|| panic!("no question: {asked}"))
        .to_owned();

    let (_, crossed) = send(
        app,
        hub(
            Some(&token),
            json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": {
                "name": "create_subscription",
                "arguments": { "endpoint": BIKES, "subscription": subscription },
                "elicitation": { "elicitationId": elicitation_id, "action": "accept" },
            }}),
        ),
    )
    .await;
    assert_eq!(crossed["result"]["isError"], json!(true), "{crossed}");
    assert!(
        broker.hops().is_empty(),
        "AIR's answer created a subscription on BIKES"
    );
}

/// Section 3, row 4: a burst spread over several Endpoints spends one per-subject bucket, and
/// another subject's bucket is their own.
#[tokio::test]
async fn a_burst_spread_over_endpoints_spends_one_subject_bucket() {
    let realm = common::Realm::new();
    let broker = common::BrokerStub::start(vec![json!([])]).await;
    let limits = RateLimits {
        requests_per_minute: 60,
        burst: Some(3),
    };
    let app = app_limited(&broker.url, &realm, Some(limits));
    let token = edge(&realm);

    for endpoint in [AIR, BIKES, AIR] {
        let (status, _, body) = raw(app.clone(), hub(Some(&token), query(endpoint))).await;
        assert_eq!(status, StatusCode::OK, "{body}");
    }
    let (status, headers, _) = raw(app.clone(), hub(Some(&token), query(BIKES))).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    assert!(headers.contains_key("retry-after"));

    let other = steward(&realm, "7c1d", json!("context-gateway"), "openid");
    let (status, _, _) = raw(app, hub(Some(&other), query(BIKES))).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "another subject has a bucket of their own"
    );
}

/// Section 3, row 4, the Endpoint's side: a call through the hub spends the Endpoint's own
/// `(slug, caller)` bucket, the one a call to its URL spends.
#[tokio::test]
async fn a_call_through_the_hub_spends_the_endpoints_own_bucket() {
    let realm = common::Realm::new();
    let broker = common::BrokerStub::start(vec![json!([])]).await;
    let mut air = endpoint(AIR, vec![grant("steward", "queryEntity")]);
    air.rate_limit = Some(RateLimits {
        requests_per_minute: 60,
        burst: Some(1),
    });
    let app = router(Arc::new(
        Gateway::new(
            Broker::new(&broker.url),
            Box::new(PolicyPdp),
            "banskabystrica.sk",
        )
        .serve([air])
        .authenticate(
            Arc::new(realm.verifier()),
            ServiceAccounts::new(),
            Some(HOST.to_owned()),
        ),
    ));
    let token = edge(&realm);
    let (status, _, _) = raw(app.clone(), hub(Some(&token), query(AIR))).await;
    assert_eq!(status, StatusCode::OK);
    let (status, _, _) = raw(
        app,
        post(
            &format!("/api/endpoint/{AIR}/mcp"),
            Some(&token),
            call("query_entities", json!({ "type": "AirQualityObserved" })),
        ),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::TOO_MANY_REQUESTS,
        "the hub call spent AIR's burst"
    );
}

/// Section 4: a token whose audience is one Endpoint lists and reaches that Endpoint alone, and
/// its reach is not widened by sending it to the hub (PF-45, PF-46).
#[tokio::test]
async fn a_token_for_one_endpoint_reaches_that_endpoint_only() {
    let realm = common::Realm::new();
    let broker = common::BrokerStub::start(vec![json!([])]).await;
    let app = app(&broker.url, &realm);
    let token = steward(&realm, "0f5a", json!(AIR), "openid");

    let (slugs, enumerated) = listed(app.clone(), &token).await;
    assert_eq!(slugs, [AIR]);
    assert_eq!(enumerated, [AIR]);
    let unknown = raw(app.clone(), hub(Some(&token), query(UNKNOWN))).await;
    let bikes = raw(app.clone(), hub(Some(&token), query(BIKES))).await;
    assert_eq!(bikes.2, unknown.2);
    let (_, air) = send(app, hub(Some(&token), query(AIR))).await;
    assert_eq!(air["result"]["isError"], json!(false), "{air}");
}

/// Section 4: a hub token reaches only the Endpoints its `endpoint:{slug}` scopes pick; a scope
/// grants nothing, so a picked Endpoint without a grant is still not listed (EP-88).
#[tokio::test]
async fn a_hub_token_reaches_only_the_endpoints_its_scopes_pick() {
    let realm = common::Realm::new();
    let broker = common::BrokerStub::start(vec![json!([])]).await;
    let app = app(&broker.url, &realm);
    let token = steward(
        &realm,
        "0f5a",
        json!(["mcp-hub", "account"]),
        &format!("openid endpoint:{BIKES} endpoint:{CLOSED} endpoint:"),
    );

    let (slugs, enumerated) = listed(app.clone(), &token).await;
    assert_eq!(slugs, [BIKES]);
    assert_eq!(enumerated, [BIKES]);
    let unknown = raw(app.clone(), hub(Some(&token), query(UNKNOWN))).await;
    let air = raw(app.clone(), hub(Some(&token), query(AIR))).await;
    assert_eq!(air.2, unknown.2, "AIR was not picked");

    // The call re-enters the NGSI-LD path with the same token, through the hub's door.
    let (_, bikes) = send(app, hub(Some(&token), query(BIKES))).await;
    assert_eq!(bikes["result"]["isError"], json!(false), "{bikes}");
    assert_eq!(broker.hops().len(), 1);

    let nothing = steward(&realm, "0f5a", json!("mcp-hub"), "openid");
    let (slugs, enumerated) = listed(app_limited(&broker.url, &realm, None), &nothing).await;
    assert!(
        slugs.is_empty() && enumerated.is_empty(),
        "no scope picks nothing"
    );
}

/// Section 4: a hub token is not a key to any Endpoint's own URL, MCP or REST.
#[tokio::test]
async fn a_hub_token_opens_no_endpoint_url() {
    let realm = common::Realm::new();
    let broker = common::BrokerStub::start(vec![json!([])]).await;
    let app = app(&broker.url, &realm);
    let token = steward(
        &realm,
        "0f5a",
        json!("mcp-hub"),
        &format!("endpoint:{BIKES}"),
    );

    let (status, _, _) = raw(
        app.clone(),
        post(&format!("/api/endpoint/{BIKES}/mcp"), Some(&token), list()),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let rest = Request::builder()
        .uri(format!(
            "/api/endpoint/{BIKES}/ngsi-ld/v1/entities?type=AirQualityObserved"
        ))
        .header("authorization", format!("Bearer {token}"))
        .body(Body::empty())
        .expect("a request");
    let (status, _, _) = raw(app, rest).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert!(broker.hops().is_empty());
}

/// Section 4 and 2.7: the edge audience reaches through the hub what it reaches Endpoint by
/// Endpoint, and the per-Endpoint URL answers exactly as before.
#[tokio::test]
async fn the_hub_answers_what_the_endpoint_url_answers() {
    let realm = common::Realm::new();
    let entity = json!([{ "id": "urn:ngsi-ld:AirQualityObserved:banskabystrica.sk:ovzdusie:st-1", "type": "AirQualityObserved" }]);
    let broker = common::BrokerStub::start(vec![entity]).await;
    let app = app(&broker.url, &realm);
    let token = edge(&realm);

    let (_, through_hub) = send(app.clone(), hub(Some(&token), query(AIR))).await;
    let (_, direct) = send(
        app,
        post(
            &format!("/api/endpoint/{AIR}/mcp"),
            Some(&token),
            call("query_entities", json!({ "type": "AirQualityObserved" })),
        ),
    )
    .await;
    assert_eq!(through_hub["result"], direct["result"]);
    assert_eq!(
        through_hub["result"]["isError"],
        json!(false),
        "{through_hub}"
    );
}

/// Section 2.8: a tool is listed when one Endpoint of the list grants it; naming an Endpoint
/// that does not grant it is the SP-17 tool error naming the Endpoint and the operation.
#[tokio::test]
async fn a_tool_another_endpoint_grants_is_refused_by_name_on_this_one() {
    let realm = common::Realm::new();
    let broker = common::BrokerStub::start(vec![json!({})]).await;
    let app = app(&broker.url, &realm);
    let token = edge(&realm);

    let (_, tools) = send(app.clone(), hub(Some(&token), list())).await;
    let names: Vec<&str> = tools["result"]["tools"]
        .as_array()
        .expect("tools")
        .iter()
        .filter_map(|tool| tool["name"].as_str())
        .collect();
    assert!(names.contains(&"list_endpoints") && names.contains(&"create_subscription"));
    assert_eq!(
        names
            .iter()
            .filter(|name| **name == "query_entities")
            .count(),
        1,
        "one catalogue, never one per Endpoint"
    );

    let (_, refused) = send(
        app,
        hub(
            Some(&token),
            call(
                "create_subscription",
                json!({ "endpoint": BIKES, "subscription": { "type": "Subscription" } }),
            ),
        ),
    )
    .await;
    assert_eq!(refused["result"]["isError"], json!(true), "{refused}");
    let text = refused["result"]["content"][0]["text"]
        .as_str()
        .expect("text");
    assert!(
        text.contains(BIKES) && text.contains("createSubscription"),
        "{text}"
    );
    assert!(broker.hops().is_empty());
}

/// AG-32, RFC 9728: without a token the hub says where to get one, and its metadata names the
/// hub as the resource.
#[tokio::test]
async fn a_client_without_a_token_is_told_where_to_get_one() {
    let realm = common::Realm::new();
    let app = app("http://127.0.0.1:1", &realm);
    let (status, headers, _) = raw(app.clone(), hub(None, list())).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(
        headers["www-authenticate"],
        format!("Bearer resource_metadata=\"{HOST}/api/mcp/.well-known/oauth-protected-resource\"")
            .as_str()
    );
    let metadata = Request::builder()
        .uri("/api/mcp/.well-known/oauth-protected-resource")
        .body(Body::empty())
        .expect("a request");
    let (status, document) = send(app, metadata).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(document["resource"], json!(format!("{HOST}/api/mcp")));
    assert_eq!(document["authorization_servers"], json!([common::ISSUER]));
}

/// `list_endpoints` pages like `list_types`, and describes only what the caller may see.
#[tokio::test]
async fn list_endpoints_pages_and_describes_each_endpoint() {
    let realm = common::Realm::new();
    let app = app("http://127.0.0.1:1", &realm);
    let token = edge(&realm);

    let (_, first) = send(
        app.clone(),
        hub(Some(&token), call("list_endpoints", json!({ "limit": 1 }))),
    )
    .await;
    let page = &first["result"]["structuredContent"];
    assert_eq!(page["total"], json!(2), "{first}");
    assert_eq!(page["nextCursor"], json!(1));
    let entry = &page["endpoints"][0];
    assert_eq!(entry["slug"], json!(AIR));
    assert_eq!(entry["space"], json!("ovzdusie"));
    assert_eq!(entry["types"], json!(["AirQualityObserved"]));
    assert_eq!(
        entry["mcp"],
        json!(format!("{HOST}/api/endpoint/{AIR}/mcp"))
    );
    assert_eq!(
        entry["schema"],
        json!(format!(
            "{HOST}/api/endpoint/{AIR}/schema/v1/model.linkml.yaml"
        ))
    );

    let (_, last) = send(
        app.clone(),
        hub(
            Some(&token),
            call("list_endpoints", json!({ "limit": 1, "cursor": 1 })),
        ),
    )
    .await;
    assert_eq!(
        last["result"]["structuredContent"]["endpoints"][0]["slug"],
        json!(BIKES)
    );
    assert!(last["result"]["structuredContent"]
        .get("nextCursor")
        .is_none());

    for arguments in [
        json!({ "limit": 0 }),
        json!({ "limit": "5" }),
        json!({ "space": "x" }),
    ] {
        let (_, refused) = send(
            app.clone(),
            hub(Some(&token), call("list_endpoints", arguments.clone())),
        )
        .await;
        assert_eq!(
            refused["result"]["isError"],
            json!(true),
            "{arguments}: {refused}"
        );
    }
}
