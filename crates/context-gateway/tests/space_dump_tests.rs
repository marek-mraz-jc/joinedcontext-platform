//! `GET /cs/{space}/dump/`: the space's dump, generated per request (T-2391; SP-04, SP-10, SP-13,
//! EP-41).
//!
//! Contract, in one sentence: the dump is the `file.zip` bundle over everything in the space the
//! caller's grants read, so no member of the archive carries an attribute or a type the caller
//! could not read through `ngsi-ld/v1/`; a caller whose grants reach nothing gets the `404` of a
//! space that does not exist; past the ceiling the whole dump is refused; and the record names
//! the dump only when the space serves it.
//!
//! A dump is the widest read the platform offers, so the projection is asserted on the archive's
//! own members, not on the status code.

mod common;

use axum::body::Body;
use axum::http::{Request as HttpRequest, StatusCode};
use common::BrokerStub;
use context_gateway::app::{router, Gateway};
use context_gateway::pdp::PolicyPdp;
use context_gateway::proxy::Broker;
use context_gateway::resolver::{Endpoint, Space};
use jc_core::kinds::{Audience, FileLimits, PolicySpec, Representation};
use serde_json::{json, Value};
use std::io::Read;
use std::sync::Arc;
use tower::ServiceExt;

const SPACE: &str = "ovzdusie";

/// The public may read `pm10` and `location` of an air-quality station, and nothing else.
fn public_grant() -> PolicySpec {
    serde_norway::from_str(
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
    .expect("the policy spec parses")
}

/// Only the city's stewards may read the space: an anonymous caller holds no grant in it.
fn stewards_only() -> PolicySpec {
    serde_norway::from_str(
        r#"contextSpaceRef: ovzdusie
assigner: did:web:banskabystrica.sk
assignee: { kind: role, id: steward }
operations: [queryEntity, retrieveEntity]
"#,
    )
    .expect("the policy spec parses")
}

fn space(
    representations: Vec<Representation>,
    policies: Vec<PolicySpec>,
    limits: Option<FileLimits>,
) -> Space {
    Space {
        endpoint: Arc::new(Endpoint {
            declared_types: None,
            roles: Default::default(),
            slug: SPACE.to_owned(),
            title: Default::default(),
            description: Default::default(),
            space: SPACE.to_owned(),
            project: SPACE.to_owned(),
            audience: Audience::Public,
            allowed_projects: Vec::new(),
            representations,
            rate_limit: None,
            file_limits: limits,
            hidden_attributes: Default::default(),
            projection: None,
            base_path: format!("/cs/{SPACE}"),
            models: Vec::new(),
            view_mapping: None,
            catalog: None,
            policies,
        }),
        title: Default::default(),
        description: Default::default(),
        is_sandbox: false,
        default_locale: None,
    }
}

fn dumping() -> Vec<Representation> {
    vec![
        Representation::NgsiLd,
        Representation::Mcp,
        Representation::Zip,
    ]
}

fn gateway(broker: &str, space: Space) -> axum::Router {
    router(Arc::new(
        Gateway::new(
            Broker::new(broker),
            Box::new(PolicyPdp),
            "banskabystrica.sk",
        )
        .serve_spaces([space]),
    ))
}

fn station(local: &str) -> Value {
    json!({
        "id": format!("urn:ngsi-ld:AirQualityObserved:banskabystrica.sk:ovzdusie:{local}"),
        "type": "AirQualityObserved",
        "pm10": { "type": "Property", "value": 34.2 },
        "operatorPhone": { "type": "Property", "value": "+421 900 000 000" },
        "location": { "type": "GeoProperty", "value": { "type": "Point", "coordinates": [19.15, 48.73] } }
    })
}

fn invoice() -> Value {
    json!({
        "id": "urn:ngsi-ld:Invoice:banskabystrica.sk:ovzdusie:invoice-1",
        "type": "Invoice",
        "amount": { "type": "Property", "value": 4200 }
    })
}

async fn get(app: axum::Router, path: &str) -> (StatusCode, axum::http::HeaderMap, Vec<u8>) {
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
    let headers = response.headers().clone();
    let body = axum::body::to_bytes(response.into_body(), 64 * 1024 * 1024)
        .await
        .expect("a readable body");
    (status, headers, body.to_vec())
}

fn members(archive: &[u8]) -> Vec<(String, String)> {
    let mut zip =
        zip::ZipArchive::new(std::io::Cursor::new(archive.to_vec())).expect("a zip archive");
    (0..zip.len())
        .map(|index| {
            let mut member = zip.by_index(index).expect("a member");
            let mut body = String::new();
            member.read_to_string(&mut body).expect("a text member");
            (member.name().to_owned(), body)
        })
        .collect()
}

#[tokio::test]
async fn the_dump_holds_what_the_callers_grants_read_and_nothing_else() {
    let broker = BrokerStub::start(vec![json!([station("station-01"), invoice()])]).await;
    let app = gateway(&broker.url, space(dumping(), vec![public_grant()], None));
    let (status, headers, body) = get(app, &format!("/cs/{SPACE}/dump/")).await;
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
    assert_eq!(headers["content-type"], "application/zip");
    let disposition = headers["content-disposition"].to_str().expect("ascii");
    assert!(
        disposition.starts_with(&format!("attachment; filename=\"{SPACE}-")),
        "{disposition}"
    );

    let members = members(&body);
    let entities = members
        .iter()
        .find(|(name, _)| name.ends_with("data/entities.jsonld"))
        .map(|(_, body)| body.as_str())
        .expect("the entities are in the dump");
    assert!(
        entities.contains("station-01") && entities.contains("pm10"),
        "{entities}"
    );
    // Every member, not only the data: the projection holds in each shape and document.
    for (name, text) in &members {
        assert!(
            !text.contains("operatorPhone"),
            "{name} carries an attribute no grant reads"
        );
        assert!(!text.contains("+421 900"), "{name} carries its value");
        assert!(
            !text.contains("Invoice"),
            "{name} carries a type no grant reads"
        );
        assert!(!text.contains("4200"), "{name} carries its value");
    }
}

#[tokio::test]
async fn a_caller_with_no_grant_gets_the_404_of_a_space_that_does_not_exist() {
    let broker = BrokerStub::start(vec![json!([station("station-01")])]).await;
    let app = gateway(&broker.url, space(dumping(), vec![stewards_only()], None));
    let (hidden, _, hidden_body) = get(app.clone(), &format!("/cs/{SPACE}/dump/")).await;
    let (missing, _, missing_body) = get(app, "/cs/no-such-space/dump/").await;
    assert_eq!(hidden, StatusCode::NOT_FOUND);
    assert_eq!(missing, StatusCode::NOT_FOUND);
    assert_eq!(
        hidden_body, missing_body,
        "the two answers tell nothing apart (SP-06)"
    );
    assert!(
        broker.hops().is_empty(),
        "nothing was read for a caller who may read nothing"
    );
}

#[tokio::test]
async fn past_the_ceiling_the_whole_dump_is_refused_rather_than_truncated() {
    for limits in [
        FileLimits {
            max_file_rows: Some(1),
            max_file_bytes: None,
        },
        FileLimits {
            max_file_rows: None,
            max_file_bytes: Some(32),
        },
    ] {
        let broker =
            BrokerStub::start(vec![json!([station("station-01"), station("station-02")])]).await;
        let app = gateway(
            &broker.url,
            space(dumping(), vec![public_grant()], Some(limits.clone())),
        );
        let (status, headers, body) = get(app, &format!("/cs/{SPACE}/dump/")).await;
        assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE, "{limits:?}");
        assert!(
            headers["content-type"]
                .to_str()
                .expect("ascii")
                .starts_with("application/problem+json"),
            "{limits:?}"
        );
        assert!(
            !body.starts_with(b"PK"),
            "no partial archive for {limits:?}"
        );
    }
}

#[tokio::test]
async fn the_record_names_the_dump_as_a_distribution_only_when_the_space_serves_it() {
    let broker = BrokerStub::start(Vec::new()).await;
    let served = gateway(&broker.url, space(dumping(), vec![public_grant()], None));
    let (status, _, body) = get(served.clone(), &format!("/cs/{SPACE}")).await;
    assert_eq!(status, StatusCode::OK);
    let record: Value = serde_json::from_slice(&body).expect("a JSON-LD record");
    let distribution = &record["dcat:distribution"][0];
    assert_eq!(distribution["@type"], "dcat:Distribution");
    assert_eq!(
        distribution["dcat:downloadURL"],
        format!("/cs/{SPACE}/dump/")
    );
    assert_eq!(distribution["dcat:mediaType"], "application/zip");
    let services = record["dcat:service"].to_string();
    assert!(
        !services.contains("dump"),
        "a download is not a data service: {services}"
    );

    let turtle_request = HttpRequest::builder()
        .uri(format!("/cs/{SPACE}"))
        .header(axum::http::header::ACCEPT, "text/turtle")
        .body(Body::empty())
        .expect("a request");
    let turtle = served.oneshot(turtle_request).await.expect("an answer");
    let turtle = axum::body::to_bytes(turtle.into_body(), 1024 * 1024)
        .await
        .expect("a body");
    let turtle = String::from_utf8_lossy(&turtle);
    assert!(
        turtle.contains(&format!("</cs/{SPACE}/dump/> a dcat:Distribution")),
        "{turtle}"
    );

    let without = gateway(
        &broker.url,
        space(
            vec![Representation::NgsiLd, Representation::Mcp],
            vec![public_grant()],
            None,
        ),
    );
    let (_, _, body) = get(without.clone(), &format!("/cs/{SPACE}")).await;
    assert!(
        !String::from_utf8_lossy(&body).contains("dump"),
        "a space that does not serve its dump does not name one"
    );
    let (status, _, _) = get(without, &format!("/cs/{SPACE}/dump/")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}
