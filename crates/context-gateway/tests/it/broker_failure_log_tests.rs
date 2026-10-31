//! A broker failure behind a representation is logged by its problem type and a cut of its
//! detail, never its whole body: the body can hold up to 8 MiB of entity values (F-SEC1-019,
//! F-R2PLT2-013).

use crate::common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::response::IntoResponse;
use axum::Router;
use context_gateway::app::{router, Gateway};
use context_gateway::pdp::PolicyPdp;
use context_gateway::proxy::Broker;
use context_gateway::resolver::Endpoint;
use jc_core::kinds::{Audience, Representation};
use serde_json::json;
use std::sync::Arc;
use tower::ServiceExt;

const SLUG: &str = "fq3lmz7wtk2pxv4nbrs6cjd5ha";

fn endpoint() -> Endpoint {
    Endpoint {
        declared_types: None,
        roles: Default::default(),
        slug: SLUG.to_owned(),
        title: Default::default(),
        description: Default::default(),
        space: "ovzdusie".to_owned(),
        project: "ovzdusie".to_owned(),
        audience: Audience::Public,
        allowed_projects: Vec::new(),
        representations: vec![Representation::NgsiLd, Representation::Csv],
        rate_limit: None,
        creates: None,
        file_limits: None,
        hidden_attributes: Default::default(),
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
            "contextSpaceRef: ovzdusie\nassigner: did:web:banskabystrica.sk\n\
             assignee: { kind: role, id: public }\noperations: [queryEntity, retrieveEntity]\n",
        )
        .expect("the policy spec parses")],
    }
}

#[tokio::test]
async fn a_failed_read_behind_a_representation_logs_no_body() {
    let captured = common::Log::capture();
    let held = "v".repeat(20_000);
    let failing = Router::new().fallback(move || {
        let held = held.clone();
        async move {
            let problem = json!({"type": "urn:antares:error:InternalError", "detail": held});
            (StatusCode::INTERNAL_SERVER_ERROR, axum::Json(problem)).into_response()
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("a free port");
    let upstream = format!("http://{}", listener.local_addr().expect("an address"));
    tokio::spawn(async move {
        let _ = axum::serve(listener, failing).await;
    });
    let app = router(Arc::new(
        Gateway::new(
            Broker::new(&upstream),
            Box::new(PolicyPdp),
            "banskabystrica.sk",
        )
        .serve([endpoint()]),
    ));

    let response = app
        .oneshot(
            Request::get(format!("/api/endpoint/{SLUG}/file.csv"))
                .body(Body::empty())
                .expect("a request"),
        )
        .await
        .expect("the gateway answers");

    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    let log = captured.text();
    assert!(log.contains("behind a representation"), "{log}");
    assert!(log.contains("urn:antares:error:InternalError"), "{log}");
    assert!(!log.contains(&"v".repeat(301)), "the whole body was logged");
}
