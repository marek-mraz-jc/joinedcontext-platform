//! T-2963 (R9, GW2, AP-04): a grant that names an entity's identity as an attribute.
//!
//! An App's `dataNeeds.attrs` may name `id`, and the Portal copies it into the Policy's
//! `propertyNames`. The gateway forwarded it to the broker as `attrs=id,…`, which a broker
//! refuses, so on dev every read of such an App endpoint was a `400`. The identity is kept on
//! every entity by the projection, so the broker never needs to be asked for it.

mod common;

use axum::body::Body;
use axum::http::{Request as HttpRequest, StatusCode};
use common::BrokerStub;
use context_gateway::app::{router, Gateway};
use context_gateway::pdp::PolicyPdp;
use context_gateway::proxy::Broker;
use context_gateway::resolver::Endpoint;
use jc_core::kinds::{Audience, Representation};
use serde_json::{json, Value};
use std::collections::BTreeSet;
use std::sync::Arc;
use tower::ServiceExt;

const SLUG: &str = "quzxomaqocceis7ur5xozo35t4";

/// An App endpoint whose one grant names `attrs` the way the builder writes them.
fn app_endpoint(property_names: &str) -> Endpoint {
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
        representations: vec![Representation::NgsiLd, Representation::GeoJson],
        rate_limit: None,
        file_limits: None,
        hidden_attributes: BTreeSet::new(),
        projection: None,
        base_path: format!("/api/endpoint/{SLUG}"),
        models: Vec::new(),
        view_mapping: None,
        catalog: None,
        policies: vec![serde_norway::from_str(&format!(
            r#"contextSpaceRef: helsinki
assigner: did:web:hel.fi
assignee: {{ kind: role, id: public }}
operations: [queryEntity, retrieveEntity]
information:
  - entities:
      - type: BikeHireDockingStation
    propertyNames: {property_names}
"#
        ))
        .expect("the policy spec parses")],
    }
}

fn station() -> Value {
    json!({
        "id": "urn:ngsi-ld:BikeHireDockingStation:hel.fi:helsinki:001",
        "type": "BikeHireDockingStation",
        "name": { "type": "Property", "value": "Kaivopuisto" },
        "availableBikeNumber": { "type": "Property", "value": 7 },
        "location": {
            "type": "GeoProperty",
            "value": { "type": "Point", "coordinates": [24.95, 60.16] }
        }
    })
}

async fn read(broker: &BrokerStub, property_names: &str, path: &str) -> (StatusCode, Value) {
    let app = router(Arc::new(
        Gateway::new(Broker::new(&broker.url), Box::new(PolicyPdp), "hel.fi")
            .serve([app_endpoint(property_names)]),
    ));
    let response = app
        .oneshot(
            HttpRequest::builder()
                .uri(format!("/api/endpoint/{SLUG}{path}"))
                .body(Body::empty())
                .expect("a request"),
        )
        .await
        .expect("the gateway answers");
    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), 16 * 1024 * 1024)
        .await
        .expect("a readable body");
    (status, serde_json::from_slice(&body).unwrap_or(Value::Null))
}

/// The dev case: `[id, name, availableBikeNumber]` reads the stations, and the broker is asked
/// for the two attributes, never for `id`.
#[tokio::test]
async fn a_grant_naming_id_reads_its_entities_and_never_asks_the_broker_for_id() {
    let broker = BrokerStub::start(vec![json!([station()])]).await;
    let (status, body) = read(
        &broker,
        "[id, name, availableBikeNumber]",
        "/ngsi-ld/v1/entities?type=BikeHireDockingStation",
    )
    .await;

    assert_eq!(status, StatusCode::OK, "{body}");
    let entity = &body[0];
    assert_eq!(entity["id"], station()["id"]);
    assert_eq!(entity["availableBikeNumber"]["value"], json!(7));
    assert!(
        entity.get("location").is_none(),
        "the grant does not name location: {entity}"
    );
    let hops = broker.hops();
    assert!(
        hops.iter().all(|hop| !hop
            .query
            .split('&')
            .any(|pair| pair.starts_with("attrs=")
                && pair.split(['=', ',']).any(|part| part == "id"))),
        "the broker is never asked for id: {hops:?}"
    );
}

/// A grant of the identity alone answers the identity alone: the grant's type selects, and the
/// projection leaves nothing but `id` and `type`.
#[tokio::test]
async fn a_grant_of_the_identity_alone_answers_ids_and_types_only() {
    let broker = BrokerStub::start(vec![json!([station()])]).await;
    let (status, body) = read(
        &broker,
        "[id]",
        "/ngsi-ld/v1/entities?type=BikeHireDockingStation",
    )
    .await;

    assert_eq!(status, StatusCode::OK, "{body}");
    let members: BTreeSet<&str> = body[0]
        .as_object()
        .expect("an entity")
        .keys()
        .map(String::as_str)
        .collect();
    assert!(
        members
            .iter()
            .all(|member| matches!(*member, "id" | "type" | "@context")),
        "identity only: {members:?}"
    );
}

/// The caller who asks the broker for `id` in their own words gets the broker's own answer:
/// the gateway changes no status code of the NGSI-LD surface.
#[tokio::test]
async fn the_callers_own_attrs_id_still_gets_the_brokers_answer() {
    let broker = BrokerStub::start(vec![json!([station()])]).await;
    let (status, body) = read(
        &broker,
        "[id, name]",
        "/ngsi-ld/v1/entities?type=BikeHireDockingStation&attrs=id",
    )
    .await;

    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(
        body["detail"],
        json!("invalid attribute name \"id\" in attrs")
    );
}
