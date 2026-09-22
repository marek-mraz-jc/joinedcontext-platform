//! Edge cases of `sta::sta_series` and `sta_paging` (T-2495, EP-12, EP-13, GW26).
//!
//! **The contract.** A SensorThings `Observations` request of one `Datastream` is one bounded
//! temporal query: `lastN` is `$top + $skip + 1`, a page deeper than `LAST_N_CAP` is refused by
//! name rather than clamped, `$orderby` sorts the page the broker gave, and `@iot.count` is
//! written only when the broker answered less than the ceiling, so it is never the ceiling passed
//! off as a total.
//!
//! **Inputs.** The datastream key (`{urn}/{attribute}`) and `$top`, `$skip`, `$filter`,
//! `$orderby`, `$count`.

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
const HOST: &str = "https://city.example";
const URN: &str = "urn:ngsi-ld:AirQualityObserved:banskabystrica.sk:ovzdusie:st-1";

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

fn app(broker: &str, hidden: &[&str]) -> axum::Router {
    let mut gateway = Gateway::new(
        Broker::new(broker),
        Box::new(PolicyPdp),
        "banskabystrica.sk",
    );
    gateway.public_url = Some(HOST.to_owned());
    router(Arc::new(gateway.serve([endpoint(hidden)])))
}

/// The broker's temporal answer: `n` instances of `pm10`, one a minute, oldest first.
fn series(n: usize) -> Value {
    let instances: Vec<Value> = (0..n)
        .map(|i| {
            json!({
                "type": "Property",
                "value": i,
                "observedAt": format!("2026-09-01T{:02}:{:02}:00Z", i / 60, i % 60),
            })
        })
        .collect();
    json!({ "id": URN, "type": "AirQualityObserved", "pm10": instances })
}

fn observations(query: &str) -> String {
    format!("/api/endpoint/{SLUG}/sta/v1.1/Datastreams('{URN}/pm10')/Observations{query}")
}

async fn get(app: axum::Router, path: &str) -> (StatusCode, Value, axum::http::HeaderMap) {
    get_with(app, path, &[]).await
}

async fn get_with(
    app: axum::Router,
    path: &str,
    headers: &[(&str, &str)],
) -> (StatusCode, Value, axum::http::HeaderMap) {
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
    (
        status,
        serde_json::from_slice(&body).unwrap_or(Value::Null),
        headers,
    )
}

/// The `lastN` the gateway asked the broker for, from the one hop it made.
fn last_n(broker: &BrokerStub) -> Vec<usize> {
    broker
        .hops()
        .iter()
        .flat_map(|hop| {
            hop.query
                .split('&')
                .filter_map(|pair| pair.strip_prefix("lastN="))
                .filter_map(|n| n.parse().ok())
                .collect::<Vec<usize>>()
        })
        .collect()
}

/// EP-13: a series that filled the ceiling may hold more, so it carries no count; one that did
/// not fill it carries its true total.
#[tokio::test]
async fn count_is_omitted_when_the_true_total_could_exceed_the_ceiling() {
    // `$top=5` asks for 6: the broker answering 6 means there may be more.
    let broker = BrokerStub::start(vec![series(6)]).await;
    let (status, body, _) = get(app(&broker.url, &[]), &observations("?$top=5&$count=true")).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body.get("@iot.count").is_none(), "{body}");
    assert_eq!(body["value"].as_array().map(Vec::len), Some(5));

    let broker = BrokerStub::start(vec![series(3)]).await;
    let (_, body, _) = get(app(&broker.url, &[]), &observations("?$top=5&$count=true")).await;
    assert_eq!(body["@iot.count"], json!(3), "{body}");

    let broker = BrokerStub::start(vec![series(3)]).await;
    let (_, body, _) = get(app(&broker.url, &[]), &observations("?$top=5")).await;
    assert!(
        body.get("@iot.count").is_none(),
        "no $count, no count: {body}"
    );
}

/// GW26: `$top` and `$skip` together reach at most `LAST_N_CAP - 1` back, and past it the
/// request is refused by name before the broker is asked.
#[tokio::test]
async fn top_and_skip_together_past_the_cap_are_refused_by_name_not_clamped() {
    for query in [
        "?$top=1000&$skip=1",
        "?$top=500&$skip=500",
        "?$top=1&$skip=999",
    ] {
        let broker = BrokerStub::start(vec![series(3)]).await;
        let (status, body, _) = get(app(&broker.url, &[]), &observations(query)).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{query}: {body}");
        assert!(body.to_string().contains("$skip"), "{query}: {body}");
        assert!(broker.hops().is_empty(), "{query} reached the broker");
    }
    let broker = BrokerStub::start(vec![series(3)]).await;
    let (status, _, _) = get(app(&broker.url, &[]), &observations("?$top=1&$skip=998")).await;
    assert_eq!(status, StatusCode::OK, "the deepest page still answers");
    assert_eq!(last_n(&broker), [1000]);
}

/// EP-13: an offset past `MAX_SKIP`, or one that is not a whole number, is refused by name.
#[tokio::test]
async fn skip_past_max_skip_is_refused_rather_than_silently_capped() {
    // Struck: an empty `$skip=` is read as no `$skip` (`query::first`), the first page.
    for skip in ["100001", "-1", "1.5", "abc", "99999999999999999999999"] {
        let broker = BrokerStub::start(vec![series(3)]).await;
        let (status, body, _) = get(
            app(&broker.url, &[]),
            &observations(&format!("?$skip={skip}")),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{skip:?}: {body}");
        assert!(body.to_string().contains("$skip"), "{skip:?}: {body}");
        assert!(broker.hops().is_empty(), "{skip:?} reached the broker");
    }
}

/// EP-12: a datastream id without the attribute names no datastream, not every entity's history.
#[tokio::test]
async fn a_key_with_no_slash_is_404_not_every_entitys_observations() {
    for key in [URN, "pm10", ""] {
        let broker = BrokerStub::start(vec![series(3)]).await;
        let path = format!("/api/endpoint/{SLUG}/sta/v1.1/Datastreams('{key}')/Observations");
        let (status, body, _) = get(app(&broker.url, &[]), &path).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{key:?}: {body}");
        assert!(broker.hops().is_empty(), "{key:?} reached the broker");
    }
}

/// EP-12: `$orderby` sorts the page the broker gave; it is not a second query.
#[tokio::test]
async fn orderby_desc_sorts_the_page_the_caller_was_given_not_a_second_broker_query() {
    let broker = BrokerStub::start(vec![series(4)]).await;
    let (status, body, _) = get(
        app(&broker.url, &[]),
        &observations("?$orderby=phenomenonTime%20desc"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let results: Vec<i64> = body["value"]
        .as_array()
        .expect("a page")
        .iter()
        .filter_map(|o| o["result"].as_i64())
        .collect();
    assert_eq!(results, [3, 2, 1, 0], "{body}");
    assert_eq!(broker.hops().len(), 1);
}

/// EP-13: a full page links the next one at `$skip + $top`; an exactly complete one links none.
#[tokio::test]
async fn the_next_link_offset_is_correct_when_the_page_is_exactly_full() {
    // Asked for 2 after 2, the broker holds 5: the peek says there is more.
    let broker = BrokerStub::start(vec![series(5)]).await;
    let (_, body, _) = get(app(&broker.url, &[]), &observations("?$top=2&$skip=2")).await;
    let next = body["@iot.nextLink"].as_str().expect("a next link");
    assert!(next.ends_with("$top=2&$skip=4"), "{next}");
    assert!(next.starts_with(HOST), "{next}");

    // The broker holds exactly the four the page reaches: no next page.
    let broker = BrokerStub::start(vec![series(4)]).await;
    let (_, body, _) = get(app(&broker.url, &[]), &observations("?$top=2&$skip=2")).await;
    assert!(body.get("@iot.nextLink").is_none(), "{body}");
    assert_eq!(body["value"].as_array().map(Vec::len), Some(2));
}

/// EP-12: a `$filter` that does not parse is a 400 naming `$filter`, before the broker.
#[tokio::test]
async fn a_malformed_dollar_filter_names_the_parameter_in_its_400() {
    for filter in [
        "phenomenonTime%20gt",
        "phenomenonTime%20gt%20yesterday",
        "phenomenonTime%20between%202026-09-01T00:00:00Z",
    ] {
        let broker = BrokerStub::start(vec![series(3)]).await;
        let (status, body, headers) = get(
            app(&broker.url, &[]),
            &observations(&format!("?$filter={filter}")),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{filter}: {body}");
        // The problem names the parameter in `x-parameter` and says what is wrong in `detail`.
        assert_eq!(
            headers.get("x-parameter").and_then(|v| v.to_str().ok()),
            Some("$filter"),
            "{filter}"
        );
        assert!(
            body["detail"].as_str().is_some_and(|d| !d.is_empty()),
            "{body}"
        );
        assert!(broker.hops().is_empty(), "{filter} reached the broker");
    }
}

/// GW26: a time window and the paging ceiling travel together, with one `lastN`.
#[tokio::test]
async fn a_filter_and_the_paging_ceiling_combine_without_double_counting_lastn() {
    let broker = BrokerStub::start(vec![series(3)]).await;
    let (status, body, _) = get(
        app(&broker.url, &[]),
        &observations("?$top=10&$skip=5&$filter=phenomenonTime%20ge%202026-09-01T00:00:00Z"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(last_n(&broker), [16], "one lastN of top + skip + 1");
    let query = &broker.hops()[0].query;
    assert!(query.contains("timerel="), "{query}");
}

/// EP-13: a `$top` of zero is a page of one and a negative one is the default page; neither asks
/// the broker for nothing.
#[tokio::test]
async fn top_of_zero_is_one_and_a_negative_top_is_the_default_never_an_empty_broker_call() {
    for (top, asked) in [("0", 2), ("-1", 101), ("-100", 101), ("abc", 101)] {
        let broker = BrokerStub::start(vec![series(3)]).await;
        let (status, body, _) = get(
            app(&broker.url, &[]),
            &observations(&format!("?$top={top}")),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{top}: {body}");
        assert_eq!(last_n(&broker), [asked], "{top}");
    }
}

/// EP-61: a series narrowed by the endpoint's publication says it is.
#[tokio::test]
async fn a_restricted_series_sets_the_results_restricted_header() {
    let broker = BrokerStub::start(vec![series(3)]).await;
    // CIM 009: the signal goes to a caller who asked for it.
    let asked = [("NGSILD-Results-Restricted", "true")];
    let (status, _, headers) = get_with(
        app(&broker.url, &["operatorPhone"]),
        &observations(""),
        &asked,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        headers
            .get("ngsild-results-restricted")
            .and_then(|v| v.to_str().ok()),
        Some("true")
    );

    let broker = BrokerStub::start(vec![series(3)]).await;
    let (_, _, headers) = get_with(app(&broker.url, &[]), &observations(""), &asked).await;
    assert!(
        headers.get("ngsild-results-restricted").is_none(),
        "nothing was narrowed"
    );
}

/// EP-12: what `series_filter` refuses on sense rather than syntax is relayed as the same 400.
#[tokio::test]
async fn series_filter_errors_from_the_callee_are_relayed_as_400_dollar_filter() {
    for filter in [
        // A window inside an `or` cannot be the window of the whole request.
        "result%20gt%201%20or%20phenomenonTime%20gt%202026-09-01T00:00:00Z",
        "(phenomenonTime%20gt%202026-09-01T00:00:00Z)",
        "phenomenonTime%20gt%202026-09-01T00:00:00Z%202026-09-02T00:00:00Z",
    ] {
        let broker = BrokerStub::start(vec![series(3)]).await;
        let (status, body, headers) = get(
            app(&broker.url, &[]),
            &observations(&format!("?$filter={filter}")),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{filter}: {body}");
        // The problem names the parameter in `x-parameter` and says what is wrong in `detail`.
        assert_eq!(
            headers.get("x-parameter").and_then(|v| v.to_str().ok()),
            Some("$filter"),
            "{filter}"
        );
        assert!(
            body["detail"].as_str().is_some_and(|d| !d.is_empty()),
            "{body}"
        );
        assert!(broker.hops().is_empty(), "{filter} reached the broker");
    }
}
