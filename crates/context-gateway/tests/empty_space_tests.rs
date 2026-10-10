//! A space nobody has written to yet (EP-22, GW33, T-3434).
//!
//! The broker keeps no tenant for such a space and answers every read `404 NonexistentTenant`
//! (CIM 009 5.5.2). The caller was admitted to the space, so a query of it is a query of an empty
//! space: `200` with the empty result; a retrieval of one thing is still a miss. A caller the
//! Endpoint refuses, and a slug that is not there, never reach the broker at all.

mod common;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::routing::any;
use axum::Router;
use context_gateway::app::{router, Gateway};
use context_gateway::auth::accounts::ServiceAccounts;
use context_gateway::pdp::PolicyPdp;
use context_gateway::proxy::Broker;
use context_gateway::resolver::Endpoint;
use jc_core::kinds::{Audience, PolicySpec, Representation};
use serde_json::{json, Value};
use tower::ServiceExt;

const OPEN: &str = "k4y7pq2mzt6vhx3nbwrs5cjd8f";
const CLOSED: &str = "p9d2wc5kzn8mth4rqvb7xj3sfy";
const ENTITY: &str = "urn:ngsi-ld:KeyPerformanceIndicator:zilina-kpi:population";

fn endpoint(slug: &str, audience: Audience) -> Endpoint {
    let policy: PolicySpec = serde_norway::from_str(
        "contextSpaceRef: zilina-kpi\nassigner: did:web:zilina.sk\nassignee: { kind: role, id: public }\n\
         operations: [queryEntity, retrieveEntity, queryTemporal, retrieveTemporal, retrieveEntityTypes, retrieveAttrTypes]\n\
         information:\n  - entities: [{ type: KeyPerformanceIndicator }]\n    propertyNames: [kpiValue]\n",
    )
    .expect("the policy spec parses");
    Endpoint {
        declared_types: None,
        roles: Default::default(),
        slug: slug.to_owned(),
        title: Default::default(),
        description: Default::default(),
        space: "zilina-kpi".to_owned(),
        project: "zilina".to_owned(),
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
        policies: vec![policy],
    }
}

/// A broker that answers every request with `404` of `kind`, counting what reached it.
async fn broker(kind: &'static str) -> (String, Arc<AtomicUsize>) {
    let hits = Arc::new(AtomicUsize::new(0));
    let counted = Arc::clone(&hits);
    let app = Router::new().fallback(any(move || {
        let counted = Arc::clone(&counted);
        async move {
            counted.fetch_add(1, Ordering::SeqCst);
            (
                StatusCode::NOT_FOUND,
                [(axum::http::header::CONTENT_TYPE, "application/json")],
                json!({
                    "type": format!("https://uri.etsi.org/ngsi-ld/errors/{kind}"),
                    "title": kind,
                    "detail": "tenant zilina-kpi does not exist"
                })
                .to_string(),
            )
        }
    }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("a port");
    let address = listener.local_addr().expect("an address");
    tokio::spawn(async move { axum::serve(listener, app).await.expect("the stub serves") });
    (format!("http://{address}"), hits)
}

async fn call(upstream: &str, uri: &str) -> (StatusCode, Value) {
    let gateway = Gateway::new(Broker::new(upstream), Box::new(PolicyPdp), "zilina.sk")
        .serve([
            endpoint(OPEN, Audience::Public),
            endpoint(CLOSED, Audience::Organization),
        ])
        .authenticate(
            Arc::new(common::Realm::new().verifier()),
            ServiceAccounts::new(),
            Some("https://zilina.example.sk".to_owned()),
        );
    let response = router(Arc::new(gateway))
        .oneshot(
            Request::builder()
                .uri(uri)
                .body(Body::empty())
                .expect("a request"),
        )
        .await
        .expect("the gateway answers");
    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), 256 * 1024)
        .await
        .expect("a body");
    (status, serde_json::from_slice(&body).unwrap_or(Value::Null))
}

#[tokio::test]
async fn a_query_of_a_space_with_no_tenant_answers_an_empty_list() {
    let (upstream, _) = broker("NonexistentTenant").await;
    for rest in [
        "entities?type=KeyPerformanceIndicator",
        "temporal/entities?type=KeyPerformanceIndicator&timerel=after&timeAt=2026-01-01T00:00:00Z",
    ] {
        let (status, body) = call(
            &upstream,
            &format!("/api/endpoint/{OPEN}/ngsi-ld/v1/{rest}"),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{rest}: {body}");
        assert_eq!(body, json!([]), "{rest}");
    }
}

#[tokio::test]
async fn the_type_and_attribute_listings_of_an_empty_space_are_empty_documents() {
    let (upstream, _) = broker("NonexistentTenant").await;

    let (status, types) = call(&upstream, &format!("/api/endpoint/{OPEN}/ngsi-ld/v1/types")).await;
    assert_eq!(status, StatusCode::OK, "{types}");
    assert_eq!(types["type"], "EntityTypeList");
    assert_eq!(types["typeList"], json!([]));
    assert!(types["id"]
        .as_str()
        .is_some_and(|id| id.starts_with("urn:ngsi-ld:EntityTypeList:")));

    let (status, attributes) = call(
        &upstream,
        &format!("/api/endpoint/{OPEN}/ngsi-ld/v1/attributes"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{attributes}");
    assert_eq!(attributes["type"], "AttributeList");
    assert_eq!(attributes["attributeList"], json!([]));
}

#[tokio::test]
async fn a_retrieval_by_id_in_an_empty_space_is_still_not_found() {
    let (upstream, _) = broker("NonexistentTenant").await;
    for rest in [
        format!("entities/{ENTITY}"),
        format!("temporal/entities/{ENTITY}"),
    ] {
        let (status, body) = call(
            &upstream,
            &format!("/api/endpoint/{OPEN}/ngsi-ld/v1/{rest}"),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{rest}: {body}");
        assert_eq!(
            body["type"],
            "https://uri.etsi.org/ngsi-ld/errors/ResourceNotFound"
        );
        // The broker's wording, which names the tenant, is not passed on (R20).
        assert!(!body.to_string().contains("tenant"), "{body}");
    }
}

#[tokio::test]
async fn any_other_miss_of_the_broker_stays_a_miss() {
    let (upstream, _) = broker("ResourceNotFound").await;
    let (status, _) = call(
        &upstream,
        &format!("/api/endpoint/{OPEN}/ngsi-ld/v1/entities?type=KeyPerformanceIndicator"),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn a_refused_caller_and_an_unknown_endpoint_get_their_errors_never_an_empty_list() {
    let (upstream, hits) = broker("NonexistentTenant").await;

    let (refused, body) = call(
        &upstream,
        &format!("/api/endpoint/{CLOSED}/ngsi-ld/v1/entities?type=KeyPerformanceIndicator"),
    )
    .await;
    assert_eq!(refused, StatusCode::UNAUTHORIZED, "{body}");

    let (missing, body) = call(
        &upstream,
        "/api/endpoint/zz9zz9zz9zz9zz9zz9zz9zz9zz/ngsi-ld/v1/entities?type=KeyPerformanceIndicator",
    )
    .await;
    assert_eq!(missing, StatusCode::NOT_FOUND, "{body}");
    assert_ne!(body, json!([]));
    assert_eq!(hits.load(Ordering::SeqCst), 0, "neither reached the broker");
}
