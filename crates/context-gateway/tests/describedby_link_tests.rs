//! Every data response says where its schema is, and no refusal does (T-2381; EP-50, EP-51,
//! EP-46).
//!
//! A client that follows `rel="describedby"` is how a validator, a GIS client and an agent find
//! the shapes without a person telling them the URL. The link is written in the response layer
//! rather than in the handlers, so a representation added later carries it without anyone
//! remembering to; what the tests below hold is the other half of that: a refusal carries none,
//! because a `404` that named a schema would tell a caller the endpoint exists.

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use axum::routing::any;
use axum::Router;
use context_gateway::app::{router, Gateway};
use context_gateway::pdp::PolicyPdp;
use context_gateway::proxy::Broker;
use context_gateway::resolver::{Endpoint, Model, Space};
use jc_core::kinds::{Audience, PolicySpec, Representation};
use serde_json::{json, Value};
use std::sync::Arc;
use tower::ServiceExt;

const SLUG: &str = "k4y7pq2mzt6vhx3nbwrs5cjd8f";
const SPACE: &str = "ovzdusie";
const DOMAIN: &str = "bb.sk";
const PUBLIC: &str = "https://city.example";

/// The entity every representation is built from: it has a geometry, so the GeoJSON and OGC
/// doors can answer it too.
fn station() -> Value {
    json!({
        "id": "urn:ngsi-ld:AirQualityObserved:bb.sk:ovzdusie:station-01",
        "type": "AirQualityObserved",
        "pm10": { "type": "Property", "value": 34.2 },
        "location": {
            "type": "GeoProperty",
            "value": { "type": "Point", "coordinates": [19.15, 48.73] },
        },
    })
}

async fn broker() -> String {
    let app = Router::new().fallback(any(|| async { axum::Json(json!([station()])) }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("a free port");
    let address = listener.local_addr().expect("an address");
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    format!("http://{address}")
}

fn model(name: &str, major: u32) -> Model {
    Model {
        name: name.to_owned(),
        version: format!("{major}.0.0"),
        major,
        classes: vec!["AirQualityObserved".to_owned()],
        json_schema: None,
        context: None,
    }
}

fn policy(yaml: &str) -> PolicySpec {
    serde_norway::from_str(yaml).expect("the policy spec parses")
}

/// A public endpoint serving every representation, over two model majors.
fn endpoint(models: Vec<Model>) -> Endpoint {
    Endpoint {
        declared_types: None,
        roles: Default::default(),
        slug: SLUG.to_owned(),
        title: Default::default(),
        description: Default::default(),
        space: SPACE.to_owned(),
        project: SPACE.to_owned(),
        audience: Audience::Public,
        allowed_projects: Vec::new(),
        representations: vec![
            Representation::NgsiLd,
            Representation::GeoJson,
            Representation::Csv,
            Representation::OgcFeatures,
            Representation::Sta,
        ],
        rate_limit: None,
        file_limits: None,
        hidden_attributes: Default::default(),
        projection: None,
        view_mapping: None,
        base_path: format!("/api/endpoint/{SLUG}"),
        models,
        policies: vec![policy(
            "contextSpaceRef: ovzdusie\n\
             assigner: did:web:bb.sk\n\
             assignee: { kind: role, id: public }\n\
             operations: [queryEntity, retrieveEntity]\n\
             information:\n  - entities:\n      - type: AirQualityObserved\n",
        )],
    }
}

async fn app(models: Vec<Model>) -> Router {
    let upstream = broker().await;
    let served = endpoint(models);
    let space = Space {
        endpoint: Arc::new(served.clone()),
        title: Default::default(),
        description: Default::default(),
        is_sandbox: false,
        default_locale: None,
    };
    let mut gateway = Gateway::new(Broker::new(upstream), Box::new(PolicyPdp), DOMAIN)
        .serve([served])
        .serve_spaces([space]);
    // What the deployment publishes as its own address: the link is absolute where it is
    // configured and a path where it is not, and never built from a `Host` header.
    gateway.public_url = Some(PUBLIC.to_owned());
    router(Arc::new(gateway))
}

/// Every `Link` header of one request, in the order they were written.
async fn links_of(uri: &str) -> (StatusCode, Vec<String>) {
    sent("GET", uri).await
}

async fn sent(method: &str, uri: &str) -> (StatusCode, Vec<String>) {
    let response = app(vec![model("air-quality", 1)])
        .await
        .oneshot(
            Request::builder()
                .method(method)
                .uri(uri)
                .body(Body::empty())
                .expect("a request"),
        )
        .await
        .expect("the gateway answers");
    let status = response.status();
    let links = response
        .headers()
        .get_all(header::LINK)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .map(str::to_owned)
        .collect();
    (status, links)
}

fn describedby(links: &[String]) -> Vec<&String> {
    links
        .iter()
        .filter(|link| link.contains("rel=\"describedby\""))
        .collect()
}

/// EP-50: the data representations all point at the schema surface that serves the shapes.
#[tokio::test]
async fn every_data_representation_names_its_schema() {
    for uri in [
        format!("/api/endpoint/{SLUG}/ngsi-ld/v1/entities?type=AirQualityObserved"),
        format!("/api/endpoint/{SLUG}/ngsi-ld/v1/entities/urn:ngsi-ld:AirQualityObserved:bb.sk:ovzdusie:station-01"),
        format!("/api/endpoint/{SLUG}/file.geojson"),
        format!("/api/endpoint/{SLUG}/file.csv"),
        format!("/api/endpoint/{SLUG}/ogc/features/collections/AirQualityObserved/items"),
        format!("/api/endpoint/{SLUG}/sta/v1.1/Things"),
        format!("/cs/{SPACE}/ngsi-ld/v1/entities?type=AirQualityObserved"),
    ] {
        let (status, links) = links_of(&uri).await;
        assert!(status.is_success(), "{uri} answered {status}");
        let described = describedby(&links);
        assert_eq!(described.len(), 2, "{uri} carries {links:?}");

        let schema = format!(
            "<{PUBLIC}/api/endpoint/{SLUG}/schema/v1/model.schema.json>; \
             rel=\"describedby\"; type=\"application/schema+json\""
        );
        let shapes = format!(
            "<{PUBLIC}/api/endpoint/{SLUG}/schema/v1/model.shacl.ttl>; \
             rel=\"describedby\"; type=\"text/turtle\""
        );
        assert!(described.contains(&&schema), "{uri} carries {described:?}");
        assert!(described.contains(&&shapes), "{uri} carries {described:?}");
    }
}

/// An endpoint publishing two majors names both, because a client reading the older data
/// needs the older shapes (EP-46).
#[tokio::test]
async fn an_endpoint_with_two_majors_names_four_documents() {
    let response = app(vec![model("air-quality", 1), model("air-quality", 2)])
        .await
        .oneshot(
            Request::builder()
                .uri(format!(
                    "/api/endpoint/{SLUG}/ngsi-ld/v1/entities?type=AirQualityObserved"
                ))
                .body(Body::empty())
                .expect("a request"),
        )
        .await
        .expect("the gateway answers");

    let links: Vec<String> = response
        .headers()
        .get_all(header::LINK)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .map(str::to_owned)
        .collect();
    let described = describedby(&links);
    assert_eq!(described.len(), 4, "{links:?}");
    for major in [1, 2] {
        assert!(
            described
                .iter()
                .any(|link| link.contains(&format!("/schema/v{major}/model.schema.json"))),
            "major {major} is named: {described:?}"
        );
    }
}

/// EP-03, R20: a refusal names no schema. A `404` that carried one would tell a caller who
/// may not use this endpoint, or who asked for an entity nobody has, that it exists.
#[tokio::test]
async fn no_refusal_names_a_schema() {
    for uri in [
        // A slug nobody serves.
        "/api/endpoint/zzz7pq2mzt6vhx3nbwrs5cjd8f/ngsi-ld/v1/entities?type=AirQualityObserved".to_owned(),
        // An entity id of another space.
        format!("/api/endpoint/{SLUG}/ngsi-ld/v1/entities/urn:ngsi-ld:AirQualityObserved:bb.sk:doprava:station-01"),
        // A query naming no selector at all.
        format!("/api/endpoint/{SLUG}/ngsi-ld/v1/entities"),
    ] {
        let (status, links) = links_of(&uri).await;
        assert!(!status.is_success(), "{uri} answered {status}");
        assert!(
            describedby(&links).is_empty(),
            "{uri} answered {status} and named a schema: {links:?}"
        );
    }

    // And an operation this grant does not hold at all, which is the other refusal (GW1).
    let (status, links) = sent(
        "DELETE",
        &format!(
            "/api/endpoint/{SLUG}/ngsi-ld/v1/entities/\
             urn:ngsi-ld:AirQualityObserved:bb.sk:ovzdusie:station-01"
        ),
    )
    .await;
    assert!(!status.is_success(), "a delete answered {status}");
    assert!(
        describedby(&links).is_empty(),
        "a refused delete named a schema: {links:?}"
    );
}

/// The schema documents describe themselves, and so do the endpoint record and the access
/// surface: a `describedby` pointing at the document you are reading is noise.
#[tokio::test]
async fn a_document_that_is_not_data_names_no_schema() {
    for uri in [
        format!("/api/endpoint/{SLUG}"),
        format!("/api/endpoint/{SLUG}/schema/index.json"),
        format!("/api/endpoint/{SLUG}/access"),
        "/healthz".to_owned(),
    ] {
        let (status, links) = links_of(&uri).await;
        assert!(status.is_success(), "{uri} answered {status}");
        assert!(
            describedby(&links).is_empty(),
            "{uri} named a schema: {links:?}"
        );
    }
}
