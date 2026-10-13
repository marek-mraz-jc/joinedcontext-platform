//! T-0299: one decision, one projected entity set, many encodings (EP-05, EP-06, EP-07,
//! EP-24, EP-61, GW10).
//!
//! The point of these tests is not that each representation works. It is that they cannot
//! disagree: an attribute a policy withholds, and an attribute the endpoint hides, are
//! absent from GeoJSON, CSV, OGC Features and SensorThings alike, for both demo spaces. A
//! translator added later that reads the broker instead of the projection would fail here
//! rather than in a leak. NGSI-LD and MCP through the slug are the broker's view since T-3643
//! and T-3647, the files and SensorThings since T-3651, and their narrowing is proved there
//! (Antares `views_read.rs`, `policy_filter_by_type.rs`, `mcp_discovery.rs`) and end to end in
//! `antares_integration_tests`; here the view is the stub's, so the test holds the translators
//! that read it to the same answer as the ones that still project.

mod common;

use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use axum::Router;
use context_gateway::app::{router, Gateway};
use context_gateway::auth::accounts::ServiceAccounts;
use context_gateway::pdp::PolicyPdp;
use context_gateway::proxy::Broker;
use context_gateway::resolver::Endpoint;
use jc_core::kinds::{Audience, PolicySpec, Representation};
use serde_json::{json, Value};
use std::collections::BTreeSet;
use std::sync::Arc;
use tower::ServiceExt;

const AIR_SLUG: &str = "k4y7pq2mzt6vhx3nbwrs5cjd8f";
const BUS_SLUG: &str = "zt4qm7ge2xdv6ksb3ncf5arw2y";

/// One air-quality station as the broker holds it: five attributes, two of which nobody
/// outside the operator has any business reading.
fn station() -> Value {
    json!({
        "id": "urn:ngsi-ld:AirQualityObserved:banskabystrica.sk:ovzdusie:st-1",
        "type": "AirQualityObserved",
        "pm10": { "type": "Property", "value": 34.2 },
        "pm25": { "type": "Property", "value": 12.0 },
        "calibrationOffset": { "type": "Property", "value": 0.7 },
        "operatorPhone": { "type": "Property", "value": "+421 900 000 000" },
        "location": { "type": "GeoProperty", "value": { "type": "Point", "coordinates": [19.15, 48.73] } }
    })
}

/// One bus of the transport space, with the driver's identifier the public may not see.
fn vehicle() -> Value {
    json!({
        "id": "urn:ngsi-ld:Vehicle:banskabystrica.sk:transport:bus-12",
        "type": "Vehicle",
        "serviceLine": { "type": "Property", "value": "12" },
        "speed": { "type": "Property", "value": 31.5 },
        "driverId": { "type": "Property", "value": "emp-4471" },
        "fleetInternalId": { "type": "Property", "value": "BB-2019-114" },
        "location": { "type": "GeoProperty", "value": { "type": "Point", "coordinates": [19.14, 48.74] } }
    })
}

fn policy(yaml: &str) -> PolicySpec {
    serde_norway::from_str(yaml).expect("the policy spec parses")
}

/// A demo endpoint: everything a public reader may do, on one space, one type.
fn endpoint(
    slug: &str,
    space: &str,
    entity_type: &str,
    granted: &[&str],
    hidden: &[&str],
) -> Endpoint {
    let attributes = granted
        .iter()
        .map(|name| format!("      - {name}\n"))
        .collect::<String>();
    Endpoint {
        declared_types: None,
        roles: Default::default(),
        slug: slug.to_owned(),
        title: std::collections::BTreeMap::new(),
        description: std::collections::BTreeMap::new(),
        space: space.to_owned(),
        project: space.to_owned(),
        audience: Audience::Public,
        allowed_projects: Vec::new(),
        representations: vec![
            Representation::NgsiLd,
            Representation::GeoJson,
            Representation::Csv,
            Representation::Mcp,
            Representation::OgcFeatures,
            Representation::Sta,
        ],
        rate_limit: None,
        creates: None,
        file_limits: None,
        hidden_attributes: hidden.iter().map(|name| (*name).to_owned()).collect(),
        projection: None,
        base_path: format!("/api/endpoint/{slug}"),
        models: Vec::new(),
        view_mapping: None,
        catalog: None,
        policy_names: Vec::new(),
        policies: vec![policy(&format!(
            r#"contextSpaceRef: {space}
assigner: did:web:banskabystrica.sk
assignee: {{ kind: role, id: public }}
operations: [queryEntity, retrieveEntity]
information:
  - entities:
      - type: {entity_type}
    propertyNames:
{attributes}"#
        ))],
    }
}

/// The broker's view of a demo endpoint as the Portal writes it (T-3651): the public role reads
/// the one type with what the policy grants less what the endpoint hides. The file and
/// SensorThings translators read through it; OGC Features still decides in the gateway,
/// and the two must agree.
async fn viewed(slug: &str, entity: &Value) -> common::BrokerStub {
    let (entity_type, attrs): (&str, &[&str]) = if slug == AIR_SLUG {
        ("AirQualityObserved", &["pm10", "pm25", "location"])
    } else {
        ("Vehicle", &["serviceLine", "speed", "location"])
    };
    common::BrokerStub::narrowing(
        vec![json!([entity.clone()])],
        common::View::of(slug, "public", &[entity_type], Some(attrs)),
    )
    .await
}

fn app(broker: &str) -> Router {
    router(Arc::new(
        Gateway::new(
            Broker::new(broker),
            Box::new(PolicyPdp),
            "banskabystrica.sk",
        )
        .serve([
            // The public air endpoint hides the calibration offset the policy would allow.
            endpoint(
                AIR_SLUG,
                "ovzdusie",
                "AirQualityObserved",
                &["pm10", "pm25", "calibrationOffset", "location"],
                &["calibrationOffset"],
            ),
            // The public transport endpoint hides the fleet's internal id the same way.
            endpoint(
                BUS_SLUG,
                "transport",
                "Vehicle",
                &["serviceLine", "speed", "fleetInternalId", "location"],
                &["fleetInternalId"],
            ),
        ])
        .authenticate(
            Arc::new(common::Realm::new().verifier()),
            ServiceAccounts::new(),
            None,
        ),
    ))
}

async fn get(app: Router, uri: &str) -> (StatusCode, Vec<u8>) {
    let response = app
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri(uri)
                .body(Body::empty())
                .expect("a request"),
        )
        .await
        .expect("the gateway answers");
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 4 * 1024 * 1024)
        .await
        .expect("a body");
    (status, bytes.to_vec())
}

/// The attribute names a CSV header describes: one column per leaf, so the stems are the
/// attributes and the rest is the flattening (EP-08).
fn attributes_of_csv(csv: &str) -> BTreeSet<String> {
    csv.lines()
        .next()
        .unwrap_or_default()
        .split(',')
        .map(|column| column.trim_matches('"'))
        .filter(|column| !matches!(*column, "id" | "type"))
        .map(|column| column.split(['.', '[']).next().unwrap_or(column).to_owned())
        .collect()
}

/// EP-06, EP-07, EP-61: every representation of one endpoint answers with exactly the
/// same attributes, for both demo spaces.
#[tokio::test]
async fn every_representation_of_one_endpoint_shows_the_same_attributes() {
    for (slug, entity, expected) in [
        (AIR_SLUG, station(), ["location", "pm10", "pm25"]),
        (BUS_SLUG, vehicle(), ["location", "serviceLine", "speed"]),
    ] {
        let broker = viewed(slug, &entity).await;
        let expected: BTreeSet<String> = expected.iter().map(|name| (*name).to_owned()).collect();

        // 2. GeoJSON: `location` becomes the geometry, so it is a property no more.
        let (status, body) = get(
            app(&broker.url),
            &format!("/api/endpoint/{slug}/file.geojson"),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let collection: Value = serde_json::from_slice(&body).expect("JSON");
        let mut geojson: BTreeSet<String> = collection["features"][0]["properties"]
            .as_object()
            .expect("a properties object")
            .keys()
            .filter(|name| name.as_str() != "type")
            .cloned()
            .collect();
        assert!(
            collection["features"][0]["geometry"]["type"]
                .as_str()
                .is_some(),
            "the geometry is where `location` went"
        );
        geojson.insert("location".to_owned());
        assert_eq!(
            geojson, expected,
            "GeoJSON shows what the grants and the endpoint leave"
        );

        // 3. CSV: one column per leaf, and the stems are the same attributes.
        let (status, body) = get(app(&broker.url), &format!("/api/endpoint/{slug}/file.csv")).await;
        assert_eq!(status, StatusCode::OK);
        let csv = String::from_utf8(body).expect("UTF-8");
        assert_eq!(
            attributes_of_csv(&csv),
            expected,
            "the CSV header describes the same attributes: {csv}"
        );

        // OGC API Features is the broker's view since T-3752; Antares holds its parity.

        // 4. SensorThings: the same attributes, split across the sets the profile has for
        // them. A numeric attribute is a Datastream, a label stays a property, the geometry
        // is the Location; together they are the whole projected entity and nothing else.
        let (status, body) = get(
            app(&broker.url),
            &format!("/api/endpoint/{slug}/sta/v1.1/Things?$expand=Datastreams,Locations"),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let things: Value = serde_json::from_slice(&body).expect("JSON");
        let thing = &things["value"][0];
        let mut observed: BTreeSet<String> = thing["properties"]
            .as_object()
            .expect("a properties object")
            .keys()
            .filter(|name| name.as_str() != "type")
            .cloned()
            .collect();
        for stream in thing["Datastreams"].as_array().expect("a list") {
            let id = stream["@iot.id"].as_str().expect("an id");
            observed.insert(id.rsplit('/').next().unwrap_or(id).to_owned());
        }
        if !thing["Locations"].as_array().expect("a list").is_empty() {
            observed.insert("location".to_owned());
        }
        assert_eq!(observed, expected, "SensorThings shows what GeoJSON shows");
    }
}

/// EP-61: the attribute the endpoint hides is granted by the policy and served by nobody.
#[tokio::test]
async fn an_attribute_the_endpoint_hides_is_absent_from_every_representation() {
    for (slug, entity, hidden) in [
        (AIR_SLUG, station(), "calibrationOffset"),
        (BUS_SLUG, vehicle(), "fleetInternalId"),
    ] {
        let broker = viewed(slug, &entity).await;

        for uri in [
            format!("/api/endpoint/{slug}/file.geojson"),
            format!("/api/endpoint/{slug}/file.csv"),
            format!("/api/endpoint/{slug}/sta/v1.1/Things?$expand=Datastreams,Locations"),
            format!("/api/endpoint/{slug}/sta/v1.1/Observations"),
        ] {
            let (status, body) = get(app(&broker.url), &uri).await;
            assert_eq!(status, StatusCode::OK, "{uri}");
            let text = String::from_utf8(body).expect("UTF-8");
            assert!(
                !text.contains(hidden),
                "{hidden} is hidden by the endpoint and must not appear in {uri}"
            );
        }

        // The broker was asked for the whole entity: the narrowing is the gateway's, and
        // that is exactly why it has to hold in every encoding.
        assert!(
            !broker.hops().is_empty(),
            "the entity came from the broker, not from a fixture the gateway kept"
        );
    }
}
