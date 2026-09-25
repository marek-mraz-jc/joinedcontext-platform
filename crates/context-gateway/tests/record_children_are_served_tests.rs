//! Every child a record advertises is a child the router serves (T-2373, T-2382; SP-04, SP-10,
//! EP-41, EP-55).
//!
//! Contract, in one sentence: a URL that appears in the DCAT-AP record of a space or an
//! endpoint — in any of its three serializations — answers something other than `404` when it
//! is asked for through the very router that served the record.
//!
//! A record is a promise a harvester follows without a person in the loop. The space record
//! advertised `schema/` as a `dcat:DataService` and its HTML page linked `dump/` and `access`;
//! none of the three was routed. The endpoint record advertised `file.json` for every endpoint
//! whose manifest enabled the `json` representation, and that was not routed either. Nothing
//! leaked — a `404` says nothing — so every test stayed green while three published catalogues
//! pointed at nothing.
//!
//! The first case below is the one most likely to be red: it walks the space record and asks
//! for every service IRI it finds, rather than for the paths this file happens to know.

mod common;

use axum::body::Body;
use axum::http::{Request as HttpRequest, StatusCode};
use common::BrokerStub;
use context_gateway::app::{router, Gateway};
use context_gateway::pdp::PolicyPdp;
use context_gateway::proxy::Broker;
use context_gateway::resolver::{Endpoint, Model, Space};
use jc_core::kinds::{Audience, PolicySpec, Representation};
use serde_json::{json, Value};
use std::sync::Arc;
use tower::ServiceExt;

const SLUG: &str = "zt4qm7ge2xdv6ksb3ncf5arw2y";
const SPACE: &str = "ovzdusie";
const MCPLESS: &str = "doprava";
const HOST: &str = "https://bb.example.sk";

fn public_grant(space: &str) -> Vec<PolicySpec> {
    vec![serde_norway::from_str(&format!(
        r#"contextSpaceRef: {space}
assigner: did:web:banskabystrica.sk
assignee: {{ kind: role, id: public }}
operations: [queryEntity, retrieveEntity]
information:
  - entities:
      - type: AirQualityObserved
    propertyNames: [pm10, location]
"#
    ))
    .expect("the policy spec parses")]
}

/// One model, so the schema surface the record advertises has something to answer with.
fn air_quality() -> Model {
    Model {
        name: "bb-air-quality".to_owned(),
        version: "1.4.0".to_owned(),
        major: 1,
        classes: vec!["AirQualityObserved".to_owned()],
        json_schema: Some(json!({
            "$schema": "http://json-schema.org/draft-07/schema#",
            "$defs": {
                "AirQualityObserved": {
                    "type": "object",
                    "properties": {
                        "id": { "type": "string" },
                        "type": { "const": "AirQualityObserved" },
                        "pm10": { "type": "number" },
                        "operatorPhone": { "type": "string" },
                    },
                },
            },
        })),
        context: Some(json!({
            "@context": { "pm10": "https://bb.example.sk/schema/air-quality/pm10" }
        })),
    }
}

/// An endpoint that enables every representation an Endpoint manifest can name, which is the
/// fixture this file needs: a record only advertises what the manifest enabled.
fn endpoint(slug: &str, space: &str, representations: Vec<Representation>) -> Endpoint {
    Endpoint {
        declared_types: None,
        roles: Default::default(),
        slug: slug.to_owned(),
        title: Default::default(),
        description: Default::default(),
        space: space.to_owned(),
        project: space.to_owned(),
        audience: Audience::Public,
        allowed_projects: Vec::new(),
        representations,
        rate_limit: None,
        file_limits: None,
        hidden_attributes: Default::default(),
        projection: None,
        base_path: format!("/api/endpoint/{slug}"),
        models: vec![air_quality()],
        view_mapping: None,
        policies: public_grant(space),
    }
}

fn every_representation() -> Vec<Representation> {
    vec![
        Representation::NgsiLd,
        Representation::Mcp,
        Representation::GeoJson,
        Representation::Csv,
        Representation::Xlsx,
        Representation::Json,
        Representation::Zip,
        Representation::OgcFeatures,
        Representation::Sta,
    ]
}

fn space(name: &str, representations: Vec<Representation>) -> Space {
    Space {
        endpoint: Arc::new(Endpoint {
            roles: Default::default(),
            base_path: format!("/cs/{name}"),
            ..endpoint(name, name, representations)
        }),
        title: Default::default(),
        description: Default::default(),
        is_sandbox: false,
        default_locale: Some("sk".to_owned()),
    }
}

/// A gateway that knows its own public URL, because a record without one carries relative
/// identifiers and this file follows the URLs it publishes.
fn published(broker: &str) -> Gateway {
    let mut gateway = Gateway::new(
        Broker::new(broker),
        Box::new(PolicyPdp),
        "banskabystrica.sk",
    );
    gateway.public_url = Some(HOST.to_owned());
    gateway
}

fn gateway(broker: &str) -> axum::Router {
    router(Arc::new(
        published(broker)
            .serve([endpoint(SLUG, SPACE, every_representation())])
            .serve_spaces([
                space(SPACE, every_representation()),
                space(MCPLESS, vec![Representation::NgsiLd]),
            ]),
    ))
}

fn station() -> Value {
    json!({
        "id": "urn:ngsi-ld:AirQualityObserved:banskabystrica.sk:ovzdusie:station-01",
        "type": "AirQualityObserved",
        "pm10": { "type": "Property", "value": 34.2 },
        "operatorPhone": { "type": "Property", "value": "+421 900 000 000" },
        "location": { "type": "GeoProperty", "value": { "type": "Point", "coordinates": [19.15, 48.73] } }
    })
}

async fn call(app: &axum::Router, path: &str, accept: Option<&str>) -> (StatusCode, String) {
    let mut builder = HttpRequest::builder().uri(path);
    if let Some(accept) = accept {
        builder = builder.header(axum::http::header::ACCEPT, accept);
    }
    let response = app
        .clone()
        .oneshot(builder.body(Body::empty()).expect("a request"))
        .await
        .expect("the gateway answers");
    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), 4 * 1024 * 1024)
        .await
        .expect("a readable body");
    (status, String::from_utf8_lossy(&body).into_owned())
}

/// Every absolute URL of this gateway that a document mentions, whatever serialization it is
/// written in: the JSON-LD `@id`s and `dcat:endpointURL`s, the Turtle `<…>` IRIs and the HTML
/// `href`s all spell a URL the same way.
fn urls_in(document: &str) -> Vec<String> {
    let mut found = Vec::new();
    let mut rest = document;
    while let Some(start) = rest.find(HOST) {
        let tail = &rest[start..];
        let end = tail
            .find(['"', '<', '>', ' ', ',', ';', '\n'])
            .unwrap_or(tail.len());
        let url = tail[..end].trim_end_matches('.').to_owned();
        if url.len() > HOST.len() {
            found.push(url);
        }
        rest = &tail[end.max(1)..];
    }
    found.sort();
    found.dedup();
    found
}

fn path_of(url: &str) -> String {
    url.strip_prefix(HOST).unwrap_or(url).to_owned()
}

/// Whether a path is a service base rather than a document.
///
/// DCAT's `dcat:endpointURL` is "the root location or primary endpoint of the service", and
/// CIM 009 defines no document at the root of its resource tree — `entities`, `types` and the
/// rest live under it. So `ngsi-ld/v1/` is the one advertised URL that legitimately answers
/// nothing itself, and the test asks for a resource inside it instead
/// ([`the_ngsi_ld_base_is_a_tree_rather_than_a_document`]).
fn is_service_base(path: &str) -> bool {
    path.ends_with("/ngsi-ld/v1/")
}

/// SP-10, SP-04: the case this file exists for. Whatever the space record names, the router
/// answers — in all three serializations, because a triple store harvests the Turtle and a
/// person clicks the HTML.
#[tokio::test]
async fn every_child_a_space_record_names_is_a_child_the_router_serves() {
    let broker = BrokerStub::start(vec![json!([station()])]).await;
    let app = gateway(&broker.url);

    for accept in [
        Some("application/ld+json"),
        Some("text/turtle"),
        Some("text/html"),
    ] {
        let (status, record) = call(&app, &format!("/cs/{SPACE}"), accept).await;
        assert_eq!(status, StatusCode::OK, "{accept:?}: {record}");

        let urls = urls_in(&record);
        assert!(
            urls.len() > 1,
            "{accept:?} advertised nothing but the space itself: {record}"
        );
        for url in urls {
            let path = path_of(&url);
            if path == format!("/cs/{SPACE}") {
                continue;
            }
            if is_service_base(&path) {
                continue;
            }
            let (status, body) = call(&app, &path, None).await;
            assert_ne!(
                status,
                StatusCode::NOT_FOUND,
                "{accept:?} advertises {path}, which the router does not serve: {body}"
            );
        }
    }
}

/// The three serializations are one record, so they name one set of children. They drifted
/// before: the Turtle carried the NGSI-LD service alone while the JSON-LD carried three and
/// the HTML page linked five.
#[tokio::test]
async fn the_three_serializations_of_a_space_record_name_the_same_children() {
    let broker = BrokerStub::start(vec![json!([station()])]).await;
    let app = gateway(&broker.url);

    let mut seen: Vec<(String, Vec<String>)> = Vec::new();
    for accept in ["application/ld+json", "text/turtle", "text/html"] {
        let (_, record) = call(&app, &format!("/cs/{SPACE}"), Some(accept)).await;
        let children: Vec<String> = urls_in(&record)
            .into_iter()
            .map(|url| path_of(&url))
            .filter(|path| *path != format!("/cs/{SPACE}"))
            .collect();
        seen.push((accept.to_owned(), children));
    }

    let (first_name, first) = &seen[0];
    for (name, children) in &seen[1..] {
        assert_eq!(
            children, first,
            "{name} names other children than {first_name}"
        );
    }
    assert_eq!(
        first,
        &vec![
            format!("/cs/{SPACE}/mcp"),
            format!("/cs/{SPACE}/ngsi-ld/v1/"),
            format!("/cs/{SPACE}/schema/index.json"),
        ]
    );
}

/// A record names what this space serves, not what a space can serve: an MCP link on a space
/// whose manifest does not enable MCP is a `404` waiting for whoever follows it.
#[tokio::test]
async fn a_space_that_does_not_enable_mcp_does_not_advertise_it() {
    let broker = BrokerStub::start(vec![json!([station()])]).await;
    let app = gateway(&broker.url);

    for accept in ["application/ld+json", "text/turtle", "text/html"] {
        let (status, record) = call(&app, &format!("/cs/{MCPLESS}"), Some(accept)).await;
        assert_eq!(status, StatusCode::OK, "{accept}: {record}");
        assert!(
            !record.contains(&format!("/cs/{MCPLESS}/mcp")),
            "{accept} advertises an MCP instance this space does not serve: {record}"
        );
    }
}

/// SP-04 permits `schema/` as a child of a space, and SP-03 makes the space surface the
/// endpoint surface under a name: the two prefixes answer the same schema catalogue.
#[tokio::test]
async fn the_schema_surface_answers_under_both_prefixes() {
    let broker = BrokerStub::start(vec![json!([station()])]).await;
    let app = gateway(&broker.url);

    let (space_status, space_index) =
        call(&app, &format!("/cs/{SPACE}/schema/index.json"), None).await;
    let (endpoint_status, endpoint_index) = call(
        &app,
        &format!("/api/endpoint/{SLUG}/schema/index.json"),
        None,
    )
    .await;
    assert_eq!(space_status, StatusCode::OK, "{space_index}");
    assert_eq!(endpoint_status, StatusCode::OK, "{endpoint_index}");

    let space_document: Value = serde_json::from_str(&space_index).expect("a JSON catalogue");
    assert_eq!(space_document["models"][0]["name"], json!("bb-air-quality"));
    assert_eq!(space_document["models"][0]["version"], json!(1));

    // And one document of it, by both spellings of the artifact name.
    for artifact in ["model.schema.json", "json-schema", "shacl"] {
        let (status, body) = call(&app, &format!("/cs/{SPACE}/schema/v1/{artifact}"), None).await;
        assert_eq!(status, StatusCode::OK, "{artifact}: {body}");
        assert!(
            !body.contains("operatorPhone"),
            "{artifact} carries a slot the public grant does not name: {body}"
        );
    }

    // A major nobody published, and a word that is no artifact, are both 404 — the same
    // answer an unknown path gets, on both prefixes.
    for path in [
        format!("/cs/{SPACE}/schema/v9/model.schema.json"),
        format!("/cs/{SPACE}/schema/1/model.schema.json"),
        format!("/cs/{SPACE}/schema/v1/model.exe"),
    ] {
        let (status, body) = call(&app, &path, None).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{path}: {body}");
    }
}

/// EP-41, T-2382: the endpoint record advertises one distribution per enabled representation,
/// and `file.json` was the one nobody routed.
#[tokio::test]
async fn every_distribution_an_endpoint_record_names_is_one_the_router_serves() {
    let broker = BrokerStub::start(vec![json!([station()]), json!([])]).await;
    let app = gateway(&broker.url);

    for accept in [
        Some("application/ld+json"),
        Some("text/turtle"),
        Some("text/html"),
    ] {
        let (status, record) = call(&app, &format!("/api/endpoint/{SLUG}"), accept).await;
        assert_eq!(status, StatusCode::OK, "{accept:?}: {record}");

        for url in urls_in(&record) {
            let path = path_of(&url);
            if path == format!("/api/endpoint/{SLUG}") {
                continue;
            }
            if is_service_base(&path) {
                continue;
            }
            let (status, body) = call(&app, &path, None).await;
            assert_ne!(
                status,
                StatusCode::NOT_FOUND,
                "{accept:?} advertises {path}, which the router does not serve: {body}"
            );
        }
    }
}

/// The download the record has always promised: the projected entities, as an attachment
/// named after the endpoint (EP-41, EP-43).
#[tokio::test]
async fn file_json_serves_the_projected_entities_as_an_attachment() {
    let broker = BrokerStub::start(vec![json!([station()]), json!([])]).await;
    let app = gateway(&broker.url);

    let response = app
        .clone()
        .oneshot(
            HttpRequest::builder()
                .uri(format!(
                    "/api/endpoint/{SLUG}/file.json?type=AirQualityObserved"
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
        Some("application/json")
    );
    assert_eq!(
        response
            .headers()
            .get(axum::http::header::CONTENT_DISPOSITION)
            .and_then(|value| value.to_str().ok()),
        Some(format!("attachment; filename=\"{SLUG}.json\"").as_str())
    );

    let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .expect("a readable body");
    let entities: Value = serde_json::from_slice(&body).expect("a JSON array");
    assert_eq!(entities[0]["pm10"]["value"], json!(34.2));
    assert!(
        entities[0].get("operatorPhone").is_none(),
        "the download served an attribute the grant does not name: {entities}"
    );
}

/// A representation the manifest does not enable is not served, whichever surface asks for
/// it: `admit` refuses before a broker request exists (EP-03, R20).
#[tokio::test]
async fn file_json_is_not_served_by_an_endpoint_that_does_not_enable_it() {
    let broker = BrokerStub::start(vec![json!([station()])]).await;
    let app = router(Arc::new(published(&broker.url).serve([endpoint(
        SLUG,
        SPACE,
        vec![Representation::NgsiLd],
    )])));

    let (status, body) = call(&app, &format!("/api/endpoint/{SLUG}/file.json"), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert!(
        !body.contains("AirQualityObserved"),
        "the refusal carried data: {body}"
    );
}

/// EP-44: the endpoint's own byte ceiling bounds this download as it bounds the tabular ones.
/// Half a JSON array is not a smaller answer, so the whole download is refused.
#[tokio::test]
async fn a_json_download_past_the_endpoints_byte_limit_is_refused_whole() {
    let broker = BrokerStub::start(vec![json!([station()]), json!([])]).await;
    let limited = Endpoint {
        roles: Default::default(),
        file_limits: Some(
            serde_norway::from_str("maxFileBytes: 32").expect("the file limits parse"),
        ),
        ..endpoint(SLUG, SPACE, every_representation())
    };
    let app = router(Arc::new(published(&broker.url).serve([limited])));

    let (status, body) = call(
        &app,
        &format!("/api/endpoint/{SLUG}/file.json?type=AirQualityObserved"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE, "{body}");
    assert!(
        !body.contains("station-01"),
        "the refusal carried the data it refused to serve: {body}"
    );
}

/// What the one exempted URL really is: the root of the CIM 009 resource tree, which defines
/// no document of its own, while everything the tree does define is routed under it. A client
/// that stored the advertised base and appends `entities` — which is what an NGSI-LD client
/// does — reaches the data, on both prefixes (SP-03, EP-01).
#[tokio::test]
async fn the_ngsi_ld_base_is_a_tree_rather_than_a_document() {
    let broker = BrokerStub::start(vec![json!([station()]), json!([station()])]).await;
    let app = gateway(&broker.url);

    for base in [
        format!("/cs/{SPACE}/ngsi-ld/v1"),
        format!("/api/endpoint/{SLUG}/ngsi-ld/v1"),
    ] {
        let (status, body) = call(
            &app,
            &format!("{base}/entities?type=AirQualityObserved"),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{base}/entities: {body}");
        assert!(body.contains("station-01"), "{base}/entities: {body}");
    }
}
