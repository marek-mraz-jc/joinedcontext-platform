//! `endpoint: catalog` on the hub: each caller's own catalogue of Datasets (context spaces) and
//! DataServices (Endpoints), built from what the request reaches by the function the Portal
//! writes the public catalogue with (T-4503, T-4500, ADR-N-025).

mod common;

use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use axum::Router;
use context_gateway::app::{router, Gateway};
use context_gateway::auth::accounts::ServiceAccounts;
use context_gateway::pdp::PolicyPdp;
use context_gateway::proxy::Broker;
use context_gateway::resolver::{DeclaredTypes, Endpoint};
use jc_core::kinds::{Audience, PolicySpec, QualityRule, Representation};
use jc_core::ngsi_catalog::{self, Place};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::sync::Arc;
use tower::ServiceExt;

const AIR: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaa";
const BIKES: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbb";
const INNER: &str = "iiiiiiiiiiiiiiiiiiiiiiiiii";
const BBOX: [f64; 4] = [19.05, 48.68, 19.25, 48.8];

fn grant(role: &str) -> PolicySpec {
    serde_norway::from_str(&format!(
        "contextSpaceRef: mesto\n\
         assigner: did:web:banskabystrica.sk\n\
         assignee: {{ kind: role, id: {role} }}\n\
         operations: [retrieveOps]\n"
    ))
    .expect("the policy spec parses")
}

fn area() -> DeclaredTypes {
    let rule: QualityRule = serde_json::from_value(json!({
        "name": "in-the-city", "entityType": "AirQualityObserved", "kind": "area",
        "attribute": "location", "bbox": BBOX,
    }))
    .expect("a quality rule");
    DeclaredTypes {
        model: "air".to_owned(),
        classes: ["AirQualityObserved".to_owned()].into_iter().collect(),
        units: Default::default(),
        relationships: Default::default(),
        quality: vec![rule],
        schema: None,
    }
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
        base_path: format!("/api/endpoint/{slug}"),
        models: Vec::new(),
        policy_names: Vec::new(),
        policies: vec![grant(role)],
    }
}

fn app(realm: &common::Realm, endpoints: Vec<Endpoint>) -> Router {
    router(Arc::new(
        Gateway::new(
            Broker::new("http://127.0.0.1:1"),
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

/// Two public Endpoints, one of them on a space with an area; one for the organization; and the
/// broker-backed public catalogue, which is not a dataset of its own.
fn city() -> Vec<Endpoint> {
    let mut air = endpoint(
        AIR,
        "Air quality in Banská Bystrica",
        "mesto-ovzdusie",
        Audience::Public,
        "public",
    );
    air.declared_types = Some(Arc::new(area()));
    vec![
        air,
        endpoint(
            BIKES,
            "Bike stations",
            "mesto-doprava",
            Audience::Public,
            "public",
        ),
        endpoint(
            INNER,
            "Internal registry",
            "mesto-register",
            Audience::Organization,
            "steward",
        ),
        endpoint(
            "cccccccccccccccccccccccccc",
            "Catalogue",
            "mesto-catalog",
            Audience::Public,
            "public",
        ),
    ]
}

fn steward(realm: &common::Realm) -> String {
    realm.mint(&json!({
        "iss": common::ISSUER,
        "sub": "0f5a",
        "aud": "context-gateway",
        "scope": "openid",
        "azp": "mcp-hub",
        "preferred_username": "jana",
        "groups": ["/mesto"],
        "realm_access": { "roles": ["steward"] },
        "exp": common::in_seconds(300),
        "iat": common::in_seconds(-10),
    }))
}

async fn send(app: Router, request: Request<Body>) -> (StatusCode, Value) {
    let response = app.oneshot(request).await.expect("the gateway answers");
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 64 * 1024 * 1024)
        .await
        .expect("a body");
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

async fn call(app: Router, token: Option<&str>, tool: &str, arguments: Value) -> Value {
    let mut builder = Request::builder()
        .method(Method::POST)
        .uri("/api/mcp")
        .header("content-type", "application/json");
    if let Some(token) = token {
        builder = builder.header("authorization", format!("Bearer {token}"));
    }
    let body = json!({
        "jsonrpc": "2.0", "id": 1, "method": "tools/call",
        "params": { "name": tool, "arguments": arguments },
    });
    let (status, answer) = send(
        app,
        builder
            .body(Body::from(body.to_string()))
            .expect("a request"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{answer}");
    answer
}

fn entities(answer: &Value) -> Vec<Value> {
    answer["result"]["structuredContent"]["entities"]
        .as_array()
        .unwrap_or_else(|| panic!("entities: {answer}"))
        .clone()
}

fn ids(answer: &Value) -> Vec<String> {
    entities(answer)
        .iter()
        .map(|e| e["id"].as_str().unwrap_or_default().to_owned())
        .collect()
}

#[tokio::test]
async fn the_anonymous_catalog_is_the_public_catalog_entity_for_entity() {
    let realm = common::Realm::new();
    let app = app(&realm, city());
    let answer = call(
        app.clone(),
        None,
        "query_entities",
        json!({ "endpoint": "catalog", "limit": 1000 }),
    )
    .await;
    let hub: BTreeMap<String, Value> = entities(&answer)
        .into_iter()
        .map(|e| (e["id"].as_str().unwrap_or_default().to_owned(), e))
        .collect();

    // What the Portal writes into the public catalogue: the feed, read anonymously.
    let request = Request::builder()
        .uri("/catalog.jsonld")
        .body(Body::empty())
        .expect("a request");
    let (status, feed) = send(app, request).await;
    assert_eq!(status, StatusCode::OK);
    let places = BTreeMap::from([
        (AIR.to_owned(), Place::public("mesto-ovzdusie", Some(BBOX))),
        (BIKES.to_owned(), Place::public("mesto-doprava", None)),
    ]);
    let public = ngsi_catalog::entities(feed["@included"].as_array().expect("nodes"), &places);
    assert_eq!(hub, public);
    assert_eq!(
        hub.len(),
        4,
        "two Datasets, two DataServices: {:?}",
        hub.keys()
    );
    assert!(!serde_json::to_string(&hub)
        .expect("json")
        .contains("Internal"));
}

#[tokio::test]
async fn an_organization_member_sees_its_endpoints_and_why() {
    let realm = common::Realm::new();
    let token = steward(&realm);
    let answer = call(
        app(&realm, city()),
        Some(&token),
        "query_entities",
        json!({
            "endpoint": "catalog", "type": "DataService", "format": "keyValues",
        }),
    )
    .await;
    let inner = entities(&answer)
        .into_iter()
        .find(|e| e["identifier"] == INNER)
        .unwrap_or_else(|| panic!("the organization's Endpoint: {answer}"));
    assert_eq!(inner["accessLevel"], "organization");
    assert_eq!(inner["visibleBecause"], "organization");
    assert_eq!(inner["title"]["en"], "Internal registry");
}

#[tokio::test]
async fn text_place_and_type_find_the_air_dataset() {
    let realm = common::Realm::new();
    let app = app(&realm, city());
    let dataset = vec!["urn:ngsi-ld:Dataset:mesto-ovzdusie".to_owned()];
    let by_text = call(
        app.clone(),
        None,
        "query_entities",
        json!({
            "endpoint": "catalog", "type": "Dataset", "q": "searchText~=\"bystrica\"",
        }),
    )
    .await;
    assert_eq!(ids(&by_text), dataset);
    let near = call(
        app.clone(),
        None,
        "query_entities",
        json!({
            "endpoint": "Catalog", "type": "Dataset", "georel": "near;maxDistance==2000",
            "geometry": "Point", "coordinates": "[19.146,48.736]",
        }),
    )
    .await;
    assert_eq!(ids(&near), dataset, "{near}");
    let far = call(
        app.clone(),
        None,
        "query_entities",
        json!({
            "endpoint": "catalog", "type": "Dataset", "georel": "near;maxDistance==2000",
            "geometry": "Point", "coordinates": "[17.107,48.148]",
        }),
    )
    .await;
    assert!(ids(&far).is_empty(), "{far}");
    let one = call(
        app,
        None,
        "get_entity",
        json!({
            "endpoint": "catalog", "id": "urn:ngsi-ld:DataService:aaaaaaaaaaaaaaaaaaaaaaaaaa",
        }),
    )
    .await;
    assert_eq!(
        one["result"]["structuredContent"]["entity"]["servesDataset"]["object"],
        dataset[0]
    );
}

#[tokio::test]
async fn a_page_counts_and_continues_and_a_foreign_argument_or_tool_is_refused() {
    let realm = common::Realm::new();
    let app = app(&realm, city());
    let first = call(
        app.clone(),
        None,
        "query_entities",
        json!({
            "endpoint": "catalog", "limit": 3, "count": true,
        }),
    )
    .await;
    let structured = &first["result"]["structuredContent"];
    assert_eq!(
        (structured["count"].as_u64(), ids(&first).len()),
        (Some(4), 3)
    );
    let rest = call(
        app.clone(),
        None,
        "query_entities",
        json!({
            "endpoint": "catalog", "limit": 3, "cursor": structured["nextCursor"],
        }),
    )
    .await;
    assert_eq!(ids(&rest).len(), 1);
    assert!(rest["result"]["structuredContent"]
        .get("nextCursor")
        .is_none());

    let joined = call(
        app.clone(),
        None,
        "query_entities",
        json!({ "endpoint": "catalog", "join": "inline" }),
    )
    .await;
    assert_eq!(joined["result"]["isError"], true, "{joined}");
    let attributes = call(
        app,
        None,
        "list_attributes",
        json!({ "endpoint": "catalog" }),
    )
    .await;
    assert!(
        attributes["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("dataService"),
        "{attributes}"
    );
}

/// A measurement, run on its own in release:
/// `cargo test --release -p context-gateway --test mcp_hub_catalog_tests ten_thousand -- --ignored --nocapture`.
/// Measured 2026-10-17: cold 1.8 s, a warm search p95 65 ms of which the hub's reach over
/// 10,000 Endpoints is 50 ms (`list_types` on the catalog, which filters nothing).
#[tokio::test]
#[ignore = "a 10,000-Endpoint measurement; run in release"]
async fn ten_thousand_endpoints_answer_a_warm_catalog_query_quickly() {
    let realm = common::Realm::new();
    let endpoints: Vec<Endpoint> = (0..10_000)
        .map(|n| {
            endpoint(
                &format!("{n:0>26}"),
                &format!("Dataset {n}"),
                &format!("mesto-s{n}"),
                Audience::Public,
                "public",
            )
        })
        .collect();
    let app = app(&realm, endpoints);
    let ask =
        json!({ "endpoint": "catalog", "type": "Dataset", "q": "searchText~=\"dataset 4242 \"" });
    let started = std::time::Instant::now();
    let cold = call(app.clone(), None, "query_entities", ask.clone()).await;
    let cold_took = started.elapsed();
    assert_eq!(ids(&cold), ["urn:ngsi-ld:Dataset:mesto-s4242"]);
    let mut warm = Vec::new();
    for _ in 0..20 {
        let started = std::time::Instant::now();
        call(app.clone(), None, "query_entities", ask.clone()).await;
        warm.push(started.elapsed());
    }
    warm.sort();
    let p95 = warm[18];
    // What every hub call over 10,000 Endpoints costs before any tool runs: the reach.
    let mut hub = Vec::new();
    for _ in 0..20 {
        let started = std::time::Instant::now();
        call(app.clone(), None, "list_endpoints", json!({ "limit": 1 })).await;
        hub.push(started.elapsed());
    }
    hub.sort();
    let mut types = Vec::new();
    for _ in 0..20 {
        let started = std::time::Instant::now();
        call(
            app.clone(),
            None,
            "list_types",
            json!({ "endpoint": "catalog" }),
        )
        .await;
        types.push(started.elapsed());
    }
    types.sort();
    eprintln!("list_types on the catalog p95 {:?}", types[18]);
    // The catalogue's own work, beside what every hub call over these Endpoints costs.
    assert!(
        p95.saturating_sub(types[18]) < std::time::Duration::from_millis(50),
        "{p95:?}"
    );
    eprintln!(
        "10,000 Endpoints: cold {cold_took:?}, warm p95 {p95:?}, list_endpoints p95 {:?}",
        hub[18]
    );
}
