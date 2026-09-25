//! Edge cases of `app::sensorthings`, `ANY /api/endpoint/{slug}/sta/v1.1/…` (T-1889, EP-26,
//! MP-02).
//!
//! Contract, in one sentence: every SensorThings request is one projected entity page of this
//! endpoint's own space, translated — so no set, id, `$filter`, `$expand` or page parameter may
//! reach data the caller could not read through the NGSI-LD surface, and anything the profile does
//! not have is the one `404` an ungranted resource gets (EP-12, EP-13, R20, T-1862).
//!
//! The translation itself is `sta_translator_tests.rs`. What is here is the bounds: the page
//! numbers, the shapes of an id, the sets that are not sets, and what a client may not add.

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
const HOST: &str = "https://city.example";
const URN: &str = "urn:ngsi-ld:AirQualityObserved:banskabystrica.sk:ovzdusie:st-1";

fn endpoint(hidden: &[&str]) -> Endpoint {
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
        representations: vec![Representation::NgsiLd, Representation::Sta],
        rate_limit: None,
        file_limits: None,
        hidden_attributes: hidden.iter().map(|name| (*name).to_owned()).collect(),
        projection: None,
        base_path: format!("/api/endpoint/{SLUG}"),
        models: Vec::new(),
        view_mapping: None,
        policies: vec![serde_norway::from_str(
            r#"contextSpaceRef: ovzdusie
assigner: did:web:banskabystrica.sk
assignee: { kind: role, id: public }
operations: [queryEntity, retrieveEntity, retrieveTemporal]
"#,
        )
        .expect("the policy spec parses")],
    }
}

fn gateway(broker: &str, endpoint: Endpoint) -> axum::Router {
    let mut gateway = Gateway::new(
        Broker::new(broker),
        Box::new(PolicyPdp),
        "banskabystrica.sk",
    );
    gateway.public_url = Some(HOST.to_owned());
    router(Arc::new(gateway.serve([endpoint])))
}

fn station() -> Value {
    json!({
        "id": URN,
        "type": "AirQualityObserved",
        "name": { "type": "Property", "value": "Kallio" },
        "pm10": {
            "type": "Property",
            "value": 34.2,
            "observedAt": "2026-09-01T10:00:00Z"
        },
        "operatorPhone": { "type": "Property", "value": "+421 900 000 000" },
        "location": {
            "type": "GeoProperty",
            "value": { "type": "Point", "coordinates": [19.146, 48.736] }
        }
    })
}

fn sta(path: &str) -> String {
    format!("/api/endpoint/{SLUG}/sta/v1.1{path}")
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

/// The first case: `$top` is clamped to the gateway's own page and `$skip` is refused by name past
/// its ceiling, because a silently clamped offset answers a page the caller did not ask for and
/// pages through the same rows forever (EP-13, T-0809).
#[tokio::test]
async fn every_spelling_of_a_page_parameter_is_bounded_or_refused() {
    for (query, expected_limit) in [
        ("$top=0", "1"),
        ("$top=1", "1"),
        ("$top=1000", "1000"),
        ("$top=1001", "1000"),
        ("$top=99999999", "1000"),
        ("$top=-1", "100"),
        ("$top=1.5", "100"),
        ("$top=abc", "100"),
        ("$top=", "100"),
        ("$skip=0", "100"),
        ("$skip=100000", "100"),
    ] {
        let broker = BrokerStub::start(vec![json!([station()])]).await;
        let (status, body) = get(
            gateway(&broker.url, endpoint(&[])),
            &sta(&format!("/Things?{query}")),
        )
        .await;

        assert_eq!(status, StatusCode::OK, "{query}: {body}");
        let hops = broker.hops();
        let asked = &hops.last().expect("an entity query").query;
        assert!(
            asked.contains(&format!("limit={expected_limit}")),
            "{query} reached the broker as {asked}"
        );
    }

    for query in [
        "$skip=-1",
        "$skip=abc",
        "$skip=1.5",
        "$skip=100001",
        "$skip=1e3",
    ] {
        let broker = BrokerStub::start(vec![json!([station()])]).await;
        let (status, body, headers) = call(
            gateway(&broker.url, endpoint(&[])),
            Method::GET,
            &sta(&format!("/Things?{query}")),
        )
        .await;

        assert_eq!(status, StatusCode::BAD_REQUEST, "{query}: {body}");
        assert_eq!(
            headers
                .get("x-parameter")
                .and_then(|value| value.to_str().ok()),
            Some("$skip"),
            "{query}"
        );
        assert!(broker.hops().is_empty(), "{query}: {:?}", broker.hops());
    }
}

/// An id is an entity URN, so anything that is not one names nothing: a number, a name, an empty
/// key, a URN of another space and a URN of another organization all answer `404` (EP-12, R20).
#[tokio::test]
async fn an_id_that_is_not_a_urn_of_this_space_names_nothing() {
    for key in [
        "1",
        "'1'",
        "st-1",
        "",
        "''",
        "urn:ngsi-ld:AirQualityObserved:banskabystrica.sk:uctovnictvo:st-1",
        "urn:ngsi-ld:AirQualityObserved:another.sk:ovzdusie:st-1",
        "URN:ngsi-ld:AirQualityObserved:banskabystrica.sk:ovzdusie:st-1",
        "%20urn:ngsi-ld:AirQualityObserved:banskabystrica.sk:ovzdusie:st-1",
    ] {
        let broker = BrokerStub::start(vec![json!([])]).await;
        let (status, body) = get(
            gateway(&broker.url, endpoint(&[])),
            &sta(&format!("/Things({key})")),
        )
        .await;

        assert_eq!(status, StatusCode::NOT_FOUND, "{key}: {body}");
        assert!(
            broker.hops().iter().all(|hop| hop.tenant == "ovzdusie"),
            "{key}: {:?}",
            broker.hops()
        );
    }
}

/// A set the profile does not have is a `404`, and so is a set spelled another way: the eight names
/// are the surface, and a caller cannot reach a ninth by guessing at its case.
#[tokio::test]
async fn a_set_the_profile_does_not_have_is_not_found() {
    for path in [
        "/things",
        "/THINGS",
        "/Thing",
        "/Entities",
        "/Datastream",
        "/$metadata",
        "/Things'",
        "/Observations(1)/FeatureOfInterest",
    ] {
        let broker = BrokerStub::start(vec![json!([station()])]).await;
        let (status, body) = get(gateway(&broker.url, endpoint(&[])), &sta(path)).await;

        assert_eq!(status, StatusCode::NOT_FOUND, "{path}: {body}");
    }
}

/// The tree is two segments deep and a child needs a parent: three segments, or a child set under a
/// collection nobody addressed, are not part of the surface (EP-13).
#[tokio::test]
async fn the_tree_is_two_segments_deep_and_a_child_needs_a_parent() {
    for path in [
        format!("/Things({URN})/Datastreams/Observations"),
        format!("/Things({URN})/Locations/Things"),
        "/Things/Datastreams".to_owned(),
        "/Things/Locations".to_owned(),
        "/Datastreams/Observations".to_owned(),
        format!("/Things({URN})/Datastreams({URN}%2Fpm10)/Observations"),
    ] {
        let broker = BrokerStub::start(vec![json!([station()])]).await;
        let (status, body) = get(gateway(&broker.url, endpoint(&[])), &sta(&path)).await;

        assert_eq!(status, StatusCode::NOT_FOUND, "{path}: {body}");
    }
}

/// The sets the platform holds nothing for are empty rather than missing, so a conformance suite can
/// walk them — but only as collections: one of them addressed by an id is a `404`, because there is
/// no instance to address (EP-13).
#[tokio::test]
async fn a_set_with_no_data_is_empty_as_a_collection_and_absent_as_an_instance() {
    for set in ["Sensors", "FeaturesOfInterest", "HistoricalLocations"] {
        let broker = BrokerStub::start(vec![json!([station()])]).await;
        let (status, body) = get(
            gateway(&broker.url, endpoint(&[])),
            &sta(&format!("/{set}?$count=true")),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{set}: {body}");
        let collection: Value = serde_json::from_str(&body).expect("a collection");
        assert_eq!(collection["value"], json!([]), "{set}: {body}");
        assert_eq!(collection["@iot.count"], json!(0), "{set}: {body}");
        assert!(
            broker.hops().is_empty(),
            "{set}: an empty set costs no broker call: {:?}",
            broker.hops()
        );

        let broker = BrokerStub::start(vec![json!([station()])]).await;
        let (status, body) = get(
            gateway(&broker.url, endpoint(&[])),
            &sta(&format!("/{set}({URN})")),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{set}: {body}");
    }
}

/// `$count` is the word `true` and nothing else: no other spelling turns the count on, because a
/// count of what was withheld is the same disclosure as the rows (R22, T-2131).
#[tokio::test]
async fn only_the_word_true_turns_the_count_on() {
    for (raw, counted) in [
        ("true", true),
        ("TRUE", false),
        ("True", false),
        ("1", false),
        ("yes", false),
        ("false", false),
        ("", false),
    ] {
        let broker = BrokerStub::start(vec![json!([station()])]).await;
        let (status, body) = get(
            gateway(&broker.url, endpoint(&[])),
            &sta(&format!("/Things?$count={raw}")),
        )
        .await;

        assert_eq!(status, StatusCode::OK, "$count={raw}: {body}");
        let collection: Value = serde_json::from_str(&body).expect("a collection");
        assert_eq!(
            collection.get("@iot.count").is_some(),
            counted,
            "$count={raw}: {body}"
        );
    }
}

/// `$expand` inlines what a navigation link would answer, so it can only ever name a set of the
/// profile: an unknown name, a repeated one and a list of everything add nothing and break nothing.
#[tokio::test]
async fn an_expand_the_profile_does_not_have_adds_nothing() {
    for raw in [
        "Nonsense",
        "Things",
        "Datastreams,Datastreams",
        "Datastreams,Locations,Nonsense,",
        "%20Datastreams%20",
        "operatorPhone",
        "../../etc/passwd",
    ] {
        let broker = BrokerStub::start(vec![json!([station()])]).await;
        let (status, body) = get(
            gateway(&broker.url, endpoint(&["operatorPhone"])),
            &sta(&format!("/Things?$expand={raw}")),
        )
        .await;

        assert_eq!(status, StatusCode::OK, "$expand={raw}: {body}");
        assert!(
            !body.contains("operatorPhone") && !body.contains("+421"),
            "$expand={raw} reached a hidden attribute: {body}"
        );
        assert!(!body.contains("passwd"), "$expand={raw}: {body}");
    }
}

/// A `$filter` on an attribute the endpoint hides asks the broker nothing and answers nothing: the
/// type whose attributes do not cover the filter leaves the query (T-1862, EP-61).
#[tokio::test]
async fn a_filter_on_a_hidden_attribute_reaches_neither_the_broker_nor_the_answer() {
    for filter in [
        "operatorPhone%20eq%20%27x%27",
        "operatorPhone%20ne%20%27y%27",
    ] {
        let broker = BrokerStub::start(vec![json!([station()])]).await;
        let (status, body) = get(
            gateway(&broker.url, endpoint(&["operatorPhone"])),
            &sta(&format!("/Things?$filter={filter}")),
        )
        .await;

        assert_eq!(status, StatusCode::OK, "{filter}: {body}");
        let collection: Value = serde_json::from_str(&body).expect("a collection");
        assert_eq!(collection["value"], json!([]), "{filter}: {body}");
        assert!(
            broker
                .hops()
                .iter()
                .all(|hop| !hop.query.contains("operatorPhone")),
            "{filter}: {:?}",
            broker.hops()
        );
    }
}

/// A parameter this representation does not have changes nothing and breaks nothing: it is not
/// forwarded to the broker and it is not a `400` either, because a client that sends `$select` is
/// asking for less and gets the whole set.
#[tokio::test]
async fn a_parameter_the_representation_does_not_have_is_ignored() {
    for raw in [
        "$select=name",
        "$orderby=name%20desc",
        "$search=%22Kallio%22",
        "$apply=groupby((name))",
        "$skiptoken=abc",
        "limit=1",
        "offset=99",
        "type=Invoice",
    ] {
        let broker = BrokerStub::start(vec![json!([station()])]).await;
        let (status, body) = get(
            gateway(&broker.url, endpoint(&[])),
            &sta(&format!("/Things?{raw}")),
        )
        .await;

        assert_eq!(status, StatusCode::OK, "{raw}: {body}");
        let hops = broker.hops();
        let asked = &hops.last().expect("an entity query").query;
        assert!(asked.contains("limit=100"), "{raw}: {asked}");
        assert!(asked.contains("offset=0"), "{raw}: {asked}");
        assert!(!asked.contains("Invoice"), "{raw}: {asked}");
    }
}

/// The service document is the only answer that needs no data, and it is the same document however
/// the caller spells the root: it names the sets and nothing about this deployment's insides.
#[tokio::test]
async fn the_service_document_is_one_document_for_every_spelling_of_the_root() {
    let broker = BrokerStub::start(vec![json!([station()])]).await;
    let (status, first) = get(gateway(&broker.url, endpoint(&[])), &sta("")).await;
    assert_eq!(status, StatusCode::OK, "{first}");

    for path in ["/", "//", "///"] {
        let broker = BrokerStub::start(vec![json!([station()])]).await;
        let (status, body) = get(gateway(&broker.url, endpoint(&[])), &sta(path)).await;
        assert_eq!(status, StatusCode::OK, "{path}: {body}");
        assert_eq!(body, first, "{path}");
        assert!(
            broker.hops().is_empty(),
            "{path}: the service document costs no broker call"
        );
    }
    for internal in ["ovzdusie", "127.0.0.1", "Bearer", "policy"] {
        assert!(!first.contains(internal), "{internal} came out in {first}");
    }
}

/// A caller no grant names reaches no set at all, and the broker is not asked; the service document
/// stays readable, because it describes the surface and not the data.
#[tokio::test]
async fn a_caller_no_grant_names_reaches_no_set() {
    for path in [
        "/Things",
        &format!("/Things({URN})"),
        "/Datastreams",
        "/Observations",
        &format!("/Datastreams({URN}%2Fpm10)/Observations"),
    ] {
        let broker = BrokerStub::start(vec![json!([station()])]).await;
        let mut ungranted = endpoint(&[]);
        ungranted.policies = Vec::new();

        let (status, body) = get(gateway(&broker.url, ungranted), &sta(path)).await;

        assert!(
            status == StatusCode::FORBIDDEN || status == StatusCode::NOT_FOUND,
            "{path}: {status} {body}"
        );
        assert!(!body.contains("Kallio"), "{path}: {body}");
        assert!(broker.hops().is_empty(), "{path}: {:?}", broker.hops());
    }
}

/// A tenant the caller forged never reaches the broker on this surface either, and none comes back
/// (GW25, SP-05).
#[tokio::test]
async fn a_forged_tenant_neither_reaches_the_broker_nor_comes_back() {
    let broker = BrokerStub::start(vec![json!([station()])]).await;
    let response = gateway(&broker.url, endpoint(&[]))
        .oneshot(
            HttpRequest::builder()
                .uri(sta("/Things"))
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

/// The narrowing signal is opt-in here too: a caller who did not ask is answered as if the page were
/// simply what it is (R22, GW12).
#[tokio::test]
async fn the_narrowing_signal_is_only_sent_to_a_caller_who_asked() {
    let broker = BrokerStub::start(vec![json!([station()])]).await;
    let (_, _, quiet) = call(
        gateway(&broker.url, endpoint(&["operatorPhone"])),
        Method::GET,
        &sta("/Things"),
    )
    .await;
    assert!(
        quiet.get("ngsild-results-restricted").is_none(),
        "{quiet:?}"
    );

    let broker = BrokerStub::start(vec![json!([station()])]).await;
    let response = gateway(&broker.url, endpoint(&["operatorPhone"]))
        .oneshot(
            HttpRequest::builder()
                .uri(sta("/Things"))
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

/// An endpoint that does not serve this representation answers `404` to every method, the write ones
/// included, and the broker is never asked (EP-05, R20).
#[tokio::test]
async fn an_endpoint_without_the_representation_is_404_even_to_a_write() {
    for method in [
        Method::GET,
        Method::OPTIONS,
        Method::POST,
        Method::PATCH,
        Method::DELETE,
    ] {
        let broker = BrokerStub::start(vec![json!([station()])]).await;
        let mut without = endpoint(&[]);
        without.representations = vec![Representation::NgsiLd];
        let (status, body, _) = call(
            gateway(&broker.url, without),
            method.clone(),
            &sta("/Things"),
        )
        .await;

        assert_eq!(status, StatusCode::NOT_FOUND, "{method}: {body}");
        assert!(broker.hops().is_empty(), "{method}: {:?}", broker.hops());
    }
}
