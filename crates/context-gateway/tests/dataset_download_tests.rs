//! A file download of an endpoint whose grant narrows nothing (T-0379, EP-07, EP-09).
//!
//! `DEMO.md` offered `file.geojson` with no query string, and on the dev cluster that was a
//! `400`: the demo's `public-read` Policy names no entity type, so the gateway forwarded a
//! query that selected nothing and a conformant broker refused it under CIM 009 5.7.2. Every
//! test that covered the download used a grant that *did* name a type, so the case the demo
//! actually exercises had no test at all.
//!
//! What a download means is the fix. A file names a dataset, not a query, so when
//! nothing else selects, the selector is every type the space holds. It can only narrow: the
//! broker's view decides what this caller may see and leaves the rest out (T-3651). The one
//! file left after T-4597 is `file.csv`, so the cases are told on it.

mod common;

use axum::body::Body;
use axum::http::{Request as HttpRequest, StatusCode};
use common::{BrokerStub, View};
use context_gateway::app::{router, Gateway};
use context_gateway::pdp::PolicyPdp;
use context_gateway::proxy::Broker;
use context_gateway::resolver::Endpoint;
use jc_core::kinds::{Audience, Representation};
use serde_json::{json, Value};
use std::collections::BTreeSet;
use std::sync::Arc;
use tower::ServiceExt;

const SLUG: &str = "mluyob4nz52lok3ssk7pgn5vwt";

/// The dev seed's own grant, word for word: anonymous callers may read, and nothing narrows
/// what they read. This is the shape that produced the `400`.
fn unrestricted_endpoint() -> Endpoint {
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
        representations: vec![Representation::NgsiLd, Representation::Csv],
        rate_limit: None,
        creates: None,
        file_limits: None,
        // Nothing narrows what the anonymous caller reads, which is the whole point of this
        // fixture: the endpoint hides no attribute either (EP-61).
        hidden_attributes: BTreeSet::new(),
        projection: None,
        base_path: format!("/api/endpoint/{SLUG}"),
        models: Vec::new(),
        view_mapping: None,
        catalog: None,
        deprecation: None,
        feeds: Vec::new(),
        calendars: Vec::new(),
        policy_names: Vec::new(),
        manifest_name: String::new(),
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

fn station() -> Value {
    json!({
        "id": "urn:ngsi-ld:AirQualityObserved:banskabystrica.sk:ovzdusie:station-01",
        "type": "AirQualityObserved",
        "pm10": { "type": "Property", "value": 34.2 },
        "location": {
            "type": "GeoProperty",
            "value": { "type": "Point", "coordinates": [19.146, 48.736] }
        }
    })
}

async fn get(app: axum::Router, path: &str) -> (StatusCode, String) {
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
    let body = axum::body::to_bytes(response.into_body(), 16 * 1024 * 1024)
        .await
        .expect("a readable body");
    (status, String::from_utf8_lossy(&body).into_owned())
}

/// The URL `DEMO.md` prints. Before the fix this was the broker's `400`, because the gateway
/// forwarded `/entities?` with nothing in it.
#[tokio::test]
async fn a_bare_download_selects_the_types_the_space_holds() {
    let broker = BrokerStub::start(vec![json!([station()])]).await;
    let (status, body) = get(
        gateway(&broker.url, unrestricted_endpoint()),
        &format!("/api/endpoint/{SLUG}/file.csv"),
    )
    .await;

    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body.contains("pm10"), "{body}");
    assert_eq!(
        body.lines()
            .filter(|line| line.contains("station-01"))
            .count(),
        1,
        "{body}"
    );

    // The selector came from the broker's own answer to "what is in here", and the entity
    // query that followed carries it. Neither hop is unselected.
    let hops = broker.hops();
    let types_hop = hops.first().expect("a first hop");
    assert_eq!(
        types_hop.path, "/ngsi-ld/v1/types",
        "the fallback asks the space what it holds first: {hops:?}"
    );
    // The question is asked of this endpoint's tenant and no other, or the type list of a
    // neighbouring space would become this endpoint's selector.
    assert_eq!(types_hop.tenant, "ovzdusie", "{types_hop:?}");
    // The list only narrows, and no grant gives a caller 5.7.5, so the gateway asks it on its
    // own account (T-3790): its client and no caller; the entity query is the caller's.
    let header = |hop: &common::Hop, name: &str| {
        hop.headers
            .get(name)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned)
    };
    assert_eq!(
        header(types_hop, "x-jc-client").as_deref(),
        Some("context-gateway")
    );
    assert_eq!(
        header(types_hop, "x-jc-roles").as_deref(),
        Some(""),
        "{types_hop:?}"
    );
    let entities_hop = hops.last().expect("an entity query");
    assert_eq!(
        header(entities_hop, "x-jc-roles").as_deref(),
        Some("public")
    );
    assert_eq!(header(entities_hop, "x-jc-client"), None);
    let query = &hops.last().expect("an entity query").query;
    assert!(
        query.contains("type=AirQualityObserved"),
        "the entity query must select: {query}"
    );
}

/// The caller's `Accept` names what it wants back, not what the broker speaks: `jcctl publish
/// ckan` asks `file.csv` with `Accept: text/csv`, and forwarded to the broker that was a `406`.
#[tokio::test]
async fn the_callers_accept_never_reaches_the_broker() {
    let (path, accept) = ("file.csv", "text/csv");
    let broker = BrokerStub::start(vec![json!([station()])]).await;
    let response = gateway(&broker.url, unrestricted_endpoint())
        .oneshot(
            HttpRequest::builder()
                .uri(format!("/api/endpoint/{SLUG}/{path}"))
                .header("Accept", accept)
                .body(Body::empty())
                .expect("a request"),
        )
        .await
        .expect("the gateway answers");
    assert_eq!(response.status(), StatusCode::OK, "{path}");
    let hops = broker.hops();
    assert!(!hops.is_empty(), "{path}: no hop");
    assert!(
        hops.iter().all(|hop| hop.accept == "application/json"),
        "{path}: {hops:?}"
    );
    assert!(
        hops.iter().all(|hop| hop.accept != accept),
        "{path}: {hops:?}"
    );
}

/// A space with nothing in it is an empty file. Asking the broker for "no types at all"
/// would be an unselected query again, so the gateway does not ask.
#[tokio::test]
async fn an_empty_space_is_an_empty_file_and_not_a_second_question() {
    let broker = BrokerStub::start(vec![json!([])]).await;
    let (status, body) = get(
        gateway(&broker.url, unrestricted_endpoint()),
        &format!("/api/endpoint/{SLUG}/file.csv"),
    )
    .await;

    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(!body.contains("urn:ngsi-ld:"), "{body}");
    let hops = broker.hops();
    assert_eq!(
        hops.iter()
            .filter(|hop| hop.path == "/ngsi-ld/v1/entities")
            .count(),
        0,
        "an empty type list means there is nothing to query: {hops:?}"
    );
}

/// The NGSI-LD surface is the API, not a download, and an unselected query there stays the
/// `400` CIM 009 5.7.2 asks for. A gateway that answered it would be less conformant than
/// the broker behind it.
#[tokio::test]
async fn the_ngsi_ld_surface_still_refuses_an_unselected_query() {
    let broker = BrokerStub::start(vec![json!([station()])]).await;
    let (status, body) = get(
        gateway(&broker.url, unrestricted_endpoint()),
        &format!("/api/endpoint/{SLUG}/ngsi-ld/v1/entities"),
    )
    .await;

    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(body.contains("5.7.2"), "the broker's own refusal: {body}");
    assert!(
        broker
            .hops()
            .iter()
            .all(|hop| hop.path != "/ngsi-ld/v1/types"),
        "the API surface never asks for a selector on the caller's behalf"
    );
}

/// A grant that names a type still decides, and the fallback never widens it (T-3651): the
/// download names no selector, so the gateway asks the view for every type the space holds and
/// the view answers only what the caller's grant shows, the one type and its two attributes.
/// The engine owns that narrowing now (Antares `views_matrix` tests).
#[tokio::test]
async fn a_grant_that_narrows_is_never_widened_by_the_fallback() {
    let depot = json!({
        "id": "urn:ngsi-ld:Depot:banskabystrica.sk:ovzdusie:north",
        "type": "Depot",
        "capacity": { "type": "Property", "value": 7 },
    });
    let mut aqo = station();
    aqo["no2"] = json!({ "type": "Property", "value": 11 });
    let broker = BrokerStub::narrowing(
        vec![json!([aqo, depot])],
        View::of(
            SLUG,
            "public",
            &["AirQualityObserved"],
            Some(&["pm10", "location"]),
        ),
    )
    .await;
    let (status, body) = get(
        gateway(&broker.url, unrestricted_endpoint()),
        &format!("/api/endpoint/{SLUG}/file.csv"),
    )
    .await;

    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body.contains("station-01"), "{body}");
    for trace in ["Depot", "capacity", "no2"] {
        assert!(!body.contains(trace), "the view showed {trace}: {body}");
    }
}

/// T-3790, restated for the view (T-3651): the type list is asked on the gateway's account and
/// names every type the space holds, but it only ever selects at the view, never reaches the
/// caller, and the view leaves out the type the caller's grant keeps from them. No
/// representation names anything of that type, in its body or its headers.
#[tokio::test]
async fn a_type_the_caller_may_not_read_is_never_listed_for_them() {
    let depot = json!({
        "id": "urn:ngsi-ld:Depot:banskabystrica.sk:ovzdusie:north",
        "type": "Depot",
        "capacity": { "type": "Property", "value": 7 },
    });
    let endpoint = unrestricted_endpoint();
    let file = "file.csv";
    let broker = BrokerStub::narrowing(
        vec![json!([station(), depot.clone()])],
        View::of(SLUG, "public", &["AirQualityObserved"], None),
    )
    .await;
    let response = gateway(&broker.url, endpoint.clone())
        .oneshot(
            HttpRequest::builder()
                .uri(format!("/api/endpoint/{SLUG}/{file}"))
                .body(Body::empty())
                .expect("a request"),
        )
        .await
        .expect("the gateway answers");
    let status = response.status();
    let headers = format!("{:?}", response.headers());
    let body = axum::body::to_bytes(response.into_body(), 16 * 1024 * 1024)
        .await
        .expect("a readable body");
    let body = String::from_utf8_lossy(&body);
    assert_eq!(status, StatusCode::OK, "{file}: {body}");
    assert!(body.contains("station-01"), "{file}: {body}");
    for trace in ["Depot", "capacity", "north"] {
        assert!(!body.contains(trace), "{file} names {trace}: {body}");
        assert!(!headers.contains(trace), "{file} names {trace}: {headers}");
    }
}
