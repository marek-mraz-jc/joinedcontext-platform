//! `endpoint: catalog` on the hub: each caller's own catalogue of Datasets (context spaces) and
//! DataServices (Endpoints), built from what the request reaches by the function the Portal
//! writes the public catalogue with (T-4503, T-4500, ADR-N-025).

mod common;

use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use axum::Router;
use context_gateway::app::{router, Gateway};
use context_gateway::auth::accounts::{accounts_of, ServiceAccounts};
use context_gateway::mcp::server::McpServer;
use context_gateway::pdp::PolicyPdp;
use context_gateway::proxy::Broker;
use context_gateway::resolver::{DeclaredTypes, Endpoint};
use jc_core::kinds::{Audience, PolicySpec, QualityRule, RateLimits, Representation};
use jc_core::ngsi_catalog::{self, Place};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use tower::ServiceExt;

const AIR: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaa";
const BIKES: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbb";
const INNER: &str = "iiiiiiiiiiiiiiiiiiiiiiiiii";
const BBOX: [f64; 4] = [19.05, 48.68, 19.25, 48.8];

fn grant(role: &str) -> PolicySpec {
    granted("role", role)
}

fn granted(kind: &str, id: &str) -> PolicySpec {
    serde_norway::from_str(&format!(
        "contextSpaceRef: mesto\n\
         assigner: did:web:banskabystrica.sk\n\
         assignee: {{ kind: {kind}, id: {id} }}\n\
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
    app_on("http://127.0.0.1:1", realm, endpoints)
}

fn app_on(broker: &str, realm: &common::Realm, endpoints: Vec<Endpoint>) -> Router {
    app_with(broker, realm, endpoints, ServiceAccounts::new(), Vec::new())
}

fn app_with(
    broker: &str,
    realm: &common::Realm,
    endpoints: Vec<Endpoint>,
    accounts: ServiceAccounts,
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
        accounts,
        Some("https://city.example".to_owned()),
    );
    gateway.replace_servers(servers);
    router(Arc::new(gateway))
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
    call_at(app, "/api/mcp", token, tool, arguments).await
}

async fn call_at(
    app: Router,
    path: &str,
    token: Option<&str>,
    tool: &str,
    arguments: Value,
) -> Value {
    let mut builder = Request::builder()
        .method(Method::POST)
        .uri(path)
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
        (structured["total"].as_u64(), ids(&first).len()),
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

#[tokio::test]
async fn a_named_server_lists_only_its_members_whatever_else_the_token_reads() {
    let realm = common::Realm::new();
    let token = steward(&realm);
    let server = McpServer {
        project: "mesto".to_owned(),
        name: "air".to_owned(),
        title: Some("Air".to_owned()),
        description: None,
        members: vec![AIR.to_owned()],
        select: None,
        endpoints: None,
        audience: Audience::Organization,
        allowed_projects: Vec::new(),
    };
    let app = app_with(
        "http://127.0.0.1:1",
        &realm,
        city(),
        ServiceAccounts::new(),
        vec![server],
    );
    // The same token on the hub reaches the organization's Endpoint too.
    let hub = call(
        app.clone(),
        Some(&token),
        "query_entities",
        json!({ "endpoint": "catalog", "type": "DataService", "format": "keyValues" }),
    )
    .await;
    assert!(
        entities(&hub).iter().any(|e| e["identifier"] == INNER),
        "{hub}"
    );

    let answer = call_at(
        app.clone(),
        "/api/mcp/mesto/air",
        Some(&token),
        "query_entities",
        json!({ "endpoint": "catalog", "limit": 1000, "count": true }),
    )
    .await;
    let mut listed = ids(&answer);
    listed.sort();
    assert_eq!(
        listed,
        [
            format!("urn:ngsi-ld:DataService:{AIR}"),
            "urn:ngsi-ld:Dataset:mesto-ovzdusie".to_owned(),
        ]
    );
    assert_eq!(
        answer["result"]["structuredContent"]["total"], 2,
        "{answer}"
    );
    let text = answer.to_string();
    for other in [
        "Internal registry",
        "Bike stations",
        INNER,
        BIKES,
        "mesto-register",
        "mesto-doprava",
    ] {
        assert!(
            !text.contains(other),
            "{other} leaks through the server: {text}"
        );
    }
    // A search for the rest finds nothing rather than a hint of it.
    let searched = call_at(
        app,
        "/api/mcp/mesto/air",
        Some(&token),
        "query_entities",
        json!({ "endpoint": "catalog", "q": "searchText~=\"registry\"", "count": true }),
    )
    .await;
    assert!(ids(&searched).is_empty(), "{searched}");
    assert_eq!(
        searched["result"]["structuredContent"]["total"], 0,
        "{searched}"
    );
}

/// A ServiceAccount `reader` of `mesto` and an `auditor` of another project, each with a
/// client `{project}-{name}` (Architecture/12 section 3); an account without a role is no account.
fn accounts() -> ServiceAccounts {
    let dir = std::env::temp_dir().join(format!("catalog-accounts-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    for (project, name) in [("mesto", "reader"), ("kraj", "auditor")] {
        let folder = dir.join(format!("projects/{project}/access/serviceaccounts"));
        std::fs::create_dir_all(&folder).expect("the folder");
        std::fs::write(
            folder.join(format!("{name}.yaml")),
            format!(
                "apiVersion: joinedcontext.com/v1alpha1\nkind: ServiceAccount\nmetadata:\n  \
                 name: {name}\n  namespace: {project}\nspec:\n  owner:\n    user: demo.steward\n  \
                 purpose: \"catalog test\"\n  roles:\n    - role: reader\n      scope:\n        project: {project}\n  credentials:\n    - kind: oauth-client\n      name: default\n"
            ),
        )
        .expect("the manifest");
    }
    let repo = jcctl::loader::Repository::load(&dir).expect("the repository loads");
    let _ = std::fs::remove_dir_all(&dir);
    accounts_of(&repo)
}

fn workload(realm: &common::Realm, azp: &str, aud: &str) -> String {
    realm.mint(&json!({
        "iss": common::ISSUER,
        "sub": format!("service-account-{azp}"),
        "aud": aud,
        "azp": azp,
        "scope": "openid",
        "exp": common::in_seconds(300),
        "iat": common::in_seconds(-10),
    }))
}

#[tokio::test]
async fn a_service_account_grant_shows_its_endpoint_and_the_public_ones_and_nothing_foreign() {
    let realm = common::Realm::new();
    let mut endpoints = city();
    endpoints
        .iter_mut()
        .find(|e| e.slug == INNER)
        .expect("the organization's Endpoint")
        .policies = vec![granted("serviceAccount", "reader")];
    let mut foreign = endpoint(
        "ffffffffffffffffffffffffff",
        "Regional registry",
        "kraj-register",
        Audience::Organization,
        "steward",
    );
    foreign.project = "kraj".to_owned();
    endpoints.push(foreign);
    let app = app_with(
        "http://127.0.0.1:1",
        &realm,
        endpoints,
        accounts(),
        Vec::new(),
    );

    let token = workload(&realm, "mesto-reader", INNER);
    let answer = call(
        app.clone(),
        Some(&token),
        "query_entities",
        json!({ "endpoint": "catalog", "type": "DataService", "format": "keyValues" }),
    )
    .await;
    let seen: BTreeMap<String, String> = entities(&answer)
        .iter()
        .map(|e| {
            (
                e["identifier"].as_str().unwrap_or_default().to_owned(),
                e["visibleBecause"].as_str().unwrap_or_default().to_owned(),
            )
        })
        .collect();
    assert_eq!(
        seen,
        BTreeMap::from([
            (AIR.to_owned(), "public".to_owned()),
            (BIKES.to_owned(), "public".to_owned()),
            (INNER.to_owned(), "grant".to_owned()),
        ]),
        "{answer}"
    );
    assert!(
        !answer.to_string().contains("Regional registry"),
        "{answer}"
    );

    // Another project's account, for the same Endpoint, sees only what is public.
    let outsider = workload(&realm, "kraj-auditor", INNER);
    let answer = call(
        app,
        Some(&outsider),
        "query_entities",
        json!({ "endpoint": "catalog", "type": "DataService", "format": "keyValues" }),
    )
    .await;
    let mut seen: Vec<String> = entities(&answer)
        .iter()
        .map(|e| e["identifier"].as_str().unwrap_or_default().to_owned())
        .collect();
    seen.sort();
    assert_eq!(seen, [AIR, BIKES], "{answer}");
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

/// T-4501: the first call names no Endpoint and finds the dataset in the catalogue by the place
/// as a person writes it; the second reads the Endpoint the Dataset lists.
#[tokio::test]
async fn two_calls_find_the_air_dataset_and_read_it_without_a_token() {
    let broker = common::BrokerStub::start(vec![json!([{
        "id": "urn:ngsi-ld:AirQualityObserved:eea-SK0263A",
        "type": "AirQualityObserved",
        "pm10": { "type": "Property", "value": 10.362 },
    }])])
    .await;
    let realm = common::Realm::new();
    let app = app_on(&broker.url, &realm, city());
    let found = call(
        app.clone(),
        None,
        "query_entities",
        json!({
            "type": "Dataset", "q": "searchText~=\"Banská Bystrica\"", "format": "keyValues",
        }),
    )
    .await;
    let datasets = entities(&found);
    assert_eq!(datasets.len(), 1, "{found}");
    let endpoint = datasets[0]["dataService"][0]["identifier"]
        .as_str()
        .expect("the Dataset names its Endpoint")
        .to_owned();
    assert_eq!(endpoint, AIR);
    let read = call(
        app,
        None,
        "query_entities",
        json!({
            "endpoint": endpoint, "type": "AirQualityObserved",
        }),
    )
    .await;
    assert_eq!(entities(&read)[0]["pm10"]["value"], 10.362, "{read}");
    assert_eq!(broker.hops().len(), 1, "the catalogue read no broker");
}

/// T-4492: the whole list of dev's size reads in a few thousand tokens, one line per Endpoint,
/// and pages the same way twice; a row's URLs are get_endpoint's.
#[tokio::test]
async fn one_hundred_eleven_endpoints_list_in_under_25000_characters() {
    let realm = common::Realm::new();
    let endpoints: Vec<Endpoint> = (0..111)
        .map(|n| {
            let mut e = endpoint(
                &format!("{n:0>26}"),
                &format!("Kvalita ovzdušia a merania v Banskej Bystrici {n}"),
                &format!("banska-bystrica-ovzdusie-{n}"),
                Audience::Public,
                "public",
            );
            e.declared_types = Some(Arc::new(area()));
            e
        })
        .collect();
    let app = app(&realm, endpoints);
    let mut chars = 0;
    let mut slugs = Vec::new();
    let mut cursor = None;
    loop {
        let mut arguments = json!({ "limit": 100 });
        if let Some(next) = &cursor {
            arguments["cursor"] = json!(next);
        }
        let page = call(app.clone(), None, "list_endpoints", arguments).await;
        let text = page["result"]["content"][0]["text"].as_str().expect("text");
        chars += text.chars().count();
        let rows = page["result"]["structuredContent"]["endpoints"]
            .as_array()
            .expect("rows")
            .clone();
        assert!(
            rows.iter().all(|row| row["place"] == json!(BBOX)),
            "{rows:?}"
        );
        slugs.extend(
            rows.iter()
                .map(|row| row["slug"].as_str().expect("slug").to_owned()),
        );
        cursor = page["result"]["structuredContent"]["nextCursor"]
            .as_str()
            .map(str::to_owned);
        if cursor.is_none() {
            break;
        }
    }
    assert_eq!(slugs.len(), 111);
    assert!(slugs.windows(2).all(|w| w[0] < w[1]), "stable slug order");
    assert!(chars < 25_000, "{chars} characters for 111 Endpoints");
    eprintln!("list_endpoints: 111 Endpoints in {chars} characters");
    for (filter, total) in [
        (json!({ "theme": "envi" }), 111),
        (json!({ "domain": "Environment" }), 111),
        (json!({ "domain": "Transportation" }), 0),
    ] {
        let page = call(app.clone(), None, "list_endpoints", filter.clone()).await;
        assert_eq!(
            page["result"]["structuredContent"]["total"],
            json!(total),
            "{filter}"
        );
    }
    let whole = call(app, None, "get_endpoint", json!({ "endpoint": slugs[7] })).await;
    let entry = &whole["result"]["structuredContent"]["endpoint"];
    assert_eq!(entry["slug"], json!(slugs[7]));
    assert_eq!(
        (&entry["domains"], &entry["themes"]),
        (&json!(["Environment"]), &json!(["ENVI"]))
    );
    assert!(
        entry["surfaces"]["ngsi-ld"]
            .as_str()
            .is_some_and(|url| url.ends_with("/ngsi-ld/v1/")),
        "{entry}"
    );
}

fn holding(mut e: Endpoint, class: &str, sk: &str) -> Endpoint {
    let mut declared = area();
    declared.classes = [class.to_owned()].into_iter().collect();
    e.declared_types = Some(Arc::new(declared));
    e.title.insert("sk".to_owned(), sk.to_owned());
    e
}

/// T-4493: one search call from a question in either language, with or without diacritics,
/// to the Dataset, its Endpoint and the call that reads it; an internal Endpoint never shows
/// to a caller who may not read it.
#[tokio::test]
async fn search_ranks_the_dataset_a_question_names_first() {
    let realm = common::Realm::new();
    let endpoints = vec![
        holding(
            endpoint(
                AIR,
                "Air quality Banská Bystrica",
                "banskabystrica-ovzdusie",
                Audience::Public,
                "public",
            ),
            "AirQualityObserved",
            "Kvalita ovzdušia Banská Bystrica",
        ),
        holding(
            endpoint(
                BIKES,
                "Bike sharing Banská Bystrica",
                "banskabystrica-bicykle",
                Audience::Public,
                "public",
            ),
            "BikeHireDockingStation",
            "Zdieľané bicykle Banská Bystrica",
        ),
        holding(
            endpoint(
                "zzzzzzzzzzzzzzzzzzzzzzzzzz",
                "Events in Žilina",
                "zilina-podujatia",
                Audience::Public,
                "public",
            ),
            "Event",
            "Podujatia v Žiline",
        ),
        holding(
            endpoint(
                "pppppppppppppppppppppppppp",
                "Air quality Prešov",
                "presov-ovzdusie",
                Audience::Public,
                "public",
            ),
            "AirQualityObserved",
            "Kvalita ovzdušia Prešov",
        ),
        holding(
            endpoint(
                INNER,
                "Air quality sensors, internal Banská Bystrica",
                "banskabystrica-senzory",
                Audience::Organization,
                "steward",
            ),
            "AirQualityObserved",
            "Interné senzory ovzdušia Banská Bystrica",
        ),
    ];
    let app = app(&realm, endpoints);
    let top = |answer: &Value| answer["result"]["structuredContent"]["hits"][0].clone();
    for question in [
        "ovzdušie Banská Bystrica",
        "air quality banska bystrica",
        "NO2 Bystrica",
    ] {
        let answer = call(app.clone(), None, "search", json!({ "query": question })).await;
        let hit = top(&answer);
        assert_eq!(hit["endpoint"], json!(AIR), "{question}: {answer}");
        assert_eq!(
            hit["dataset"],
            json!("urn:ngsi-ld:Dataset:banskabystrica-ovzdusie")
        );
        assert_eq!(
            hit["nextQuery"]["arguments"]["type"],
            json!("AirQualityObserved")
        );
        assert_eq!(hit["domains"], json!(["Environment"]));
        eprintln!("search {question:?}: {}", hit["title"]);
    }
    let events = call(
        app.clone(),
        None,
        "search",
        json!({ "query": "events Zilina", "lang": "sk" }),
    )
    .await;
    assert_eq!(
        top(&events)["endpoint"],
        json!("zzzzzzzzzzzzzzzzzzzzzzzzzz"),
        "{events}"
    );
    assert_eq!(top(&events)["title"], json!("Podujatia v Žiline"));
    // Facets narrow; a place every hit must name.
    let near = call(
        app.clone(),
        None,
        "search",
        json!({ "query": "air quality", "place": "Prešov" }),
    )
    .await;
    assert_eq!(
        near["result"]["structuredContent"]["total"],
        json!(1),
        "{near}"
    );
    let none = call(
        app.clone(),
        None,
        "search",
        json!({ "query": "air quality", "domain": "Transportation" }),
    )
    .await;
    assert_eq!(none["result"]["structuredContent"]["total"], json!(0));
    // The internal Endpoint is no hit anonymously, and is one for its steward.
    let anonymous = call(
        app.clone(),
        None,
        "search",
        json!({ "query": "senzory interné" }),
    )
    .await;
    assert!(!anonymous.to_string().contains(INNER), "{anonymous}");
    let token = steward(&realm);
    let member = call(
        app.clone(),
        Some(&token),
        "search",
        json!({ "query": "senzory interné" }),
    )
    .await;
    assert_eq!(top(&member)["endpoint"], json!(INNER), "{member}");
    // A missing question and a foreign argument are refused by name.
    for (arguments, word) in [
        (json!({}), "query"),
        (json!({ "query": "x", "slug": "y" }), "slug"),
    ] {
        let refused = call(app.clone(), None, "search", arguments).await;
        assert_eq!(refused["result"]["isError"], json!(true), "{refused}");
        assert!(refused["result"]["content"][0]["text"]
            .as_str()
            .is_some_and(|t| t.contains(word)));
    }
}

/// T-4497: a catalogue of `n` Datasets a person would search, one Endpoint each: an SDM topic
/// in sk and en, a place read from dev's AdministrativeArea entities (`fixtures/places-dev.txt`),
/// a year and a publisher, so near-duplicates differ by a year or a place only. Seeded: the
/// same seed gives the same catalogue.
mod scale {
    use super::*;

    /// (type, en topic, sk topic, sk publisher prefix)
    pub const TOPICS: &[(&str, &str, &str, &str)] = &[
        (
            "AirQualityObserved",
            "Air quality",
            "Kvalita ovzdušia",
            "SHMÚ",
        ),
        (
            "WeatherObserved",
            "Weather observations",
            "Meteorologické pozorovania",
            "SHMÚ",
        ),
        ("Event", "Events", "Podujatia", "Mesto"),
        (
            "BikeHireDockingStation",
            "Bike sharing",
            "Zdieľané bicykle",
            "Mesto",
        ),
        ("ParkingSpot", "Parking spots", "Parkovacie miesta", "Mesto"),
        (
            "TrafficFlowObserved",
            "Traffic counts",
            "Sčítanie dopravy",
            "Kraj",
        ),
        (
            "KeyPerformanceIndicator",
            "Population statistics",
            "Štatistika obyvateľstva",
            "ŠÚ SR",
        ),
        (
            "PointOfInterest",
            "Points of interest",
            "Body záujmu",
            "Mesto",
        ),
        (
            "WasteContainer",
            "Waste containers",
            "Kontajnery na odpad",
            "Mesto",
        ),
        ("Alert", "Weather warnings", "Výstrahy pred počasím", "SHMÚ"),
        (
            "NoiseLevelObserved",
            "Noise levels",
            "Hladina hluku",
            "Mesto",
        ),
        (
            "WaterQualityObserved",
            "Water quality",
            "Kvalita vody",
            "Kraj",
        ),
        (
            "Streetlight",
            "Street lighting",
            "Verejné osvetlenie",
            "Mesto",
        ),
        (
            "Vehicle",
            "Public transport vehicles",
            "Vozidlá MHD",
            "Dopravný podnik",
        ),
    ];
    pub const YEARS: [u32; 4] = [2023, 2024, 2025, 2026];

    pub fn places() -> Vec<&'static str> {
        include_str!("fixtures/places-dev.txt")
            .lines()
            .filter(|line| !line.is_empty() && !line.starts_with('#'))
            .collect()
    }

    /// One generated Dataset: its slug and the (topic, place, year) it is about.
    pub struct Generated {
        pub endpoint: Endpoint,
        pub about: (usize, usize, u32),
    }

    /// xorshift64*: enough spread for a catalogue, no dependency.
    fn next(state: &mut u64) -> u64 {
        *state ^= *state >> 12;
        *state ^= *state << 25;
        *state ^= *state >> 27;
        state.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    pub fn catalogue(seed: u64, n: usize) -> Vec<Generated> {
        let places = places();
        let mut state = seed.max(1);
        (0..n)
            .map(|i| {
                let topic = (next(&mut state) % TOPICS.len() as u64) as usize;
                let place = (next(&mut state) % places.len() as u64) as usize;
                let year = YEARS[(next(&mut state) % YEARS.len() as u64) as usize];
                let (class, en, sk, publisher) = TOPICS[topic];
                let at = places[place];
                let mut e = holding(
                    endpoint(
                        &format!("{i:0>26}"),
                        &format!("{en} {at} {year}"),
                        &format!("d{i}"),
                        Audience::Public,
                        "public",
                    ),
                    class,
                    &format!("{sk} {at} {year}"),
                );
                let by = if publisher == "Mesto" || publisher == "Kraj" {
                    format!("{publisher} {at}")
                } else {
                    publisher.to_owned()
                };
                e.description.insert(
                    "sk".to_owned(),
                    format!("{sk}, {at}, rok {year}. Zverejňuje {by}."),
                );
                e.description.insert(
                    "en".to_owned(),
                    format!("{en} in {at}, {year}, published by {by}."),
                );
                Generated {
                    endpoint: e,
                    about: (topic, place, year),
                }
            })
            .collect()
    }

    /// The share of questions whose target is among the first three hits: a question's targets
    /// are every slug about the same (topic, place, year), which no title tells apart.
    pub fn top3_rate(answers: &[(Vec<String>, BTreeSet<String>)]) -> f64 {
        if answers.is_empty() {
            return 0.0;
        }
        let found = answers
            .iter()
            .filter(|(hits, targets)| hits.iter().take(3).any(|hit| targets.contains(hit)))
            .count();
        found as f64 / answers.len() as f64
    }

    /// p50 and p95 of measured durations.
    pub fn percentiles(
        mut took: Vec<std::time::Duration>,
    ) -> (std::time::Duration, std::time::Duration) {
        took.sort();
        let at = |p: usize| {
            took.get((took.len() * p / 100).min(took.len().saturating_sub(1)))
                .copied()
                .unwrap_or_default()
        };
        (at(50), at(95))
    }
}

#[test]
fn the_generated_catalogue_is_the_same_for_the_same_seed() {
    let titles = |seed| {
        scale::catalogue(seed, 200)
            .into_iter()
            .map(|g| {
                (
                    g.endpoint.title.clone(),
                    g.endpoint.description.clone(),
                    g.about,
                )
            })
            .collect::<Vec<_>>()
    };
    assert_eq!(titles(7), titles(7));
    assert_ne!(titles(7), titles(8));
    let one = scale::catalogue(7, 200);
    assert!(one
        .iter()
        .any(|g| g.endpoint.title["sk"].contains('ž') || g.endpoint.title["sk"].contains('á')));
    assert!(
        one.iter().map(|g| g.about.0).collect::<BTreeSet<_>>().len() > 10,
        "every topic shows"
    );
}

#[test]
fn the_relevance_rate_counts_a_target_in_the_first_three() {
    let set = |s: &[&str]| s.iter().map(|x| (*x).to_owned()).collect::<BTreeSet<_>>();
    let hits = |s: &[&str]| s.iter().map(|x| (*x).to_owned()).collect::<Vec<_>>();
    let answers = vec![
        (hits(&["a", "b", "c"]), set(&["c"])),
        (hits(&["a", "b", "c", "d"]), set(&["d"])),
        (hits(&[]), set(&["a"])),
        (hits(&["x"]), set(&["x", "y"])),
    ];
    assert!((scale::top3_rate(&answers) - 0.5).abs() < f64::EPSILON);
    assert!(scale::top3_rate(&[]).abs() < f64::EPSILON);
    let ms = std::time::Duration::from_millis;
    assert_eq!(
        scale::percentiles((1..=100).map(ms).collect()),
        (ms(51), ms(96))
    );
}

/// T-4497, run on its own in release:
/// `cargo test --release -p context-gateway --test mcp_hub_catalog_tests ten_thousand_generated -- --ignored --nocapture`.
/// 200 questions each through MCP search, list_endpoints (first page, a deep cursor) and the
/// catalog Endpoint's query_entities (searchText, a theme); 30 with a known target for top-3.
#[tokio::test]
#[ignore = "a 10,000-Dataset measurement; run in release"]
async fn ten_thousand_generated_datasets_are_found_fast() {
    let realm = common::Realm::new();
    let generated = scale::catalogue(4497, 10_000);
    let places = scale::places();
    // The quotas guard dev from a scraper; this loop is one, on purpose.
    let unlimited = RateLimits {
        requests_per_minute: 1_000_000,
        burst: Some(1_000_000),
    };
    let gateway = Gateway::new(
        Broker::new("http://127.0.0.1:1"),
        Box::new(PolicyPdp),
        "banskabystrica.sk",
    )
    .serve(generated.iter().map(|g| g.endpoint.clone()))
    .authenticate(
        Arc::new(realm.verifier()),
        ServiceAccounts::new(),
        Some("https://city.example".to_owned()),
    )
    .limit_hub_to(unlimited.clone())
    .limit_anonymous_mcp_to(unlimited);
    let app = router(Arc::new(gateway));
    let mut questions = Vec::new();
    for (i, g) in generated.iter().step_by(50).take(200).enumerate() {
        let (topic, place, year) = g.about;
        let (_, en, sk, _) = scale::TOPICS[topic];
        let at = places[place];
        let text = match i % 3 {
            0 => format!("{sk} {at} {year}"),
            1 => jc_core::ngsi_catalog::fold(&format!("{sk} {at} {year}")),
            _ => format!("{en} {at} {year}"),
        };
        let targets: BTreeSet<String> = generated
            .iter()
            .filter(|other| other.about == g.about)
            .map(|other| other.endpoint.slug.clone())
            .collect();
        questions.push((text, targets));
    }
    let timed = |tool: &'static str, arguments: Value| {
        let app = app.clone();
        async move {
            let started = std::time::Instant::now();
            let answer = call(app, None, tool, arguments).await;
            (started.elapsed(), answer)
        }
    };
    let _ = timed("search", json!({ "query": "warm" })).await;
    let mut took = Vec::new();
    let mut answers = Vec::new();
    for (text, targets) in &questions {
        let (t, answer) = timed("search", json!({ "query": text })).await;
        took.push(t);
        let hits = answer["result"]["structuredContent"]["hits"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|h| h["endpoint"].as_str().map(str::to_owned))
            .collect();
        answers.push((hits, targets.clone()));
    }
    let search = scale::percentiles(took);
    let relevance = scale::top3_rate(&answers[..30]);
    let relevance_all = scale::top3_rate(&answers);
    let mut first = Vec::new();
    let mut deep = Vec::new();
    for i in 0..200 {
        first.push(timed("list_endpoints", json!({ "limit": 20 })).await.0);
        let cursor = (9_000 + i * 4).to_string();
        let (t, page) = timed("list_endpoints", json!({ "limit": 20, "cursor": cursor })).await;
        assert_eq!(
            page["result"]["structuredContent"]["total"],
            json!(10_000),
            "{page}"
        );
        deep.push(t);
    }
    let mut text = Vec::new();
    let mut theme = Vec::new();
    for (i, (question, _)) in questions.iter().enumerate() {
        let word = jc_core::ngsi_catalog::fold(question.split(' ').nth(1).unwrap_or("x"));
        let q = format!("searchText~=\"{word}\"");
        text.push(
            timed(
                "query_entities",
                json!({ "endpoint": "catalog", "type": "Dataset", "q": q, "limit": 20 }),
            )
            .await
            .0,
        );
        let themes = ["ENVI", "TRAN", "SOCI", "GOVE"];
        let q = format!("theme==\"{}\"", themes[i % themes.len()]);
        theme.push(
            timed(
                "query_entities",
                json!({ "endpoint": "catalog", "type": "Dataset", "q": q, "limit": 20 }),
            )
            .await
            .0,
        );
    }
    let rows = [
        ("MCP search", search, 100),
        ("list_endpoints first page", scale::percentiles(first), 100),
        (
            "list_endpoints cursor 9,000+",
            scale::percentiles(deep),
            100,
        ),
        (
            "catalog query_entities searchText",
            scale::percentiles(text),
            200,
        ),
        (
            "catalog query_entities theme",
            scale::percentiles(theme),
            200,
        ),
    ];
    eprintln!("| path | p50 | p95 | target p95 |\n|---|---|---|---|");
    for (path, (p50, p95), target) in rows {
        eprintln!("| {path} | {p50:.1?} | {p95:.1?} | < {target} ms |");
    }
    eprintln!("relevance: top-3 {relevance:.2} of 30 known targets, {relevance_all:.2} of all 200");
    for (path, (_, p95), target) in rows {
        assert!(p95.as_millis() < target, "{path}: p95 {p95:?}");
    }
    assert!(relevance >= 0.9, "top-3 rate {relevance}");
}
