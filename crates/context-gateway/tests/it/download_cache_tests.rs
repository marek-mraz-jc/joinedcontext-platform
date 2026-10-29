//! An anonymous `file.csv` is built once and served to every caller for a while (T-5629).
//!
//! On dev the helsinki-air-quality file (3 KB) took 4-22 s, nearly all of it the broker's
//! space-wide type list, and the third of three parallel downloads met the build ceiling as a
//! `503 Retry-After: 1`. The stub here answers each read after 100 ms and counts them.

use axum::body::Body;
use axum::extract::Request;
use axum::http::{Request as HttpRequest, StatusCode};
use axum::response::IntoResponse;
use axum::Router;
use context_gateway::app::{router, Gateway};
use context_gateway::pdp::PolicyPdp;
use context_gateway::proxy::Broker;
use context_gateway::resolver::Endpoint;
use jc_core::kinds::{Audience, Representation};
use serde_json::json;
use std::collections::BTreeSet;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tower::ServiceExt;

const SLUG: &str = "jtjnhkaau64zmzz5csmcwem52icvufy2";
const SLOW: Duration = Duration::from_millis(100);

#[derive(Default)]
struct Reads {
    types: AtomicUsize,
    entities: AtomicUsize,
}

async fn broker() -> (String, Arc<Reads>) {
    let reads = Arc::new(Reads::default());
    let counted = Arc::clone(&reads);
    let app = Router::new().fallback(move |request: Request| {
        let reads = Arc::clone(&counted);
        async move {
            tokio::time::sleep(SLOW).await;
            let path = request.uri().path();
            if path == "/ngsi-ld/v1/types" {
                reads.types.fetch_add(1, Ordering::SeqCst);
                return axum::Json(json!({
                    "type": "EntityTypeList",
                    "typeList": ["AirQualityObserved"]
                }))
                .into_response();
            }
            if path != format!("/x/views/{SLUG}/ngsi-ld/v1/entities") {
                return StatusCode::NOT_FOUND.into_response();
            }
            reads.entities.fetch_add(1, Ordering::SeqCst);
            axum::Json(json!([{
                "id": "urn:ngsi-ld:AirQualityObserved:helsinki:kallio",
                "type": "AirQualityObserved",
                "pm10": { "type": "Property", "value": 12 }
            }]))
            .into_response()
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("a free port");
    let port = listener.local_addr().expect("an address").port();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (format!("http://127.0.0.1:{port}"), reads)
}

fn endpoint() -> Endpoint {
    Endpoint {
        declared_types: None,
        roles: Default::default(),
        slug: SLUG.to_owned(),
        title: std::collections::BTreeMap::new(),
        description: std::collections::BTreeMap::new(),
        space: "helsinki".to_owned(),
        project: "helsinki".to_owned(),
        audience: Audience::Public,
        allowed_projects: Vec::new(),
        representations: vec![Representation::NgsiLd, Representation::Csv],
        rate_limit: None,
        creates: None,
        file_limits: None,
        hidden_attributes: BTreeSet::new(),
        projection: None,
        base_path: format!("/api/endpoint/{SLUG}"),
        models: Vec::new(),
        view_mapping: None,
        catalog: None,
        deprecation: None,
        feeds: Vec::new(),
        calendars: Vec::new(),
        map_defaults: Default::default(),
        policy_names: Vec::new(),
        manifest_name: String::new(),
        policies: vec![serde_norway::from_str(
            "contextSpaceRef: helsinki\n\
             assigner: did:web:hel.fi\n\
             assignee: { kind: role, id: public }\n\
             operations: [queryEntity, retrieveEntity]\n",
        )
        .expect("the policy spec parses")],
    }
}

/// One build slot, so a second build at the same time is the `503` dev answered.
async fn gateway() -> (Router, Arc<Reads>) {
    let (url, reads) = broker().await;
    let gateway = Gateway::new(Broker::new(url), Box::new(PolicyPdp), "hel.fi")
        .serve([endpoint()])
        .limit_in_flight(128, 32, 1);
    (router(Arc::new(gateway)), reads)
}

async fn download(app: &Router, query: &str) -> (StatusCode, String) {
    let response = app
        .clone()
        .oneshot(
            HttpRequest::builder()
                .uri(format!("/api/endpoint/{SLUG}/file.csv{query}"))
                .body(Body::empty())
                .expect("a request"),
        )
        .await
        .expect("the gateway answers");
    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .expect("a readable body");
    (status, String::from_utf8_lossy(&body).into_owned())
}

#[tokio::test]
async fn parallel_anonymous_downloads_share_one_build_slot() {
    let (app, reads) = gateway().await;
    let (a, b, c) = tokio::join!(download(&app, ""), download(&app, ""), download(&app, ""));
    for (status, body) in [&a, &b, &c] {
        assert_eq!(*status, StatusCode::OK, "{body}");
    }
    assert!(
        a.1.contains("urn:ngsi-ld:AirQualityObserved:helsinki:kallio"),
        "{}",
        a.1
    );
    assert_eq!(a.1, b.1);
    assert_eq!(a.1, c.1);
    assert_eq!(reads.entities.load(Ordering::SeqCst), 1);
    assert_eq!(reads.types.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn a_cached_file_serves_without_the_broker_or_a_build_slot() {
    let (app, reads) = gateway().await;
    let (status, first) = download(&app, "").await;
    assert_eq!(status, StatusCode::OK, "{first}");
    // The timing bound: rebuilt, ten downloads would take at least 2 s behind this broker.
    let started = Instant::now();
    for _ in 0..10 {
        assert_eq!(download(&app, "").await, (StatusCode::OK, first.clone()));
    }
    let took = started.elapsed();
    assert!(took < SLOW * 5, "ten cached downloads took {took:?}");
    assert_eq!(reads.entities.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn another_query_is_built_on_the_held_type_list() {
    let (app, reads) = gateway().await;
    assert_eq!(download(&app, "").await.0, StatusCode::OK);
    let (status, body) = download(&app, "?delimiter=semicolon").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(reads.entities.load(Ordering::SeqCst), 2);
    assert_eq!(reads.types.load(Ordering::SeqCst), 1);
}
