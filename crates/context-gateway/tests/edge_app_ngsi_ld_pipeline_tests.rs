//! Edge cases of `app::serve_ngsi_ld` and `app::project_answer`, the enforcement pipeline behind
//! `/api/endpoint/{slug}/ngsi-ld/v1` (T-2492, EP-25, EP-26, GW26, GW31, GW33, R20, R22).
//!
//! **The contract.** An admitted caller and a raw request become a PDP verdict, a narrowed call to
//! the broker, and an answer projected to what the grants reach. What the grants cannot reach is
//! answered here without asking the broker, and the broker's own failure text never reaches the
//! caller.
//!
//! **Inputs.** The path as sent, the query string, the body and its media type, the verdict, and
//! the broker's answer (status, headers, body).
//!
//! Already proved elsewhere and extended rather than repeated: the subscription narrowing
//! (`edge_egress_notifications_narrow_subscription_tests.rs`, and the pipeline's own delivery in
//! `attack_foreign_space_ids_tests.rs`), a `caller`-identity registration answering 501 before any
//! data is read (`hub_federation_tests.rs`), the 415 of an unsupported payload
//! (`ngsi_ld_errors_tests.rs::an_unsupported_payload_media_type_is_415_and_the_accepted_ones_reach_the_guard`),
//! and one 500 body kept from the caller (`leak_sweep_tests.rs::an_upstream_error_body_never_reaches_the_caller`,
//! T-2260), which the 5xx case below widens to every 5xx, a body that is not JSON, and the
//! broker's headers.

mod common;

use axum::body::Body;
use axum::http::{HeaderMap, Method, Request, StatusCode};
use axum::routing::any;
use axum::Router;
use context_gateway::app::{router, Gateway};
use context_gateway::pdp::PolicyPdp;
use context_gateway::proxy::Broker;
use context_gateway::resolver::Endpoint;
use jc_core::kinds::{Audience, Representation};
use std::sync::{Arc, Mutex};
use tower::ServiceExt;

const SLUG: &str = "mluyob4nz52lok3ssk7pgn5vwt";
const AQ: &str = "urn:ngsi-ld:AirQualityObserved:banskabystrica.sk:ovzdusie:st-1";

/// One type, two attributes, an area drawn on `location`, and a window from 2026 on.
const GRANT: &str = r#"contextSpaceRef: ovzdusie
assigner: did:web:banskabystrica.sk
assignee: { kind: role, id: public }
operations: [queryEntity, retrieveEntity, queryBatch, queryTemporal, retrieveTemporal, createEntity, updateAttrs, appendAttrs]
information:
  - entities:
      - type: AirQualityObserved
    propertyNames: [pm10, location]
geoQ: "georel=within;geometry=Polygon;coordinates=[[[19.0,48.6],[19.3,48.6],[19.3,48.8],[19.0,48.8],[19.0,48.6]]]"
temporalQ: "timerel=after;timeAt=2026-01-01T00:00:00Z"
"#;

fn endpoint() -> Endpoint {
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
        creates: None,
        file_limits: None,
        hidden_attributes: ["operatorPhone".to_owned()].into_iter().collect(),
        projection: None,
        base_path: format!("/api/endpoint/{SLUG}"),
        models: Vec::new(),
        view_mapping: None,
        catalog: None,
        policy_names: Vec::new(),
        manifest_name: String::new(),
        policies: vec![serde_norway::from_str(GRANT).expect("the policy spec parses")],
    }
}

/// A broker that answers every request with one status, headers and body, and records what it
/// was asked: `(method, path?query, body)`.
struct Stub {
    url: String,
    asked: Arc<Mutex<Vec<(String, String, String)>>>,
}

impl Stub {
    async fn start(
        status: StatusCode,
        headers: &[(&'static str, &'static str)],
        body: &str,
    ) -> Self {
        let asked = Arc::new(Mutex::new(Vec::new()));
        let recorder = Arc::clone(&asked);
        let headers: Vec<(&'static str, &'static str)> = headers.to_vec();
        let body = body.to_owned();
        let app = Router::new().fallback(any(move |request: axum::extract::Request| {
            let (recorder, headers, body) = (Arc::clone(&recorder), headers.clone(), body.clone());
            async move {
                let method = request.method().to_string();
                let target = request.uri().to_string();
                let sent = axum::body::to_bytes(request.into_body(), usize::MAX)
                    .await
                    .map(|b| String::from_utf8_lossy(&b).into_owned())
                    .unwrap_or_default();
                recorder
                    .lock()
                    .expect("no poisoned lock")
                    .push((method, target, sent));
                let mut response = axum::response::Response::new(Body::from(body));
                *response.status_mut() = status;
                for (name, value) in headers {
                    response
                        .headers_mut()
                        .insert(name, value.parse().expect("a header value"));
                }
                response
            }
        }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("a port");
        let port = listener.local_addr().expect("an address").port();
        tokio::spawn(async move {
            let _ = axum::serve(listener, common::plain_tree(app)).await;
        });
        Self {
            url: format!("http://127.0.0.1:{port}"),
            asked,
        }
    }

    fn asked(&self) -> Vec<(String, String, String)> {
        self.asked.lock().expect("no poisoned lock").clone()
    }
}

fn app(broker: &str) -> Router {
    router(Arc::new(
        Gateway::new(
            Broker::new(broker),
            Box::new(PolicyPdp),
            "banskabystrica.sk",
        )
        .serve([endpoint()]),
    ))
}

fn ngsi(tail: &str) -> String {
    format!("/api/endpoint/{SLUG}/ngsi-ld/v1{tail}")
}

async fn send(
    app: Router,
    method: Method,
    path: &str,
    content_type: Option<&str>,
    body: Vec<u8>,
) -> (StatusCode, HeaderMap, String) {
    let mut request = Request::builder().method(method).uri(path);
    if let Some(media) = content_type {
        request = request.header("content-type", media);
    }
    let response = app
        .oneshot(request.body(Body::from(body)).expect("a request"))
        .await
        .expect("the gateway answers");
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("a body");
    (
        status,
        headers,
        String::from_utf8_lossy(&bytes).into_owned(),
    )
}

async fn get(app: Router, path: &str) -> (StatusCode, HeaderMap, String) {
    send(app, Method::GET, path, None, Vec::new()).await
}

/// GW33: a query that selects nothing is 400 as the caller sent it, before the broker is asked.
#[tokio::test]
async fn a_query_with_no_selector_is_400_before_the_broker_is_asked() {
    let broker = Stub::start(StatusCode::OK, &[], "[]").await;
    for query in ["", "?limit=10", &format!("?id={AQ}"), "?idPattern=.*"] {
        let (status, _, body) = get(app(&broker.url), &ngsi(&format!("/entities{query}"))).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{query}: {body}");
    }
    assert!(broker.asked().is_empty(), "{:?}", broker.asked());
}

/// A body past `MAX_BODY` (8 MiB) is a 400 naming the reason, never a panic and never forwarded.
#[tokio::test]
async fn a_body_larger_than_max_body_is_400_not_a_panic() {
    let broker = Stub::start(StatusCode::CREATED, &[], "").await;
    let big = vec![b' '; 8 * 1024 * 1024 + 1];
    let (status, _, body) = send(
        app(&broker.url),
        Method::POST,
        &ngsi("/entities"),
        Some("application/json"),
        big,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(broker.asked().is_empty());
}

/// ADR-N-035: the organization's `spec.limits.gateway.maxRequestBodyMegabytes` is the ceiling,
/// and the refusal names it and who can change it; a body inside it is not refused for its size.
#[tokio::test]
async fn a_body_past_the_organizations_limit_is_400_naming_the_setting() {
    let broker = Stub::start(StatusCode::CREATED, &[], "").await;
    let gateway = Gateway::new(
        Broker::new(&broker.url),
        Box::new(PolicyPdp),
        "banskabystrica.sk",
    )
    .serve([endpoint()]);
    gateway.replace_limits(context_gateway::store::Limits {
        max_request_body: 1024 * 1024,
        ..Default::default()
    });
    let app = router(Arc::new(gateway));

    let (status, _, body) = send(
        app.clone(),
        Method::POST,
        &ngsi("/entities"),
        Some("application/json"),
        vec![b' '; 1024 * 1024 + 1],
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(body.contains("1 MiB"), "{body}");
    assert!(body.contains("maxRequestBodyMegabytes"), "{body}");
    assert!(body.contains("Organization settings"), "{body}");
    assert!(broker.asked().is_empty());

    let mut inside = br#"{"id":"urn:ngsi-ld:T:banskabystrica.sk:ovzdusie:1","type":"T"}"#.to_vec();
    inside.resize(1024 * 1024, b' ');
    let (_, _, body) = send(
        app,
        Method::POST,
        &ngsi("/entities"),
        Some("application/json"),
        inside,
    )
    .await;
    assert!(!body.contains("maxRequestBodyMegabytes"), "{body}");
}

/// CIM 009 6.3.2: a write whose media type the surface does not take is refused before its body
/// is parsed, so a body that would not parse either still gets the media-type answer.
#[tokio::test]
async fn a_write_with_an_unsupported_media_type_is_refused_before_parsing() {
    let broker = Stub::start(StatusCode::CREATED, &[], "").await;
    for media in [
        "text/plain",
        "application/xml",
        "multipart/form-data; boundary=x",
    ] {
        let (status, _, body) = send(
            app(&broker.url),
            Method::POST,
            &ngsi("/entities"),
            Some(media),
            b"{ not json <xml/>".to_vec(),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "{media}: {body}"
        );
    }
    assert!(broker.asked().is_empty());
}

/// T-2260: whatever the status, the body and the headers of a broker's failure, the caller gets
/// the gateway's own document and none of the broker's.
#[tokio::test]
async fn a_5xx_broker_body_never_reaches_the_caller() {
    for status in [500u16, 502, 503, 504] {
        for body in [
            r#"{"detail":"while reading urn:ngsi-ld:Invoice:banskabystrica.sk:ovzdusie:inv-1 CANARY"}"#,
            "<html>upstream at 10.42.0.17:1026 CANARY</html>",
            "",
        ] {
            let broker = Stub::start(
                StatusCode::from_u16(status).expect("a status"),
                &[
                    ("x-broker-host", "antares-0.internal"),
                    ("location", "http://10.42.0.17:1026/x"),
                ],
                body,
            )
            .await;
            let (answered, headers, text) =
                get(app(&broker.url), &ngsi("/entities?type=AirQualityObserved")).await;
            assert_eq!(answered.as_u16(), status, "{body}");
            for held in ["CANARY", "10.42.0.17", "inv-1", "antares"] {
                assert!(!text.contains(held), "{status} carried {held}: {text}");
            }
            assert!(
                headers.get("x-broker-host").is_none(),
                "{status}: {headers:?}"
            );
            assert!(headers.get("location").is_none(), "{status}: {headers:?}");
        }
    }
}

/// A `4xx` is the caller's own request coming back, and it still says what to send instead. The
/// `q` parses, so it reaches the broker; one the parser refuses is answered at the edge
/// (T-4479).
#[tokio::test]
async fn a_4xx_broker_body_still_explains_itself_to_the_caller() {
    let problem = r#"{"type":"https://uri.etsi.org/ngsi-ld/errors/BadRequestData","title":"Bad request","detail":"q: unknown operator ~~"}"#;
    let broker = Stub::start(
        StatusCode::BAD_REQUEST,
        &[("content-type", "application/json")],
        problem,
    )
    .await;
    let (status, _, body) = get(
        app(&broker.url),
        &ngsi("/entities?type=AirQualityObserved&q=pm10%3E1"),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(body.contains("unknown operator"), "{body}");
}
