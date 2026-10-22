//! A read through an Endpoint that serves a personal attribute is logged with the reader, the
//! Endpoint and the Entities' URNs, never a value (ADR-N-054 2.6, DM-128, T-4666).
//!
//! Alone in its binary: it installs the process's subscriber.

mod common;

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use context_gateway::app::{router, Gateway};
use context_gateway::auth::accounts::ServiceAccounts;
use context_gateway::pdp::PolicyPdp;
use context_gateway::proxy::Broker;
use context_gateway::resolver::{DeclaredTypes, Endpoint};
use jc_core::kinds::{Audience, PolicySpec, Representation};
use jc_manifest::model_schema::ModelSchema;
use serde_json::json;
use std::sync::{Arc, Mutex};
use tower::ServiceExt;

const PERSONAL: &str = "pppppppppppppppppppppppppp";
const PLAIN: &str = "qqqqqqqqqqqqqqqqqqqqqqqqqq";
const RIDER: &str = "rider-jane-doe";

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

fn schema() -> Arc<ModelSchema> {
    let model = json!({
        "$schema": "http://json-schema.org/draft-07/schema#",
        "definitions": {
            "BikeTrip": {
                "type": "object",
                "properties": {
                    "id": { "type": "string" },
                    "rider": { "type": "string", "x-personal-data": true },
                    "startStation": { "type": "string" }
                }
            }
        }
    });
    Arc::new(ModelSchema::compile(
        "bike-trips",
        "1.0.0",
        &model,
        &["BikeTrip".to_owned()],
        true,
    ))
}

fn endpoint(slug: &str, hidden: &[&str]) -> Endpoint {
    let grant: PolicySpec = serde_norway::from_str(
        "contextSpaceRef: trips\nassigner: did:web:banskabystrica.sk\n\
         assignee: { kind: role, id: steward }\noperations: [queryEntity]\n",
    )
    .expect("the policy spec parses");
    Endpoint {
        declared_types: Some(Arc::new(DeclaredTypes {
            model: "bike-trips".into(),
            classes: ["BikeTrip".to_owned()].into(),
            units: Default::default(),
            relationships: Default::default(),
            quality: Vec::new(),
            schema: Some(schema()),
        })),
        roles: Default::default(),
        slug: slug.to_owned(),
        title: Default::default(),
        description: Default::default(),
        space: "trips".to_owned(),
        project: "trips".to_owned(),
        audience: Audience::Organization,
        allowed_projects: Vec::new(),
        representations: vec![Representation::NgsiLd],
        rate_limit: None,
        creates: None,
        file_limits: None,
        hidden_attributes: hidden.iter().map(|h| (*h).to_owned()).collect(),
        projection: None,
        view_mapping: None,
        catalog: None,
        deprecation: None,
        feeds: Vec::new(),
        base_path: format!("/api/endpoint/{slug}"),
        models: Vec::new(),
        policy_names: Vec::new(),
        manifest_name: String::new(),
        policies: vec![grant],
    }
}

#[tokio::test]
async fn a_read_of_a_personal_attribute_names_reader_endpoint_and_entities_never_values() {
    let captured = Captured::default();
    let writer = captured.clone();
    tracing::subscriber::set_global_default(
        tracing_subscriber::fmt()
            .with_writer(move || writer.clone())
            .with_ansi(false)
            .finish(),
    )
    .expect("the only subscriber of this binary");

    let trips = json!([
        {"id": "urn:ngsi-ld:BikeTrip:t1", "type": "BikeTrip",
         "rider": {"type": "Property", "value": RIDER}},
        {"id": "urn:ngsi-ld:BikeTrip:t2", "type": "BikeTrip",
         "rider": {"type": "Property", "value": RIDER}}
    ]);
    let broker = common::BrokerStub::start(vec![trips.clone(), trips]).await;
    let realm = common::Realm::new();
    let app = router(Arc::new(
        Gateway::new(
            Broker::new(&broker.url),
            Box::new(PolicyPdp),
            "banskabystrica.sk",
        )
        .serve(vec![endpoint(PERSONAL, &[]), endpoint(PLAIN, &["rider"])])
        .authenticate(
            Arc::new(realm.verifier()),
            ServiceAccounts::new(),
            Some("https://city.example".to_owned()),
        ),
    ));
    let token = realm.mint(&json!({
        "iss": common::ISSUER,
        "sub": "0f5a",
        "aud": "context-gateway",
        "preferred_username": "jana",
        "groups": ["/trips"],
        "realm_access": { "roles": ["steward"] },
        "exp": common::in_seconds(300),
        "iat": common::in_seconds(-10),
    }));
    for slug in [PERSONAL, PLAIN] {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!(
                        "/api/endpoint/{slug}/ngsi-ld/v1/entities?type=BikeTrip"
                    ))
                    .header(header::AUTHORIZATION, format!("Bearer {token}"))
                    .body(Body::empty())
                    .expect("a request"),
            )
            .await
            .expect("the gateway answers");
        assert_eq!(response.status(), StatusCode::OK, "{slug}");
        let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .expect("a body");
        assert!(
            String::from_utf8_lossy(&body).contains("urn:ngsi-ld:BikeTrip:t2"),
            "the answer reaches the reader unchanged"
        );
    }

    let log = String::from_utf8(captured.0.lock().expect("the log").clone()).expect("utf-8");
    let reads: Vec<&str> = log
        .lines()
        .filter(|line| line.contains("personal attributes read"))
        .collect();
    assert_eq!(
        reads.len(),
        1,
        "only the Endpoint serving the rider logs: {log}"
    );
    let line = reads[0];
    for named in [
        PERSONAL,
        "BikeTrip.rider",
        "urn:ngsi-ld:BikeTrip:t1,urn:ngsi-ld:BikeTrip:t2",
        "jana",
    ] {
        assert!(line.contains(named), "{named} missing: {line}");
    }
    assert!(!log.contains(RIDER), "a value reached the log: {log}");
}
