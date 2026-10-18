//! Every route the gateway served before its surfaces moved out of `app.rs` is still served
//! (T-4472): a request on each reaches a handler, never the router's fallback.

use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use axum::Router;
use context_gateway::app::{router, Gateway};
use context_gateway::pdp::PolicyPdp;
use context_gateway::proxy::Broker;
use std::sync::Arc;
use tower::ServiceExt;

/// The router's own fallback answers 404 like an unknown slug does, so a test cannot tell the
/// two apart; this one answers a status no handler ever does.
const UNROUTED: StatusCode = StatusCode::IM_A_TEAPOT;

/// One request per route `app::router` held on 2026-10-17 (origin/main 92403bf), both spellings
/// where it routes both, a path parameter filled in and a wildcard given a tail.
const ROUTES: &[(&str, &str)] = &[
    ("GET", "/api/endpoint/s"),
    ("GET", "/api/endpoint/s/"),
    ("GET", "/api/endpoint/s/ngsi-ld/v1"),
    ("GET", "/api/endpoint/s/ngsi-ld/v1/"),
    ("GET", "/api/endpoint/s/ngsi-ld/v1/entities"),
    ("POST", "/api/endpoint/s/ngsi-ld/v1/entities"),
    ("GET", "/api/endpoint/s/access"),
    ("POST", "/api/endpoint/s/mcp"),
    ("POST", "/api/mcp"),
    ("GET", "/api/endpoints"),
    ("GET", "/api/mcp/.well-known/oauth-protected-resource"),
    ("POST", "/api/mcp/p/n"),
    ("GET", "/api/mcp/p/n/.well-known/oauth-protected-resource"),
    ("POST", "/api/endpoint/s/access/check"),
    ("POST", "/api/endpoint/s/access/simulate"),
    ("GET", "/api/endpoint/s/file.csv"),
    ("GET", "/api/endpoint/s/ogc/features"),
    ("GET", "/api/endpoint/s/ogc/features/"),
    ("GET", "/api/endpoint/s/ogc/features/collections"),
    ("GET", "/api/endpoint/s/ws"),
    ("GET", "/api/endpoint/s/sta/v1.1"),
    ("GET", "/api/endpoint/s/sta/v1.1/"),
    ("GET", "/api/endpoint/s/sta/v1.1/Things"),
    ("GET", "/api/endpoint/s/odata/v4"),
    ("GET", "/api/endpoint/s/odata/v4/"),
    ("GET", "/api/endpoint/s/odata/v4/Entities"),
    ("GET", "/catalog.jsonld"),
    ("GET", "/catalog.ttl"),
    ("GET", "/cs"),
    ("GET", "/cs/sp"),
    ("GET", "/cs/sp/ngsi-ld/v1/entities"),
    ("POST", "/cs/sp/mcp"),
    ("GET", "/cs/sp/schema/index.json"),
    ("GET", "/cs/sp/schema/1/model.json"),
    (
        "GET",
        "/api/endpoint/s/.well-known/oauth-protected-resource",
    ),
    ("GET", "/api/endpoint/s/schema/index.json"),
    ("GET", "/api/endpoint/s/openapi.json"),
    ("GET", "/api/endpoint/s/schema/1/model.json"),
    ("GET", "/api/endpoint/s/schema/1/mappings/"),
    ("GET", "/api/endpoint/s/schema/1/mappings/m.yaml"),
    ("GET", "/healthz"),
    ("GET", "/livez"),
    ("GET", "/metrics"),
];

/// The files an endpoint stopped publishing (T-4597, API/02 §5): no handler serves them.
const REMOVED: &[&str] = &[
    "/api/endpoint/s/file.geojson",
    "/api/endpoint/s/file.json",
    "/api/endpoint/s/file.xlsx",
    "/api/endpoint/s/file.parquet",
    "/api/endpoint/s/file.zip",
    "/cs/sp/dump/",
];

fn app() -> Router {
    router(Arc::new(Gateway::new(
        Broker::new("http://127.0.0.1:1"),
        Box::new(PolicyPdp),
        "example.org",
    )))
    .fallback(|| async { UNROUTED })
}

#[tokio::test]
async fn every_route_of_the_surface_reaches_a_handler() {
    let mut unrouted = Vec::new();
    for (method, path) in ROUTES {
        let request = Request::builder()
            .method(Method::from_bytes(method.as_bytes()).expect("a method"))
            .uri(*path)
            .header("upgrade", "websocket")
            .body(Body::empty())
            .expect("a request");
        let status = app().oneshot(request).await.expect("an answer").status();
        if status == UNROUTED {
            unrouted.push(format!("{method} {path}"));
        }
    }
    assert!(unrouted.is_empty(), "not routed any more: {unrouted:?}");
}

#[tokio::test]
async fn a_path_off_the_surface_reaches_the_fallback() {
    let request = Request::builder()
        .uri("/api/endpoint/s/nothing-here")
        .body(Body::empty())
        .expect("a request");
    let status = app().oneshot(request).await.expect("an answer").status();
    assert_eq!(status, UNROUTED);
}

/// T-4597: a removed file reaches no handler, and the gateway answers it with the `404` of any
/// child an endpoint does not serve.
#[tokio::test]
async fn a_removed_file_is_routed_nowhere_and_answers_404() {
    for path in REMOVED {
        let request = || Request::builder().uri(*path).body(Body::empty());
        let status = app()
            .oneshot(request().expect("a request"))
            .await
            .expect("an answer")
            .status();
        assert_eq!(status, UNROUTED, "{path} still has a handler");
        let served = router(Arc::new(Gateway::new(
            Broker::new("http://127.0.0.1:1"),
            Box::new(PolicyPdp),
            "example.org",
        )))
        .oneshot(request().expect("a request"))
        .await
        .expect("an answer")
        .status();
        assert_eq!(served, StatusCode::NOT_FOUND, "{path}");
    }
}
