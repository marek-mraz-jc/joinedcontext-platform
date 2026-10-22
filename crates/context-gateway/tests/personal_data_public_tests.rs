//! A public Endpoint never serves a personal attribute (ADR-N-054 2.6, DM-122, T-4666).
//!
//! The model marks a slot `x-personal-data` (Model Tools carries the LinkML `personalData`
//! annotation into the generated schema). An Endpoint whose audience is public and whose
//! projection and `hiddenAttributes` still serve such a slot is refused whole, naming the
//! attributes and never a value; hiding them, or an audience that is not public, serves it.

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use context_gateway::app::{router, Gateway};
use context_gateway::pdp::PolicyPdp;
use context_gateway::proxy::Broker;
use context_gateway::resolver::{DeclaredTypes, Endpoint};
use jc_core::kinds::{Audience, PolicySpec, Representation};
use jc_manifest::model_schema::ModelSchema;
use serde_json::{json, Value};
use std::sync::Arc;
use tower::ServiceExt;

const SLUG: &str = "pppppppppppppppppppppppppp";

fn schema() -> Arc<ModelSchema> {
    let model = json!({
        "$schema": "http://json-schema.org/draft-07/schema#",
        "definitions": {
            "BikeTrip": {
                "type": "object",
                "x-personal-data": true,
                "properties": {
                    "id": { "type": "string" },
                    "rider": { "type": "string", "x-personal-data": true, "x-ingest": "keyedHash" },
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

fn endpoint(audience: Audience, hidden: &[&str]) -> Endpoint {
    let grant: PolicySpec = serde_norway::from_str(
        "contextSpaceRef: trips\nassigner: did:web:banskabystrica.sk\n\
         assignee: { kind: role, id: public }\noperations: [queryEntity]\n",
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
        slug: SLUG.to_owned(),
        title: Default::default(),
        description: Default::default(),
        space: "trips".to_owned(),
        project: "trips".to_owned(),
        audience,
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
        calendars: Vec::new(),
        base_path: format!("/api/endpoint/{SLUG}"),
        models: Vec::new(),
        policy_names: Vec::new(),
        manifest_name: String::new(),
        policies: vec![grant],
    }
}

async fn query(endpoint: Endpoint) -> (StatusCode, Value, usize) {
    let broker = common::BrokerStub::start(vec![json!([])]).await;
    let gateway = Gateway::new(
        Broker::new(&broker.url),
        Box::new(PolicyPdp),
        "banskabystrica.sk",
    )
    .serve(vec![endpoint]);
    let request = Request::builder()
        .uri(format!(
            "/api/endpoint/{SLUG}/ngsi-ld/v1/entities?type=BikeTrip"
        ))
        .body(Body::empty())
        .expect("a request");
    let response = router(Arc::new(gateway))
        .oneshot(request)
        .await
        .expect("the gateway answers");
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .expect("a body");
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        broker.hops().len(),
    )
}

#[tokio::test]
async fn a_public_endpoint_serving_a_personal_attribute_is_refused_whole() {
    let (status, problem, hops) = query(endpoint(Audience::Public, &[])).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{problem}");
    let named = |key: &str| {
        problem[key]
            .as_str()
            .is_some_and(|v| v.ends_with("personal-data-public"))
    };
    assert!(named("type") || named("code"), "{problem}");
    assert!(
        problem["detail"]
            .as_str()
            .is_some_and(|d| d.contains("BikeTrip.rider")),
        "{problem}"
    );
    assert_eq!(hops, 0, "a refused Endpoint never reaches the broker");
}

#[tokio::test]
async fn hiding_the_personal_attribute_serves_the_public_endpoint() {
    let (status, problem, hops) = query(endpoint(Audience::Public, &["rider"])).await;
    assert!(status.is_success(), "{status}: {problem}");
    assert_eq!(hops, 1);
}

#[test]
fn only_a_public_audience_counts() {
    assert_eq!(
        endpoint(Audience::Public, &[]).public_personal_attributes(),
        ["BikeTrip.rider"]
    );
    assert!(endpoint(Audience::Organization, &[])
        .public_personal_attributes()
        .is_empty());
    assert!(endpoint(Audience::Public, &["rider"])
        .public_personal_attributes()
        .is_empty());
}
