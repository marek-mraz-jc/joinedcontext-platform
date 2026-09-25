//! The DCAT-AP record an endpoint answers with at its own root (T-0337, T-0338, EP-27,
//! EP-68, EP-69).
//!
//! The record is what a catalogue harvests and what the CKAN publisher takes its metadata
//! from, so the two properties that matter are that it names every artifact with the digest
//! of the document this caller would actually download, and that it never names one the
//! caller may not read.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use context_gateway::app::{router, Gateway};
use context_gateway::pdp::PolicyPdp;
use context_gateway::proxy::Broker;
use context_gateway::resolver::{Endpoint, Model, Space};
use jc_core::kinds::{Audience, PolicySpec, Representation};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::sync::Arc;
use tower::ServiceExt;

const SLUG: &str = "k4y7pq2mzt6vhx3nbwrs5cjd8f";

fn policy(yaml: &str) -> PolicySpec {
    serde_norway::from_str(yaml).expect("the policy spec parses")
}

fn compiled_schema() -> Value {
    json!({
        "$schema": "http://json-schema.org/draft-07/schema#",
        "$defs": {
            "AirQualityObserved": {
                "type": "object",
                "properties": {
                    "id": { "type": "string" },
                    "type": { "const": "AirQualityObserved" },
                    "pm10": { "type": "number" },
                    "pm25": { "type": "number" },
                    "internalNote": { "type": "string" },
                },
            },
            "InternalIncident": {
                "type": "object",
                "properties": { "severity": { "type": "string" } },
            },
        },
    })
}

fn air_quality() -> Model {
    Model {
        name: "bb-air-quality".to_owned(),
        version: "1.4.0".to_owned(),
        major: 1,
        classes: vec![
            "AirQualityObserved".to_owned(),
            "InternalIncident".to_owned(),
        ],
        json_schema: Some(compiled_schema()),
        context: Some(json!({ "@context": { "pm10": "https://bb.example.sk/pm10" } })),
    }
}

/// The demo endpoint: three representations, one model, the public grant of DEMO step 4.
fn endpoint(audience: Audience) -> Endpoint {
    Endpoint {
        declared_types: None,
        roles: Default::default(),
        slug: SLUG.to_owned(),
        title: std::collections::BTreeMap::new(),
        description: std::collections::BTreeMap::new(),
        space: "ovzdusie".to_owned(),
        project: "ovzdusie".to_owned(),
        audience,
        allowed_projects: Vec::new(),
        representations: vec![
            Representation::NgsiLd,
            Representation::GeoJson,
            Representation::Csv,
        ],
        rate_limit: None,
        file_limits: None,
        hidden_attributes: Default::default(),
        projection: None,
        view_mapping: None,
        base_path: format!("/api/endpoint/{SLUG}"),
        models: vec![air_quality()],
        policies: vec![policy(
            r#"contextSpaceRef: ovzdusie
assigner: did:web:banskabystrica.sk
assignee: { kind: role, id: public }
operations: [queryEntity, retrieveEntity]
information:
  - entities:
      - type: AirQualityObserved
    propertyNames: [pm10, pm25]
"#,
        )],
    }
}

fn space(endpoint: Endpoint) -> Space {
    Space {
        endpoint: Arc::new(endpoint),
        title: BTreeMap::from([
            ("sk".to_owned(), "Kvalita ovzdušia".to_owned()),
            ("en".to_owned(), "Air quality".to_owned()),
        ]),
        description: BTreeMap::from([("en".to_owned(), "Stations of the city".to_owned())]),
        is_sandbox: false,
        default_locale: Some("sk".to_owned()),
    }
}

fn app(audience: Audience) -> axum::Router {
    let resolved = endpoint(audience);
    router(Arc::new(
        Gateway::new(
            Broker::new("http://127.0.0.1:1"),
            Box::new(PolicyPdp),
            "banskabystrica.sk",
        )
        .serve([resolved.clone()])
        .serve_spaces([space(resolved)]),
    ))
}

async fn get(audience: Audience, path: &str, accept: &str) -> (StatusCode, String, String) {
    let response = app(audience)
        .oneshot(
            Request::builder()
                .uri(path)
                .header("accept", accept)
                .body(Body::empty())
                .expect("a request"),
        )
        .await
        .expect("the gateway answers");
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

async fn record(audience: Audience) -> Value {
    let (status, media, body) = get(audience, &format!("/api/endpoint/{SLUG}/"), "*/*").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(media, "application/ld+json");
    serde_json::from_str(&body).expect("the record is JSON-LD")
}

fn distributions(record: &Value) -> Vec<Value> {
    record["dcat:distribution"]
        .as_array()
        .expect("the record lists distributions")
        .clone()
}

fn urls(record: &Value) -> Vec<String> {
    distributions(record)
        .iter()
        .map(|d| d["dcat:accessURL"].as_str().unwrap_or_default().to_owned())
        .collect()
}

#[tokio::test]
async fn the_record_lists_one_distribution_per_enabled_representation() {
    let record = record(Audience::Public).await;
    let urls = urls(&record);

    for path in ["ngsi-ld/v1/", "file.geojson", "file.csv"] {
        assert!(
            urls.iter().any(|url| url.ends_with(path)),
            "{path} is not listed among {urls:?}"
        );
    }
    // A representation the endpoint does not serve must not be advertised: a caller
    // following it would get a 404 from a document that promised it (EP-05).
    assert!(!urls.iter().any(|url| url.ends_with("file.xlsx")));
    assert!(!urls.iter().any(|url| url.ends_with("sta/v1.1/")));
}

#[tokio::test]
async fn every_schema_artifact_is_a_distribution_with_its_media_type_and_digest() {
    let record = record(Audience::Public).await;
    let schema: Vec<Value> = distributions(&record)
        .into_iter()
        .filter(|d| {
            d["dcat:accessURL"]
                .as_str()
                .is_some_and(|url| url.contains("/schema/v1/"))
        })
        .collect();

    // The seven documents of EP-46, each with the digest of the projected document.
    assert_eq!(schema.len(), 7, "{schema:#?}");
    for distribution in &schema {
        let url = distribution["dcat:accessURL"]
            .as_str()
            .expect("an accessURL");
        assert!(
            distribution["dcat:mediaType"].is_string(),
            "{url} names no media type"
        );
        let sha = distribution["spdx:checksum"]["spdx:checksumValue"]
            .as_str()
            .unwrap_or_else(|| panic!("{url} carries no sha256"));
        assert_eq!(sha.len(), 64, "{url} carries {sha}, not a sha256");
        assert!(
            distribution["dct:conformsTo"].is_string(),
            "{url} names no formalism"
        );
    }
}

#[tokio::test]
async fn the_digest_in_the_record_is_the_one_the_artifact_answers_with() {
    // EP-68: a harvester that stored the record can tell a stale copy without fetching the
    // artifact, which only holds if the digest is the artifact's own ETag.
    let record = record(Audience::Public).await;
    let json_schema = distributions(&record)
        .into_iter()
        .find(|d| {
            d["dcat:accessURL"]
                .as_str()
                .is_some_and(|url| url.ends_with("model.schema.json"))
        })
        .expect("the JSON Schema is listed");
    let declared = json_schema["spdx:checksum"]["spdx:checksumValue"]
        .as_str()
        .expect("a digest")
        .to_owned();

    let response = app(Audience::Public)
        .oneshot(
            Request::builder()
                .uri(format!("/api/endpoint/{SLUG}/schema/v1/model.schema.json"))
                .body(Body::empty())
                .expect("a request"),
        )
        .await
        .expect("the gateway answers");
    let etag = response
        .headers()
        .get("etag")
        .and_then(|value| value.to_str().ok())
        .expect("the artifact carries an ETag")
        .to_owned();

    assert_eq!(etag, format!("\"{declared}\""));
}

#[tokio::test]
async fn a_type_the_grant_does_not_reach_is_absent_from_the_record() {
    // R20: the record is the granted projection like every other surface. The public grant
    // names AirQualityObserved and nothing else, so the digests are of a document without
    // InternalIncident, and the record must not mention it either.
    let record = record(Audience::Public).await;
    let text = serde_json::to_string(&record).expect("the record serializes");
    assert!(!text.contains("InternalIncident"), "{text}");
    assert!(!text.contains("internalNote"), "{text}");
}

#[tokio::test]
async fn a_public_endpoint_says_public_and_points_at_no_policy() {
    let record = record(Audience::Public).await;
    assert_eq!(
        record["dct:accessRights"],
        json!("http://publications.europa.eu/resource/authority/access-right/PUBLIC")
    );
    // EP-69: only a restricted endpoint carries the connector's pointer.
    assert!(record.get("odrl:hasPolicy").is_none(), "{record:#?}");
}

#[tokio::test]
async fn a_restricted_endpoint_says_restricted_and_points_at_its_access_document() {
    // An organization endpoint refuses an anonymous caller outright, so the record it
    // would serve is exercised through the same document a member receives.
    let record = context_gateway::handlers::endpoint_surface::dataset(
        &endpoint(Audience::Organization),
        None,
        &json!({ "models": [] }),
        "https://joinedcontext.test",
    );
    assert_eq!(
        record["dct:accessRights"],
        json!("http://publications.europa.eu/resource/authority/access-right/RESTRICTED")
    );
    assert_eq!(
        record["odrl:hasPolicy"],
        json!(format!(
            "https://joinedcontext.test/api/endpoint/{SLUG}/access"
        ))
    );
}

#[tokio::test]
async fn the_record_carries_the_title_of_the_space_behind_it() {
    let record = record(Audience::Public).await;
    let titles = record["dct:title"]
        .as_array()
        .expect("a language map")
        .clone();
    assert!(titles.iter().any(|t| t["@value"] == json!("Air quality")));
    assert!(titles
        .iter()
        .any(|t| t["@value"] == json!("Kvalita ovzdušia")));
}

/// EP-27: several endpoints publish slices of one space (EP-14, GW8), so the record carries
/// the endpoint's own `metadata.title` and `description` and reads the space's only when
/// the endpoint names none; a catalogue that harvested the space's for every one of them
/// would list four datasets under one name.
#[tokio::test]
async fn the_record_prefers_the_endpoints_own_title_and_description() {
    let mut named = endpoint(Audience::Public);
    named.title = BTreeMap::from([
        ("en".to_owned(), "Air quality stations".to_owned()),
        ("sk".to_owned(), "Stanice kvality ovzdušia".to_owned()),
    ]);
    named.description = BTreeMap::from([("en".to_owned(), "Only the stations".to_owned())]);
    let app = router(Arc::new(
        Gateway::new(
            Broker::new("http://127.0.0.1:1"),
            Box::new(PolicyPdp),
            "banskabystrica.sk",
        )
        .serve([named.clone()])
        .serve_spaces([space(named)]),
    ));

    let json_ld = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/api/endpoint/{SLUG}/"))
                .header("accept", "application/ld+json")
                .body(Body::empty())
                .expect("a request"),
        )
        .await
        .expect("the gateway answers");
    assert_eq!(json_ld.status(), StatusCode::OK);
    let body = axum::body::to_bytes(json_ld.into_body(), 512 * 1024)
        .await
        .expect("a readable body");
    let record: Value = serde_json::from_slice(&body).expect("the record is JSON-LD");
    let titles: Vec<&str> = record["dct:title"]
        .as_array()
        .expect("a language map")
        .iter()
        .filter_map(|t| t["@value"].as_str())
        .collect();
    assert_eq!(
        titles,
        vec!["Air quality stations", "Stanice kvality ovzdušia"]
    );
    assert_eq!(
        record["dct:description"][0]["@value"],
        json!("Only the stations")
    );

    let turtle = app
        .oneshot(
            Request::builder()
                .uri(format!("/api/endpoint/{SLUG}/"))
                .header("accept", "text/turtle")
                .body(Body::empty())
                .expect("a request"),
        )
        .await
        .expect("the gateway answers");
    let body = axum::body::to_bytes(turtle.into_body(), 512 * 1024)
        .await
        .expect("a readable body");
    let turtle = String::from_utf8_lossy(&body);
    assert!(
        turtle.contains("dct:title \"Air quality stations\""),
        "{turtle}"
    );
    assert!(!turtle.contains("\"Air quality\""), "{turtle}");
}

#[tokio::test]
async fn the_same_record_is_served_as_turtle_and_as_a_page() {
    let (status, media, turtle) = get(
        Audience::Public,
        &format!("/api/endpoint/{SLUG}/"),
        "text/turtle",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(media, "text/turtle");
    assert!(turtle.contains("a dcat:Dataset"), "{turtle}");
    assert!(turtle.contains("dcat:distribution"), "{turtle}");
    assert!(turtle.contains("spdx:checksum"), "{turtle}");
    assert!(!turtle.contains("InternalIncident"), "{turtle}");

    let (status, media, html) = get(
        Audience::Public,
        &format!("/api/endpoint/{SLUG}/"),
        "text/html",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(media, "text/html; charset=utf-8");
    assert!(html.contains("Air quality"), "{html}");
    assert!(html.contains("model.shacl.ttl"), "{html}");
}

#[tokio::test]
async fn the_base_url_answers_with_and_without_the_trailing_slash() {
    // EP-01 writes the base URL with the slash and every client that stores one drops it.
    let (status, media, _) = get(Audience::Public, &format!("/api/endpoint/{SLUG}"), "*/*").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(media, "application/ld+json");
}

#[tokio::test]
async fn an_unknown_slug_answers_404_rather_than_an_empty_record() {
    let (status, _, _) = get(
        Audience::Public,
        "/api/endpoint/zzzzzzzzzzzzzzzzzzzzzzzzzz/",
        "*/*",
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}
