//! Attack vector: the file and bulk surfaces as a resource bomb (T-1700; GW26, EP-44, EP-20).
//!
//! A download is the one read where the gateway, not the caller, decides how many broker pages
//! are fetched: the caller asks for a file and the surface pages until the dataset ends. What an
//! attacker asks for is therefore a dataset that does not end, and the question is whether the
//! loop stops, whether the caller's own `limit` can lift the ceiling, and whether many such
//! requests at once cost the caller anything.
//!
//! `zip_export_tests.rs` proves the row and byte ceilings of the bundle, `rate_limit_tests.rs`
//! the bucket itself (including a forged `X-Forwarded-For`), and `edge_query_upstream_tests.rs`
//! that a `lastN` over the cap is cut before the broker is asked. This file plays the attack on
//! the surfaces those leave: the paging download of every file representation, against a broker
//! that always answers a full page, and the temporal window and the rate limit as a caller meets
//! them on the way in.
//!
//! The ceiling is a refusal and never a truncation (EP-44): a short CSV looks exactly like a
//! complete one, so a caller who cannot tell would act on half the data.

use axum::body::Body;
use axum::extract::Request;
use axum::http::{Method, StatusCode};
use axum::routing::any;
use axum::Router;
use context_gateway::app::{router, Gateway};
use context_gateway::pdp::PolicyPdp;
use context_gateway::proxy::Broker;
use context_gateway::resolver::Endpoint;
use jc_core::kinds::{Audience, FileLimits, PolicySpec, RateLimits, Representation};
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
use tower::ServiceExt;

const SLUG: &str = "6pt4wzn8qkxr2m5jvc9dyh3fbs";
const SPACE: &str = "ovzdusie";
const DOMAIN: &str = "banskabystrica.sk";

/// The page the gateway asks the broker for; the download's loop is counted in these.
const PAGE: usize = 1_000;

/// The endpoint's own row ceiling, small enough that the attack costs three pages instead of a
/// hundred and large enough that it is more than one.
const MAX_ROWS: u32 = 2_500;

type Hops = Arc<Mutex<Vec<String>>>;

/// A broker holding a dataset that never ends: every page is full, so the download only stops
/// when the gateway decides it does.
async fn endless_broker() -> (String, Hops) {
    broker_answering(PAGE).await
}

async fn broker_answering(per_page: usize) -> (String, Hops) {
    let hops: Hops = Arc::new(Mutex::new(Vec::new()));
    let recorder = Arc::clone(&hops);
    let app = Router::new().fallback(any(move |request: Request| {
        let recorder = Arc::clone(&recorder);
        async move {
            recorder
                .lock()
                .expect("the hop log")
                .push(request.uri().query().unwrap_or_default().to_owned());
            let page: Vec<Value> = (0..per_page).map(station).collect();
            axum::Json(Value::Array(page))
        }
    }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("a free port");
    let address = listener.local_addr().expect("the bound address");
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (format!("http://{address}"), hops)
}

fn station(n: usize) -> Value {
    json!({
        "id": format!("urn:ngsi-ld:AirQualityObserved:banskabystrica.sk:ovzdusie:s-{n}"),
        "type": "AirQualityObserved",
        "pm10": { "type": "Property", "value": 21.5 },
        "location": {
            "type": "GeoProperty",
            "value": { "type": "Point", "coordinates": [19.15, 48.73] }
        }
    })
}

fn policy() -> PolicySpec {
    serde_norway::from_str(&format!(
        "contextSpaceRef: {SPACE}\n\
         assigner: did:web:{DOMAIN}\n\
         assignee: {{ kind: role, id: public }}\n\
         operations: [queryEntity, retrieveEntity, queryTemporal, retrieveTemporal]\n\
         information:\n\
         \x20 - entities:\n\
         \x20     - type: AirQualityObserved\n"
    ))
    .expect("the policy spec parses")
}

fn endpoint(rate_limit: Option<RateLimits>) -> Endpoint {
    Endpoint {
        slug: SLUG.to_owned(),
        title: std::collections::BTreeMap::new(),
        description: std::collections::BTreeMap::new(),
        space: SPACE.to_owned(),
        project: DOMAIN.to_owned(),
        audience: Audience::Public,
        allowed_projects: Vec::new(),
        representations: vec![
            Representation::NgsiLd,
            Representation::Csv,
            Representation::Json,
            Representation::GeoJson,
            Representation::Xlsx,
        ],
        rate_limit,
        file_limits: Some(FileLimits {
            max_file_rows: Some(MAX_ROWS),
            max_file_bytes: None,
        }),
        hidden_attributes: Default::default(),
        projection: None,
        view_mapping: None,
        base_path: format!("/api/endpoint/{SLUG}"),
        models: Vec::new(),
        policies: vec![policy()],
    }
}

/// One request against a broker whose dataset never ends.
async fn ask(uri: &str) -> (StatusCode, Vec<String>) {
    let (upstream, hops) = endless_broker().await;
    let gateway = Arc::new(
        Gateway::new(Broker::new(upstream), Box::new(PolicyPdp), DOMAIN).serve([endpoint(None)]),
    );
    let response = router(gateway)
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri(format!("/api/endpoint/{SLUG}{uri}"))
                .body(Body::empty())
                .expect("a request"),
        )
        .await
        .expect("the gateway answers");
    let status = response.status();
    let asked = hops.lock().expect("the hop log").clone();
    (status, asked)
}

/// EP-44: the loop ends at the endpoint's row ceiling, and it ends after the pages that ceiling
/// pays for — not after as many as the dataset has.
#[tokio::test]
async fn a_download_of_a_dataset_that_never_ends_is_refused_at_the_row_ceiling() {
    let (status, asked) = ask("/file.csv").await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
    let pages = (MAX_ROWS as usize).div_ceil(PAGE);
    assert!(
        asked.len() <= pages,
        "the download read {} pages for a ceiling of {MAX_ROWS} rows",
        asked.len()
    );
}

/// The ceiling belongs to the endpoint, not to the representation: a caller cannot walk the
/// list of file types until one of them holds the whole space.
///
/// `file.json` is not in the list because the router does not serve it, although an endpoint
/// record advertises it for a `json` representation — T-2382, found here.
#[tokio::test]
async fn every_file_representation_stops_at_the_same_ceiling() {
    for file in ["/file.csv", "/file.geojson", "/file.xlsx"] {
        let (status, asked) = ask(file).await;
        assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE, "{file}");
        assert!(
            asked.len() <= (MAX_ROWS as usize).div_ceil(PAGE),
            "{file} read {} pages",
            asked.len()
        );
    }
}

/// The caller's own `limit` is a ceiling on the download and never a lift of the endpoint's.
#[tokio::test]
async fn a_callers_own_limit_cannot_raise_the_endpoints_ceiling() {
    let (status, asked) = ask("/file.csv?limit=100000000").await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
    assert!(
        asked.len() <= (MAX_ROWS as usize).div_ceil(PAGE),
        "a caller's limit bought {} pages",
        asked.len()
    );
    for asked in asked {
        assert!(
            asked.contains(&format!("limit={PAGE}")),
            "the broker was asked for more than one page at a time: {asked}"
        );
    }
}

/// The control: a dataset inside the ceiling is served, so the cases above are about the
/// ceiling and not about a surface that refuses every download.
#[tokio::test]
async fn a_dataset_inside_the_ceiling_is_served() {
    let (upstream, hops) = broker_answering(2).await;
    let gateway = Arc::new(
        Gateway::new(Broker::new(upstream), Box::new(PolicyPdp), DOMAIN).serve([endpoint(None)]),
    );
    let response = router(gateway)
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri(format!("/api/endpoint/{SLUG}/file.csv"))
                .body(Body::empty())
                .expect("a request"),
        )
        .await
        .expect("the gateway answers");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(hops.lock().expect("the hop log").len(), 1);
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("a body");
    let text = String::from_utf8_lossy(&bytes);
    assert_eq!(
        text.lines().count(),
        3,
        "a header and two rows, not a truncation: {text}"
    );
}

/// The other half of the same rule: a dataset larger than one broker page is served whole, not
/// as the first page with nothing to say it was cut (EP-44, T-1700). Until this was fixed
/// `file.geojson` asked the broker once and rendered whatever came back.
#[tokio::test]
async fn a_download_larger_than_one_broker_page_is_served_whole() {
    let pages = Arc::new(Mutex::new(0usize));
    let counter = Arc::clone(&pages);
    let app = Router::new().fallback(any(move |_request: Request| {
        let counter = Arc::clone(&counter);
        async move {
            let mut seen = counter.lock().expect("the page count");
            *seen += 1;
            // Two full pages and then a short one, which is how a broker says "no more".
            let size = if *seen <= 2 { PAGE } else { 3 };
            let offset = (*seen - 1) * PAGE;
            let page: Vec<Value> = (0..size).map(|n| station(offset + n)).collect();
            axum::Json(Value::Array(page))
        }
    }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("a free port");
    let address = listener.local_addr().expect("the bound address");
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    let gateway = Arc::new(
        Gateway::new(
            Broker::new(format!("http://{address}")),
            Box::new(PolicyPdp),
            DOMAIN,
        )
        .serve([endpoint(None)]),
    );
    let response = router(gateway)
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri(format!("/api/endpoint/{SLUG}/file.geojson"))
                .body(Body::empty())
                .expect("a request"),
        )
        .await
        .expect("the gateway answers");
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("a body");
    let collection: Value = serde_json::from_slice(&bytes).expect("a FeatureCollection");
    assert_eq!(
        collection["features"]
            .as_array()
            .map(Vec::len)
            .unwrap_or_default(),
        PAGE * 2 + 3,
        "the download stopped at a page boundary and said nothing about it"
    );
    assert_eq!(*pages.lock().expect("the page count"), 3);
}

/// GW26: one request may not make the shared broker read an unbounded history. The cap is cut
/// into the question rather than refused, so a client that asks for too much still gets an
/// answer — the broker is simply never asked for more than the cap.
#[tokio::test]
async fn a_temporal_window_is_cut_to_the_cap_before_the_broker_is_asked() {
    let (status, asked) = ask(
        "/ngsi-ld/v1/temporal/entities?type=AirQualityObserved&lastN=1000000\
         &timerel=after&timeAt=2020-01-01T00:00:00Z",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(asked.len(), 1, "{asked:?}");
    assert!(
        asked[0].contains("lastN=1000") && !asked[0].contains("lastN=1000000"),
        "the broker was asked for {}",
        asked[0]
    );
}

/// EP-20: the file surfaces are behind the same bucket as everything else, and an anonymous
/// caller's burst is spent on refusals that never reach the broker.
#[tokio::test]
async fn many_downloads_at_once_spend_one_anonymous_callers_burst_and_stop() {
    let (upstream, hops) = broker_answering(2).await;
    let gateway = Arc::new(
        Gateway::new(Broker::new(upstream), Box::new(PolicyPdp), DOMAIN).serve([endpoint(Some(
            RateLimits {
                requests_per_minute: 60,
                burst: Some(2),
            },
        ))]),
    );
    let app = router(gateway);
    let mut statuses = Vec::new();
    for _ in 0..4 {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri(format!("/api/endpoint/{SLUG}/file.csv"))
                    .header("x-forwarded-for", "203.0.113.7")
                    .body(Body::empty())
                    .expect("a request"),
            )
            .await
            .expect("the gateway answers");
        statuses.push(response.status());
        if response.status() == StatusCode::TOO_MANY_REQUESTS {
            assert!(
                response.headers().contains_key("retry-after"),
                "a 429 that does not say when to come back is a client that retries at once"
            );
        }
    }
    assert_eq!(
        statuses,
        vec![
            StatusCode::OK,
            StatusCode::OK,
            StatusCode::TOO_MANY_REQUESTS,
            StatusCode::TOO_MANY_REQUESTS
        ],
        "the burst was not the ceiling on what one caller can start"
    );
    assert_eq!(
        hops.lock().expect("the hop log").len(),
        2,
        "a refused download still cost the broker a page"
    );
}
