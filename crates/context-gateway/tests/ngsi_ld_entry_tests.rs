//! The NGSI-LD access URL every Endpoint advertises (EP-90, T-2932): the DCAT record and the
//! catalogue give a person `…/ngsi-ld/v1/`, which answered `404` because the ETSI tree starts
//! one segment deeper. It answers an entry document now, naming only what the caller may read.

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use context_gateway::app::{router, Gateway};
use context_gateway::auth::accounts::ServiceAccounts;
use context_gateway::pdp::PolicyPdp;
use context_gateway::proxy::Broker;
use context_gateway::resolver::{Endpoint, Model};
use jc_core::kinds::{Audience, PolicySpec, Representation};
use serde_json::{json, Value};
use std::sync::Arc;
use tower::ServiceExt;

const OPEN: &str = "k4y7pq2mzt6vhx3nbwrs5cjd8f";
const WIDE: &str = "w8k3zq6nxv2htb5rjs9cyd4gpm";
const CLOSED: &str = "p9d2wc5kzn8mth4rqvb7xj3sfy";
const FILES_ONLY: &str = "f3m8rt2wqz5nkc7xhv4bpj6dsy";
const BASE: &str = "https://bb.example.sk";

fn policy(types: &[&str]) -> PolicySpec {
    let entities: String = types
        .iter()
        .map(|t| format!("      - type: {t}\n"))
        .collect();
    serde_norway::from_str(&format!(
        "contextSpaceRef: ovzdusie\nassigner: did:web:banskabystrica.sk\n\
         assignee: {{ kind: role, id: public }}\noperations: [queryEntity, retrieveEntity]\n\
         information:\n  - entities:\n{entities}"
    ))
    .expect("the policy spec parses")
}

fn model() -> Model {
    Model {
        name: "bb-air-quality".to_owned(),
        version: "1.4.0".to_owned(),
        major: 1,
        classes: vec![
            "AirQualityObserved".to_owned(),
            "InternalIncident".to_owned(),
        ],
        json_schema: None,
        context: None,
    }
}

fn endpoint(
    slug: &str,
    audience: Audience,
    representations: Vec<Representation>,
    types: &[&str],
) -> Endpoint {
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
        representations,
        rate_limit: None,
        file_limits: None,
        hidden_attributes: Default::default(),
        projection: None,
        base_path: format!("/api/endpoint/{slug}"),
        models: vec![model()],
        view_mapping: None,
        catalog: None,
        policies: vec![policy(types)],
    }
}

fn app() -> axum::Router {
    let realm = common::Realm::new();
    let ngsi = || vec![Representation::NgsiLd];
    router(Arc::new(
        Gateway::new(
            Broker::new("http://127.0.0.1:1"),
            Box::new(PolicyPdp),
            "banskabystrica.sk",
        )
        .serve([
            endpoint(OPEN, Audience::Public, ngsi(), &["AirQualityObserved"]),
            endpoint(
                WIDE,
                Audience::Public,
                ngsi(),
                &["AirQualityObserved", "InternalIncident"],
            ),
            endpoint(
                CLOSED,
                Audience::Organization,
                ngsi(),
                &["AirQualityObserved"],
            ),
            endpoint(
                FILES_ONLY,
                Audience::Public,
                vec![Representation::GeoJson],
                &["AirQualityObserved"],
            ),
        ])
        .authenticate(
            Arc::new(realm.verifier()),
            ServiceAccounts::new(),
            Some(BASE.to_owned()),
        ),
    ))
}

async fn get(path: &str) -> (StatusCode, String) {
    let response = app()
        .oneshot(
            Request::builder()
                .uri(path)
                .body(Body::empty())
                .expect("a request"),
        )
        .await
        .expect("the gateway answers");
    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), 256 * 1024)
        .await
        .expect("a readable body");
    (status, String::from_utf8_lossy(&body).into_owned())
}

/// The URL the catalogue links answers 200 with where to go next, on both spellings.
#[tokio::test]
async fn the_advertised_access_url_answers_an_entry_document() {
    for path in [
        format!("/api/endpoint/{OPEN}/ngsi-ld/v1/"),
        format!("/api/endpoint/{OPEN}/ngsi-ld/v1"),
    ] {
        let (status, body) = get(&path).await;
        assert_eq!(status, StatusCode::OK, "{path}: {body}");
        let document: Value = serde_json::from_str(&body).expect("JSON");
        let root = format!("{BASE}/api/endpoint/{OPEN}");
        assert_eq!(
            document,
            json!({
                "entities": format!("{root}/ngsi-ld/v1/entities"),
                "types": [{
                    "type": "AirQualityObserved",
                    "query": format!("{root}/ngsi-ld/v1/entities?type=AirQualityObserved"),
                }],
                "access": format!("{root}/access"),
                "schema": format!("{root}/schema/index.json"),
            }),
            "{path}"
        );
    }
}

/// The types are the grant's: a class of the model nobody granted is not named, and a wider
/// grant names both.
#[tokio::test]
async fn the_entry_names_only_the_types_the_caller_may_read() {
    let (_, narrow) = get(&format!("/api/endpoint/{OPEN}/ngsi-ld/v1/")).await;
    assert!(!narrow.contains("InternalIncident"), "{narrow}");

    let (status, wide) = get(&format!("/api/endpoint/{WIDE}/ngsi-ld/v1/")).await;
    assert_eq!(status, StatusCode::OK, "{wide}");
    assert!(wide.contains("\"type\":\"InternalIncident\""), "{wide}");
    assert!(wide.contains("\"type\":\"AirQualityObserved\""), "{wide}");
}

/// A refused caller, an unknown slug and an Endpoint that serves no NGSI-LD are answered as the
/// ETSI tree below would answer them (EP-23): the entry tells nobody more than the tree does.
#[tokio::test]
async fn the_entry_refuses_exactly_where_the_tree_refuses() {
    for slug in [CLOSED, FILES_ONLY, "zz0000000000000000000000zz"] {
        let (entry, entry_body) = get(&format!("/api/endpoint/{slug}/ngsi-ld/v1/")).await;
        let (tree, tree_body) = get(&format!(
            "/api/endpoint/{slug}/ngsi-ld/v1/entities?type=AirQualityObserved"
        ))
        .await;
        assert!(!entry.is_success(), "{slug}: {entry_body}");
        assert_eq!(entry, tree, "{slug}: {entry_body} vs {tree_body}");
        assert!(!entry_body.contains("AirQualityObserved"), "{entry_body}");
    }
}

/// CIM 009 5.7.2.4 stays a 400, and its detail sends the caller to a document it may read,
/// no longer to `/types`, which the grants of a public Endpoint refuse (T-1211).
#[tokio::test]
async fn an_unselected_query_points_at_the_entry() {
    let (status, body) = get(&format!("/api/endpoint/{OPEN}/ngsi-ld/v1/entities")).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(body.contains("/ngsi-ld/v1/ lists the types"), "{body}");
    assert!(!body.contains("Ask /ngsi-ld/v1/types"), "{body}");
}
