//! The subject the broker decides on is the one the gateway verified (T-3641, map G15, K36).
//!
//! The broker's `joinedcontext` engine reads the caller from `x-jc-subject`, `x-jc-groups`,
//! `x-jc-roles`, `x-jc-client` and `x-jc-restricted`, and treats a request with none of them as
//! the cluster's own. So every hop to the broker carries the verified values, none the client
//! wrote, at least `x-jc-roles` even for an anonymous caller, and never the caller's
//! `Authorization` or `Cookie`: the broker is not the audience of either.

mod common;

use axum::body::Body;
use axum::http::{HeaderMap, Method, Request, StatusCode};
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

const HOST: &str = "https://city.example";
const AIR: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaa";
const OPEN: &str = "eeeeeeeeeeeeeeeeeeeeeeeeee";
const ENTITY: &str = "urn:ngsi-ld:AirQualityObserved:st-1";

/// What a client writes to pass for somebody else.
const FORGED: &[(&str, &str)] = &[
    ("x-jc-subject", "admin"),
    ("X-JC-Groups", "/admins"),
    ("x-jc-roles", "admin"),
    ("x-jc-client", "portal-api"),
    ("x-jc-restricted", "true"),
    ("cookie", "session=stolen"),
];

fn grant(role: &str) -> PolicySpec {
    serde_norway::from_str(&format!(
        "contextSpaceRef: ovzdusie\n\
         assigner: did:web:banskabystrica.sk\n\
         assignee: {{ kind: role, id: {role} }}\n\
         operations: [queryEntity, createEntity]\n\
         information:\n  \
         - entities:\n      \
         - type: AirQualityObserved\n"
    ))
    .expect("the policy spec parses")
}

fn endpoint(slug: &str, audience: Audience, role: &str) -> Endpoint {
    Endpoint {
        declared_types: None,
        roles: Default::default(),
        slug: slug.to_owned(),
        title: Default::default(),
        description: Default::default(),
        space: "ovzdusie".to_owned(),
        project: "ovzdusie".to_owned(),
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
        policies: vec![grant(role)],
    }
}

fn app(broker: &str, realm: &common::Realm) -> Router {
    let gateway = Gateway::new(
        Broker::new(broker),
        Box::new(PolicyPdp),
        "banskabystrica.sk",
    )
    .serve(vec![
        endpoint(AIR, Audience::Organization, "steward"),
        endpoint(OPEN, Audience::Public, "public"),
    ])
    .seal_subscribers_with(common::delivery_key())
    .authenticate(
        Arc::new(realm.verifier()),
        ServiceAccounts::new(),
        Some(HOST.to_owned()),
    );
    router(Arc::new(gateway))
}

fn token(realm: &common::Realm, groups: Value) -> String {
    realm.mint(&json!({
        "iss": common::ISSUER,
        "sub": "0f5a",
        "aud": "context-gateway",
        "preferred_username": "jana",
        "groups": groups,
        "realm_access": { "roles": ["steward"] },
        "exp": common::in_seconds(300),
        "iat": common::in_seconds(-10),
    }))
}

fn request(
    method: Method,
    path: &str,
    token: Option<&str>,
    extra: &[(&str, &str)],
    body: Option<Value>,
) -> Request<Body> {
    let mut builder = Request::builder().method(method).uri(path);
    if let Some(token) = token {
        builder = builder.header("authorization", format!("Bearer {token}"));
    }
    for (name, value) in FORGED.iter().chain(extra) {
        builder = builder.header(*name, *value);
    }
    let body = match body {
        Some(body) => {
            builder = builder.header("content-type", "application/json");
            Body::from(body.to_string())
        }
        None => Body::empty(),
    };
    builder.body(body).expect("a request")
}

async fn status(app: Router, request: Request<Body>) -> StatusCode {
    let response = app.oneshot(request).await.expect("the gateway answers");
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .expect("a body");
    assert!(
        status.is_success(),
        "{status}: {}",
        String::from_utf8_lossy(&bytes)
    );
    status
}

/// Every value of `name` the hop carried.
fn values(headers: &HeaderMap, name: &str) -> Vec<String> {
    headers
        .get_all(name)
        .iter()
        .map(|value| String::from_utf8_lossy(value.as_bytes()).into_owned())
        .collect()
}

fn list(headers: &HeaderMap, name: &str) -> Vec<String> {
    let mut members: Vec<String> = values(headers, name)
        .iter()
        .flat_map(|value| value.split(',').map(str::to_owned).collect::<Vec<_>>())
        .collect();
    members.sort();
    members
}

/// What reached the broker carries the verified caller and nothing the client wrote; groups as
/// the gateway matches them, without the leading slash.
fn assert_verified(headers: &HeaderMap, groups: &[&str]) {
    assert_eq!(values(headers, "x-jc-subject"), ["jana"], "{headers:?}");
    assert_eq!(list(headers, "x-jc-groups"), groups, "{headers:?}");
    let roles = list(headers, "x-jc-roles");
    assert!(roles.contains(&"steward".to_owned()), "{headers:?}");
    assert!(!roles.contains(&"admin".to_owned()), "{headers:?}");
    assert!(values(headers, "x-jc-client").is_empty(), "{headers:?}");
    assert!(values(headers, "x-jc-restricted").is_empty(), "{headers:?}");
    assert!(headers.get("authorization").is_none(), "{headers:?}");
    assert!(headers.get("cookie").is_none(), "{headers:?}");
}

fn query() -> String {
    format!("/api/endpoint/{AIR}/ngsi-ld/v1/entities?type=AirQualityObserved")
}

#[tokio::test]
async fn a_query_carries_the_verified_subject_and_not_the_forged_one() {
    let realm = common::Realm::new();
    let broker = common::BrokerStub::start(vec![json!([])]).await;
    let edge = token(&realm, json!(["/ovzdusie", "/doprava"]));

    status(
        app(&broker.url, &realm),
        request(Method::GET, &query(), Some(&edge), &[], None),
    )
    .await;

    let hops = broker.hops();
    assert!(!hops.is_empty());
    for hop in hops {
        assert_verified(&hop.headers, &["doprava", "ovzdusie"]);
    }
}

#[tokio::test]
async fn a_write_carries_the_verified_subject_and_not_the_token() {
    let realm = common::Realm::new();
    let broker = common::BrokerStub::start(vec![json!([])]).await;
    let edge = token(&realm, json!(["/ovzdusie"]));
    let entity = json!({ "id": ENTITY, "type": "AirQualityObserved" });

    status(
        app(&broker.url, &realm),
        request(
            Method::POST,
            &format!("/api/endpoint/{AIR}/ngsi-ld/v1/entities"),
            Some(&edge),
            &[],
            Some(entity),
        ),
    )
    .await;

    let hops = broker.hops();
    assert_eq!(hops.len(), 1, "{hops:?}");
    assert_verified(&hops[0].headers, &["ovzdusie"]);
}

#[tokio::test]
async fn an_anonymous_caller_still_names_its_roles_so_the_broker_never_reads_it_as_internal() {
    let realm = common::Realm::new();
    let broker = common::BrokerStub::start(vec![json!([])]).await;

    status(
        app(&broker.url, &realm),
        request(
            Method::GET,
            &format!("/api/endpoint/{OPEN}/ngsi-ld/v1/entities?type=AirQualityObserved"),
            None,
            &[],
            None,
        ),
    )
    .await;

    let hops = broker.hops();
    assert_eq!(hops.len(), 1, "{hops:?}");
    let headers = &hops[0].headers;
    assert!(values(headers, "x-jc-subject").is_empty(), "{headers:?}");
    assert!(values(headers, "x-jc-client").is_empty(), "{headers:?}");
    assert!(values(headers, "x-jc-groups").is_empty(), "{headers:?}");
    assert_eq!(list(headers, "x-jc-roles"), ["public"], "{headers:?}");
    assert!(headers.get("cookie").is_none(), "{headers:?}");
}

#[tokio::test]
async fn the_restricted_signal_is_forwarded_only_when_the_caller_opted_in() {
    let realm = common::Realm::new();
    let broker = common::BrokerStub::start(vec![json!([])]).await;
    let edge = token(&realm, json!(["/ovzdusie"]));

    status(
        app(&broker.url, &realm),
        request(
            Method::GET,
            &query(),
            Some(&edge),
            &[("NGSILD-Results-Restricted", "true")],
            None,
        ),
    )
    .await;

    let hops = broker.hops();
    assert_eq!(hops.len(), 1, "{hops:?}");
    assert_eq!(values(&hops[0].headers, "x-jc-restricted"), ["true"]);
}

#[tokio::test]
async fn a_group_whose_name_holds_a_comma_is_not_split_into_two_groups() {
    let realm = common::Realm::new();
    let broker = common::BrokerStub::start(vec![json!([])]).await;
    // The broker splits the list on commas: forwarded as is, this one group would hand the
    // caller `/admins` as well.
    let edge = token(&realm, json!(["/ovzdusie", "/x,/admins"]));

    status(
        app(&broker.url, &realm),
        request(Method::GET, &query(), Some(&edge), &[], None),
    )
    .await;

    let hops = broker.hops();
    assert_eq!(hops.len(), 1, "{hops:?}");
    assert_eq!(list(&hops[0].headers, "x-jc-groups"), ["ovzdusie"]);
}

#[tokio::test]
async fn a_tool_call_reenters_with_the_verified_subject_and_not_the_token() {
    let realm = common::Realm::new();
    let broker = common::BrokerStub::start(vec![json!([
        { "id": ENTITY, "type": "AirQualityObserved" }
    ])])
    .await;
    let edge = token(&realm, json!(["/ovzdusie"]));
    let call = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": {
            "name": "query_entities",
            "arguments": { "type": "AirQualityObserved" },
        },
    });

    status(
        app(&broker.url, &realm),
        request(
            Method::POST,
            &format!("/api/endpoint/{AIR}/mcp"),
            Some(&edge),
            &[],
            Some(call),
        ),
    )
    .await;

    let hops = broker.hops();
    assert!(!hops.is_empty());
    for hop in hops {
        assert_verified(&hop.headers, &["ovzdusie"]);
    }
}
