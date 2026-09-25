//! A broker that fails a read the gateway made for one of its own representations (T-2340, EP-26,
//! MP-02, SP-05, R22).
//!
//! A `file.*` download, an OGC items page and a SensorThings collection are the gateway's
//! documents, not the broker's: when the broker fails the read behind one of them, the caller gets
//! the gateway's own problem document and the broker's words go to the log. Whatever a failing
//! broker prints — a DSN with its password, a host inside the cluster, a source path, the tenant —
//! never reaches a caller who needs no token to ask. The NGSI-LD surface is the broker's protocol,
//! so its `4xx` still comes back word for word (CIM 009 5.7.2).

use axum::body::Body;
use axum::http::{Request as HttpRequest, StatusCode};
use axum::response::IntoResponse;
use axum::routing::any;
use axum::Router;
use context_gateway::app::{router, Gateway};
use context_gateway::pdp::PolicyPdp;
use context_gateway::proxy::Broker;
use context_gateway::resolver::Endpoint;
use jc_core::kinds::{Audience, Representation};
use serde_json::json;
use std::collections::BTreeSet;
use std::sync::Arc;
use tower::ServiceExt;

const SLUG: &str = "mluyob4nz52lok3ssk7pgn5vwt";
const PUBLIC_READ: &str = r#"contextSpaceRef: ovzdusie
assigner: did:web:banskabystrica.sk
assignee: { kind: role, id: public }
operations: [queryEntity, retrieveEntity]
"#;

/// What a failing broker might print: every string in `LEAKS` is in it.
fn confession() -> serde_json::Value {
    json!({
        "detail": "ERROR: relation \"entity\" does not exist at postgres://broker:hunter2@cnpg-rw.dev.svc:5432/ngsild",
        "tenant": "ovzdusie",
        "audience": "context-broker-internal",
        "file": "/src/broker/store.rs:118"
    })
}

const LEAKS: [&str; 6] = [
    "postgres://",
    "hunter2",
    "cnpg-rw.dev.svc",
    "store.rs",
    "context-broker-internal",
    "\"tenant\"",
];

fn endpoint() -> Endpoint {
    Endpoint {
        roles: Default::default(),
        slug: SLUG.to_owned(),
        title: std::collections::BTreeMap::new(),
        description: std::collections::BTreeMap::new(),
        space: "ovzdusie".to_owned(),
        project: "ovzdusie".to_owned(),
        audience: Audience::Public,
        allowed_projects: Vec::new(),
        representations: vec![
            Representation::NgsiLd,
            Representation::GeoJson,
            Representation::Json,
            Representation::Csv,
            Representation::Xlsx,
            Representation::Zip,
            Representation::OgcFeatures,
            Representation::Sta,
        ],
        rate_limit: None,
        file_limits: None,
        hidden_attributes: BTreeSet::new(),
        projection: None,
        base_path: format!("/api/endpoint/{SLUG}"),
        models: Vec::new(),
        view_mapping: None,
        catalog: None,
        policies: vec![serde_norway::from_str(PUBLIC_READ).expect("the policy parses")],
    }
}

/// A broker that answers every request with `status` and the confession, counting the calls.
async fn failing_broker(status: StatusCode) -> (String, Arc<std::sync::atomic::AtomicUsize>) {
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counted = Arc::clone(&calls);
    let app = Router::new().fallback(any(move || {
        let counted = Arc::clone(&counted);
        async move {
            counted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let mut answer = axum::Json(confession()).into_response();
            *answer.status_mut() = status;
            answer
        }
    }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("a free port");
    let port = listener.local_addr().expect("an address").port();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (format!("http://127.0.0.1:{port}"), calls)
}

async fn ask(broker: &str, path: &str) -> (StatusCode, String) {
    let app = router(Arc::new(
        Gateway::new(
            Broker::new(broker),
            Box::new(PolicyPdp),
            "banskabystrica.sk",
        )
        .serve([endpoint()]),
    ));
    let request = HttpRequest::builder()
        .uri(format!("/api/endpoint/{SLUG}{path}"))
        .body(Body::empty())
        .expect("a request");
    let response = app.oneshot(request).await.expect("the gateway answers");
    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), 16 * 1024 * 1024)
        .await
        .expect("a readable body");
    (status, String::from_utf8_lossy(&body).into_owned())
}

fn assert_nothing_leaked(what: &str, body: &str) {
    for leak in LEAKS {
        assert!(!body.contains(leak), "{what} carried `{leak}` out: {body}");
    }
}

/// The red case of T-2340: a broker `500` behind `file.geojson` answered the caller with the DSN,
/// its password, the cluster host and the source path.
#[tokio::test]
async fn a_broker_error_body_is_not_forwarded() {
    let (broker, calls) = failing_broker(StatusCode::INTERNAL_SERVER_ERROR).await;
    let (status, body) = ask(&broker, "/file.geojson?type=AirQualityObserved").await;
    assert!(
        calls.load(std::sync::atomic::Ordering::SeqCst) > 0,
        "the broker was asked"
    );
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR, "{body}");
    assert!(
        body.contains("broker-failure"),
        "the gateway's own document: {body}"
    );
    assert_nothing_leaked("file.geojson 500", &body);
}

/// Every download, whatever the broker's failure: its body never comes out, and a broker that
/// refused the gateway's own credentials or rate is the gateway's failure, not the caller's.
#[tokio::test]
async fn no_download_carries_a_failing_brokers_words() {
    for failure in [
        StatusCode::INTERNAL_SERVER_ERROR,
        StatusCode::SERVICE_UNAVAILABLE,
        StatusCode::UNAUTHORIZED,
        StatusCode::TOO_MANY_REQUESTS,
    ] {
        let (broker, _) = failing_broker(failure).await;
        for path in [
            "/file.geojson?type=AirQualityObserved",
            "/file.json?type=AirQualityObserved",
            "/file.csv?type=AirQualityObserved",
            "/file.xlsx?type=AirQualityObserved",
            "/file.zip?type=AirQualityObserved",
            // No selector: the gateway asks the broker what the space holds first, and that
            // read fails the same way.
            "/file.geojson",
            "/ogc/features/collections/AirQualityObserved/items",
            "/sta/v1.1/Things",
        ] {
            let (status, body) = ask(&broker, path).await;
            let what = format!("{path} behind a broker {failure}");
            assert_nothing_leaked(&what, &body);
            assert!(!status.is_success(), "{what} answered {status}: {body}");
            if failure.is_client_error() {
                assert_eq!(status, StatusCode::BAD_GATEWAY, "{what}: {body}");
            }
        }
    }
}

/// A broker that refuses the query itself says so in the gateway's words, as a `400`.
#[tokio::test]
async fn a_query_the_broker_refuses_is_a_400_in_the_gateways_words() {
    let (broker, _) = failing_broker(StatusCode::BAD_REQUEST).await;
    let (status, body) = ask(&broker, "/file.csv?type=AirQualityObserved").await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_nothing_leaked("file.csv 400", &body);
}

/// What still passes through: on the NGSI-LD surface a broker `4xx` is the specification's answer
/// to the caller's own request, and the caller reads it as the broker wrote it.
#[tokio::test]
async fn the_ngsi_ld_surface_still_answers_the_brokers_own_4xx() {
    let (broker, _) = failing_broker(StatusCode::BAD_REQUEST).await;
    let (status, body) = ask(&broker, "/ngsi-ld/v1/entities?type=AirQualityObserved").await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(
        body.contains("store.rs"),
        "the broker's own refusal: {body}"
    );
}

/// And a `5xx` does not pass through there either (T-2260).
#[tokio::test]
async fn the_ngsi_ld_surface_never_answers_a_brokers_5xx_body() {
    let (broker, _) = failing_broker(StatusCode::INTERNAL_SERVER_ERROR).await;
    let (status, body) = ask(&broker, "/ngsi-ld/v1/entities?type=AirQualityObserved").await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR, "{body}");
    assert_nothing_leaked("ngsi-ld 500", &body);
}
