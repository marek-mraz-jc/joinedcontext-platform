//! T-3999 (ADR-N-047 §2.4, PF-48, PF-113, PF-114): a read over a space whose registration asks
//! for `identity: caller` carries a token of the caller the gateway exchanged for the member
//! broker's audience, and the member's gateway admits that token for a read and nothing else.
//!
//! The hub side: the hop carries the exchanged token and never the caller's own; a refused
//! exchange is a 502, a missing or silent realm a 503, a source outside the platform a 501, and
//! none of them forwards anything. The member side: a token naming only the broker's audience
//! reads as the person it names when the gateway's own client minted it, and writes nothing.

mod common;

use axum::body::Body;
use axum::extract::Form;
use axum::http::{header, Method, Request, StatusCode};
use axum::routing::post;
use axum::{Json, Router};
use context_gateway::app::{router, Gateway};
use context_gateway::auth::accounts::ServiceAccounts;
use context_gateway::federation::exchange::Exchanger;
use context_gateway::federation::{federations_of, Federations};
use context_gateway::pdp::PolicyPdp;
use context_gateway::proxy::Broker;
use context_gateway::resolver::Endpoint;
use context_gateway::store::SpaceBrokers;
use jc_core::kinds::{Audience, PolicySpec, Representation};
use jc_core::{ClientCredential, Secret};
use jc_manifest::loader::Repository;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tower::ServiceExt;

const PROJECT: &str = "helsinki";
const HUB: &str = "hub";
const HUB_SLUG: &str = "h4y7pq2mzt6vhx3nbwrs5cjd8f";
const MEMBER_SLUG: &str = "m4y7pq2mzt6vhx3nbwrs5cjd8f";
/// An Endpoint on the member's broker that no registration names.
const BYSTANDER_SLUG: &str = "b4y7pq2mzt6vhx3nbwrs5cjd8f";
const EXCHANGED: &str = "exchanged-for-archive";

fn policy(space: &str) -> PolicySpec {
    serde_norway::from_str(&format!(
        "contextSpaceRef: {space}\nassigner: did:web:hel.fi\nassignee: {{ kind: role, id: public }}\n\
         operations: [queryEntity, retrieveEntity, createEntity]\n\
         information:\n  - entities:\n      - type: Vehicle\n"
    ))
    .expect("the policy parses")
}

/// The person may write and subscribe with their own token, so a refusal of the exchanged one is
/// the federation's limit and not a missing grant.
fn writer(space: &str) -> PolicySpec {
    serde_norway::from_str(&format!(
        "contextSpaceRef: {space}\nassigner: did:web:hel.fi\nassignee: {{ kind: group, id: helsinki }}\n\
         operations: [createEntity, updateAttrs, createSubscription, querySubscription]\n\
         information:\n  - entities:\n      - type: Vehicle\n"
    ))
    .expect("the policy parses")
}

fn endpoint(slug: &str, space: &str, audience: Audience) -> Endpoint {
    Endpoint {
        declared_types: None,
        roles: Default::default(),
        slug: slug.to_owned(),
        title: Default::default(),
        description: Default::default(),
        space: space.to_owned(),
        project: PROJECT.to_owned(),
        audience,
        allowed_projects: Vec::new(),
        representations: vec![Representation::NgsiLd],
        rate_limit: None,
        creates: None,
        file_limits: None,
        hidden_attributes: Default::default(),
        projection: None,
        base_path: format!("/api/endpoint/{slug}"),
        models: Vec::new(),
        view_mapping: None,
        catalog: None,
        deprecation: None,
        feeds: Vec::new(),
        calendars: Vec::new(),
        map_defaults: Default::default(),
        policy_names: Vec::new(),
        manifest_name: String::new(),
        policies: vec![policy(space), writer(space)],
    }
}

/// The repository's registrations: one on the hub for the member Endpoint, asking `caller`,
/// or for an address elsewhere when `external`.
fn federations(external: bool) -> Federations {
    static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let dir = std::env::temp_dir().join(format!(
        "jc-exchange-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    let write = |path: &str, body: String| {
        let path = dir.join(path);
        std::fs::create_dir_all(path.parent().expect("a parent")).expect("a directory");
        std::fs::write(path, body).expect("written");
    };
    for space in [HUB, "air"] {
        write(
            &format!("projects/{PROJECT}/spaces/{space}/space.yaml"),
            format!(
                "apiVersion: joinedcontext.com/v1alpha1\nkind: ContextSpace\nmetadata:\n  \
                 name: {space}\n  namespace: {PROJECT}\nspec:\n  urnSegment: {space}\n"
            ),
        );
    }
    write(
        &format!("projects/{PROJECT}/spaces/air/endpoints/air-all.yaml"),
        format!(
            "apiVersion: joinedcontext.com/v1alpha1\nkind: Endpoint\nmetadata:\n  name: air-all\n  \
             namespace: {PROJECT}\nspec:\n  contextSpaceRef: air\n  slug: {MEMBER_SLUG}\n  \
             audience: public\n  enabledRepresentations: [\"ngsi-ld\"]\n"
        ),
    );
    let target = if external {
        "  endpoint: https://other.example/ngsi-ld/v1".to_owned()
    } else {
        "  endpointRef: { kind: Endpoint, name: air-all }".to_owned()
    };
    write(
        &format!("projects/{PROJECT}/spaces/{HUB}/registrations/air.yaml"),
        format!(
            "apiVersion: joinedcontext.com/v1alpha1\nkind: ContextSourceRegistration\nmetadata:\n  \
             name: air\n  namespace: {PROJECT}\nspec:\n  contextSpaceRef: {HUB}\n{target}\n  \
             information:\n    - entities:\n        - type: Vehicle\n  federation:\n    identity: caller\n"
        ),
    );
    let table = federations_of(&Repository::load(&dir).expect("the repository loads"));
    let _ = std::fs::remove_dir_all(&dir);
    table
}

type Asked = Arc<Mutex<Vec<HashMap<String, String>>>>;

/// A realm token endpoint answering every exchange with `status`.
async fn realm_answering(status: StatusCode) -> (String, Asked) {
    let asked: Asked = Arc::default();
    let kept = Arc::clone(&asked);
    let app = Router::new().route(
        "/token",
        post(move |Form(form): Form<HashMap<String, String>>| {
            let kept = Arc::clone(&kept);
            async move {
                kept.lock().expect("lock").push(form);
                (status, Json(json!({ "access_token": EXCHANGED })))
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let url = format!("http://{}/token", listener.local_addr().expect("addr"));
    tokio::spawn(async move { axum::serve(listener, app).await });
    (url, asked)
}

fn person(realm: &common::Realm, audience: Value, azp: &str) -> String {
    realm.mint(&json!({
        "iss": common::ISSUER,
        "sub": "f:1:jana",
        "aud": audience,
        "azp": azp,
        "preferred_username": "jana@hel.fi",
        "groups": [PROJECT],
        "exp": common::in_seconds(300),
        "iat": common::in_seconds(-10),
    }))
}

/// The broker the member's space lives on: named after the stub's port, so the address
/// template `http://127.0.0.1:{broker}` reaches the stub (as many_brokers_tests.rs does).
fn broker_of(url: &str) -> String {
    url.trim_end_matches('/')
        .rsplit(':')
        .next()
        .expect("a port")
        .to_owned()
}

/// The gateway: the hub and the member served, the member on the stub's broker, and an
/// exchanger at `token_url` when one is given.
fn gateway(
    realm: &common::Realm,
    broker: &str,
    table: Federations,
    token_url: Option<String>,
) -> Arc<Gateway> {
    let gateway = Gateway::new(
        Broker::new(broker).with_template(Some("http://127.0.0.1:{broker}".to_owned())),
        Box::new(PolicyPdp),
        "hel.fi",
    )
    .serve([
        endpoint(HUB_SLUG, HUB, Audience::Public),
        endpoint(MEMBER_SLUG, "air", Audience::Organization),
        endpoint(BYSTANDER_SLUG, "air", Audience::Organization),
    ])
    .authenticate(Arc::new(realm.verifier()), ServiceAccounts::new(), None);
    let gateway = match token_url {
        Some(url) => gateway.exchange_with(Exchanger::new(
            url,
            "context-gateway".to_owned(),
            ClientCredential::Secret(Secret::new("gateway-secret")),
        )),
        None => gateway,
    };
    gateway.replace_federation(table);
    let mut homes = SpaceBrokers::default();
    homes
        .by_view
        .insert(MEMBER_SLUG.to_owned(), broker_of(broker));
    homes
        .by_view
        .insert(BYSTANDER_SLUG.to_owned(), broker_of(broker));
    homes.by_space.insert("air".to_owned(), broker_of(broker));
    gateway.broker.route(&homes);
    Arc::new(gateway)
}

async fn send(
    gateway: Arc<Gateway>,
    method: Method,
    slug: &str,
    token: Option<&str>,
) -> (StatusCode, Value) {
    let mut request = Request::builder().method(method.clone()).uri(format!(
        "/api/endpoint/{slug}/ngsi-ld/v1/entities?type=Vehicle"
    ));
    if let Some(token) = token {
        request = request.header(header::AUTHORIZATION, format!("Bearer {token}"));
    }
    let body = if method == Method::POST {
        request = request.header(header::CONTENT_TYPE, "application/json");
        Body::from(
            json!({ "id": format!("urn:ngsi-ld:Vehicle:{}", "bus-1"), "type": "Vehicle" })
                .to_string(),
        )
    } else {
        Body::empty()
    };
    let response = router(gateway)
        .oneshot(request.body(body).expect("a request"))
        .await
        .expect("the gateway answers");
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .expect("a body");
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

fn authorization_of(hop: &common::Hop) -> Option<String> {
    hop.headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned)
}

#[tokio::test]
async fn the_hop_carries_the_token_exchanged_for_the_member_broker_and_never_the_callers() {
    let realm = common::Realm::new();
    let broker = common::BrokerStub::start(vec![json!([])]).await;
    let (url, asked) = realm_answering(StatusCode::OK).await;
    let caller = person(&realm, json!([HUB_SLUG, "context-gateway"]), "edge");

    let (status, body) = send(
        gateway(&realm, &broker.url, federations(false), Some(url)),
        Method::GET,
        HUB_SLUG,
        Some(&caller),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let hops = broker.hops.lock().expect("hops").clone();
    assert_eq!(hops.len(), 1, "one hop");
    assert_eq!(
        authorization_of(&hops[0]).as_deref(),
        Some(format!("Bearer {EXCHANGED}").as_str())
    );
    let asked = asked.lock().expect("lock").clone();
    assert_eq!(asked.len(), 1);
    assert_eq!(asked[0]["subject_token"], caller);
    assert_eq!(
        asked[0]["scope"],
        format!("context-broker-{}", broker_of(&broker.url))
    );
    assert_eq!(asked[0]["client_id"], "context-gateway");
}

#[tokio::test]
async fn a_failed_exchange_answers_502_or_503_and_forwards_nothing() {
    let realm = common::Realm::new();
    let caller = person(&realm, json!(HUB_SLUG), "edge");

    for (realm_status, expected) in [
        (StatusCode::BAD_REQUEST, StatusCode::BAD_GATEWAY),
        (
            StatusCode::SERVICE_UNAVAILABLE,
            StatusCode::SERVICE_UNAVAILABLE,
        ),
    ] {
        let broker = common::BrokerStub::start(vec![json!([])]).await;
        let (url, _) = realm_answering(realm_status).await;
        let (status, body) = send(
            gateway(&realm, &broker.url, federations(false), Some(url)),
            Method::GET,
            HUB_SLUG,
            Some(&caller),
        )
        .await;
        assert_eq!(status, expected, "{body}");
        assert!(
            body["type"]
                .as_str()
                .is_some_and(|t| t.ends_with("federation-exchange-failed"))
                || body["code"] == "federation-exchange-failed",
            "{body}"
        );
        assert!(!body.to_string().contains(&caller), "{body}");
        assert!(broker.hops.lock().expect("hops").is_empty());
    }

    // No exchanger configured: the same 503, never a hop as somebody else.
    let broker = common::BrokerStub::start(vec![json!([])]).await;
    let (status, _) = send(
        gateway(&realm, &broker.url, federations(false), None),
        Method::GET,
        HUB_SLUG,
        Some(&caller),
    )
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert!(broker.hops.lock().expect("hops").is_empty());
}

#[tokio::test]
async fn an_anonymous_caller_is_forwarded_without_a_token_and_an_outside_source_is_501() {
    let realm = common::Realm::new();
    let broker = common::BrokerStub::start(vec![json!([])]).await;
    let (url, asked) = realm_answering(StatusCode::OK).await;
    let (status, body) = send(
        gateway(&realm, &broker.url, federations(false), Some(url.clone())),
        Method::GET,
        HUB_SLUG,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let hops = broker.hops.lock().expect("hops").clone();
    assert_eq!(authorization_of(&hops[0]), None);
    assert!(asked.lock().expect("lock").is_empty());

    let broker = common::BrokerStub::start(vec![json!([])]).await;
    let caller = person(&realm, json!(HUB_SLUG), "edge");
    let (status, _) = send(
        gateway(&realm, &broker.url, federations(true), Some(url)),
        Method::GET,
        HUB_SLUG,
        Some(&caller),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_IMPLEMENTED);
    assert!(broker.hops.lock().expect("hops").is_empty());
}

#[tokio::test]
async fn the_member_admits_the_exchanged_token_for_a_read_and_refuses_it_a_write() {
    let realm = common::Realm::new();
    let broker = common::BrokerStub::start(vec![json!([])]).await;
    let exchanged = person(
        &realm,
        json!(format!("context-broker-{}", broker_of(&broker.url))),
        "context-gateway",
    );
    let member = || gateway(&realm, &broker.url, federations(false), None);

    let (status, body) = send(member(), Method::GET, MEMBER_SLUG, Some(&exchanged)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, body) = send(member(), Method::POST, MEMBER_SLUG, Some(&exchanged)).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert_eq!(
        broker.hops.lock().expect("hops").len(),
        1,
        "the write never reached the broker"
    );
}

#[tokio::test]
async fn a_broker_audience_token_of_another_client_or_another_broker_is_refused() {
    let realm = common::Realm::new();
    let broker = common::BrokerStub::start(vec![json!([])]).await;
    let member = || gateway(&realm, &broker.url, federations(false), None);

    let minted_elsewhere = person(
        &realm,
        json!(format!("context-broker-{}", broker_of(&broker.url))),
        "edge",
    );
    let (status, _) = send(member(), Method::GET, MEMBER_SLUG, Some(&minted_elsewhere)).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    let for_main = person(&realm, json!("context-broker-main"), "context-gateway");
    let (status, _) = send(member(), Method::GET, MEMBER_SLUG, Some(&for_main)).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // The hub's own Endpoint lives on main: a token for the member's broker is not one for it.
    let for_archive = person(
        &realm,
        json!(format!("context-broker-{}", broker_of(&broker.url))),
        "context-gateway",
    );
    let (status, _) = send(member(), Method::GET, HUB_SLUG, Some(&for_archive)).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert!(broker.hops.lock().expect("hops").is_empty());
}

/// T-3999 review: a conditional write and a subscription take another way through the gateway
/// than a plain write, and the exchanged token is refused on both before the broker is asked.
#[tokio::test]
async fn the_exchanged_token_neither_writes_with_if_match_nor_touches_a_subscription() {
    let realm = common::Realm::new();
    let broker = common::BrokerStub::start(vec![json!([])]).await;
    let exchanged = person(
        &realm,
        json!(format!("context-broker-{}", broker_of(&broker.url))),
        "context-gateway",
    );
    let base = format!("/api/endpoint/{MEMBER_SLUG}/ngsi-ld/v1");
    let attempts = [
        (
            Method::PATCH,
            format!("{base}/entities/urn:ngsi-ld:Vehicle:bus-1/attrs?type=Vehicle"),
            Some(json!({ "speed": { "type": "Property", "value": 1 } })),
        ),
        (
            Method::POST,
            format!("{base}/subscriptions?type=Vehicle"),
            Some(json!({
                "type": "Subscription",
                "entities": [{ "type": "Vehicle" }],
                "notification": { "endpoint": { "uri": "https://example.org/n" } }
            })),
        ),
        (
            Method::GET,
            format!("{base}/subscriptions?type=Vehicle"),
            None,
        ),
    ];
    for (method, uri, body) in attempts {
        let mut request = Request::builder()
            .method(method.clone())
            .uri(&uri)
            .header(header::AUTHORIZATION, format!("Bearer {exchanged}"))
            .header(header::IF_MATCH, "*");
        let body = match body {
            Some(body) => {
                request = request.header(header::CONTENT_TYPE, "application/json");
                Body::from(body.to_string())
            }
            None => Body::empty(),
        };
        let response = router(gateway(&realm, &broker.url, federations(false), None))
            .oneshot(request.body(body).expect("a request"))
            .await
            .expect("the gateway answers");
        let status = response.status();
        let said = axum::body::to_bytes(response.into_body(), 65536)
            .await
            .expect("a body");
        let said = String::from_utf8_lossy(&said);
        assert_eq!(status, StatusCode::FORBIDDEN, "{method} {uri}: {said}");
        assert!(said.contains("federated read"), "{method} {uri}: {said}");
    }
    assert!(
        broker.hops.lock().expect("hops").is_empty(),
        "nothing reached the broker"
    );
}

/// F-SEC1-010: the exchanged token reaches only an Endpoint a caller-identity registration names
/// as its member; another Endpoint on the same broker refuses it as it refuses any token of a
/// foreign audience, and the broker is never asked.
#[tokio::test]
async fn the_exchanged_token_is_refused_on_an_endpoint_no_registration_names() {
    let realm = common::Realm::new();
    let broker = common::BrokerStub::start(vec![json!([])]).await;
    let exchanged = person(
        &realm,
        json!(format!("context-broker-{}", broker_of(&broker.url))),
        "context-gateway",
    );
    let member = || gateway(&realm, &broker.url, federations(false), None);

    let (status, body) = send(member(), Method::GET, BYSTANDER_SLUG, Some(&exchanged)).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");
    assert!(broker.hops.lock().expect("hops").is_empty());

    // No registration at all: the member is a bystander too.
    let unregistered = gateway(&realm, &broker.url, Federations::new(), None);
    let (status, body) = send(unregistered, Method::GET, MEMBER_SLUG, Some(&exchanged)).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");
    assert!(broker.hops.lock().expect("hops").is_empty());
}
