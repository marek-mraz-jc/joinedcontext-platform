//! A batch query carries its whole selector in the body, and only its window in the URL (T-2995).
//!
//! CIM 009 clauses 5.6.9 and 5.6.12 put the query of `POST /entityOperations/query` and of its
//! temporal sibling in the payload. The gateway narrowed that payload to the grants and then
//! also appended the grants' `type`, `attrs`, `q` and areas to the URL, and Antares refuses a
//! parameter the operation does not define with `400`: every batch query under a narrowing
//! grant failed, T-2887's expiry sweep among them. The broker here refuses exactly what Antares
//! refuses, so a narrowing that only lived in the URL shows up as a widening of the body.

use axum::body::Body;
use axum::extract::Request;
use axum::http::{Method, StatusCode};
use axum::routing::any;
use axum::Router;
use context_gateway::app::{router, Gateway};
use context_gateway::pdp::PolicyPdp;
use context_gateway::proxy::Broker;
use context_gateway::resolver::Endpoint;
use jc_core::kinds::{Audience, Representation};
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
use tower::ServiceExt;

const SLUG: &str = "k3v9bq2mzx7rtw4ncy6dph5fsa";
const AREA: &str = "[[[19.0,48.6],[19.3,48.6],[19.3,48.8],[19.0,48.8],[19.0,48.6]]]";

/// One type, two attributes, an area drawn on `location` and a window from 2026 on.
fn grant(with_area: bool) -> String {
    let mut grant = "contextSpaceRef: ovzdusie\n\
         assigner: did:web:banskabystrica.sk\n\
         assignee: { kind: role, id: public }\n\
         operations: [queryEntity, retrieveEntity, queryBatch, queryTemporal]\n\
         information:\n\
         \x20 - entities:\n\
         \x20     - type: AirQualityObserved\n\
         \x20   propertyNames: [pm10, location]\n"
        .to_owned();
    if with_area {
        grant.push_str(&format!(
            "geoQ: \"georel=within;geometry=Polygon;coordinates={AREA}\"\n\
             temporalQ: \"timerel=after;timeAt=2026-01-01T00:00:00Z\"\n"
        ));
    }
    grant
}

fn endpoint(with_area: bool) -> Endpoint {
    Endpoint {
        declared_types: None,
        roles: Default::default(),
        slug: SLUG.to_owned(),
        title: Default::default(),
        description: Default::default(),
        space: "ovzdusie".to_owned(),
        project: "ovzdusie".to_owned(),
        audience: Audience::Public,
        allowed_projects: Vec::new(),
        representations: vec![Representation::NgsiLd],
        rate_limit: None,
        file_limits: None,
        hidden_attributes: Default::default(),
        projection: None,
        base_path: format!("/api/endpoint/{SLUG}"),
        models: Vec::new(),
        view_mapping: None,
        catalog: None,
        policies: vec![serde_norway::from_str(&grant(with_area)).expect("the policy spec parses")],
    }
}

/// What the broker was sent: the query string and the body it read.
type Asked = Arc<Mutex<Vec<(String, Value)>>>;

/// A broker that refuses, as Antares does, any URL parameter the batch queries do not define
/// (antares-api batch.rs and temporal.rs: `limit offset count options format local`).
async fn strict_broker() -> (String, Asked) {
    const WINDOW: [&str; 6] = ["limit", "offset", "count", "options", "format", "local"];
    let asked: Asked = Arc::new(Mutex::new(Vec::new()));
    let recorder = Arc::clone(&asked);
    let app = Router::new().fallback(any(move |request: Request| {
        let recorder = Arc::clone(&recorder);
        async move {
            let query = request.uri().query().unwrap_or_default().to_owned();
            let bytes = axum::body::to_bytes(request.into_body(), 1 << 20)
                .await
                .unwrap_or_default();
            let body = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
            recorder
                .lock()
                .expect("the log")
                .push((query.clone(), body));
            let unknown = query
                .split('&')
                .filter(|pair| !pair.is_empty())
                .map(|pair| pair.split('=').next().unwrap_or_default())
                .find(|name| !WINDOW.contains(name));
            match unknown {
                Some(name) => (
                    StatusCode::BAD_REQUEST,
                    axum::Json(json!({ "title": format!("unknown query parameter \"{name}\"") })),
                ),
                None => (StatusCode::OK, axum::Json(json!([]))),
            }
        }
    }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("a free port");
    let address = listener.local_addr().expect("the bound address");
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (format!("http://{address}"), asked)
}

/// One POST query through the gateway: its status, its answer and what the broker was sent.
async fn query(
    with_area: bool,
    operation: &str,
    window: &str,
    body: Value,
) -> (StatusCode, String, Vec<(String, Value)>) {
    let (upstream, asked) = strict_broker().await;
    let gateway = Arc::new(
        Gateway::new(
            Broker::new(upstream),
            Box::new(PolicyPdp),
            "banskabystrica.sk",
        )
        .serve([endpoint(with_area)]),
    );
    let response = router(gateway)
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri(format!(
                    "/api/endpoint/{SLUG}/ngsi-ld/v1/{operation}{window}"
                ))
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .expect("a request"),
        )
        .await
        .expect("the gateway answers");
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("a body");
    let sent = asked.lock().expect("the log").clone();
    (status, String::from_utf8_lossy(&bytes).into_owned(), sent)
}

fn only_hop(sent: &[(String, Value)]) -> (String, Value) {
    assert_eq!(sent.len(), 1, "the broker was asked once: {sent:?}");
    sent[0].clone()
}

/// T-2995: the expiry sweep's own request. Under a type grant the broker sees the caller's
/// window in the URL and nothing else, and the grant's type and attributes in the body.
#[tokio::test]
async fn a_batch_query_under_a_type_grant_sends_the_type_in_the_body_only() {
    let (status, answer, sent) = query(
        false,
        "entityOperations/query",
        "?options=sysAttrs&limit=1000&offset=0",
        json!({ "type": "Query", "entities": [{ "type": "AirQualityObserved" }] }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{answer}");
    let (url, body) = only_hop(&sent);
    assert_eq!(url, "options=sysAttrs&limit=1000&offset=0");
    assert_eq!(body["entities"], json!([{ "type": "AirQualityObserved" }]));
    assert_eq!(body["attrs"], json!(["location", "pm10"]));
}

/// No widening: a body that names no entity is a query of the granted types and attributes, one
/// that names its own attributes keeps those the grant covers, and one naming only an ungranted
/// type or attribute is answered empty without the broker (T-1862).
#[tokio::test]
async fn a_body_selects_nothing_the_grant_does_not() {
    let (status, answer, sent) = query(
        false,
        "entityOperations/query",
        "",
        json!({ "type": "Query", "q": "pm10>10" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{answer}");
    let (url, body) = only_hop(&sent);
    assert_eq!(url, "");
    assert_eq!(body["entities"], json!([{ "type": "AirQualityObserved" }]));
    assert_eq!(body["attrs"], json!(["location", "pm10"]));
    assert_eq!(body["q"], json!("pm10>10"));

    let (status, answer, sent) = query(
        false,
        "entityOperations/query",
        "",
        json!({ "type": "Query", "entities": [{ "type": "AirQualityObserved" }], "attrs": ["pm10"] }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{answer}");
    assert_eq!(only_hop(&sent).1["attrs"], json!(["pm10"]));

    for body in [
        json!({ "type": "Query", "entities": [{ "type": "Invoice" }] }),
        json!({ "type": "Query", "entities": [{ "type": "AirQualityObserved" }], "attrs": ["operatorPhone"] }),
    ] {
        let (status, answer, sent) = query(false, "entityOperations/query", "", body).await;
        assert_eq!((status, answer.as_str()), (StatusCode::OK, "[]"));
        assert!(sent.is_empty(), "{sent:?}");
    }
}

/// An entry that names only an id is typed from it (`urn:ngsi-ld:{Type}:…`), so the attributes
/// asked beside it are its type's, and an id of a type no grant names is not asked for.
#[tokio::test]
async fn an_entry_by_id_is_asked_as_the_type_its_id_names() {
    let station = "urn:ngsi-ld:AirQualityObserved:banskabystrica.sk:ovzdusie:st-1";
    let invoice = "urn:ngsi-ld:Invoice:banskabystrica.sk:ovzdusie:inv-1";
    let (status, answer, sent) = query(
        false,
        "entityOperations/query",
        "",
        json!({ "type": "Query", "entities": [{ "id": station }, { "id": invoice }] }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{answer}");
    assert_eq!(
        only_hop(&sent).1["entities"],
        json!([{ "id": station, "type": "AirQualityObserved" }])
    );
}

/// The grant's area and window narrow at the broker only, so they move into the body with the
/// rest: `geoQ` and `temporalQ` are the grant's, never dropped.
#[tokio::test]
async fn the_grants_area_and_window_travel_in_the_body() {
    for operation in ["entityOperations/query", "temporal/entityOperations/query"] {
        let (status, answer, sent) = query(
            true,
            operation,
            "?limit=10",
            json!({ "type": "Query", "entities": [{ "type": "AirQualityObserved" }] }),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{operation}: {answer}");
        let (url, body) = only_hop(&sent);
        assert_eq!(url, "limit=10", "{operation}");
        let area: Value = serde_json::from_str(AREA).expect("the area");
        assert_eq!(
            body["geoQ"],
            json!({ "georel": "within", "geometry": "Polygon", "coordinates": area }),
            "{operation}"
        );
        assert_eq!(
            body["temporalQ"],
            json!({ "timerel": "after", "timeAt": "2026-01-01T00:00:00Z" }),
            "{operation}"
        );
    }
}

/// A body with an area of its own beside the grant's cannot say both in one `geoQ`: refused,
/// with what to do, and the broker is not asked. Without a granted area it rides through.
#[tokio::test]
async fn a_body_area_beside_a_granted_area_is_refused_before_the_broker() {
    let own = json!({ "georel": "near;maxDistance==100", "geometry": "Point", "coordinates": [19.1, 48.7] });
    let (status, answer, sent) = query(
        true,
        "entityOperations/query",
        "",
        json!({ "type": "Query", "entities": [{ "type": "AirQualityObserved" }], "geoQ": own }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{answer}");
    assert!(answer.contains("geoQ"), "{answer}");
    assert!(sent.is_empty(), "{sent:?}");

    let (status, answer, sent) = query(
        false,
        "entityOperations/query",
        "",
        json!({ "type": "Query", "entities": [{ "type": "AirQualityObserved" }], "geoQ": own }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{answer}");
    assert_eq!(only_hop(&sent).1["geoQ"], own);
}

/// A batch query with no body selects nothing, and is refused rather than sent unnarrowed.
#[tokio::test]
async fn a_batch_query_without_a_body_is_refused() {
    let (upstream, asked) = strict_broker().await;
    let gateway = Arc::new(
        Gateway::new(
            Broker::new(upstream),
            Box::new(PolicyPdp),
            "banskabystrica.sk",
        )
        .serve([endpoint(false)]),
    );
    let response = router(gateway)
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri(format!(
                    "/api/endpoint/{SLUG}/ngsi-ld/v1/entityOperations/query"
                ))
                .header("content-type", "application/json")
                .body(Body::empty())
                .expect("a request"),
        )
        .await
        .expect("the gateway answers");
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert!(asked.lock().expect("the log").is_empty());
}
