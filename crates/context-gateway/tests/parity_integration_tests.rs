//! T-0299: one decision, one projected entity set, many encodings (EP-05, EP-06, EP-07,
//! EP-24, EP-61, GW10).
//!
//! The point of these tests is not that each representation works. It is that they cannot
//! disagree: an attribute a policy withholds, and an attribute the endpoint hides, are
//! absent from NGSI-LD, GeoJSON, CSV and the MCP tool result alike, for both demo spaces.
//! A translator added later that reads the broker instead of the projection would fail
//! here rather than in a leak.

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
        file_limits: None,
        hidden_attributes: hidden.iter().map(|name| (*name).to_owned()).collect(),
        projection: None,
        base_path: format!("/api/endpoint/{slug}"),
        models: Vec::new(),
        view_mapping: None,
        catalog: None,
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

async fn post(app: Router, uri: &str, body: Value) -> Value {
    let response = app
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri(uri)
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .expect("a request"),
        )
        .await
        .expect("the gateway answers");
    let bytes = axum::body::to_bytes(response.into_body(), 4 * 1024 * 1024)
        .await
        .expect("a body");
    serde_json::from_slice(&bytes).unwrap_or(Value::Null)
}

/// The attribute names of one NGSI-LD entity, without the structure every entity carries.
fn attributes_of(entity: &Value) -> BTreeSet<String> {
    entity
        .as_object()
        .map(|members| {
            members
                .keys()
                .filter(|name| !matches!(name.as_str(), "id" | "type" | "@context" | "scope"))
                .cloned()
                .collect()
        })
        .unwrap_or_default()
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
        let broker = common::BrokerStub::start(vec![json!([entity.clone()])]).await;
        let entity_type = entity["type"].as_str().expect("a type").to_owned();
        let expected: BTreeSet<String> = expected.iter().map(|name| (*name).to_owned()).collect();

        // 1. NGSI-LD, the canonical form the other three are translated from.
        let (status, body) = get(
            app(&broker.url),
            &format!("/api/endpoint/{slug}/ngsi-ld/v1/entities?type={entity_type}"),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let entities: Value = serde_json::from_slice(&body).expect("JSON");
        let ngsi_ld = attributes_of(&entities[0]);
        assert_eq!(ngsi_ld, expected, "NGSI-LD is the reference for {slug}");

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
        assert_eq!(geojson, expected, "GeoJSON shows what NGSI-LD shows");

        // 3. CSV: one column per leaf, and the stems are the same attributes.
        let (status, body) = get(app(&broker.url), &format!("/api/endpoint/{slug}/file.csv")).await;
        assert_eq!(status, StatusCode::OK);
        let csv = String::from_utf8(body).expect("UTF-8");
        assert_eq!(
            attributes_of_csv(&csv),
            expected,
            "the CSV header describes the same attributes: {csv}"
        );

        // 4. OGC API Features: the same flattening as GeoJSON, reached through the resource
        // tree a GIS client walks rather than through a file download.
        let (status, body) = get(
            app(&broker.url),
            &format!("/api/endpoint/{slug}/ogc/features/collections/{entity_type}/items"),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let page: Value = serde_json::from_slice(&body).expect("JSON");
        let mut ogc: BTreeSet<String> = page["features"][0]["properties"]
            .as_object()
            .expect("a properties object")
            .keys()
            .filter(|name| name.as_str() != "type")
            .cloned()
            .collect();
        ogc.insert("location".to_owned());
        assert_eq!(ogc, expected, "OGC Features shows what NGSI-LD shows");

        // 5. SensorThings: the same attributes, split across the sets the profile has for
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
        assert_eq!(observed, expected, "SensorThings shows what NGSI-LD shows");

        // 6. MCP: the tool result is the projected entity, not a second read.
        let answer = post(
            app(&broker.url),
            &format!("/api/endpoint/{slug}/mcp"),
            json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "tools/call",
                "params": {
                    "name": "query_entities",
                    "arguments": { "type": entity_type },
                },
            }),
        )
        .await;
        // The structured half of a result is an object naming what it holds (AG-08, T-0848),
        // so the entities are under the key the tool's output schema declares.
        let structured = &answer["result"]["structuredContent"]["entities"][0];
        assert_eq!(
            attributes_of(structured),
            expected,
            "an agent sees exactly what a browser sees"
        );
        assert_eq!(
            structured, &entities[0],
            "the MCP result is the same document, byte for byte"
        );
    }
}

/// EP-61: the attribute the endpoint hides is granted by the policy and served by nobody.
#[tokio::test]
async fn an_attribute_the_endpoint_hides_is_absent_from_every_representation() {
    for (slug, entity, hidden) in [
        (AIR_SLUG, station(), "calibrationOffset"),
        (BUS_SLUG, vehicle(), "fleetInternalId"),
    ] {
        let broker = common::BrokerStub::start(vec![json!([entity.clone()])]).await;
        let entity_type = entity["type"].as_str().expect("a type").to_owned();

        for uri in [
            format!("/api/endpoint/{slug}/ngsi-ld/v1/entities?type={entity_type}"),
            format!("/api/endpoint/{slug}/file.geojson"),
            format!("/api/endpoint/{slug}/file.csv"),
            format!("/api/endpoint/{slug}/ogc/features/collections/{entity_type}/items"),
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

        let answer = post(
            app(&broker.url),
            &format!("/api/endpoint/{slug}/mcp"),
            json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "tools/call",
                "params": { "name": "query_entities", "arguments": { "type": entity_type } },
            }),
        )
        .await;
        assert!(
            !answer.to_string().contains(hidden),
            "{hidden} must not reach an agent either"
        );

        // The broker was asked for the whole entity: the narrowing is the gateway's, and
        // that is exactly why it has to hold in every encoding.
        assert!(
            !broker.hops().is_empty(),
            "the entity came from the broker, not from a fixture the gateway kept"
        );
    }
}

/// R20, EP-06: a type no grant names is empty everywhere, and never an error that says
/// the type exists.
#[tokio::test]
async fn a_type_no_grant_names_answers_the_same_nothing_everywhere() {
    let broker = common::BrokerStub::start(vec![json!([station()])]).await;

    let (status, body) = get(
        app(&broker.url),
        &format!("/api/endpoint/{AIR_SLUG}/ngsi-ld/v1/entities?type=WaterQualityObserved"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        serde_json::from_slice::<Value>(&body).expect("JSON"),
        json!([]),
        "a type no grant reaches is empty, which is what a type that does not exist is too"
    );
    assert!(
        broker.hops().is_empty(),
        "T-0381: an empty intersection must not be forwarded as no filter at all, which \
         would ask the broker for every type in the tenant"
    );

    let answer = post(
        app(&broker.url),
        &format!("/api/endpoint/{AIR_SLUG}/mcp"),
        json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": {
                "name": "query_entities",
                "arguments": { "type": "WaterQualityObserved" },
            },
        }),
    )
    .await;
    assert_eq!(
        answer["result"]["structuredContent"]["entities"],
        json!([]),
        "an agent sees the same nothing, from the same decision"
    );
    assert!(
        broker.hops().is_empty(),
        "and still nothing was asked of the broker"
    );
}
