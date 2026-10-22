//! Edge cases of `files::file_csv`, `GET /api/endpoint/{slug}/file.csv` (T-1884, EP-26, MP-02),
//! the one file an endpoint publishes since T-4597; these cases guarded `file.csv` before.
//!
//! Contract, in one sentence: it answers a table of exactly the entities this caller
//! is granted — the endpoint's own tenant, the grant's types, the grant's attributes minus the
//! endpoint's hidden ones, inside the granted areas — and for every caller it does not serve it
//! answers the one `404` that says nothing about whether the endpoint, the representation or the
//! grant was missing (EP-05, EP-08, EP-61, GW11, R9).

mod common;

use axum::body::Body;
use axum::http::{Request as HttpRequest, Response, StatusCode};
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
const PUBLIC_READ: &str = r#"contextSpaceRef: ovzdusie
assigner: did:web:banskabystrica.sk
assignee: { kind: role, id: public }
operations: [queryEntity, retrieveEntity]
"#;

/// The dev seed's public endpoint: anonymous callers read, nothing narrows what they read.
fn endpoint() -> Endpoint {
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
        hidden_attributes: BTreeSet::new(),
        projection: None,
        base_path: format!("/api/endpoint/{SLUG}"),
        models: Vec::new(),
        view_mapping: None,
        catalog: None,
        deprecation: None,
        policy_names: Vec::new(),
        manifest_name: String::new(),
        policies: vec![serde_norway::from_str(PUBLIC_READ).expect("the policy parses")],
    }
}

fn with_policy(yaml: &str) -> Endpoint {
    Endpoint {
        roles: Default::default(),
        policy_names: Vec::new(),
        manifest_name: String::new(),
        policies: vec![serde_norway::from_str(yaml).expect("the policy parses")],
        ..endpoint()
    }
}

/// A grant over one box around Banská Bystrica and nothing else.
fn with_geo_grant() -> Endpoint {
    with_policy(
        r#"contextSpaceRef: ovzdusie
assigner: did:web:banskabystrica.sk
assignee: { kind: role, id: public }
operations: [queryEntity, retrieveEntity]
information:
  - entities:
      - type: AirQualityObserved
geoQ: "georel=within;geometry=Polygon;coordinates=[[[19.0,48.6],[19.3,48.6],[19.3,48.9],[19.0,48.9],[19.0,48.6]]]"
"#,
    )
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

fn station(local: &str, lon: f64, lat: f64) -> Value {
    json!({
        "id": format!("urn:ngsi-ld:AirQualityObserved:banskabystrica.sk:ovzdusie:{local}"),
        "type": "AirQualityObserved",
        "pm10": { "type": "Property", "value": 34.2 },
        "operator": { "type": "Property", "value": "SHMU" },
        "location": {
            "type": "GeoProperty",
            "value": { "type": "Point", "coordinates": [lon, lat] }
        }
    })
}

async fn send(app: axum::Router, request: HttpRequest<Body>) -> (StatusCode, String) {
    let response: Response<Body> = app.oneshot(request).await.expect("the gateway answers");
    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), 16 * 1024 * 1024)
        .await
        .expect("a readable body");
    (status, String::from_utf8_lossy(&body).into_owned())
}

fn at(path: &str) -> HttpRequest<Body> {
    HttpRequest::builder()
        .uri(path)
        .body(Body::empty())
        .expect("a request")
}

async fn get(app: axum::Router, path: &str) -> (StatusCode, String) {
    send(app, at(path)).await
}

/// The first case: an endpoint that does not serve CSV is not an endpoint at this URL, and the
/// broker is never asked — a caller must not be able to tell a representation apart from a grant by
/// what the gateway does upstream (EP-05).
#[tokio::test]
async fn an_endpoint_that_does_not_serve_csv_is_not_found_and_the_broker_is_not_asked() {
    let broker = BrokerStub::start(vec![json!([station("station-01", 19.14, 48.73)])]).await;
    let mut only_ngsi = endpoint();
    only_ngsi.representations = vec![Representation::NgsiLd];

    let (status, body) = get(
        gateway(&broker.url, only_ngsi),
        &format!("/api/endpoint/{SLUG}/file.csv"),
    )
    .await;

    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert!(broker.hops().is_empty(), "{:?}", broker.hops());
}

/// Refused, absent and not served look the same: the same status and the same body, so nobody can
/// map the endpoints of a deployment by reading the differences.
#[tokio::test]
async fn an_unknown_slug_and_a_representation_not_served_answer_the_same_bytes() {
    let broker = BrokerStub::start(vec![json!([])]).await;
    let mut only_ngsi = endpoint();
    only_ngsi.representations = vec![Representation::NgsiLd];

    let (unknown_status, unknown_body) = get(
        gateway(&broker.url, endpoint()),
        "/api/endpoint/xxxxxxxxxxxxxxxxxxxxxxxxxx/file.csv",
    )
    .await;
    let (unserved_status, unserved_body) = get(
        gateway(&broker.url, only_ngsi),
        &format!("/api/endpoint/{SLUG}/file.csv"),
    )
    .await;

    assert_eq!(unknown_status, StatusCode::NOT_FOUND);
    assert_eq!(unserved_status, unknown_status);
    assert_eq!(unserved_body, unknown_body, "the two must be one answer");
    assert!(broker.hops().is_empty(), "neither asked the broker");
}

/// No other spelling of a slug is that slug: another case, a shorter or longer one, a trailing
/// space, a trailing slash and a percent-encoded copy are all `404`.
#[tokio::test]
async fn no_other_spelling_of_the_slug_is_that_slug() {
    let broker = BrokerStub::start(vec![json!([station("station-01", 19.14, 48.73)])]).await;
    let upper = SLUG.to_uppercase();
    let short = &SLUG[..SLUG.len() - 1];
    let long = format!("{SLUG}a");
    let spaced = format!("{SLUG}%20");
    let doubled = format!("{SLUG}%2520");

    for slug in [
        upper.as_str(),
        short,
        long.as_str(),
        spaced.as_str(),
        doubled.as_str(),
    ] {
        let (status, body) = get(
            gateway(&broker.url, endpoint()),
            &format!("/api/endpoint/{slug}/file.csv"),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{slug}: {body}");
    }
    assert!(broker.hops().is_empty(), "{:?}", broker.hops());
}

/// A tenant header the caller forged never reaches the broker: the hop carries the endpoint's own
/// space, once, whatever the request said (GW25).
#[tokio::test]
async fn a_tenant_the_caller_forged_never_reaches_the_broker() {
    let broker = BrokerStub::start(vec![json!([station("station-01", 19.14, 48.73)])]).await;
    let request = HttpRequest::builder()
        .uri(format!("/api/endpoint/{SLUG}/file.csv"))
        .header("NGSILD-Tenant", "somebody-elses-space")
        .header("ngsild-tenant", "another-space")
        .body(Body::empty())
        .expect("a request");

    let (status, body) = send(gateway(&broker.url, endpoint()), request).await;

    assert_eq!(status, StatusCode::OK, "{body}");
    let hops = broker.hops();
    assert!(!hops.is_empty(), "the query was made");
    assert!(
        hops.iter()
            .all(|hop| !hop.forged && hop.tenant == "ovzdusie"),
        "{hops:?}"
    );
}

/// The download reads through the Endpoint's view (T-3651, plan 5.7): an entity of a type the
/// view does not show the public is not in the table, and the hop carried the caller the engine
/// narrows for.
#[tokio::test]
async fn an_entity_of_an_ungranted_type_is_not_in_the_table() {
    let mut intruder = station("station-02", 19.15, 48.74);
    intruder["type"] = json!("WaterQualityObserved");
    intruder["id"] =
        json!("urn:ngsi-ld:WaterQualityObserved:banskabystrica.sk:ovzdusie:station-02");
    let broker = BrokerStub::narrowing(
        vec![json!([station("station-01", 19.14, 48.73), intruder])],
        View::of(SLUG, "public", &["AirQualityObserved"], None),
    )
    .await;

    let (status, body) = get(
        gateway(&broker.url, endpoint()),
        &format!("/api/endpoint/{SLUG}/file.csv"),
    )
    .await;

    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(!body.contains("WaterQualityObserved"), "{body}");
    assert!(body.contains("station-01"), "{body}");
    let hops = broker.hops();
    let query = hops
        .iter()
        .find(|hop| hop.path == "/ngsi-ld/v1/entities")
        .expect("a query");
    assert_eq!(query.view, SLUG, "{hops:?}");
    assert_eq!(query.headers["x-jc-roles"], "public");
}

/// An attribute the view withholds is in no column (EP-61): the Portal writes the endpoint's
/// hidden names into the view's rules, and the table holds what the view answers.
#[tokio::test]
async fn a_hidden_attribute_is_in_no_column() {
    let broker = BrokerStub::narrowing(
        vec![json!([station("station-01", 19.14, 48.73)])],
        View::of(
            SLUG,
            "public",
            &["AirQualityObserved"],
            Some(&["pm10", "location"]),
        ),
    )
    .await;

    let (status, body) = get(
        gateway(&broker.url, endpoint()),
        &format!("/api/endpoint/{SLUG}/file.csv?type=AirQualityObserved"),
    )
    .await;

    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body.contains("pm10"), "what is published comes out: {body}");
    assert!(
        !body.contains("operator"),
        "and what is hidden does not: {body}"
    );
    assert!(!body.contains("SHMU"), "its value neither: {body}");
}

/// A filter naming an attribute goes to the view as the caller wrote it, under the caller the
/// gateway verified: the engine judges a filter on a hidden attribute (Antares
/// `addons/joinedcontext-policy/tests/engine.rs` `bisecting_a_hidden_value_learns_nothing`), and
/// the gateway neither drops the filter nor widens it with a grant (T-3651).
#[tokio::test]
async fn a_filter_goes_to_the_view_as_the_caller_wrote_it() {
    for (query, sent) in [
        (
            "type=AirQualityObserved&attrs=pm10,operator",
            "attrs=pm10%2Coperator",
        ),
        (
            "type=AirQualityObserved&q=operator==%22SHMU%22",
            "q=operator%3D%3D%22SHMU%22",
        ),
        (
            "type=AirQualityObserved&orderBy=operator",
            "type=AirQualityObserved",
        ),
    ] {
        let broker = BrokerStub::narrowing(
            vec![json!([station("station-01", 19.14, 48.73)])],
            View::of(
                SLUG,
                "public",
                &["AirQualityObserved"],
                Some(&["pm10", "location"]),
            ),
        )
        .await;

        let (status, body) = get(
            gateway(&broker.url, endpoint()),
            &format!("/api/endpoint/{SLUG}/file.csv?{query}"),
        )
        .await;

        assert_eq!(status, StatusCode::OK, "{query}: {body}");
        assert!(!body.contains("SHMU"), "{query}: {body}");
        let hops = broker.hops();
        let hop = hops
            .iter()
            .find(|hop| hop.path == "/ngsi-ld/v1/entities")
            .expect("a query");
        assert_eq!(hop.view, SLUG, "{query}");
        assert!(hop.query.contains(sent), "{query}: {}", hop.query);
    }
}

/// The gateway adds no grant to the query: a download the caller drew no area for asks the view
/// with no area, and the view's engine applies the grant's own (Antares `views_q.rs`, map 7.6);
/// a caller's own area goes on as drawn.
#[tokio::test]
async fn no_grant_is_folded_into_the_query_and_the_callers_area_goes_as_drawn() {
    let wide = "georel=within&geometry=Polygon&coordinates=%5B%5B%5B0.0%2C40.0%5D%2C%5B30.0%2C40.0%5D%2C%5B30.0%2C70.0%5D%2C%5B0.0%2C70.0%5D%2C%5B0.0%2C40.0%5D%5D%5D";
    for (asked, area) in [("", false), (wide, true)] {
        let broker = BrokerStub::narrowing(
            vec![json!([station("inside", 19.14, 48.73)])],
            View::of(SLUG, "public", &["AirQualityObserved"], None),
        )
        .await;

        let (status, body) = get(
            gateway(&broker.url, with_geo_grant()),
            &format!("/api/endpoint/{SLUG}/file.csv?{asked}"),
        )
        .await;

        assert_eq!(status, StatusCode::OK, "{body}");
        let hops = broker.hops();
        let query = &hops.last().expect("an entity query").query;
        assert_eq!(query.contains("georel=within"), area, "{query}");
        assert!(
            !query.contains("19.3"),
            "the grant's corner is the engine's: {query}"
        );
    }
}

/// A caller no rule of the view names is refused by the engine, and the gateway's answer is the
/// platform's own `403`, which names nothing behind it.
#[tokio::test]
async fn a_caller_the_view_does_not_serve_is_refused() {
    let broker = BrokerStub::narrowing(
        vec![json!([station("station-01", 19.14, 48.73)])],
        View::of(SLUG, "steward", &["AirQualityObserved"], None),
    )
    .await;

    let (status, body) = get(
        gateway(&broker.url, endpoint()),
        &format!("/api/endpoint/{SLUG}/file.csv"),
    )
    .await;

    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert!(!body.contains("station-01"), "{body}");
    assert!(!body.contains("urn:antares"), "{body}");
}

/// A non-spatial answer is a `200` table: an entity without a location is a row without a
/// location column, never a refusal (EP-08, T-2939).
#[tokio::test]
async fn an_answer_with_no_geometry_is_a_table_without_location_columns() {
    let mut flat = station("station-01", 0.0, 0.0);
    flat.as_object_mut().expect("an object").remove("location");
    let broker = BrokerStub::start(vec![json!([flat])]).await;

    let (status, body) = get(
        gateway(&broker.url, endpoint()),
        &format!("/api/endpoint/{SLUG}/file.csv?type=AirQualityObserved"),
    )
    .await;

    assert_eq!(status, StatusCode::OK, "{body}");
    let lines: Vec<&str> = body.lines().collect();
    assert_eq!(lines.len(), 2, "a header and one row: {body}");
    assert!(lines[0].starts_with("id,type"), "{body}");
    assert!(!lines[0].contains("location"), "{body}");
    assert!(lines[1].contains("station-01"), "{body}");
}

/// The answer's media type is CSV's own, and the caller's `Accept` decides nothing about it:
/// this route is one document in one format.
#[tokio::test]
async fn the_answer_is_csv_whatever_the_caller_accepts() {
    for accept in [
        "application/geo+json",
        "text/csv",
        "*/*",
        "application/xml;q=1.0",
        "",
    ] {
        let broker = BrokerStub::start(vec![json!([station("station-01", 19.14, 48.73)])]).await;
        let request = HttpRequest::builder()
            .uri(format!("/api/endpoint/{SLUG}/file.csv"))
            .header("Accept", accept)
            .body(Body::empty())
            .expect("a request");
        let response = gateway(&broker.url, endpoint())
            .oneshot(request)
            .await
            .expect("the gateway answers");

        assert_eq!(response.status(), StatusCode::OK, "{accept:?}");
        assert_eq!(
            response
                .headers()
                .get("content-type")
                .and_then(|value| value.to_str().ok()),
            Some("text/csv; charset=utf-8; header=present"),
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

/// A type the view holds nothing of is an empty table, not an error.
#[tokio::test]
async fn a_type_the_view_holds_nothing_of_is_an_empty_table() {
    let broker = BrokerStub::narrowing(
        vec![json!([station("station-01", 19.14, 48.73)])],
        View::of(SLUG, "public", &["AirQualityObserved"], None),
    )
    .await;

    let (status, body) = get(
        gateway(&broker.url, endpoint()),
        &format!("/api/endpoint/{SLUG}/file.csv?type=NothingLikeThis"),
    )
    .await;

    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(!body.contains("station-01"), "{body}");
    assert!(body.lines().count() <= 1, "no row: {body}");
}

/// A refusal says nothing about what is behind it: no address, no tenant, no file path, not the
/// broker's own error type.
#[tokio::test]
async fn a_refusal_names_nothing_internal() {
    let broker = BrokerStub::narrowing(
        vec![json!([station("station-01", 0.0, 0.0)])],
        View::of(SLUG, "steward", &["AirQualityObserved"], None),
    )
    .await;

    let (status, body) = get(
        gateway(&broker.url, endpoint()),
        &format!("/api/endpoint/{SLUG}/file.csv?type=AirQualityObserved"),
    )
    .await;

    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    for internal in [
        "127.0.0.1",
        "ovzdusie",
        "/src/",
        "Bearer",
        "policy",
        "antares",
    ] {
        assert!(!body.contains(internal), "{internal} came out in {body}");
    }
}

/// Only a read reaches this route: a write method is refused by the surface itself and never
/// becomes a broker call.
#[tokio::test]
async fn a_write_method_on_a_download_is_refused() {
    for method in ["POST", "PUT", "PATCH", "DELETE"] {
        let broker = BrokerStub::start(vec![json!([station("station-01", 19.14, 48.73)])]).await;
        let request = HttpRequest::builder()
            .method(method)
            .uri(format!("/api/endpoint/{SLUG}/file.csv"))
            .body(Body::empty())
            .expect("a request");
        let (status, body) = send(gateway(&broker.url, endpoint()), request).await;

        assert!(
            status == StatusCode::METHOD_NOT_ALLOWED || status == StatusCode::NOT_FOUND,
            "{method}: {status} {body}"
        );
        assert!(broker.hops().is_empty(), "{method}: {:?}", broker.hops());
    }
}

/// A caller who asks about narrowing has the view asked too, and a download the view narrowed
/// says so in `NGSILD-Results-Restricted` (EP-43, R22); one it did not narrow, or a caller who
/// did not ask, carries no mark.
#[tokio::test]
async fn a_narrowed_download_says_so_to_a_caller_who_asked() {
    for (attrs, asked, marked) in [
        (Some(&["pm10", "location"][..]), true, true),
        (None, true, false),
        (Some(&["pm10", "location"][..]), false, false),
    ] {
        let broker = BrokerStub::narrowing(
            vec![json!([station("station-01", 19.14, 48.73)])],
            View::of(SLUG, "public", &["AirQualityObserved"], attrs),
        )
        .await;
        let mut request = HttpRequest::builder().uri(format!("/api/endpoint/{SLUG}/file.csv"));
        if asked {
            request = request.header("NGSILD-Results-Restricted", "true");
        }
        let response = gateway(&broker.url, endpoint())
            .oneshot(request.body(Body::empty()).expect("a request"))
            .await
            .expect("the gateway answers");

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers().contains_key("ngsild-results-restricted"),
            marked,
            "{attrs:?} asked {asked}"
        );
        let hops = broker.hops();
        let hop = hops
            .iter()
            .find(|hop| !hop.view.is_empty())
            .expect("a view hop");
        assert_eq!(hop.headers.contains_key("x-jc-restricted"), asked);
    }
}
