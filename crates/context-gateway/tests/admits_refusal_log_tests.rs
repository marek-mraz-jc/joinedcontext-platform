//! T-3033: a token whose groups name none of a project-bound Endpoint's projects is refused
//! before the PDP, so no decision line is written; the refusal names itself in a warn line.
//! The status stays 403 (NGSI-LD read surface rule).
//!
//! Alone in its binary: it installs the process's subscriber.

mod common;

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use context_gateway::app::{router, Gateway};
use context_gateway::auth::accounts::ServiceAccounts;
use context_gateway::pdp::PolicyPdp;
use context_gateway::proxy::Broker;
use context_gateway::resolver::Endpoint;
use jc_core::kinds::{Audience, Representation};
use serde_json::json;
use std::sync::{Arc, Mutex};
use tower::ServiceExt;

const SLUG: &str = "k4y7pq2mzt6vhx3nbwrs5cjd8f";

/// A writer the test reads back.
#[derive(Clone, Default)]
struct Captured(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for Captured {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().expect("the log").extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn endpoint() -> Endpoint {
    Endpoint {
        declared_types: None,
        roles: Default::default(),
        slug: SLUG.to_owned(),
        title: Default::default(),
        description: Default::default(),
        space: "alerts".to_owned(),
        project: "helsinki".to_owned(),
        audience: Audience::ProjectList,
        allowed_projects: Vec::new(),
        representations: vec![Representation::NgsiLd],
        rate_limit: None,
        file_limits: None,
        hidden_attributes: Default::default(),
        projection: None,
        base_path: format!("/api/endpoint/{SLUG}"),
        models: Vec::new(),
        view_mapping: None,
        catalog: None,
        policies: Vec::new(),
    }
}

#[tokio::test]
async fn a_token_naming_no_admitted_project_is_refused_and_the_refusal_is_logged() {
    let captured = Captured::default();
    let writer = captured.clone();
    tracing::subscriber::set_global_default(
        tracing_subscriber::fmt()
            .with_writer(move || writer.clone())
            .with_ansi(false)
            .finish(),
    )
    .expect("the only subscriber of this binary");

    let realm = common::Realm::new();
    let app = router(Arc::new(
        Gateway::new(
            Broker::new("http://127.0.0.1:1"),
            Box::new(PolicyPdp),
            "hel.fi",
        )
        .serve([endpoint()])
        .authenticate(Arc::new(realm.verifier()), ServiceAccounts::new(), None),
    ));
    // A person's token with no `groups` claim, as the App clients minted before T-3033 (here from
    // the edge's client: an `app-*` client needs an App in the repository, else it is a 401).
    let token = realm.mint(&json!({
        "iss": common::ISSUER,
        "sub": "f:1:steward",
        "aud": SLUG,
        "azp": "edge",
        "preferred_username": "demo.steward",
        "exp": common::in_seconds(300),
        "iat": common::in_seconds(-10),
    }));
    let response = app
        .oneshot(
            Request::builder()
                .uri(format!("/api/endpoint/{SLUG}/access"))
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .body(Body::empty())
                .expect("a request"),
        )
        .await
        .expect("the gateway answers");
    assert_eq!(response.status(), StatusCode::FORBIDDEN);

    let log = String::from_utf8(captured.0.lock().expect("the log").clone()).expect("utf-8");
    let line = log
        .lines()
        .find(|line| line.contains("the token names no project this endpoint admits"))
        .unwrap_or_else(|| panic!("no refusal line in:\n{log}"));
    assert!(line.contains("WARN"), "{line}");
    for field in [
        format!("slug={SLUG}"),
        "user=demo.steward".to_owned(),
        "azp=\"edge\"".to_owned(),
    ] {
        assert!(line.contains(&field), "{field} missing in: {line}");
    }
    assert!(!log.contains(&token), "the token never reaches the log");
}
