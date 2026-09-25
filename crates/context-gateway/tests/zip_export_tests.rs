//! One query as a bundle a colleague can open without the platform (T-0161, EP-41, EP-43,
//! EP-44, EP-51).
//!
//! The archive is read back part by part rather than asserted as bytes, because what matters is
//! that a stranger with a zip tool finds the data, the schemas and a record of where it came
//! from. Two properties are load-bearing and neither is visible from the outside: every file is
//! produced from one projected answer, so no member can carry an attribute the policy removed;
//! and the endpoint's row and byte ceilings refuse rather than truncate, because a short bundle
//! is indistinguishable from a complete one.

mod common;

use axum::body::Body;
use axum::http::{Request as HttpRequest, StatusCode};
use common::BrokerStub;
use context_gateway::app::{router, Gateway};
use context_gateway::pdp::PolicyPdp;
use context_gateway::proxy::Broker;
use context_gateway::resolver::{Endpoint, Model};
use jc_core::kinds::{Audience, FileLimits, Representation};
use serde_json::{json, Value};
use std::io::Read;
use std::sync::Arc;
use tower::ServiceExt;

const SLUG: &str = "mluyob4nz52lok3ssk7pgn5vwt";

fn endpoint(limits: Option<FileLimits>, hidden: &[&str]) -> Endpoint {
    Endpoint {
        roles: Default::default(),
        slug: SLUG.to_owned(),
        title: std::collections::BTreeMap::new(),
        description: std::collections::BTreeMap::new(),
        space: "ovzdusie".to_owned(),
        project: "ovzdusie".to_owned(),
        audience: Audience::Public,
        allowed_projects: Vec::new(),
        representations: vec![Representation::NgsiLd, Representation::Zip],
        rate_limit: None,
        file_limits: limits,
        hidden_attributes: hidden.iter().map(|name| (*name).to_owned()).collect(),
        projection: None,
        base_path: format!("/api/endpoint/{SLUG}"),
        view_mapping: None,
        catalog: None,
        models: vec![Model {
            name: "air-quality".to_owned(),
            version: "1.2.0".to_owned(),
            major: 1,
            classes: vec!["AirQualityObserved".to_owned()],
            json_schema: Some(json!({
                "$schema": "http://json-schema.org/draft-07/schema#",
                "$defs": { "AirQualityObserved": { "type": "object", "properties": {
                    "pm10": { "type": "number" }, "secret": { "type": "number" }
                } } }
            })),
            // The committed context describes the whole model, the attribute this endpoint
            // hides included: narrowing it is the gateway's job, not the model author's.
            context: Some(json!({ "@context": {
                "pm10": "https://smartdatamodels.org/pm10",
                "secret": "https://smartdatamodels.org/secret"
            } })),
        }],
        policies: vec![serde_norway::from_str(
            r#"contextSpaceRef: ovzdusie
assigner: did:web:banskabystrica.sk
assignee: { kind: role, id: public }
operations: [queryEntity, retrieveEntity]
"#,
        )
        .expect("the policy spec parses")],
    }
}

fn gateway(broker: &str, endpoint: Endpoint) -> axum::Router {
    router(Arc::new(
        Gateway::new(
            Broker::new(broker),
            Box::new(PolicyPdp),
            "banskabystrica.sk",
        )
        .serve([endpoint]),
    ))
}

fn station(local: &str, pm10: f64) -> Value {
    json!({
        "id": format!("urn:ngsi-ld:AirQualityObserved:banskabystrica.sk:ovzdusie:{local}"),
        "type": "AirQualityObserved",
        "pm10": { "type": "Property", "value": pm10 },
        "secret": { "type": "Property", "value": 1 },
        "location": {
            "type": "GeoProperty",
            "value": { "type": "Point", "coordinates": [19.146, 48.736] }
        }
    })
}

async fn download(app: axum::Router, path: &str) -> (StatusCode, Vec<u8>, Option<String>) {
    let response = app
        .oneshot(
            HttpRequest::builder()
                .uri(path)
                .body(Body::empty())
                .expect("a request"),
        )
        .await
        .expect("the gateway answers");
    let status = response.status();
    let disposition = response
        .headers()
        .get(axum::http::header::CONTENT_DISPOSITION)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let body = axum::body::to_bytes(response.into_body(), 64 * 1024 * 1024)
        .await
        .expect("a readable body");
    (status, body.to_vec(), disposition)
}

/// Every member of the archive, keyed by its path with the dated top directory removed, so a
/// test names `data/entities.csv` and not the day it ran.
fn members(archive: &[u8]) -> Vec<(String, Vec<u8>)> {
    let mut zip =
        zip::ZipArchive::new(std::io::Cursor::new(archive.to_vec())).expect("a zip archive");
    let mut found = Vec::new();
    for index in 0..zip.len() {
        let mut member = zip.by_index(index).expect("a member");
        let name = member
            .name()
            .split_once('/')
            .map(|(_, tail)| tail.to_owned())
            .expect("every member is under one directory");
        let mut body = Vec::new();
        member.read_to_end(&mut body).expect("a readable member");
        found.push((name, body));
    }
    found
}

fn member<'a>(members: &'a [(String, Vec<u8>)], name: &str) -> &'a [u8] {
    members
        .iter()
        .find(|(path, _)| path == name)
        .map(|(_, body)| body.as_slice())
        .unwrap_or_else(|| {
            panic!(
                "{name} is not in the bundle; it holds {:?}",
                members.iter().map(|(path, _)| path).collect::<Vec<_>>()
            )
        })
}

/// EP-41: the same query in every shape, plus what describes it and where it came from.
#[tokio::test]
async fn the_bundle_carries_the_data_the_schemas_and_the_catalogue_record() {
    let broker = BrokerStub::start(vec![json!([station("station-01", 34.2)])]).await;
    let (status, archive, disposition) = download(
        gateway(&broker.url, endpoint(None, &[])),
        &format!("/api/endpoint/{SLUG}/file.zip"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let members = members(&archive);
    let paths: Vec<&String> = members.iter().map(|(path, _)| path).collect();
    for expected in [
        "data/entities.jsonld",
        "data/entities.geojson",
        "data/entities.csv",
        "dcat.jsonld",
        "manifest.json",
    ] {
        assert!(
            paths.iter().any(|path| *path == expected),
            "{expected} missing from {paths:?}"
        );
    }

    // EP-51: the complete schema directory of every major the endpoint publishes, seven
    // documents each, the same names the `schema/` surface serves.
    for artifact in [
        "model.linkml.yaml",
        "model.schema.json",
        "context.jsonld",
        "model.shacl.ttl",
        "model.owl.ttl",
        "model.rdf.ttl",
        "model.md",
    ] {
        let path = format!("schema/v1/{artifact}");
        assert!(
            paths.iter().any(|found| **found == path),
            "{path} missing from {paths:?}"
        );
    }

    // EP-43: the browser saves it under a name that says what it is and when.
    let disposition = disposition.expect("a content-disposition header");
    assert!(
        disposition.starts_with("attachment; filename=\""),
        "{disposition}"
    );
    assert!(disposition.contains(SLUG), "{disposition}");
    assert!(disposition.ends_with(".zip\""), "{disposition}");
}

/// The three data files are three shapes of one answer, so they agree on what is in it.
#[tokio::test]
async fn the_three_shapes_describe_the_same_entities() {
    let broker = BrokerStub::start(vec![json!([
        station("station-01", 34.2),
        station("station-02", 51.0)
    ])])
    .await;
    let (_, archive, _) = download(
        gateway(&broker.url, endpoint(None, &[])),
        &format!("/api/endpoint/{SLUG}/file.zip"),
    )
    .await;
    let members = members(&archive);

    let entities: Value =
        serde_json::from_slice(member(&members, "data/entities.jsonld")).expect("json-ld");
    assert_eq!(entities.as_array().map(Vec::len), Some(2));

    let features: Value =
        serde_json::from_slice(member(&members, "data/entities.geojson")).expect("geojson");
    assert_eq!(features["type"], json!("FeatureCollection"));
    assert_eq!(features["features"].as_array().map(Vec::len), Some(2));

    let csv = String::from_utf8(member(&members, "data/entities.csv").to_vec()).expect("utf-8");
    assert_eq!(csv.lines().count(), 3, "a header and two rows:\n{csv}");

    let manifest: Value =
        serde_json::from_slice(member(&members, "manifest.json")).expect("a manifest");
    assert_eq!(manifest["rows"], json!(2));
    assert_eq!(manifest["endpoint"], json!(SLUG));
    assert_eq!(manifest["space"], json!("ovzdusie"));
    assert_eq!(manifest["types"], json!(["AirQualityObserved"]));
}

/// EP-07, EP-61: a bundle is a second way to read what the endpoint already allows, never a
/// second set of rules. An attribute the endpoint hides is in none of the three shapes.
#[tokio::test]
async fn a_hidden_attribute_is_absent_from_every_shape_in_the_bundle() {
    let broker = BrokerStub::start(vec![json!([station("station-01", 34.2)])]).await;
    let (_, archive, _) = download(
        gateway(&broker.url, endpoint(None, &["secret"])),
        &format!("/api/endpoint/{SLUG}/file.zip"),
    )
    .await;

    for (path, body) in members(&archive) {
        if !path.starts_with("data/") {
            continue;
        }
        let text = String::from_utf8_lossy(&body);
        assert!(
            !text.contains("secret"),
            "{path} carries a hidden attribute"
        );
    }
}

/// EP-44: the first row that would cross the ceiling ends the download, because a short bundle
/// is indistinguishable from a complete one.
#[tokio::test]
async fn a_bundle_over_the_row_limit_is_refused_and_not_truncated() {
    let broker = BrokerStub::start(vec![json!([
        station("station-01", 1.0),
        station("station-02", 2.0),
        station("station-03", 3.0)
    ])])
    .await;
    let limits = FileLimits {
        max_file_rows: Some(2),
        max_file_bytes: None,
    };
    let (status, body, _) = download(
        gateway(&broker.url, endpoint(Some(limits), &[])),
        &format!("/api/endpoint/{SLUG}/file.zip"),
    )
    .await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
    assert!(
        !body.starts_with(b"PK"),
        "a refusal must not also be an archive"
    );
}

/// The byte ceiling is measured on what goes into the archive, so a bundle is never larger than
/// the endpoint advertises even though compression would have made it smaller.
#[tokio::test]
async fn a_bundle_over_the_byte_limit_is_refused() {
    let broker = BrokerStub::start(vec![json!([station("station-01", 34.2)])]).await;
    let limits = FileLimits {
        max_file_rows: None,
        max_file_bytes: Some(64),
    };
    let (status, _, _) = download(
        gateway(&broker.url, endpoint(Some(limits), &[])),
        &format!("/api/endpoint/{SLUG}/file.zip"),
    )
    .await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
}

/// EP-05, R20: an endpoint that does not enable the bundle answers the same 404 an unknown slug
/// gets, so a probe learns nothing about which representations exist.
#[tokio::test]
async fn an_endpoint_that_does_not_enable_the_bundle_answers_404() {
    let broker = BrokerStub::start(vec![json!([station("station-01", 34.2)])]).await;
    let mut without = endpoint(None, &[]);
    without.representations = vec![Representation::NgsiLd];
    let (status, _, _) = download(
        gateway(&broker.url, without),
        &format!("/api/endpoint/{SLUG}/file.zip"),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

/// A non-spatial answer is still a bundle: the FeatureCollection is empty rather than the
/// download failing, because the caller asked for every shape and not for GeoJSON.
#[tokio::test]
async fn a_dataset_without_geometry_still_bundles() {
    let flat = json!({
        "id": "urn:ngsi-ld:AirQualityObserved:banskabystrica.sk:ovzdusie:station-09",
        "type": "AirQualityObserved",
        "pm10": { "type": "Property", "value": 12.0 }
    });
    let broker = BrokerStub::start(vec![json!([flat])]).await;
    let (status, archive, _) = download(
        gateway(&broker.url, endpoint(None, &[])),
        &format!("/api/endpoint/{SLUG}/file.zip"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let members = members(&archive);
    let features: Value =
        serde_json::from_slice(member(&members, "data/entities.geojson")).expect("geojson");
    assert_eq!(features["features"], json!([]));
    let csv = String::from_utf8(member(&members, "data/entities.csv").to_vec()).expect("utf-8");
    assert_eq!(csv.lines().count(), 2, "the row is still there:\n{csv}");
}

/// EP-61, MP-02, T-2341: the bundle's `schema/v1/context.jsonld` is a schema document like the
/// six beside it, and a term is a name *and* an IRI. A grant that names no `information` — the
/// dev seed's own `public-read` is exactly that — leaves the caller with no type whitelist, and
/// a whitelist-free caller used to cover every term of the committed context by the class test,
/// which the hidden-attribute test in front of it could then never refuse.
#[tokio::test]
async fn no_schema_document_in_the_bundle_describes_a_hidden_attribute() {
    let broker = BrokerStub::start(vec![json!([station("station-01", 34.2)])]).await;
    let (_, archive, _) = download(
        gateway(&broker.url, endpoint(None, &["secret"])),
        &format!("/api/endpoint/{SLUG}/file.zip"),
    )
    .await;

    let schemas: Vec<_> = members(&archive)
        .into_iter()
        .filter(|(path, _)| path.starts_with("schema/"))
        .collect();
    assert!(
        schemas
            .iter()
            .any(|(path, _)| path.ends_with("context.jsonld")),
        "the bundle carries no @context to check: {:?}",
        schemas.iter().map(|(p, _)| p).collect::<Vec<_>>()
    );
    for (path, body) in &schemas {
        let text = String::from_utf8_lossy(body);
        assert!(
            !text.contains("secret"),
            "{path} names an attribute the endpoint hides: {text:.400}"
        );
    }

    // The attribute the endpoint does serve is still described, or the check above would pass
    // on an empty document.
    let context = member(&members(&archive), "schema/v1/context.jsonld").to_vec();
    assert!(
        String::from_utf8_lossy(&context).contains("pm10"),
        "the granted attribute was narrowed away with the hidden one"
    );
}
