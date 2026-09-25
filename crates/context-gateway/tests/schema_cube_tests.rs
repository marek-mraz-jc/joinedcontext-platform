//! The RDF Data Cube on the schema surface (T-1187, DM-60, EP-46, EP-47).
//!
//! The first cube: population by district, age band and year. Its JSON Schema carries the roles
//! Model Tools wrote (`x-qb-dsd`, `x-qb-component`), and the gateway renders `model.qb.ttl` and
//! the component typing of the OWL from the projection, so a grant that hides a dimension hides
//! it from the cube too, and a model without a DSD has no cube at all.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use context_gateway::app::{router, Gateway};
use context_gateway::pdp::PolicyPdp;
use context_gateway::proxy::Broker;
use context_gateway::resolver::{Endpoint, Model};
use jc_core::kinds::{Audience, PolicySpec, Representation};
use serde_json::{json, Value};
use std::sync::Arc;
use tower::ServiceExt;

const SLUG: &str = "p4q7rm2zt6vhx3nbwks5cjd8fa";
const VOCAB: &str = "urn:joinedcontext:model:population:v1:";

/// What Model Tools generates for `population-cube.linkml.yaml` (tools/model-tools/tests).
fn population_schema() -> Value {
    json!({
        "$schema": "http://json-schema.org/draft-07/schema#",
        "$defs": {
            "PopulationObservation": {
                "type": "object",
                "description": "The number of inhabitants of one district in one age band at the end of one year.",
                "x-qb-dsd": true,
                "properties": {
                    "id": { "type": "string" },
                    "type": { "const": "PopulationObservation" },
                    "refDistrict": { "type": "string", "x-ngsi-ld-kind": "Relationship", "x-qb-component": "dimension" },
                    "ageBand": { "type": "string", "enum": ["0-4", "5-9", "65+"], "x-ngsi-ld-kind": "VocabProperty", "x-qb-component": "dimension" },
                    "year": { "type": "integer", "x-ngsi-ld-kind": "Property", "x-qb-component": "dimension" },
                    "population": { "type": "integer", "x-ngsi-ld-kind": "Property", "x-qb-component": "measure",
                                    "x-unit": { "exactMappings": ["unece:IE", "qudt-unit:NUM"] } },
                    "source": { "type": "string", "x-ngsi-ld-kind": "Property" }
                },
                "required": ["id", "type", "refDistrict", "ageBand", "year", "population"]
            }
        }
    })
}

fn population() -> Model {
    Model {
        name: "population".to_owned(),
        version: "1.0.0".to_owned(),
        major: 1,
        classes: vec!["PopulationObservation".to_owned()],
        json_schema: Some(population_schema()),
        context: None,
    }
}

/// A model with no Data Structure Definition: the air quality readings of the other suites.
fn readings() -> Model {
    Model {
        name: "air".to_owned(),
        version: "1.0.0".to_owned(),
        major: 1,
        classes: vec!["AirQualityObserved".to_owned()],
        json_schema: Some(json!({
            "$defs": { "AirQualityObserved": { "type": "object",
                "properties": { "id": { "type": "string" }, "pm10": { "type": "number" } } } }
        })),
        context: None,
    }
}

fn policy(yaml: &str) -> PolicySpec {
    serde_norway::from_str(yaml).expect("the policy spec parses")
}

/// A public grant on `entity_type`, naming `properties` (every attribute when empty).
fn endpoint(model: Model, entity_type: &str, properties: &[&str]) -> Endpoint {
    let names = if properties.is_empty() {
        String::new()
    } else {
        format!("    propertyNames: [{}]\n", properties.join(", "))
    };
    Endpoint {
        declared_types: None,
        roles: Default::default(),
        slug: SLUG.to_owned(),
        title: Default::default(),
        description: Default::default(),
        space: "banskabystrica-kpi".to_owned(),
        project: "banskabystrica".to_owned(),
        audience: Audience::Public,
        allowed_projects: Vec::new(),
        representations: vec![Representation::NgsiLd, Representation::Mcp],
        rate_limit: None,
        file_limits: None,
        hidden_attributes: Default::default(),
        projection: None,
        view_mapping: None,
        catalog: None,
        base_path: format!("/api/endpoint/{SLUG}"),
        models: vec![model],
        policies: vec![policy(&format!(
            "contextSpaceRef: banskabystrica-kpi\n\
             assigner: did:web:banskabystrica.sk\n\
             assignee: {{ kind: role, id: public }}\n\
             operations: [queryEntity, retrieveEntity]\n\
             information:\n  - entities:\n      - type: {entity_type}\n{names}"
        ))],
    }
}

async fn fetch(endpoint: Endpoint, request: Request<Body>) -> (StatusCode, String, String) {
    let app = router(Arc::new(
        Gateway::new(
            Broker::new("http://127.0.0.1:1"),
            Box::new(PolicyPdp),
            "banskabystrica.sk",
        )
        .serve([endpoint]),
    ));
    let response = app.oneshot(request).await.expect("the gateway answers");
    let status = response.status();
    let media = response
        .headers()
        .get("content-type")
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    let body = axum::body::to_bytes(response.into_body(), 512 * 1024)
        .await
        .expect("a readable body");
    (status, media, String::from_utf8_lossy(&body).into_owned())
}

fn get(path: &str) -> Request<Body> {
    Request::builder()
        .uri(format!("/api/endpoint/{SLUG}/schema/{path}"))
        .body(Body::empty())
        .expect("a request")
}

fn mcp(method: &str, params: Value) -> Request<Body> {
    let payload = json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": params });
    Request::builder()
        .method("POST")
        .uri(format!("/api/endpoint/{SLUG}/mcp"))
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream")
        .body(Body::from(payload.to_string()))
        .expect("a request")
}

fn whole() -> Endpoint {
    endpoint(population(), "PopulationObservation", &[])
}

#[tokio::test]
async fn the_cube_of_a_dsd_names_every_component_by_its_role() {
    let (status, media, cube) = fetch(whole(), get("v1/model.qb.ttl")).await;
    assert_eq!(status, StatusCode::OK, "{cube}");
    assert!(media.starts_with("text/turtle"), "{media}");
    assert!(cube.contains("@prefix qb: <http://purl.org/linked-data/cube#> ."));
    assert!(cube.contains(&format!(
        "<{VOCAB}PopulationObservationStructure> a qb:DataStructureDefinition"
    )));
    for dimension in ["refDistrict", "ageBand", "year"] {
        assert!(
            cube.contains(&format!("[ qb:dimension <{VOCAB}{dimension}> ]")),
            "{cube}"
        );
        assert!(cube.contains(&format!(
            "<{VOCAB}{dimension}> a rdf:Property, qb:DimensionProperty"
        )));
    }
    assert!(
        cube.contains(&format!("[ qb:measure <{VOCAB}population> ]")),
        "{cube}"
    );
    assert!(cube.contains(&format!(
        "<{VOCAB}population> a rdf:Property, qb:MeasureProperty"
    )));
    assert!(cube.contains(&format!(
        "<{VOCAB}PopulationObservation> rdfs:subClassOf qb:Observation"
    )));
    assert!(
        !cube.contains(&format!("{VOCAB}source")),
        "a slot without a role is no component"
    );
    // The short name answers the same document.
    let (_, _, again) = fetch(whole(), get("v1/qb")).await;
    assert_eq!(again, cube);
}

#[tokio::test]
async fn owl_for_a_dsd_types_its_components_and_shacl_requires_them() {
    let (_, _, owl) = fetch(whole(), get("v1/model.owl.ttl")).await;
    assert!(
        owl.contains(&format!("<{VOCAB}PopulationObservation> a owl:Class")),
        "{owl}"
    );
    assert!(owl.contains("rdfs:subClassOf qb:Observation"), "{owl}");
    assert!(
        owl.contains(&format!(
            "<{VOCAB}year> a owl:DatatypeProperty, qb:DimensionProperty"
        )),
        "{owl}"
    );
    assert!(owl.contains(&format!(
        "<{VOCAB}population> a owl:DatatypeProperty, qb:MeasureProperty"
    )));
    assert!(
        owl.contains(&format!("<{VOCAB}source> a rdf:Property ;"))
            || owl.contains(&format!("<{VOCAB}source> a owl:DatatypeProperty ;")),
        "{owl}"
    );

    // Every component is required, so every observation's shape demands one value of each.
    let (_, _, shacl) = fetch(whole(), get("v1/model.shacl.ttl")).await;
    for component in ["refDistrict", "ageBand", "year", "population"] {
        let at = shacl
            .find(&format!("sh:path <{VOCAB}{component}>"))
            .expect(component);
        let shape = &shacl[at..shacl[at..].find(']').map_or(shacl.len(), |end| at + end)];
        assert!(shape.contains("sh:minCount 1"), "{component}: {shape}");
    }
}

#[tokio::test]
async fn no_qb_for_a_model_without_a_dsd() {
    let plain = || endpoint(readings(), "AirQualityObserved", &[]);
    let (status, _, _) = fetch(plain(), get("v1/model.qb.ttl")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (_, _, owl) = fetch(plain(), get("v1/model.owl.ttl")).await;
    assert!(
        !owl.contains("qb:"),
        "a model without a cube renders its OWL as before: {owl}"
    );
    let (_, _, index) = fetch(plain(), get("index.json")).await;
    assert!(!index.contains("model.qb.ttl"), "{index}");
    let (_, _, index) = fetch(whole(), get("index.json")).await;
    assert!(index.contains("model.qb.ttl"), "{index}");
}

#[tokio::test]
async fn a_grant_that_hides_a_dimension_hides_it_from_the_cube() {
    let narrow = endpoint(
        population(),
        "PopulationObservation",
        &["population", "year"],
    );
    let (status, _, cube) = fetch(narrow, get("v1/model.qb.ttl")).await;
    assert_eq!(status, StatusCode::OK, "{cube}");
    assert!(cube.contains(&format!("{VOCAB}year")) && cube.contains(&format!("{VOCAB}population")));
    assert!(
        !cube.contains("ageBand"),
        "the grant does not name ageBand: {cube}"
    );

    // A grant on another type leaves no DSD to render, and the cube is not there.
    let elsewhere = endpoint(population(), "AirQualityObserved", &[]);
    let (status, _, _) = fetch(elsewhere, get("v1/model.qb.ttl")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn an_agent_reads_the_cube_where_there_is_one_and_is_told_why_where_there_is_none() {
    let call = |format: &str| {
        mcp(
            "tools/call",
            json!({ "name": "describe_schema", "arguments": { "format": format } }),
        )
    };
    let (_, _, answer) = fetch(whole(), call("qb")).await;
    assert!(answer.contains("qb:DataStructureDefinition"), "{answer}");
    let (_, _, answer) = fetch(endpoint(readings(), "AirQualityObserved", &[]), call("qb")).await;
    assert!(
        answer.contains("declares no Data Structure Definition"),
        "{answer}"
    );

    let (_, _, listed) = fetch(whole(), mcp("resources/list", json!({}))).await;
    assert!(
        listed.contains(&format!("schema://{SLUG}/v1/qb")),
        "{listed}"
    );
    let (_, _, listed) = fetch(
        endpoint(readings(), "AirQualityObserved", &[]),
        mcp("resources/list", json!({})),
    )
    .await;
    assert!(!listed.contains("/qb\""), "{listed}");
}
