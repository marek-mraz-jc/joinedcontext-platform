//! `file.csv` as the gateway serves it (T-0157, EP-08, EP-44, EP-45); the table itself is
//! tested in jc-tabular.

mod common;

use axum::body::Body;
use axum::http::{Request as HttpRequest, StatusCode};
use common::{BrokerStub, View};
use context_gateway::app::{router, Gateway};
use context_gateway::pdp::PolicyPdp;
use context_gateway::proxy::Broker;
use context_gateway::resolver::Endpoint;
use context_gateway::translators::tabular::flatten;
use jc_core::kinds::{Audience, FileLimits, Representation};
use serde_json::{json, Value};
use std::sync::Arc;
use tower::ServiceExt;

const SLUG: &str = "k4y7pq2mzt6vhx3nbwrs5cjd8f";

/// The public grant: two readable attributes out of the three the broker returns.
fn endpoint(representations: Vec<Representation>, file_limits: Option<FileLimits>) -> Endpoint {
    Endpoint {
        declared_types: None,
        roles: Default::default(),
        slug: SLUG.to_owned(),
        title: std::collections::BTreeMap::new(),
        description: std::collections::BTreeMap::new(),
        space: "ovzdusie".to_owned(),
        project: "ovzdusie".to_owned(),
        audience: Audience::Public,
        allowed_projects: Vec::new(),
        representations,
        rate_limit: None,
        creates: None,
        file_limits,
        hidden_attributes: Default::default(),
        projection: None,
        base_path: format!("/api/endpoint/{SLUG}"),
        models: Vec::new(),
        view_mapping: None,
        catalog: None,
        deprecation: None,
        policy_names: Vec::new(),
        manifest_name: String::new(),
        policies: vec![serde_norway::from_str(
            r#"contextSpaceRef: ovzdusie
assigner: did:web:banskabystrica.sk
assignee: { kind: role, id: public }
operations: [queryEntity, retrieveEntity]
information:
  - entities:
      - type: AirQualityObserved
    propertyNames: [pm10, location]
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

fn reading(n: usize) -> Value {
    json!({
        "id": format!("urn:ngsi-ld:AirQualityObserved:banskabystrica.sk:ovzdusie:s{n}"),
        "type": "AirQualityObserved",
        "pm10": { "type": "Property", "value": n },
        "location": { "type": "GeoProperty",
                      "value": { "type": "Point", "coordinates": [19.1, 48.7] } },
        "operatorNote": { "type": "Property", "value": "internal only" }
    })
}

async fn download(app: axum::Router, path: &str) -> (StatusCode, String, String) {
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
    let media = response
        .headers()
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    let body = axum::body::to_bytes(response.into_body(), 16 * 1024 * 1024)
        .await
        .expect("a readable body");
    (status, media, String::from_utf8_lossy(&body).into_owned())
}

/// EP-06: an attribute the view does not show has no column, exactly as it has no member in the
/// JSON representation. A second format is not a second set of rules (T-3651: the table is cut
/// from the view's answer).
#[tokio::test]
async fn an_ungranted_attribute_is_not_a_column() {
    let broker = BrokerStub::narrowing(
        vec![json!([reading(1)])],
        View::of(
            SLUG,
            "public",
            &["AirQualityObserved"],
            Some(&["pm10", "location"]),
        ),
    )
    .await;
    let (status, media, body) = download(
        gateway(&broker.url, endpoint(vec![Representation::Csv], None)),
        &format!("/api/endpoint/{SLUG}/file.csv?type=AirQualityObserved"),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(media, "text/csv; charset=utf-8; header=present");
    let header = body.lines().next().expect("a header row");
    assert!(header.contains("pm10.value"), "{header}");
    assert!(header.contains("location.value.coordinates[0]"), "{header}");
    assert!(
        !body.contains("operatorNote") && !body.contains("internal only"),
        "an ungranted attribute leaked into the table: {body}"
    );
}

/// EP-44: a file representation returns the whole result set, not one broker page, and
/// the paging window is the gateway's own rather than the caller's.
#[tokio::test]
async fn the_gateway_pages_through_the_broker() {
    let full: Vec<Value> = (0..1000).map(reading).collect();
    let broker = BrokerStub::start(vec![json!(full), json!([reading(1000)])]).await;
    let (status, _, body) = download(
        gateway(&broker.url, endpoint(vec![Representation::Csv], None)),
        &format!("/api/endpoint/{SLUG}/file.csv?type=AirQualityObserved"),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    // 1001 rows plus the header.
    assert_eq!(body.lines().count(), 1002);

    let hops = broker.hops();
    assert_eq!(
        hops.len(),
        2,
        "a full page must be followed by another read"
    );
    assert!(hops[0].query.contains("limit=1000"), "{}", hops[0].query);
    assert!(hops[0].query.contains("offset=0"), "{}", hops[0].query);
    assert!(hops[1].query.contains("offset=1000"), "{}", hops[1].query);
    assert_eq!(hops[0].tenant, "ovzdusie");
}

/// EP-44: past the endpoint's own ceiling the download is refused, never truncated.
#[tokio::test]
async fn the_endpoints_row_ceiling_answers_413() {
    let full: Vec<Value> = (0..1000).map(reading).collect();
    let broker = BrokerStub::start(vec![json!(full)]).await;
    let limits = FileLimits {
        max_file_rows: Some(10),
        max_file_bytes: None,
    };
    let (status, _, _) = download(
        gateway(
            &broker.url,
            endpoint(vec![Representation::Csv], Some(limits)),
        ),
        &format!("/api/endpoint/{SLUG}/file.csv?type=AirQualityObserved"),
    )
    .await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
}

/// EP-05: an endpoint that does not enable the representation does not have the URL.
#[tokio::test]
async fn a_representation_the_endpoint_does_not_serve_is_404() {
    let broker = BrokerStub::start(vec![json!([reading(1)])]).await;
    let (status, _, _) = download(
        gateway(&broker.url, endpoint(vec![Representation::NgsiLd], None)),
        &format!("/api/endpoint/{SLUG}/file.csv"),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(broker.hops().is_empty(), "a 404 must not reach the broker");
}

/// EP-43: the table is sent as an attachment under CSV's own media type.
#[tokio::test]
async fn the_csv_is_served_as_an_attachment() {
    let broker = BrokerStub::start(vec![json!([reading(1)])]).await;
    let response = gateway(&broker.url, endpoint(vec![Representation::Csv], None))
        .oneshot(
            HttpRequest::builder()
                .uri(format!(
                    "/api/endpoint/{SLUG}/file.csv?type=AirQualityObserved"
                ))
                .body(Body::empty())
                .expect("a request"),
        )
        .await
        .expect("the gateway answers");

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get(axum::http::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok()),
        Some("text/csv; charset=utf-8; header=present")
    );
    assert_eq!(
        response
            .headers()
            .get(axum::http::header::CONTENT_DISPOSITION)
            .and_then(|value| value.to_str().ok()),
        Some(format!("attachment; filename=\"{SLUG}.csv\"").as_str())
    );

    let body = axum::body::to_bytes(response.into_body(), 16 * 1024 * 1024)
        .await
        .expect("a readable body");
    assert!(String::from_utf8_lossy(&body).starts_with("id,type"));
}

/// T-0810, EP-44: past the endpoint's byte ceiling the file is refused whole, never built in
/// memory and answered 200.
#[tokio::test]
async fn the_endpoints_byte_ceiling_refuses_the_file() {
    let full: Vec<Value> = (0..1000).map(reading).collect();
    let limits = FileLimits {
        max_file_rows: None,
        max_file_bytes: Some(1024),
    };
    let broker = BrokerStub::start(vec![json!(full)]).await;
    let (status, _, _) = download(
        gateway(
            &broker.url,
            endpoint(vec![Representation::Csv], Some(limits)),
        ),
        &format!("/api/endpoint/{SLUG}/file.csv?type=AirQualityObserved"),
    )
    .await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
}

/// T-3141: a typed literal (`{"@type": "DateTime", "@value": …}`, NGSI-LD 4.6.3) is one cell,
/// its `@value`. Skipping every `@` member dropped the whole attribute: the Žilina KPIs'
/// `updatedAt` was missing from `file.csv`, so the CKAN DataStore held it empty and the
/// table's freshness fell back to the statistic's period.
#[test]
fn a_typed_literal_is_its_value() {
    let kpi = json!({
        "id": "urn:ngsi-ld:KeyPerformanceIndicator:zilina.sk:zilina-kpi:index-starnutia-mesto",
        "type": "KeyPerformanceIndicator",
        "updatedAt": { "type": "Property",
                       "value": { "@type": "DateTime", "@value": "2026-10-19T05:37:50Z" } },
        "calculationPeriod": { "type": "Property",
                               "value": { "start": "2025-01-01T00:00:00Z", "end": "2025-12-31T23:59:59Z" } }
    });
    let cells = flatten(&kpi);
    assert!(
        cells.contains(&("updatedAt.value".to_owned(), json!("2026-10-19T05:37:50Z"))),
        "{cells:?}"
    );
    assert!(
        cells.iter().all(|(name, _)| !name.contains('@')),
        "no JSON-LD keyword becomes a column: {cells:?}"
    );
    assert!(cells.contains(&(
        "calculationPeriod.value.start".to_owned(),
        json!("2025-01-01T00:00:00Z")
    )));
}

async fn download_with(
    app: axum::Router,
    path: &str,
    if_none_match: Option<&str>,
) -> axum::response::Response {
    let mut request = HttpRequest::builder().uri(path);
    if let Some(tag) = if_none_match {
        request = request.header(axum::http::header::IF_NONE_MATCH, tag);
    }
    app.oneshot(request.body(Body::empty()).expect("a request"))
        .await
        .expect("the gateway answers")
}

/// EP-43 (T-5209): a strong ETag over the bytes, and a 304 with no body while they still match.
#[tokio::test]
async fn a_download_carries_an_etag_and_answers_304() {
    let broker = BrokerStub::start(vec![json!([reading(1)])]).await;
    let app = gateway(&broker.url, endpoint(vec![Representation::Csv], None));
    let path = format!("/api/endpoint/{SLUG}/file.csv?type=AirQualityObserved");

    let first = download_with(app.clone(), &path, None).await;
    assert_eq!(first.status(), StatusCode::OK);
    let tag = first
        .headers()
        .get(axum::http::header::ETAG)
        .and_then(|value| value.to_str().ok())
        .expect("an ETag")
        .to_owned();
    assert!(tag.starts_with('"') && !tag.starts_with("W/"), "{tag}");
    assert!(first
        .headers()
        .get(axum::http::header::CONTENT_DISPOSITION)
        .is_some());

    let again = download_with(app.clone(), &path, Some(&tag)).await;
    assert_eq!(again.status(), StatusCode::NOT_MODIFIED);
    let body = axum::body::to_bytes(again.into_body(), 1024)
        .await
        .expect("a body");
    assert!(body.is_empty());

    let stale = download_with(app, &path, Some("\"other\"")).await;
    assert_eq!(stale.status(), StatusCode::OK);
}

/// T-5209: a dialect the download does not know is a 400 naming the parameter.
#[tokio::test]
async fn an_unknown_delimiter_is_refused_by_name() {
    let broker = BrokerStub::start(vec![json!([reading(1)])]).await;
    let (status, _, body) = download(
        gateway(&broker.url, endpoint(vec![Representation::Csv], None)),
        &format!("/api/endpoint/{SLUG}/file.csv?delimiter=pipe"),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body.contains("delimiter"), "{body}");
}

/// T-5209 finding 20: `options=keyValues` never reaches the broker, so it renames no column; the
/// spreadsheet dialect starts with the BOM and splits on `;`.
#[tokio::test]
async fn key_values_cannot_rename_columns_and_the_spreadsheet_dialect_holds() {
    let broker = BrokerStub::start(vec![json!([reading(1)])]).await;
    let (status, _, body) = download(
        gateway(&broker.url, endpoint(vec![Representation::Csv], None)),
        &format!(
            "/api/endpoint/{SLUG}/file.csv?type=AirQualityObserved&options=keyValues\
             &format=keyValues&humanHeaders=true&delimiter=semicolon"
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.starts_with("\u{feff}id;type;"), "{body}");
    assert!(broker
        .hops()
        .iter()
        .all(|hop| !hop.query.contains("keyValues")));
}

fn modelled(hidden: &[&str]) -> Endpoint {
    let model = json!({
        "$schema": "http://json-schema.org/draft-07/schema#",
        "definitions": { "AirQualityObserved": { "type": "object", "required": ["pm10"],
            "properties": { "id": { "type": "string" }, "pm10": { "type": "number" },
                "note": { "type": "string" }, "operatorNote": { "type": "string" } } } }
    });
    let schema = jc_manifest::model_schema::ModelSchema::compile(
        "air",
        "1.0.0",
        &model,
        &["AirQualityObserved".to_owned()],
        true,
    );
    let mut endpoint = endpoint(vec![Representation::Csv], None);
    endpoint.declared_types = Some(Arc::new(context_gateway::resolver::DeclaredTypes {
        model: "air".into(),
        classes: ["AirQualityObserved".to_owned()].into(),
        units: Default::default(),
        relationships: Default::default(),
        quality: Vec::new(),
        schema: Some(Arc::new(schema)),
    }));
    endpoint.hidden_attributes = hidden.iter().map(|name| (*name).to_owned()).collect();
    endpoint
}

/// T-5209 finding 15: the model fixes the columns, so an empty answer has the whole header and a
/// hidden attribute is not one of them.
#[tokio::test]
async fn the_model_fixes_the_header_of_an_empty_answer() {
    let mut endpoint = modelled(&["operatorNote"]);
    endpoint.policies = vec![serde_norway::from_str(
        r#"contextSpaceRef: ovzdusie
assigner: did:web:banskabystrica.sk
assignee: { kind: role, id: public }
operations: [queryEntity, retrieveEntity]
information:
  - entities:
      - type: AirQualityObserved
"#,
    )
    .expect("the policy spec parses")];
    let names: Vec<String> = endpoint
        .csv_slots(&[], |_, _| true)
        .into_iter()
        .map(|s| s.name)
        .collect();
    assert_eq!(names, vec!["pm10", "note"]);
    assert!(endpoint
        .csv_slots(&["Other".to_owned()], |_, _| true)
        .is_empty());

    let broker = BrokerStub::start(vec![json!([])]).await;
    let (status, _, body) = download(
        gateway(&broker.url, endpoint),
        &format!("/api/endpoint/{SLUG}/file.csv"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, "id,type,pm10.value,note.value\r\n");
}

/// EP-06 on a modelled Endpoint (T-5209 review): an attribute the model declares but the Policy
/// does not grant is no column either, not even an empty one.
#[tokio::test]
async fn an_ungranted_model_attribute_is_not_a_column() {
    let broker = BrokerStub::start(vec![json!([])]).await;
    let (status, _, body) = download(
        gateway(&broker.url, modelled(&[])),
        &format!("/api/endpoint/{SLUG}/file.csv"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, "id,type,pm10.value\r\n");
}
