//! The canonical space surface: what `/cs` and `/cs/{space}` answer, and what the broker
//! behind them actually receives (T-0167, SP-01, SP-03, SP-04, SP-06, SP-10, SP-11).

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::BrokerStub;
use context_gateway::app::{router, Gateway};
use context_gateway::pdp::PolicyPdp;
use context_gateway::proxy::Broker;
use context_gateway::resolver::{Endpoint, Space};
use jc_core::kinds::{Audience, PolicySpec, Representation};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::sync::Arc;
use tower::ServiceExt;

const OPEN: &str = "ovzdusie";
const CLOSED: &str = "uctovnictvo";

fn policy(yaml: &str) -> PolicySpec {
    serde_norway::from_str(yaml).expect("the policy spec parses")
}

/// A space whose policy set grants the `public` role, so anonymous callers discover it.
fn open_space() -> Space {
    space(
        OPEN,
        vec![policy(
            r#"contextSpaceRef: ovzdusie
assigner: did:web:banskabystrica.sk
assignee: { kind: role, id: public }
operations: [queryEntity, retrieveEntity]
information:
  - entities:
      - type: AirQualityObserved
    propertyNames: [pm10, location]
"#,
        )],
    )
}

/// A space whose only grant names a role nobody anonymous holds (SP-11, R20).
fn closed_space() -> Space {
    space(
        CLOSED,
        vec![policy(
            r#"contextSpaceRef: uctovnictvo
assigner: did:web:banskabystrica.sk
assignee: { kind: role, id: accountant }
operations: [queryEntity]
information:
  - entities:
      - type: Invoice
"#,
        )],
    )
}

fn space(name: &str, policies: Vec<PolicySpec>) -> Space {
    let mut title = BTreeMap::new();
    title.insert("sk".to_owned(), format!("Priestor {name}"));
    title.insert("en".to_owned(), format!("The {name} space"));
    Space {
        endpoint: Arc::new(Endpoint {
            declared_types: None,
            roles: Default::default(),
            slug: name.to_owned(),
            title: std::collections::BTreeMap::new(),
            description: std::collections::BTreeMap::new(),
            space: name.to_owned(),
            project: name.to_owned(),
            audience: Audience::Public,
            allowed_projects: Vec::new(),
            representations: vec![Representation::NgsiLd, Representation::Mcp],
            rate_limit: None,
            file_limits: None,
            hidden_attributes: Default::default(),
            projection: None,
            base_path: format!("/cs/{name}"),
            models: Vec::new(),
            view_mapping: None,
            policies,
        }),
        title,
        description: BTreeMap::new(),
        is_sandbox: false,
        default_locale: Some("sk".to_owned()),
    }
}

const HOST: &str = "https://bb.example.sk";

/// The gateway as it is deployed: a realm it trusts and a public URL, so the records
/// carry the IRIs a catalogue can dereference.
fn gateway(broker: &str) -> axum::Router {
    let realm = common::Realm::new();
    router(Arc::new(
        Gateway::new(
            Broker::new(broker),
            Box::new(PolicyPdp),
            "banskabystrica.sk",
        )
        .serve_spaces([open_space(), closed_space()])
        .authenticate(
            Arc::new(realm.verifier()),
            context_gateway::auth::accounts::ServiceAccounts::new(),
            Some(HOST.to_owned()),
        ),
    ))
}

async fn call(request: Request<Body>) -> (StatusCode, String, String) {
    let app = gateway("http://127.0.0.1:1");
    let response = app.oneshot(request).await.expect("the gateway answers");
    let status = response.status();
    let media = response
        .headers()
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    let body = axum::body::to_bytes(response.into_body(), 256 * 1024)
        .await
        .expect("a readable body");
    (status, media, String::from_utf8_lossy(&body).into_owned())
}

fn get_with(path: &str, accept: Option<&str>) -> Request<Body> {
    let mut builder = Request::builder().uri(path);
    if let Some(accept) = accept {
        builder = builder.header(axum::http::header::ACCEPT, accept);
    }
    builder.body(Body::empty()).expect("a request")
}

/// SP-10: the space record is a DCAT-AP dataset whose services are its children.
#[tokio::test]
async fn the_space_record_is_a_dcat_ap_dataset() {
    let (status, media, body) = call(get_with(&format!("/cs/{OPEN}"), None)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(media, "application/ld+json");

    let record: Value = serde_json::from_str(&body).expect("json-ld");
    assert_eq!(record["@type"], json!("dcat:Dataset"));
    assert_eq!(record["dct:identifier"], json!(OPEN));
    assert_eq!(record["dct:language"], json!("sk"));
    // The title is a language map, both locales tagged, so a catalogue keeps them apart.
    let titles = record["dct:title"].as_array().expect("a language map");
    assert!(titles
        .iter()
        .any(|entry| entry["@language"] == json!("sk")
            && entry["@value"] == json!("Priestor ovzdusie")));

    let services = record["dcat:service"].as_array().expect("services");
    let endpoints: Vec<&str> = services
        .iter()
        .filter_map(|service| service["dcat:endpointURL"].as_str())
        .collect();
    assert!(
        endpoints.contains(&format!("{HOST}/cs/{OPEN}/ngsi-ld/v1/").as_str()),
        "{endpoints:?}"
    );
    assert!(
        endpoints.contains(&format!("{HOST}/cs/{OPEN}/mcp").as_str()),
        "{endpoints:?}"
    );
}

/// SP-10: the same record as Turtle, for a triple store or a partner's connector.
#[tokio::test]
async fn the_space_record_is_also_turtle() {
    let (status, media, body) = call(get_with(&format!("/cs/{OPEN}"), Some("text/turtle"))).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(media, "text/turtle");
    assert!(body.contains("a dcat:Dataset ;"), "{body}");
    assert!(body.contains("a dcat:DataService ;"), "{body}");
    assert!(
        body.contains(&format!("<{HOST}/cs/{OPEN}/ngsi-ld/v1/>")),
        "{body}"
    );
}

/// SP-10: a browser gets a page, and the page cannot be closed by a manifest's title.
#[tokio::test]
async fn a_browser_gets_a_page() {
    let (status, media, body) = call(get_with(
        &format!("/cs/{OPEN}"),
        Some("text/html,application/xhtml+xml,*/*;q=0.8"),
    ))
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(media, "text/html; charset=utf-8");
    assert!(body.starts_with("<!doctype html>"), "{body}");
    assert!(
        body.contains(&format!("{HOST}/cs/{OPEN}/ngsi-ld/v1/")),
        "{body}"
    );
}

/// SP-06, SP-11, R20: a space the caller holds no grant on answers exactly what a space
/// that does not exist answers, so a probe over the namespace learns nothing.
#[tokio::test]
async fn an_ungranted_space_and_an_unknown_one_are_indistinguishable() {
    let (ungranted_status, ungranted_media, ungranted_body) =
        call(get_with(&format!("/cs/{CLOSED}"), None)).await;
    let (unknown_status, unknown_media, unknown_body) =
        call(get_with("/cs/nothing-here", None)).await;

    assert_eq!(ungranted_status, StatusCode::NOT_FOUND);
    assert_eq!(ungranted_status, unknown_status);
    assert_eq!(ungranted_media, unknown_media);
    assert_eq!(ungranted_body, unknown_body);
}

/// The record was the only surface that answered the same for both; the data surfaces told a
/// closed space from a missing one, and space names are guessable words (T-0814, R20, SP-06).
#[tokio::test]
async fn the_data_surface_of_an_ungranted_space_answers_what_an_unknown_one_answers() {
    for path in [
        "/ngsi-ld/v1/entities?type=AirQualityObserved",
        "/ngsi-ld/v1/entities/urn:ngsi-ld:AirQualityObserved:x",
    ] {
        let (closed_status, closed_media, closed_body) =
            call(get_with(&format!("/cs/{CLOSED}{path}"), None)).await;
        let (unknown_status, unknown_media, unknown_body) =
            call(get_with(&format!("/cs/nothing-here{path}"), None)).await;

        assert_eq!(closed_status, StatusCode::NOT_FOUND, "{path}");
        assert_eq!(closed_status, unknown_status, "{path}");
        assert_eq!(closed_media, unknown_media, "{path}");
        assert_eq!(closed_body, unknown_body, "{path}");
    }
}

/// A bearer that does not verify is the same oracle by another door: 401 for a space that
/// exists and 404 for one that does not tells the caller which names are real (T-0814).
#[tokio::test]
async fn a_token_that_does_not_verify_says_no_more_than_an_unknown_space_does() {
    let garbage = Some("Bearer not-a-token");
    for path in ["", "/ngsi-ld/v1/entities?type=AirQualityObserved"] {
        let (closed_status, closed_media, closed_body) =
            call(get_with(&format!("/cs/{CLOSED}{path}"), garbage)).await;
        let (unknown_status, unknown_media, unknown_body) =
            call(get_with(&format!("/cs/nothing-here{path}"), garbage)).await;

        assert_eq!(closed_status, StatusCode::NOT_FOUND, "{path}");
        assert_eq!(closed_status, unknown_status, "{path}");
        assert_eq!(closed_media, unknown_media, "{path}");
        assert_eq!(closed_body, unknown_body, "{path}");
    }
}

/// SP-11: the catalog is narrowed by the same policy layer that enforces requests.
#[tokio::test]
async fn the_catalog_lists_only_what_the_caller_may_discover() {
    let (status, media, body) = call(get_with("/cs", None)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(media, "application/ld+json");

    let catalog: Value = serde_json::from_str(&body).expect("json-ld");
    assert_eq!(catalog["@type"], json!("dcat:Catalog"));
    let names: Vec<&str> = catalog["dcat:dataset"]
        .as_array()
        .expect("datasets")
        .iter()
        .filter_map(|dataset| dataset["dct:identifier"].as_str())
        .collect();
    assert_eq!(
        names,
        vec![OPEN],
        "the closed space is absent, not listed as denied"
    );
}

/// SP-04: only the children the requirement permits exist; anything else is the same 404.
#[tokio::test]
async fn an_invented_child_path_is_not_part_of_the_surface() {
    let (status, _, _) = call(get_with(&format!("/cs/{OPEN}/entities"), None)).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _, _) = call(get_with(&format!("/cs/{OPEN}/ngsi-ld/v2/entities"), None)).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

/// SP-03, SP-07: the broker sees the plain CIM 009 path with the tenant pinned to the
/// space in the URL, and never the `/cs/{space}` prefix the client used.
#[tokio::test]
async fn the_prefix_is_stripped_and_the_tenant_is_pinned() {
    let broker = BrokerStub::start(vec![json!([])]).await;

    let response = gateway(&broker.url)
        .oneshot(get_with(
            &format!("/cs/{OPEN}/ngsi-ld/v1/entities?type=AirQualityObserved"),
            None,
        ))
        .await
        .expect("the gateway answers");
    assert_eq!(response.status(), StatusCode::OK);

    let hops = broker.hops();
    let hop = hops.first().expect("the broker was called");
    assert_eq!(hop.path, "/ngsi-ld/v1/entities");
    assert_eq!(hop.tenant, OPEN);
}

/// SP-05, SP-07, GW20: a client that forges the tenant header has it removed before
/// anything reads it, and the broker is pinned to the space the path named.
#[tokio::test]
async fn a_forged_tenant_header_never_reaches_the_broker() {
    let broker = BrokerStub::start(vec![json!([])]).await;

    let request = Request::builder()
        .uri(format!(
            "/cs/{OPEN}/ngsi-ld/v1/entities?type=AirQualityObserved"
        ))
        .header("NGSILD-Tenant", "somebody-elses-space")
        .body(Body::empty())
        .expect("a request");
    let response = gateway(&broker.url)
        .oneshot(request)
        .await
        .expect("the gateway answers");
    assert_eq!(response.status(), StatusCode::OK);

    let hops = broker.hops();
    let hop = hops.first().expect("the broker was called");
    assert_eq!(hop.tenant, OPEN);
    assert!(!hop.forged, "the client\'s tenant claim reached the broker");
}

/// OPS-17: the W3C Trace Context a caller (or APISIX in front of it) sends reaches the broker
/// as it was, so one trace spans the edge, the gateway and the broker. The gateway relays it and
/// starts no trace of its own. Both surfaces forward through `ngsi_ld_request`, so one proves
/// the path for both.
#[tokio::test]
async fn the_trace_context_reaches_the_broker_unchanged() {
    const PARENT: &str = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";
    const STATE: &str = "congo=t61rcWkgMzE";
    let broker = BrokerStub::start(vec![json!([])]).await;
    let request = Request::builder()
        .uri(format!(
            "/cs/{OPEN}/ngsi-ld/v1/entities?type=AirQualityObserved"
        ))
        .header("traceparent", PARENT)
        .header("tracestate", STATE)
        .body(Body::empty())
        .expect("a request");
    let response = gateway(&broker.url)
        .oneshot(request)
        .await
        .expect("the gateway answers");
    assert_eq!(response.status(), StatusCode::OK);
    let hops = broker.hops();
    let hop = hops.first().expect("the broker was called");
    assert_eq!(hop.trace, (PARENT.to_owned(), STATE.to_owned()));
}

/// The space surface refuses an operation the space's policy set does not grant, through
/// the same PDP the endpoint surface uses (EP-06, EP-07).
#[tokio::test]
async fn an_ungranted_operation_is_refused_on_the_space_surface_too() {
    let broker = BrokerStub::start(vec![json!([])]).await;

    let request = Request::builder()
        .method("POST")
        .uri(format!("/cs/{OPEN}/ngsi-ld/v1/entities"))
        .header("content-type", "application/ld+json")
        .body(Body::from(
            json!({ "id": "urn:x", "type": "AirQualityObserved" }).to_string(),
        ))
        .expect("a request");
    let response = gateway(&broker.url)
        .oneshot(request)
        .await
        .expect("the gateway answers");
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    assert!(
        broker.hops().is_empty(),
        "a refused write must not reach the broker"
    );
}

/// T-0941, T-0942, SP-05, R22: the real router carries the response layer. A narrowed read
/// answers the narrowing signal only to a caller who asked, and no answer carries the tenant.
#[tokio::test]
async fn the_answer_carries_no_tenant_and_no_unasked_narrowing_signal() {
    // The open space grants `pm10` and `location`; `pm25` is outside it, so the projection
    // narrows this entity and the handler sets the signal.
    let narrowed = json!([{
        "id": "urn:ngsi-ld:AirQualityObserved:banskabystrica.sk:ovzdusie:st-1",
        "type": "AirQualityObserved",
        "pm10": { "type": "Property", "value": 12 },
        "pm25": { "type": "Property", "value": 7 },
    }]);
    let path = format!("/cs/{OPEN}/ngsi-ld/v1/entities?type=AirQualityObserved&attrs=pm10,pm25");

    for (asks, expected) in [(false, false), (true, true)] {
        let broker = BrokerStub::start(vec![narrowed.clone()]).await;
        let mut builder = Request::builder().uri(&path);
        if asks {
            builder = builder.header("NGSILD-Results-Restricted", "true");
        }
        let response = gateway(&broker.url)
            .oneshot(builder.body(Body::empty()).expect("a request"))
            .await
            .expect("the gateway answers");
        assert_eq!(response.status(), StatusCode::OK);
        assert!(
            !response.headers().contains_key("ngsild-tenant"),
            "the answer handed the caller the space's internal name (SP-05)"
        );
        assert_eq!(
            response.headers().contains_key("ngsild-results-restricted"),
            expected,
            "asked={asks}: the narrowing signal is opt-in (R22)"
        );
        let body = axum::body::to_bytes(response.into_body(), 256 * 1024)
            .await
            .expect("a readable body");
        let entities: Value = serde_json::from_slice(&body).expect("a JSON array");
        assert!(
            entities[0].get("pm25").is_none(),
            "the answer was not actually narrowed, so this test proves nothing"
        );
    }
}
