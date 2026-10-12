//! `GET /api/endpoints` and a machine's token through the hub (T-3793, EP-105, EP-88,
//! ADR-N-025 §4a).
//!
//! A ServiceAccount's `client_credentials` token carries one audience per Endpoint its roles
//! scope (PF-47). Those audiences are its reach: it lists and calls exactly those Endpoints, from
//! the REST list and from the hub's `list_endpoints` alike, and a slug outside them answers as an
//! unknown one (SP-20).

mod common;

use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use axum::Router;
use context_gateway::app::{router, Gateway};
use context_gateway::auth::accounts::{accounts_of, ServiceAccounts};
use context_gateway::pdp::PolicyPdp;
use context_gateway::proxy::Broker;
use context_gateway::resolver::{Endpoint, Model};
use jc_core::kinds::{Audience, PolicySpec, Representation};
use serde_json::{json, Value};
use std::sync::Arc;
use tower::ServiceExt;

const HOST: &str = "https://city.example";
const CLIENT: &str = "ovzdusie-etl";
/// Six Endpoints that grant the account's role; its token names the first five.
const SLUGS: [&str; 6] = [
    "aaaaaaaaaaaaaaaaaaaaaaaaaa",
    "bbbbbbbbbbbbbbbbbbbbbbbbbb",
    "cccccccccccccccccccccccccc",
    "dddddddddddddddddddddddddd",
    "eeeeeeeeeeeeeeeeeeeeeeeeee",
    "ffffffffffffffffffffffffff",
];
const UNKNOWN: &str = "zzzzzzzzzzzzzzzzzzzzzzzzzy";

fn endpoint(slug: &str) -> Endpoint {
    let grant: PolicySpec = serde_norway::from_str(
        "contextSpaceRef: ovzdusie\nassigner: did:web:banskabystrica.sk\n\
         assignee: { kind: role, id: reader }\noperations: [queryEntity]\n\
         information:\n  - entities:\n      - type: AirQualityObserved\n",
    )
    .expect("the policy spec parses");
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
        models: vec![Model {
            name: "air-quality".to_owned(),
            version: "1.0.0".to_owned(),
            major: 1,
            classes: vec!["AirQualityObserved".to_owned()],
            json_schema: None,
            context: None,
        }],
        policy_names: Vec::new(),
        policies: vec![grant],
    }
}

/// The repository's one ServiceAccount, `etl`, a `reader` of the `ovzdusie` space.
fn accounts() -> ServiceAccounts {
    // One directory per call: the tests of this binary run at once.
    static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir =
        std::env::temp_dir().join(format!("gateway-endpoints-list-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("projects/ovzdusie/access/serviceaccounts"))
        .expect("a repository");
    std::fs::write(
        dir.join("projects/ovzdusie/access/serviceaccounts/etl.yaml"),
        "apiVersion: joinedcontext.com/v1alpha1\nkind: ServiceAccount\nmetadata:\n  name: etl\n  \
         namespace: ovzdusie\nspec:\n  owner:\n    user: demo.steward\n  purpose: \"a city ETL\"\n  \
         roles:\n    - role: reader\n      scope:\n        contextSpace: ovzdusie\n  \
         credentials:\n    - kind: oauth-client\n      name: default\n",
    )
    .expect("the manifest is written");
    std::fs::create_dir_all(dir.join("projects/ovzdusie/spaces/ovzdusie")).expect("a space");
    std::fs::write(
        dir.join("projects/ovzdusie/spaces/ovzdusie/space.yaml"),
        "apiVersion: joinedcontext.com/v1alpha1\nkind: ContextSpace\nmetadata:\n  name: ovzdusie\n  \
         namespace: ovzdusie\nspec:\n  urnSegment: ovzdusie\n",
    )
    .expect("the space is written");
    let repo = jcctl::loader::Repository::load(&dir).expect("the repository loads");
    let _ = std::fs::remove_dir_all(&dir);
    accounts_of(&repo)
}

fn app(broker: &str, realm: &common::Realm, slugs: &[&str]) -> Router {
    let gateway = Gateway::new(
        Broker::new(broker),
        Box::new(PolicyPdp),
        "banskabystrica.sk",
    )
    .serve(slugs.iter().map(|slug| endpoint(slug)))
    .seal_subscribers_with(common::delivery_key())
    .authenticate(
        Arc::new(realm.verifier()),
        accounts(),
        Some(HOST.to_owned()),
    );
    router(Arc::new(gateway))
}

/// The account's client_credentials token for `aud`: no person, no roles of its own.
fn machine(realm: &common::Realm, aud: Value) -> String {
    realm.mint(&json!({
        "iss": common::ISSUER,
        "sub": "service-account-ovzdusie-etl",
        "aud": aud,
        "azp": CLIENT,
        "scope": "profile email",
        "exp": common::in_seconds(300),
        "iat": common::in_seconds(-10),
    }))
}

async fn send(app: Router, request: Request<Body>) -> (StatusCode, String) {
    let response = app.oneshot(request).await.expect("the gateway answers");
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .expect("a body");
    (status, String::from_utf8_lossy(&bytes).into_owned())
}

fn get(path: &str, token: Option<&str>) -> Request<Body> {
    let mut builder = Request::builder().method(Method::GET).uri(path);
    if let Some(token) = token {
        builder = builder.header("authorization", format!("Bearer {token}"));
    }
    builder.body(Body::empty()).expect("a request")
}

fn hub(token: &str, name: &str, arguments: Value) -> Request<Body> {
    Request::builder()
        .method(Method::POST)
        .uri("/api/mcp")
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {token}"))
        .body(Body::from(
            json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/call",
                    "params": { "name": name, "arguments": arguments } })
            .to_string(),
        ))
        .expect("a request")
}

fn slugs_of(list: &Value) -> Vec<String> {
    list["endpoints"]
        .as_array()
        .unwrap_or_else(|| panic!("no list: {list}"))
        .iter()
        .map(|entry| entry["slug"].as_str().expect("a slug").to_owned())
        .collect()
}

/// EP-105, ADR-N-025 §4a: five audiences are five Endpoints, from the REST list and from the
/// hub; each answers a call, and the sixth, granted but not named, answers like an unknown slug.
#[tokio::test]
async fn a_service_account_with_five_endpoints_lists_and_calls_exactly_those_five() {
    let realm = common::Realm::new();
    let broker = common::BrokerStub::start(vec![json!([])]).await;
    let app = app(&broker.url, &realm, &SLUGS);
    let token = machine(&realm, json!(SLUGS[..5]));

    let (status, body) = send(app.clone(), get("/api/endpoints", Some(&token))).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let rest: Value = serde_json::from_str(&body).expect("JSON");
    assert_eq!(slugs_of(&rest), SLUGS[..5]);
    assert_eq!(rest["total"], json!(5));
    let first = &rest["endpoints"][0];
    assert_eq!(first["types"], json!(["AirQualityObserved"]));
    assert_eq!(
        first["surfaces"]["ngsi-ld"],
        json!(format!("{HOST}/api/endpoint/{}/ngsi-ld/v1/", SLUGS[0]))
    );

    let (_, body) = send(app.clone(), hub(&token, "list_endpoints", json!({}))).await;
    let mcp: Value = serde_json::from_str(&body).expect("JSON");
    assert_eq!(slugs_of(&mcp["result"]["structuredContent"]), SLUGS[..5]);

    for slug in &SLUGS[..5] {
        let arguments = json!({ "endpoint": slug, "type": "AirQualityObserved" });
        let (_, body) = send(app.clone(), hub(&token, "query_entities", arguments)).await;
        let answer: Value = serde_json::from_str(&body).expect("JSON");
        assert_eq!(
            answer["result"]["isError"],
            json!(false),
            "{slug}: {answer}"
        );
    }
    let sixth = json!({ "endpoint": SLUGS[5], "type": "AirQualityObserved" });
    let unknown = json!({ "endpoint": UNKNOWN, "type": "AirQualityObserved" });
    let (_, sixth) = send(app.clone(), hub(&token, "query_entities", sixth)).await;
    let (_, unknown) = send(app, hub(&token, "query_entities", unknown)).await;
    assert_eq!(sixth, unknown, "SP-20");
}

/// PF-45, PF-46: a token for one Endpoint's audience lists that Endpoint only.
#[tokio::test]
async fn a_token_for_one_endpoint_lists_that_endpoint_only() {
    let realm = common::Realm::new();
    let app = app("http://127.0.0.1:1", &realm, &SLUGS);
    let token = machine(&realm, json!(SLUGS[2]));
    let (_, body) = send(app, get("/api/endpoints", Some(&token))).await;
    let rest: Value = serde_json::from_str(&body).expect("JSON");
    assert_eq!(slugs_of(&rest), [SLUGS[2]]);
}

/// The list is the grants of now: an Endpoint whose grant is gone is gone from the next call,
/// with nothing kept between calls.
#[tokio::test]
async fn an_endpoint_whose_grant_is_gone_is_not_listed() {
    let realm = common::Realm::new();
    let token = machine(&realm, json!(SLUGS[..5]));
    let fewer = app("http://127.0.0.1:1", &realm, &SLUGS[1..]);
    let (_, body) = send(fewer, get("/api/endpoints", Some(&token))).await;
    let rest: Value = serde_json::from_str(&body).expect("JSON");
    assert_eq!(slugs_of(&rest), SLUGS[1..5]);
}

/// No token lists nothing (401), and the paging arguments are those of `list_endpoints`.
#[tokio::test]
async fn the_list_needs_a_token_and_pages() {
    let realm = common::Realm::new();
    let app = app("http://127.0.0.1:1", &realm, &SLUGS);
    let (status, _) = send(app.clone(), get("/api/endpoints", None)).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    let token = machine(&realm, json!(SLUGS[..5]));
    let (_, body) = send(
        app.clone(),
        get("/api/endpoints?limit=2&cursor=2", Some(&token)),
    )
    .await;
    let page: Value = serde_json::from_str(&body).expect("JSON");
    assert_eq!(slugs_of(&page), SLUGS[2..4]);
    assert_eq!(page["nextCursor"], json!(4));

    for query in [
        "limit=0",
        "limit=101",
        "limit=x",
        "cursor=-1",
        "space=ovzdusie",
    ] {
        let (status, body) = send(
            app.clone(),
            get(&format!("/api/endpoints?{query}"), Some(&token)),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{query}: {body}");
    }
}

/// ADR-N-047 §2.3: across brokers, the list names each Endpoint's broker, in the REST list and
/// the hub's alike; an Endpoint of no broker of its own is `main`'s.
#[tokio::test]
async fn each_listed_endpoint_names_the_broker_its_space_lives_on() {
    let realm = common::Realm::new();
    let broker = Broker::new("http://127.0.0.1:1");
    broker.route(&context_gateway::store::SpaceBrokers {
        by_view: std::collections::BTreeMap::from([(SLUGS[1].to_owned(), "canary".to_owned())]),
        ..Default::default()
    });
    let gateway = Gateway::new(broker, Box::new(PolicyPdp), "banskabystrica.sk")
        .serve(SLUGS.iter().map(|slug| endpoint(slug)))
        .seal_subscribers_with(common::delivery_key())
        .authenticate(
            Arc::new(realm.verifier()),
            accounts(),
            Some(HOST.to_owned()),
        );
    let app = router(Arc::new(gateway));
    let token = machine(&realm, json!(SLUGS[..2]));

    let brokers = |list: &Value| -> Vec<String> {
        list["endpoints"]
            .as_array()
            .expect("a list")
            .iter()
            .map(|entry| entry["broker"].as_str().expect("a broker").to_owned())
            .collect()
    };
    let (_, body) = send(app.clone(), get("/api/endpoints", Some(&token))).await;
    let rest: Value = serde_json::from_str(&body).expect("JSON");
    assert_eq!(brokers(&rest), ["main", "canary"]);
    let (_, body) = send(app, hub(&token, "list_endpoints", json!({}))).await;
    let mcp: Value = serde_json::from_str(&body).expect("JSON");
    assert_eq!(
        brokers(&mcp["result"]["structuredContent"]),
        ["main", "canary"]
    );
}
