//! Edge cases of `app::ogc_features`, `ANY /api/endpoint/{slug}/ogc/features/…` (T-1888, EP-26,
//! MP-02).
//!
//! Contract, in one sentence: every OGC parameter is translated into the NGSI-LD query the PDP
//! already decided, or refused by name — never half applied, never widened, and never carried to
//! the broker as the caller wrote it; and a collection is only ever a granted type of this
//! endpoint's own space (EP-31, EP-33..EP-40, R20, T-1862).
//!
//! The happy paths of the translation are `ogc_translator_tests.rs`. What is here is the bounds:
//! the numbers, the spellings of a collection name, the repeated parameters and the methods.

mod common;

use axum::body::Body;
use axum::http::{Method, Request as HttpRequest, StatusCode};
use common::BrokerStub;
use context_gateway::app::{router, Gateway};
use context_gateway::pdp::PolicyPdp;
use context_gateway::proxy::Broker;
use context_gateway::resolver::Endpoint;
use jc_core::kinds::{Audience, Representation};
use serde_json::{json, Value};
use std::sync::Arc;
use tower::ServiceExt;

const SLUG: &str = "mluyob4nz52lok3ssk7pgn5vwt";
const PUBLIC_READ: &str = r#"contextSpaceRef: ovzdusie
assigner: did:web:banskabystrica.sk
assignee: { kind: role, id: public }
operations: [queryEntity, retrieveEntity]
"#;
const ONE_TYPE: &str = r#"contextSpaceRef: ovzdusie
assigner: did:web:banskabystrica.sk
assignee: { kind: role, id: public }
operations: [queryEntity, retrieveEntity]
information:
  - entities:
      - type: AirQualityObserved
"#;

fn endpoint(policy: &str) -> Endpoint {
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
        representations: vec![Representation::NgsiLd, Representation::OgcFeatures],
        rate_limit: None,
        file_limits: None,
        hidden_attributes: Default::default(),
        projection: None,
        base_path: format!("/api/endpoint/{SLUG}"),
        models: Vec::new(),
        view_mapping: None,
        catalog: None,
        policies: vec![serde_norway::from_str(policy).expect("the policy spec parses")],
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

fn ogc(tail: &str) -> String {
    format!("/api/endpoint/{SLUG}/ogc/features{tail}")
}

fn station(local: &str) -> Value {
    json!({
        "id": format!("urn:ngsi-ld:AirQualityObserved:banskabystrica.sk:ovzdusie:{local}"),
        "type": "AirQualityObserved",
        "pm10": { "type": "Property", "value": 34.2 },
        "location": {
            "type": "GeoProperty",
            "value": { "type": "Point", "coordinates": [19.146, 48.736] }
        }
    })
}

fn invoice() -> Value {
    json!({
        "id": "urn:ngsi-ld:Invoice:banskabystrica.sk:ovzdusie:inv-1",
        "type": "Invoice",
        "amount": { "type": "Property", "value": 4200 },
        "location": {
            "type": "GeoProperty",
            "value": { "type": "Point", "coordinates": [19.15, 48.74] }
        }
    })
}

async fn call(
    app: axum::Router,
    method: Method,
    path: &str,
) -> (StatusCode, String, axum::http::HeaderMap) {
    let response = app
        .oneshot(
            HttpRequest::builder()
                .method(method)
                .uri(path)
                .body(Body::empty())
                .expect("a request"),
        )
        .await
        .expect("the gateway answers");
    let status = response.status();
    let headers = response.headers().clone();
    let body = axum::body::to_bytes(response.into_body(), 8 * 1024 * 1024)
        .await
        .expect("a readable body");
    (status, String::from_utf8_lossy(&body).into_owned(), headers)
}

async fn get(app: axum::Router, path: &str) -> (StatusCode, String) {
    let (status, body, _) = call(app, Method::GET, path).await;
    (status, body)
}

/// The first case: `limit` is a number the caller writes, so every spelling of it lands inside the
/// one page the gateway will ask for — zero, negative, a billion, a float, a word, one with a space
/// and one with a second parameter glued to it (EP-36).
#[tokio::test]
async fn every_spelling_of_limit_lands_inside_the_page() {
    for (raw, expected) in [
        ("0", "1"),
        ("1", "1"),
        ("-1", "10"),
        ("1000", "1000"),
        ("1001", "1000"),
        ("1000000000", "1000"),
        ("10.5", "10"),
        ("abc", "10"),
        ("", "10"),
        ("%201", "10"),
        ("1%3Blimit%3D9999", "10"),
        ("0x10", "10"),
        ("1e3", "10"),
    ] {
        let broker = BrokerStub::start(vec![json!([station("station-01")])]).await;
        let (status, body) = get(
            gateway(&broker.url, endpoint(PUBLIC_READ)),
            &ogc(&format!(
                "/collections/AirQualityObserved/items?limit={raw}"
            )),
        )
        .await;

        assert_eq!(status, StatusCode::OK, "limit={raw}: {body}");
        let hops = broker.hops();
        let query = &hops.last().expect("an entity query").query;
        assert!(
            query.contains(&format!("limit={expected}")),
            "limit={raw} reached the broker as {query}"
        );
    }
}

/// The cursor is the gateway's own, so a cursor the caller invented is no cursor: the page starts at
/// the beginning rather than at a number the caller chose to write into it (EP-36).
#[tokio::test]
async fn a_cursor_the_caller_invented_starts_at_the_beginning() {
    for raw in [
        "offset=50",
        "b2Zmc2V0PS01MA",
        "b2Zmc2V0PWFiYw",
        "Zm9vPTEyMw",
        "!!!!",
        "",
        "b2Zmc2V0PTk5OTk5OTk5OTk5OTk5OTk5OTk5OTk5OTk5",
    ] {
        let broker = BrokerStub::start(vec![json!([station("station-01")])]).await;
        let (status, body) = get(
            gateway(&broker.url, endpoint(PUBLIC_READ)),
            &ogc(&format!("/collections/AirQualityObserved/items?next={raw}")),
        )
        .await;

        assert_eq!(status, StatusCode::OK, "next={raw}: {body}");
        let hops = broker.hops();
        let query = &hops.last().expect("an entity query").query;
        assert!(
            query.contains("offset=0"),
            "next={raw} reached the broker as {query}"
        );
    }
}

/// A collection name is one type, and a name that tries to be a list is not that type: a comma in
/// the path may not add a second type to the query, and the grant decides either way (EP-31,
/// T-1862).
#[tokio::test]
async fn a_collection_name_cannot_carry_a_second_type_into_the_query() {
    let broker = BrokerStub::start(vec![json!([station("station-01"), invoice()])]).await;
    let (status, body) = get(
        gateway(&broker.url, endpoint(ONE_TYPE)),
        &ogc("/collections/AirQualityObserved,Invoice/items"),
    )
    .await;

    assert_eq!(status, StatusCode::OK, "{body}");
    let page: Value = serde_json::from_str(&body).expect("a FeatureCollection");
    let types: Vec<&str> = page["features"]
        .as_array()
        .expect("features")
        .iter()
        .filter_map(|feature| feature["properties"]["type"].as_str())
        .collect();
    assert_eq!(types, vec!["AirQualityObserved"], "{body}");
    assert!(
        !body.contains("4200"),
        "no value of the second type: {body}"
    );
    // The name the caller wrote is echoed in its own `self` and `collection` links, which is the
    // path it asked for; what may not happen is the second type reaching the query.
    let hops = broker.hops();
    let query = &hops.last().expect("an entity query").query;
    assert!(
        !query.contains("Invoice"),
        "the grant narrowed the type list before the broker saw it: {query}"
    );
}

/// No other spelling of a collection name is that collection: another case, a trailing space, a
/// percent-encoded copy, a `..` and a name the grant does not reach all answer the one `404` an
/// unknown type answers (R20, EP-31).
#[tokio::test]
async fn no_other_spelling_of_a_collection_is_that_collection() {
    let broker = BrokerStub::start(vec![json!([station("station-01")])]).await;
    let (expected, _) = get(
        gateway(&broker.url, endpoint(ONE_TYPE)),
        &ogc("/collections/NothingLikeThis"),
    )
    .await;
    assert_eq!(expected, StatusCode::NOT_FOUND);

    for name in [
        "airqualityobserved",
        "AIRQUALITYOBSERVED",
        "AirQualityObserved%20",
        "AirQualityObserved.",
        "..",
        "Invoice",
        "%2E%2E%2F%2E%2E",
    ] {
        let broker = BrokerStub::start(vec![json!([station("station-01")])]).await;
        let (status, body) = get(
            gateway(&broker.url, endpoint(ONE_TYPE)),
            &ogc(&format!("/collections/{name}")),
        )
        .await;
        assert_eq!(status, expected, "{name}: {body}");
    }
}

/// A path this representation does not have is a `404` and never a guess: not one segment too many,
/// not one too few, and not a climb out of the tree (EP-39).
#[tokio::test]
async fn a_path_the_representation_does_not_have_is_not_found() {
    for tail in [
        "/collections/AirQualityObserved/items/urn:x/attributes",
        "/collections/AirQualityObserved/item",
        "/collection/AirQualityObserved",
        "/conformance/extra",
        "/api/extra",
        "/../../schema/index.json",
        "/collections//items",
        "/openapi",
    ] {
        let broker = BrokerStub::start(vec![json!([station("station-01")])]).await;
        let (status, body) = get(gateway(&broker.url, endpoint(PUBLIC_READ)), &ogc(tail)).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{tail}: {body}");
    }
}

/// An endpoint that does not serve this representation answers `404` to every method, the write
/// ones included: the surface does not exist here, so it cannot be `405` either (EP-05, R20).
#[tokio::test]
async fn an_endpoint_without_the_representation_is_404_even_to_a_write() {
    for method in [
        Method::GET,
        Method::OPTIONS,
        Method::POST,
        Method::PUT,
        Method::DELETE,
    ] {
        let broker = BrokerStub::start(vec![json!([station("station-01")])]).await;
        let mut without = endpoint(PUBLIC_READ);
        without.representations = vec![Representation::NgsiLd];
        let (status, body, _) = call(
            gateway(&broker.url, without),
            method.clone(),
            &ogc("/collections/AirQualityObserved/items"),
        )
        .await;

        assert_eq!(status, StatusCode::NOT_FOUND, "{method}: {body}");
        assert!(broker.hops().is_empty(), "{method}: {:?}", broker.hops());
    }
}

/// A repeated parameter is the first one that says anything, so a second copy cannot widen what the
/// first asked for, and a copy with nothing in it is not the parameter at all.
#[tokio::test]
async fn a_repeated_parameter_is_the_first_one_that_says_anything() {
    let broker = BrokerStub::start(vec![json!([station("station-01")])]).await;
    let (status, body) = get(
        gateway(&broker.url, endpoint(PUBLIC_READ)),
        &ogc("/collections/AirQualityObserved/items?limit=1&limit=1000&next=b2Zmc2V0PTk5&next=b2Zmc2V0PTA"),
    )
    .await;

    assert_eq!(status, StatusCode::OK, "{body}");
    let hops = broker.hops();
    let query = &hops.last().expect("an entity query").query;
    assert!(query.contains("limit=1&"), "{query}");
    assert!(query.contains("offset=99"), "{query}");
    assert!(!query.contains("limit=1000"), "{query}");

    // An empty value is an absent parameter: the copy that carries a cursor is the one read.
    let broker = BrokerStub::start(vec![json!([station("station-01")])]).await;
    let (status, _) = get(
        gateway(&broker.url, endpoint(PUBLIC_READ)),
        &ogc("/collections/AirQualityObserved/items?next=&next=b2Zmc2V0PTk5"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let hops = broker.hops();
    assert!(
        hops.last()
            .expect("an entity query")
            .query
            .contains("offset=99"),
        "{hops:?}"
    );
}

/// A feature id is a URN, and a URN of another space or another organization is not reachable
/// through this endpoint: the tenant is pinned, so the answer is the same `404` a missing one gets
/// (EP-33, GW25, R20).
#[tokio::test]
async fn a_feature_id_of_another_space_is_not_found() {
    for id in [
        "urn:ngsi-ld:AirQualityObserved:banskabystrica.sk:uctovnictvo:station-01",
        "urn:ngsi-ld:AirQualityObserved:another.sk:ovzdusie:station-01",
        "urn:ngsi-ld:Invoice:banskabystrica.sk:ovzdusie:inv-1",
        "not-a-urn",
        "urn:ngsi-ld:AirQualityObserved:banskabystrica.sk:ovzdusie:",
    ] {
        let broker = BrokerStub::start(vec![json!([])]).await;
        let (status, body) = get(
            gateway(&broker.url, endpoint(ONE_TYPE)),
            &ogc(&format!("/collections/AirQualityObserved/items/{id}")),
        )
        .await;

        assert_eq!(status, StatusCode::NOT_FOUND, "{id}: {body}");
        let hops = broker.hops();
        assert!(
            hops.iter().all(|hop| hop.tenant == "ovzdusie"),
            "{id}: the tenant is this endpoint's: {hops:?}"
        );
    }
}

/// Every answer of this representation says which coordinate reference system it is in, because a
/// GIS client that guesses draws the city in the sea (EP-34).
#[tokio::test]
async fn every_geojson_answer_names_its_crs() {
    for tail in [
        "/collections/AirQualityObserved/items",
        "/collections/AirQualityObserved/items/urn:ngsi-ld:AirQualityObserved:banskabystrica.sk:ovzdusie:station-01",
    ] {
        let broker = BrokerStub::start(vec![json!([station("station-01")])]).await;
        let (status, body, headers) = call(
            gateway(&broker.url, endpoint(PUBLIC_READ)),
            Method::GET,
            &ogc(tail),
        )
        .await;

        assert_eq!(status, StatusCode::OK, "{tail}: {body}");
        assert_eq!(
            headers.get("content-crs").and_then(|value| value.to_str().ok()),
            // Without the angle brackets OGC API - Features Part 2 asks for; noted in chyby.md.
            Some("http://www.opengis.net/def/crs/OGC/1.3/CRS84"),
            "{tail}"
        );
    }
}

/// The narrowing signal is opt-in: a caller who did not ask is answered as if the page were simply
/// what it is, and one who asks is told (R22, GW12).
#[tokio::test]
async fn the_narrowing_signal_is_only_sent_to_a_caller_who_asked() {
    let hidden = {
        let mut endpoint = endpoint(PUBLIC_READ);
        endpoint.hidden_attributes = ["pm10".to_owned()].into_iter().collect();
        endpoint
    };

    let broker = BrokerStub::start(vec![json!([station("station-01")])]).await;
    let (_, _, quiet) = call(
        gateway(&broker.url, hidden.clone()),
        Method::GET,
        &ogc("/collections/AirQualityObserved/items"),
    )
    .await;
    assert!(
        quiet.get("ngsild-results-restricted").is_none(),
        "{quiet:?}"
    );

    let broker = BrokerStub::start(vec![json!([station("station-01")])]).await;
    let response = gateway(&broker.url, hidden)
        .oneshot(
            HttpRequest::builder()
                .uri(ogc("/collections/AirQualityObserved/items"))
                .header("NGSILD-Results-Restricted", "true")
                .body(Body::empty())
                .expect("a request"),
        )
        .await
        .expect("the gateway answers");
    assert_eq!(
        response
            .headers()
            .get("ngsild-results-restricted")
            .and_then(|value| value.to_str().ok()),
        Some("true"),
        "{:?}",
        response.headers()
    );
}

/// A language header is a client's own text, and it reaches a title: it comes back as a JSON string
/// and never as a header of its own (EP-32).
#[tokio::test]
async fn a_hostile_language_header_cannot_reach_a_header_of_the_answer() {
    for language in [
        "sk\r\nX-Injected: 1",
        "sk;q=\"\r\n\"",
        "*",
        "",
        "sk-SK,sk;q=0.9,en;q=0.8,de;q=0.7,cs;q=0.6",
    ] {
        let broker = BrokerStub::start(vec![json!([station("station-01")])]).await;
        let request = HttpRequest::builder()
            .uri(ogc(""))
            .header(
                axum::http::header::ACCEPT_LANGUAGE,
                axum::http::HeaderValue::from_bytes(language.as_bytes())
                    .unwrap_or_else(|_| axum::http::HeaderValue::from_static("sk")),
            )
            .body(Body::empty())
            .expect("a request");
        let response = gateway(&broker.url, endpoint(PUBLIC_READ))
            .oneshot(request)
            .await
            .expect("the gateway answers");

        assert_eq!(response.status(), StatusCode::OK, "{language:?}");
        for (name, value) in response.headers() {
            let text = String::from_utf8_lossy(value.as_bytes()).into_owned();
            assert!(!text.contains("X-Injected"), "{name}: {text}");
            assert!(!text.contains('\n'), "{name}: {text}");
        }
        let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .expect("a readable body");
        let landing: Value =
            serde_json::from_slice(&body).expect("the landing page is one JSON document");
        assert!(landing["title"].is_string(), "{landing}");
    }
}

/// A filter that names an attribute the endpoint hides asks the broker nothing: the type whose
/// attributes do not cover the filter leaves the query, so a hidden name cannot be tested one
/// predicate at a time (T-1862, EP-61, EP-35).
#[tokio::test]
async fn a_filter_on_a_hidden_attribute_reaches_neither_the_broker_nor_the_page() {
    for filter in [
        "pm10%20%3E%2010",
        "pm10%20%3C%2010",
        "pm10%20IS%20NOT%20NULL",
    ] {
        let broker = BrokerStub::start(vec![json!([station("station-01")])]).await;
        let mut hidden = endpoint(PUBLIC_READ);
        hidden.hidden_attributes = ["pm10".to_owned()].into_iter().collect();

        let (status, body) = get(
            gateway(&broker.url, hidden),
            &ogc(&format!(
                "/collections/AirQualityObserved/items?filter={filter}"
            )),
        )
        .await;

        assert_eq!(status, StatusCode::OK, "{filter}: {body}");
        let page: Value = serde_json::from_str(&body).expect("a FeatureCollection");
        assert_eq!(page["features"], json!([]), "{filter}: {body}");
        assert!(
            broker.hops().iter().all(|hop| !hop.query.contains("pm10")),
            "{filter}: {:?}",
            broker.hops()
        );
    }
}

/// A tenant the caller forged never reaches the broker on this surface either, and none comes back
/// (GW25, SP-05).
#[tokio::test]
async fn a_forged_tenant_neither_reaches_the_broker_nor_comes_back() {
    let broker = BrokerStub::start(vec![json!([station("station-01")])]).await;
    let response = gateway(&broker.url, endpoint(PUBLIC_READ))
        .oneshot(
            HttpRequest::builder()
                .uri(ogc("/collections/AirQualityObserved/items"))
                .header("NGSILD-Tenant", "somebody-elses-space")
                .body(Body::empty())
                .expect("a request"),
        )
        .await
        .expect("the gateway answers");

    assert_eq!(response.status(), StatusCode::OK);
    assert!(response.headers().get("ngsild-tenant").is_none());
    let hops = broker.hops();
    assert!(!hops.is_empty(), "the query was made");
    assert!(
        hops.iter()
            .all(|hop| !hop.forged && hop.tenant == "ovzdusie"),
        "{hops:?}"
    );
}

/// A caller no grant names reaches nothing here, not even the documents that describe the surface's
/// data: the landing page and the conformance list are the surface itself and stay readable, and
/// everything that would have to ask the broker is refused.
#[tokio::test]
async fn a_caller_no_grant_names_reaches_no_data_document() {
    for tail in [
        "/collections",
        "/collections/AirQualityObserved",
        "/collections/AirQualityObserved/items",
        "/api",
    ] {
        let broker = BrokerStub::start(vec![json!([station("station-01")])]).await;
        let mut ungranted = endpoint(PUBLIC_READ);
        ungranted.policies = Vec::new();

        let (status, body) = get(gateway(&broker.url, ungranted), &ogc(tail)).await;

        assert!(
            status == StatusCode::FORBIDDEN || status == StatusCode::NOT_FOUND,
            "{tail}: {status} {body}"
        );
        assert!(!body.contains("station-01"), "{tail}: {body}");
        assert!(broker.hops().is_empty(), "{tail}: {:?}", broker.hops());
    }
}
