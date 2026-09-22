//! Edge cases of `ogc::ogc_items` and `ogc::ogc_sample` (T-2494, EP-31, EP-33…EP-38, R20).
//!
//! **The contract.** OGC Features parameters (`limit`, `next`, `bbox`, `datetime`, `filter`,
//! `filter-lang`) become the NGSI-LD parameters the PDP-checked `query_entities` accepts; what the
//! CQL2 subset or the one-`geoQ`/one-`temporalQ` rule forbids is a 400 naming the parameter.
//! `ogc_sample` derives each type's extent from one bounded page, and a caller-named type no model
//! declares is still decided by the PDP inside `query_entities`.
//!
//! **Inputs.** The raw query string, the wanted type, one page of broker entities.
//!
//! Already proved elsewhere, and not repeated here: a filter with the parameter of the same kind
//! (`ogc_translator_tests.rs::a_filter_and_the_parameter_that_means_the_same_thing_cannot_both_be_sent`),
//! a filter in another language (`…::a_filter_in_another_language_is_refused`), an invalid CQL2
//! construct naming itself (`…::an_unsupported_cql2_construct_is_refused_with_its_name`), `limit`
//! clamped and an unparseable one defaulted
//! (`edge_app_ogc_features_tests.rs::every_spelling_of_limit_lands_inside_the_page`), a `next`
//! that does not parse (`…::a_cursor_the_caller_invented_starts_at_the_beginning`), and a type with
//! no geometry dropped from the list (`ogc_translator_tests.rs::a_type_without_a_geometry_is_not_a_collection`).
//! Struck: duplicate wanted types cannot occur, `wanted` is one name
//! (`std::slice::from_ref(&name)` in the `["collections", name]` arm of `ogc.rs`).

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
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
/// A grant on one type only.
const ONE_TYPE: &str = r#"contextSpaceRef: ovzdusie
assigner: did:web:banskabystrica.sk
assignee: { kind: role, id: public }
operations: [queryEntity, retrieveEntity]
information:
  - entities:
      - type: AirQualityObserved
"#;

fn endpoint(hidden: &[&str]) -> Endpoint {
    Endpoint {
        roles: Default::default(),
        slug: SLUG.to_owned(),
        title: Default::default(),
        description: Default::default(),
        space: "ovzdusie".to_owned(),
        project: "ovzdusie".to_owned(),
        audience: Audience::Public,
        allowed_projects: Vec::new(),
        representations: vec![Representation::NgsiLd, Representation::OgcFeatures],
        rate_limit: None,
        file_limits: None,
        hidden_attributes: hidden.iter().map(|name| (*name).to_owned()).collect(),
        projection: None,
        base_path: format!("/api/endpoint/{SLUG}"),
        models: Vec::new(),
        view_mapping: None,
        policies: vec![serde_norway::from_str(ONE_TYPE).expect("the policy spec parses")],
    }
}

fn app(broker: &str, hidden: &[&str]) -> axum::Router {
    router(Arc::new(
        Gateway::new(
            Broker::new(broker),
            Box::new(PolicyPdp),
            "banskabystrica.sk",
        )
        .serve([endpoint(hidden)]),
    ))
}

fn ogc(tail: &str) -> String {
    format!("/api/endpoint/{SLUG}/ogc/features{tail}")
}

fn invoice() -> Value {
    json!({
        "id": "urn:ngsi-ld:Invoice:banskabystrica.sk:ovzdusie:inv-1",
        "type": "Invoice",
        "amount": { "type": "Property", "value": 4200 },
        "location": { "type": "GeoProperty", "value": { "type": "Point", "coordinates": [19.15, 48.74] } }
    })
}

fn station() -> Value {
    json!({
        "id": "urn:ngsi-ld:AirQualityObserved:banskabystrica.sk:ovzdusie:st-1",
        "type": "AirQualityObserved",
        "pm10": { "type": "Property", "value": 34.2 },
        "operatorPhone": { "type": "Property", "value": "+421 900 000 000" },
        "location": { "type": "GeoProperty", "value": { "type": "Point", "coordinates": [19.146, 48.736] } }
    })
}

async fn get(
    app: axum::Router,
    path: &str,
    headers: &[(&str, &str)],
) -> (StatusCode, String, axum::http::HeaderMap) {
    let mut request = Request::builder().uri(path);
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    let response = app
        .oneshot(request.body(Body::empty()).expect("a request"))
        .await
        .expect("the gateway answers");
    let status = response.status();
    let headers = response.headers().clone();
    let body = axum::body::to_bytes(response.into_body(), 8 * 1024 * 1024)
        .await
        .expect("a readable body");
    (status, String::from_utf8_lossy(&body).into_owned(), headers)
}

/// R20: a type no visible model declares is asked of the PDP, which the grant on one type
/// refuses; the invoice the broker would hand back is never listed, and its collection is the same
/// 404 as one that does not exist.
#[tokio::test]
async fn a_wanted_type_none_of_the_visible_models_declare_is_still_gated_by_the_pdp_in_query_entities(
) {
    for path in ["/collections/Invoice", "/collections/Invoice/items"] {
        let broker = BrokerStub::start(vec![json!([invoice()])]).await;
        let (status, body, _) = get(app(&broker.url, &[]), &ogc(path), &[]).await;
        assert!(
            status == StatusCode::NOT_FOUND
                || status == StatusCode::FORBIDDEN
                || (status == StatusCode::OK && !body.contains("inv-1")),
            "{path}: {status} {body}"
        );
        assert!(!body.contains("4200"), "{path}: {body}");
        for hop in broker.hops() {
            assert!(
                !hop.query.contains("type=Invoice"),
                "{path} asked the broker for it: {hop:?}"
            );
        }
    }
    let (status, _, _) = get(
        app("http://127.0.0.1:1", &[]),
        &ogc("/collections/NoSuchType"),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

/// EP-35: the language is checked before the filter is read, so a filter in another language is
/// refused as `filter-lang` even when it would not compile either.
#[tokio::test]
async fn filter_lang_other_than_the_served_one_is_400_before_the_filter_is_compiled() {
    // Struck: an empty `filter-lang=` is no `filter-lang` (`query::first`), so the filter is read.
    for lang in ["cql2-json", "CQL2-TEXT", "ecql"] {
        let broker = BrokerStub::start(vec![json!([station()])]).await;
        let path = ogc(&format!(
            "/collections/AirQualityObserved/items?filter=%28%28%28&filter-lang={lang}"
        ));
        let (status, body, headers) = get(app(&broker.url, &[]), &path, &[]).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{lang:?}: {body}");
        assert_eq!(
            headers.get("x-parameter").and_then(|v| v.to_str().ok()),
            Some("filter-lang"),
            "{lang:?}: {body}"
        );
        assert!(broker.hops().is_empty(), "{lang:?} reached the broker");
    }
}

/// EP-61: a page narrowed by the endpoint's publication says so to a caller who asked, and a page
/// that was not narrowed does not.
#[tokio::test]
async fn a_restricted_page_sets_the_results_restricted_header() {
    let asked = [("NGSILD-Results-Restricted", "true")];
    let items = ogc("/collections/AirQualityObserved/items");

    let broker = BrokerStub::start(vec![json!([station()])]).await;
    let (status, body, headers) = get(app(&broker.url, &["operatorPhone"]), &items, &asked).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        headers
            .get("ngsild-results-restricted")
            .and_then(|v| v.to_str().ok()),
        Some("true")
    );
    assert!(!body.contains("+421"), "{body}");

    let broker = BrokerStub::start(vec![json!([station()])]).await;
    let (_, _, headers) = get(app(&broker.url, &[]), &items, &asked).await;
    assert!(headers.get("ngsild-results-restricted").is_none());
}

/// EP-32: whatever the caller writes as their languages, the page is served.
#[tokio::test]
async fn accept_language_with_a_pathological_value_does_not_panic_the_localizer() {
    let long = "sk;q=0.9,".repeat(2000);
    for value in [
        "",
        "*",
        ";;;,,,",
        "sk;q=abc",
        "sk;q=-1,en;q=999",
        "x-klingon-tlh-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        "zh-Hant-TW-u-nu-hanidec",
        long.as_str(),
    ] {
        for path in ["/", "/collections/AirQualityObserved/items"] {
            let broker = BrokerStub::start(vec![json!([station()])]).await;
            let (status, body, _) = get(
                app(&broker.url, &[]),
                &ogc(path),
                &[("Accept-Language", value)],
            )
            .await;
            assert_eq!(status, StatusCode::OK, "{path} {:.40}: {body}", value);
        }
    }
}
