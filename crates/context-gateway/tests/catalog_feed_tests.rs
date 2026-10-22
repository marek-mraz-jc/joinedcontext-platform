//! `GET /catalog.jsonld` and `GET /catalog.ttl`: the organization's catalogue for harvesters
//! (T-2726, EP-84, Architecture/21 §8).
//!
//! Contract, in one sentence: the feed is one `dcat:Catalog` holding the DCAT-AP record of every
//! Endpoint whose audience is `public`, each as the anonymous caller reads it, and nothing of any
//! other Endpoint. The input that could widen it is a token: the feed reads none, so an
//! organization member's token lists exactly what no token lists.

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use context_gateway::app::{router, Gateway};
use context_gateway::auth::accounts::ServiceAccounts;
use context_gateway::pdp::PolicyPdp;
use context_gateway::proxy::Broker;
use context_gateway::resolver::Endpoint;
use jc_core::kinds::{Audience, PolicySpec, Representation};
use serde_json::{json, Value};
use std::sync::Arc;
use tower::ServiceExt;

const OPEN: &str = "k4y7pq2mzt6vhx3nbwrs5cjd8f";
const ALSO_OPEN: &str = "m3x8rq5vzt2khw7pbnd4sjc6fy";
const CLOSED: &str = "p9d2wc5kzn8mth4rqvb7xj3sfy";
const HOST: &str = "https://bb.example.sk";

fn endpoint(slug: &str, space: &str, audience: Audience) -> Endpoint {
    let policy: PolicySpec = serde_norway::from_str(&format!(
        "contextSpaceRef: {space}\n\
         assigner: did:web:banskabystrica.sk\n\
         assignee: {{ kind: role, id: public }}\n\
         operations: [queryEntity, retrieveEntity]\n\
         information:\n  - entities:\n      - type: AirQualityObserved\n"
    ))
    .expect("the policy spec parses");
    Endpoint {
        declared_types: None,
        roles: Default::default(),
        slug: slug.to_owned(),
        title: [("sk".to_owned(), format!("Dáta {space}"))].into(),
        description: Default::default(),
        space: space.to_owned(),
        project: format!("{space}-project"),
        audience,
        allowed_projects: Vec::new(),
        representations: vec![Representation::NgsiLd, Representation::Csv],
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
        policy_names: Vec::new(),
        manifest_name: String::new(),
        policies: vec![policy],
    }
}

fn app_of(realm: &common::Realm, endpoints: Vec<Endpoint>) -> axum::Router {
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
            Some(HOST.to_owned()),
        ),
    ))
}

fn served() -> Vec<Endpoint> {
    vec![
        endpoint(OPEN, "ovzdusie", Audience::Public),
        endpoint(ALSO_OPEN, "doprava", Audience::Public),
        endpoint(CLOSED, "interne", Audience::Organization),
    ]
}

async fn get(
    app: axum::Router,
    path: &str,
    token: Option<&str>,
) -> (StatusCode, String, String, String) {
    let mut builder = Request::builder().uri(path);
    if let Some(token) = token {
        builder = builder.header(axum::http::header::AUTHORIZATION, format!("Bearer {token}"));
    }
    let response = app
        .oneshot(builder.body(Body::empty()).expect("a request"))
        .await
        .expect("the gateway answers");
    let header = |name| {
        response
            .headers()
            .get(name)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_owned()
    };
    let (status, media, cache) = (
        response.status(),
        header(axum::http::header::CONTENT_TYPE),
        header(axum::http::header::CACHE_CONTROL),
    );
    let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .expect("a readable body");
    (
        status,
        media,
        cache,
        String::from_utf8_lossy(&body).into_owned(),
    )
}

#[tokio::test]
async fn the_feed_lists_every_public_endpoint_and_nothing_else() {
    let realm = common::Realm::new();
    let (status, media, cache, body) = get(app_of(&realm, served()), "/catalog.jsonld", None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(media, "application/ld+json");
    // Every answer of the gateway stays out of shared caches (R9), the feed included.
    assert!(cache.contains("private"), "{cache}");
    let feed: Value = serde_json::from_str(&body).expect("JSON-LD");
    assert_eq!(feed["@type"], "dcat:Catalog");
    assert_eq!(feed["@id"], format!("{HOST}/catalog"));
    assert_eq!(feed["dct:publisher"]["foaf:name"], "banskabystrica.sk");
    let mut datasets: Vec<&str> = feed["dcat:dataset"]
        .as_array()
        .expect("datasets")
        .iter()
        .filter_map(|d| d["@id"].as_str())
        .collect();
    datasets.sort_unstable();
    assert_eq!(
        datasets,
        vec![
            format!("{HOST}/api/endpoint/{OPEN}"),
            format!("{HOST}/api/endpoint/{ALSO_OPEN}"),
        ]
    );
    for secret in [CLOSED, "interne"] {
        assert!(!body.contains(secret), "the feed names {secret}: {body}");
    }
}

/// EP-139, T-4744: a door over a space whose types are split into datasets of their own stays
/// public with its record, licence included, and the feed leaves it out; `harvest: true` is
/// listed as an unset one is.
#[tokio::test]
async fn a_door_with_harvest_false_keeps_its_record_and_leaves_the_feed() {
    let realm = common::Realm::new();
    let block = |harvest| {
        Some(Arc::new(jc_core::kinds::Catalog {
            harvest,
            license: Some(jc_core::kinds::catalog::Licence::CcBy4),
            ..Default::default()
        }))
    };
    let mut door = endpoint(OPEN, "ovzdusie", Audience::Public);
    door.catalog = block(Some(false));
    let mut dataset = endpoint(ALSO_OPEN, "doprava", Audience::Public);
    dataset.catalog = block(Some(true));
    let served = || vec![door.clone(), dataset.clone()];

    let (status, _, _, body) = get(app_of(&realm, served()), "/catalog.jsonld", None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let feed: Value = serde_json::from_str(&body).expect("JSON-LD");
    let datasets: Vec<&str> = feed["dcat:dataset"]
        .as_array()
        .expect("datasets")
        .iter()
        .filter_map(|d| d["@id"].as_str())
        .collect();
    assert_eq!(datasets, vec![format!("{HOST}/api/endpoint/{ALSO_OPEN}")]);
    assert!(!body.contains(OPEN), "the feed names the door: {body}");
    let (_, _, _, turtle) = get(app_of(&realm, served()), "/catalog.ttl", None).await;
    assert!(
        !turtle.contains(OPEN),
        "the Turtle names the door: {turtle}"
    );

    let (status, _, _, record) = get(
        app_of(&realm, served()),
        &format!("/api/endpoint/{OPEN}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{record}");
    assert!(
        record.contains("licence/CC_BY_4_0"),
        "the door keeps its licence: {record}"
    );
}

#[tokio::test]
async fn the_turtle_says_the_same_and_a_members_token_widens_nothing() {
    let realm = common::Realm::new();
    let member = realm.mint(&json!({
        "iss": common::ISSUER,
        "sub": "an-employee",
        "aud": CLOSED,
        "preferred_username": "jana",
        "groups": ["/interne"],
        "exp": common::in_seconds(300),
        "iat": common::in_seconds(-10),
    }));
    let (anonymous_status, _, _, anonymous) =
        get(app_of(&realm, served()), "/catalog.ttl", None).await;
    let (status, media, _, body) =
        get(app_of(&realm, served()), "/catalog.ttl", Some(&member)).await;
    assert_eq!((anonymous_status, status), (StatusCode::OK, StatusCode::OK));
    assert_eq!(media, "text/turtle");
    assert_eq!(body, anonymous, "a token changes nothing the feed says");
    assert!(body.contains(&format!("<{HOST}/api/endpoint/{OPEN}>")));
    assert!(body.contains("dcat:Catalog"));
    assert!(!body.contains(CLOSED));
}

#[tokio::test]
async fn an_organization_that_publishes_nothing_answers_an_empty_catalogue() {
    let realm = common::Realm::new();
    let only_closed = vec![endpoint(CLOSED, "interne", Audience::Organization)];
    let (status, _, _, body) = get(app_of(&realm, only_closed), "/catalog.jsonld", None).await;
    assert_eq!(status, StatusCode::OK);
    let feed: Value = serde_json::from_str(&body).expect("JSON-LD");
    assert_eq!(feed["dcat:dataset"], json!([]));
    assert!(!body.contains(CLOSED));
}

/// The gateway of [`app_of`] for an Organization that serves `listed`, its public Endpoints
/// licensed CC BY 4.0 (T-4499).
fn app_serving(realm: &common::Realm, listed: &[jc_core::kinds::DcatProfile]) -> axum::Router {
    let licensed: Vec<Endpoint> = served()
        .into_iter()
        .map(|mut endpoint| {
            endpoint.catalog = Some(Arc::new(jc_core::kinds::Catalog {
                license: Some(jc_core::kinds::Licence::CcBy4),
                ..Default::default()
            }));
            endpoint
        })
        .collect();
    let gateway = Gateway::new(
        Broker::new("http://127.0.0.1:1"),
        Box::new(PolicyPdp),
        "banskabystrica.sk",
    )
    .serve(licensed)
    .authenticate(
        Arc::new(realm.verifier()),
        ServiceAccounts::new(),
        Some(HOST.to_owned()),
    );
    gateway.replace_limits(context_gateway::store::Limits {
        dcat_profiles: jc_dcat::profile::Offered::of(listed),
        ..Default::default()
    });
    router(Arc::new(gateway))
}

/// API/08 section 5: `?profile=` picks a rendering the Organization serves; DCAT-AP is the
/// default and always served, anything else is 404, and a profile changes no dataset listed.
#[tokio::test]
async fn a_profile_the_organization_serves_is_rendered_and_any_other_is_not_found() {
    use jc_core::kinds::DcatProfile;
    let realm = common::Realm::new();
    let sk_only = || app_serving(&realm, &[DcatProfile::DcatApSk]);

    let (_, _, _, core) = get(sk_only(), "/catalog.jsonld", None).await;
    let (status, _, _, explicit) = get(sk_only(), "/catalog.jsonld?profile=dcat-ap", None).await;
    assert_eq!((status, &explicit), (StatusCode::OK, &core));
    assert!(!core.contains("leg:termsOfUse"));

    let (status, media, _, sk) = get(sk_only(), "/catalog.jsonld?profile=dcat-ap-sk", None).await;
    assert_eq!(status, StatusCode::OK, "{sk}");
    assert_eq!(media, "application/ld+json");
    assert!(sk.contains("leg:termsOfUse"), "{sk}");
    let ids = |body: &str| {
        let feed: Value = serde_json::from_str(body).expect("JSON-LD");
        feed["dcat:dataset"].clone()
    };
    assert_eq!(ids(&sk), ids(&core));
    assert!(!sk.contains(CLOSED));

    let (status, _, _, turtle) = get(sk_only(), "/catalog.ttl?profile=dcat-ap-sk", None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(turtle.contains("leg:termsOfUse"), "{turtle}");

    for refused in [
        "/catalog.jsonld?profile=dcat-ap-cz",
        "/catalog.ttl?profile=dcat-ap-cz",
        "/catalog.jsonld?profile=nonsense",
        "/catalog.jsonld?profile=",
        &format!("/api/endpoint/{OPEN}/?profile=dcat-ap-cz"),
    ] {
        let (status, _, _, body) = get(sk_only(), refused, None).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{refused}: {body}");
    }

    let (status, _, _, record) = get(
        sk_only(),
        &format!("/api/endpoint/{OPEN}/?profile=dcat-ap-sk"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{record}");
    assert!(record.contains("leg:TermsOfUse"), "{record}");

    // an Organization that lists nothing serves the core alone
    let (status, _, _, _) = get(
        app_serving(&realm, &[]),
        "/catalog.jsonld?profile=dcat-ap-sk",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}
