//! Edge cases of `app::file_zip`, `GET /api/endpoint/{slug}/file.zip` (T-1887, EP-26, MP-02).
//!
//! Contract, in one sentence: the archive is one projected answer rendered several ways plus the
//! documents that describe it, so no member of it — data, schema, catalogue record or manifest —
//! may carry an attribute, a type or a description the caller could not read through the endpoint
//! itself; and it refuses rather than truncates, because a short bundle cannot be told from a
//! complete one (EP-07, EP-41, EP-43, EP-44, EP-51, EP-61).

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
const PUBLIC_READ: &str = r#"contextSpaceRef: ovzdusie
assigner: did:web:banskabystrica.sk
assignee: { kind: role, id: public }
operations: [queryEntity, retrieveEntity]
"#;

fn model() -> Model {
    Model {
        name: "air-quality".to_owned(),
        version: "1.2.0".to_owned(),
        major: 1,
        classes: vec!["AirQualityObserved".to_owned()],
        json_schema: Some(json!({
            "$schema": "http://json-schema.org/draft-07/schema#",
            "$defs": { "AirQualityObserved": { "type": "object", "properties": {
                "pm10": { "type": "number" },
                "secret": { "type": "number", "description": "the operator's own reading" }
            } } }
        })),
        context: Some(json!({ "@context": {
            "pm10": "https://smartdatamodels.org/pm10",
            "secret": "https://smartdatamodels.org/secret"
        } })),
    }
}

fn endpoint(limits: Option<FileLimits>, hidden: &[&str]) -> Endpoint {
    Endpoint {
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
        models: vec![model()],
        policies: vec![serde_norway::from_str(PUBLIC_READ).expect("the policy spec parses")],
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

struct Download {
    status: StatusCode,
    body: Vec<u8>,
    headers: axum::http::HeaderMap,
}

async fn download(app: axum::Router, request: HttpRequest<Body>) -> Download {
    let response = app.oneshot(request).await.expect("the gateway answers");
    let status = response.status();
    let headers = response.headers().clone();
    let body = axum::body::to_bytes(response.into_body(), 64 * 1024 * 1024)
        .await
        .expect("a readable body");
    Download {
        status,
        body: body.to_vec(),
        headers,
    }
}

async fn get(app: axum::Router, path: &str) -> Download {
    download(
        app,
        HttpRequest::builder()
            .uri(path)
            .body(Body::empty())
            .expect("a request"),
    )
    .await
}

/// Every member of the archive under its full stored name, so a test can read the names as a zip
/// tool would.
fn stored(archive: &[u8]) -> Vec<(String, Vec<u8>)> {
    let mut zip =
        zip::ZipArchive::new(std::io::Cursor::new(archive.to_vec())).expect("a zip archive");
    let mut found = Vec::new();
    for index in 0..zip.len() {
        let mut member = zip.by_index(index).expect("a member");
        let name = member.name().to_owned();
        let mut body = Vec::new();
        member.read_to_end(&mut body).expect("a readable member");
        found.push((name, body));
    }
    found
}

fn member<'a>(members: &'a [(String, Vec<u8>)], suffix: &str) -> &'a [u8] {
    members
        .iter()
        .find(|(path, _)| path.ends_with(suffix))
        .map(|(_, body)| body.as_slice())
        .unwrap_or_else(|| {
            panic!(
                "{suffix} is not in the bundle; it holds {:?}",
                members.iter().map(|(path, _)| path).collect::<Vec<_>>()
            )
        })
}

/// The first case: the schema documents in the bundle are the caller's own projection, so a hidden
/// attribute is neither described nor named in them — the data files were already covered, the
/// schema artifacts are the other half (EP-51, EP-61).
///
/// `schema/v1/context.jsonld` is excluded here because it does carry the hidden term today: that is
/// T-2341 (priority 1), which holds the red test. Everything else in the bundle is checked, so the
/// leak cannot spread to another member while that task is open.
#[tokio::test]
async fn no_schema_document_in_the_bundle_describes_a_hidden_attribute() {
    let broker = BrokerStub::start(vec![json!([station("station-01", 34.2)])]).await;
    let answer = get(
        gateway(&broker.url, endpoint(None, &["secret"])),
        &format!("/api/endpoint/{SLUG}/file.zip"),
    )
    .await;
    assert_eq!(answer.status, StatusCode::OK);

    let mut checked = 0;
    for (path, body) in stored(&answer.body) {
        if path.ends_with("context.jsonld") {
            continue; // T-2341
        }
        let text = String::from_utf8_lossy(&body);
        assert!(!text.contains("secret"), "{path} names a hidden attribute");
        assert!(
            !text.contains("the operator's own reading"),
            "{path} carries its description"
        );
        checked += 1;
    }
    assert!(checked >= 10, "every other member was read: {checked}");
    // What is served is still described, so the case is not green by describing nothing.
    let members = stored(&answer.body);
    let schema = String::from_utf8_lossy(member(&members, "schema/v1/model.schema.json"));
    assert!(schema.contains("pm10"), "{schema}");
}

/// An entity of a type no grant names is in none of the three shapes, even when the broker hands it
/// over: a bundle is a second way to read what the endpoint allows and never a second set of rules
/// (EP-07, T-1862).
#[tokio::test]
async fn an_entity_of_an_ungranted_type_is_in_no_shape_of_the_bundle() {
    let mut intruder = station("invoice-1", 0.0);
    intruder["type"] = json!("Invoice");
    intruder["id"] = json!("urn:ngsi-ld:Invoice:banskabystrica.sk:ovzdusie:invoice-1");
    intruder["amount"] = json!({ "type": "Property", "value": 4200 });
    let broker = BrokerStub::start(vec![json!([station("station-01", 34.2), intruder])]).await;

    let mut narrowed = endpoint(None, &[]);
    narrowed.policies = vec![serde_norway::from_str(
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

    let answer = get(
        gateway(&broker.url, narrowed),
        &format!("/api/endpoint/{SLUG}/file.zip"),
    )
    .await;
    assert_eq!(answer.status, StatusCode::OK);

    for (path, body) in stored(&answer.body) {
        let text = String::from_utf8_lossy(&body);
        assert!(!text.contains("Invoice"), "{path} names an ungranted type");
        assert!(!text.contains("4200"), "{path} carries its value");
    }
}

/// No member of the archive can write outside the directory it is unpacked into: no absolute path,
/// no `..`, no backslash and no drive letter, whatever the endpoint is called.
#[tokio::test]
async fn no_member_of_the_archive_escapes_its_directory() {
    let broker = BrokerStub::start(vec![json!([station("station-01", 34.2)])]).await;
    let answer = get(
        gateway(&broker.url, endpoint(None, &[])),
        &format!("/api/endpoint/{SLUG}/file.zip"),
    )
    .await;
    assert_eq!(answer.status, StatusCode::OK);

    let members = stored(&answer.body);
    assert!(!members.is_empty(), "the archive has members");
    let first_directory = members[0]
        .0
        .split_once('/')
        .map(|(head, _)| head.to_owned())
        .expect("one top directory");
    for (path, _) in &members {
        assert!(!path.starts_with('/'), "{path} is absolute");
        assert!(!path.contains(".."), "{path} climbs out");
        assert!(!path.contains('\\'), "{path} carries a backslash");
        assert!(!path.contains(':'), "{path} carries a drive letter");
        assert!(
            path.starts_with(&format!("{first_directory}/")),
            "{path} is outside {first_directory}"
        );
    }
}

/// The caller's query is written into the manifest, and nothing it contains can leave the manifest:
/// the archive is still one zip, the manifest is still one JSON document, and no header carries a
/// line break the caller wrote (EP-41).
#[tokio::test]
async fn a_hostile_query_stays_inside_the_manifest() {
    for hostile in [
        "type=AirQualityObserved&note=%0D%0AX-Injected:%201",
        "type=AirQualityObserved&note=%22%2C%22rows%22%3A9999",
        "type=AirQualityObserved&note=%00",
        "type=AirQualityObserved&note=../../etc/passwd",
        "type=AirQualityObserved&note=%E2%80%A8line%E2%80%A9separator",
    ] {
        let broker = BrokerStub::start(vec![json!([station("station-01", 34.2)])]).await;
        let answer = get(
            gateway(&broker.url, endpoint(None, &[])),
            &format!("/api/endpoint/{SLUG}/file.zip?{hostile}"),
        )
        .await;

        assert_eq!(answer.status, StatusCode::OK, "{hostile}");
        for (name, value) in answer.headers.iter() {
            let text = String::from_utf8_lossy(value.as_bytes()).into_owned();
            assert!(
                !text.contains('\n') && !text.contains('\r'),
                "{name}: {text}"
            );
            assert!(!text.contains("X-Injected"), "{name}: {text}");
        }
        let members = stored(&answer.body);
        let manifest: Value =
            serde_json::from_slice(member(&members, "manifest.json")).expect("a manifest");
        assert_eq!(manifest["rows"], json!(1), "{hostile}: {manifest}");
        assert_eq!(manifest["endpoint"], json!(SLUG), "{hostile}");
    }
}

/// The filename a browser saves under is the slug and a date and nothing else: no quote to close
/// the header with, no line break, no path.
#[tokio::test]
async fn the_saved_filename_is_the_slug_and_a_date() {
    let broker = BrokerStub::start(vec![json!([station("station-01", 34.2)])]).await;
    let answer = get(
        gateway(&broker.url, endpoint(None, &[])),
        &format!("/api/endpoint/{SLUG}/file.zip"),
    )
    .await;

    let disposition = answer
        .headers
        .get(axum::http::header::CONTENT_DISPOSITION)
        .and_then(|value| value.to_str().ok())
        .expect("a content-disposition header");
    let name = disposition
        .trim_start_matches("attachment; filename=\"")
        .trim_end_matches('"');
    assert!(name.starts_with(SLUG), "{disposition}");
    assert!(name.ends_with(".zip"), "{disposition}");
    assert!(
        name.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.'),
        "{disposition}"
    );
}

/// A caller no grant names gets no archive at all, and the broker is not asked.
#[tokio::test]
async fn a_caller_no_grant_names_gets_no_archive() {
    let broker = BrokerStub::start(vec![json!([station("station-01", 34.2)])]).await;
    let mut ungranted = endpoint(None, &[]);
    ungranted.policies = Vec::new();

    let answer = get(
        gateway(&broker.url, ungranted),
        &format!("/api/endpoint/{SLUG}/file.zip"),
    )
    .await;

    assert_eq!(answer.status, StatusCode::FORBIDDEN);
    assert!(
        !answer.body.starts_with(b"PK"),
        "a refusal is not an archive"
    );
    assert!(broker.hops().is_empty(), "{:?}", broker.hops());
}

/// An empty answer is an empty bundle, not a refusal: the files are there, the manifest counts
/// nothing, and a colleague can still see what was asked.
#[tokio::test]
async fn an_empty_answer_is_still_a_bundle() {
    let broker = BrokerStub::start(vec![json!([])]).await;
    let answer = get(
        gateway(&broker.url, endpoint(None, &[])),
        &format!("/api/endpoint/{SLUG}/file.zip?type=AirQualityObserved"),
    )
    .await;

    assert_eq!(answer.status, StatusCode::OK);
    assert!(answer.body.starts_with(b"PK"));
    let members = stored(&answer.body);
    let manifest: Value =
        serde_json::from_slice(member(&members, "manifest.json")).expect("a manifest");
    assert_eq!(manifest["rows"], json!(0), "{manifest}");
    let entities: Value =
        serde_json::from_slice(member(&members, "data/entities.jsonld")).expect("json-ld");
    assert_eq!(entities, json!([]));
}

/// An endpoint that publishes no model has no schema directory, and the bundle is still a bundle:
/// the data and the record are what it was asked for.
#[tokio::test]
async fn an_endpoint_without_a_model_bundles_without_a_schema_directory() {
    let broker = BrokerStub::start(vec![json!([station("station-01", 34.2)])]).await;
    let mut modelless = endpoint(None, &[]);
    modelless.models = Vec::new();

    let answer = get(
        gateway(&broker.url, modelless),
        &format!("/api/endpoint/{SLUG}/file.zip"),
    )
    .await;

    assert_eq!(answer.status, StatusCode::OK);
    let members = stored(&answer.body);
    assert!(
        members.iter().all(|(path, _)| !path.contains("/schema/")),
        "{:?}",
        members.iter().map(|(path, _)| path).collect::<Vec<_>>()
    );
    let _ = member(&members, "data/entities.jsonld");
    let _ = member(&members, "dcat.jsonld");
}

/// The ceiling is the endpoint's, whichever of the two is set and whichever is lower: the download
/// is refused, nothing is truncated, and no archive comes out with the refusal (EP-44).
#[tokio::test]
async fn either_ceiling_refuses_and_neither_truncates() {
    for limits in [
        FileLimits {
            max_file_rows: Some(1),
            max_file_bytes: None,
        },
        FileLimits {
            max_file_rows: None,
            max_file_bytes: Some(32),
        },
        FileLimits {
            max_file_rows: Some(1),
            max_file_bytes: Some(32),
        },
        FileLimits {
            max_file_rows: Some(0),
            max_file_bytes: None,
        },
    ] {
        let broker = BrokerStub::start(vec![json!([
            station("station-01", 1.0),
            station("station-02", 2.0),
            station("station-03", 3.0)
        ])])
        .await;
        let answer = get(
            gateway(&broker.url, endpoint(Some(limits.clone()), &[])),
            &format!("/api/endpoint/{SLUG}/file.zip"),
        )
        .await;

        assert_eq!(answer.status, StatusCode::PAYLOAD_TOO_LARGE, "{limits:?}");
        assert!(!answer.body.starts_with(b"PK"), "{limits:?}");
        let text = String::from_utf8_lossy(&answer.body);
        assert!(!text.contains("station-01"), "{limits:?}: {text}");
    }
}

/// The archive is what this route answers, and the caller's `Accept` decides nothing: the broker is
/// asked for JSON whatever the browser said it wanted.
#[tokio::test]
async fn the_answer_is_a_zip_whatever_the_caller_accepts() {
    for accept in ["application/zip", "text/csv", "*/*", "application/pdf", ""] {
        let broker = BrokerStub::start(vec![json!([station("station-01", 34.2)])]).await;
        let request = HttpRequest::builder()
            .uri(format!("/api/endpoint/{SLUG}/file.zip"))
            .header(axum::http::header::ACCEPT, accept)
            .body(Body::empty())
            .expect("a request");
        let answer = download(gateway(&broker.url, endpoint(None, &[])), request).await;

        assert_eq!(answer.status, StatusCode::OK, "{accept:?}");
        assert_eq!(
            answer
                .headers
                .get(axum::http::header::CONTENT_TYPE)
                .and_then(|value| value.to_str().ok()),
            Some("application/zip"),
            "{accept:?}"
        );
        assert!(
            broker
                .hops()
                .iter()
                .all(|hop| hop.accept == "application/json"),
            "{accept:?}: {:?}",
            broker.hops()
        );
    }
}

/// A tenant the caller forged never reaches the broker, and the answer never carries one back
/// (GW25, SP-05).
#[tokio::test]
async fn a_forged_tenant_neither_reaches_the_broker_nor_comes_back() {
    let broker = BrokerStub::start(vec![json!([station("station-01", 34.2)])]).await;
    let request = HttpRequest::builder()
        .uri(format!("/api/endpoint/{SLUG}/file.zip"))
        .header("NGSILD-Tenant", "somebody-elses-space")
        .body(Body::empty())
        .expect("a request");

    let answer = download(gateway(&broker.url, endpoint(None, &[])), request).await;

    assert_eq!(answer.status, StatusCode::OK);
    assert!(
        broker
            .hops()
            .iter()
            .all(|hop| !hop.forged && hop.tenant == "ovzdusie"),
        "{:?}",
        broker.hops()
    );
    assert!(
        answer.headers.get("ngsild-tenant").is_none(),
        "{:?}",
        answer.headers
    );
}

/// A download is one caller's answer, so it says so: no shared cache may hand this archive to the
/// next caller of the same URL (R9, T-2261).
#[tokio::test]
async fn the_archive_is_never_stored_in_a_shared_cache() {
    let broker = BrokerStub::start(vec![json!([station("station-01", 34.2)])]).await;
    let answer = get(
        gateway(&broker.url, endpoint(None, &[])),
        &format!("/api/endpoint/{SLUG}/file.zip"),
    )
    .await;

    let cache = answer
        .headers
        .get(axum::http::header::CACHE_CONTROL)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default();
    assert!(cache.contains("private"), "{cache}");
    assert!(
        cache.contains("no-store") || cache.contains("no-cache"),
        "{cache}"
    );
}

/// Only a read reaches this route: a write method is not an archive and not a 500.
#[tokio::test]
async fn a_write_method_on_the_bundle_is_refused() {
    for method in ["POST", "PUT", "PATCH", "DELETE"] {
        let broker = BrokerStub::start(vec![json!([station("station-01", 34.2)])]).await;
        let request = HttpRequest::builder()
            .method(method)
            .uri(format!("/api/endpoint/{SLUG}/file.zip"))
            .body(Body::empty())
            .expect("a request");
        let answer = download(gateway(&broker.url, endpoint(None, &[])), request).await;

        assert!(
            answer.status == StatusCode::METHOD_NOT_ALLOWED
                || answer.status == StatusCode::NOT_FOUND,
            "{method}: {}",
            answer.status
        );
        assert!(broker.hops().is_empty(), "{method}: {:?}", broker.hops());
    }
}
