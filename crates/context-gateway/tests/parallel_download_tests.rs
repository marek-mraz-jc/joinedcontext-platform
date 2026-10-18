//! A large download through the view reads its pages a few at a time (T-4082).
//!
//! On dev a 25 000-entity Endpoint (praha-mesto) took 20-28 s for file.csv and
//! sometimes passed the edge's timeout: the gateway read 1000-entity pages one after another
//! from a broker answering each in about 0.9 s. The stub view here answers slowly, counts the
//! requests in flight, and sends `NGSILD-Results-Count` only when asked to.

use axum::body::Body;
use axum::extract::Request;
use axum::http::{Request as HttpRequest, StatusCode};
use axum::response::IntoResponse;
use axum::Router;
use context_gateway::app::{router, Gateway};
use context_gateway::pdp::PolicyPdp;
use context_gateway::proxy::Broker;
use context_gateway::resolver::Endpoint;
use jc_core::kinds::{Audience, FileLimits, Representation};
use serde_json::{json, Value};
use std::collections::BTreeSet;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tower::ServiceExt;

const SLUG: &str = "jw7tffshzip7igmzcszgffkkkuetnc6v";
/// Two types of a space, 2 500 and 1 200 entities: three pages and two.
const HELD: [(&str, usize); 2] = [("WasteContainer", 2500), ("PointOfInterest", 1200)];

fn param<'a>(query: &'a str, name: &str) -> Option<&'a str> {
    query
        .split('&')
        .filter_map(|pair| pair.split_once('='))
        .find(|(key, _)| *key == name)
        .map(|(_, value)| value)
}

#[derive(Default)]
struct Seen {
    now: AtomicUsize,
    most: AtomicUsize,
    asked: Mutex<Vec<String>>,
}

/// The view: every page takes 50 ms; `counts` decides whether a `count=true` read says the total.
async fn view(counts: bool) -> (String, Arc<Seen>) {
    let seen = Arc::new(Seen::default());
    let recorder = Arc::clone(&seen);
    let app = Router::new().fallback(move |request: Request| {
        let seen = Arc::clone(&recorder);
        async move {
            let query = request.uri().query().unwrap_or_default().to_owned();
            if request.uri().path() == "/ngsi-ld/v1/types" {
                let list: Vec<&str> = HELD.iter().map(|(name, _)| *name).collect();
                return axum::Json(json!({ "type": "EntityTypeList", "typeList": list }))
                    .into_response();
            }
            if request.uri().path() != format!("/x/views/{SLUG}/ngsi-ld/v1/entities") {
                return StatusCode::NOT_FOUND.into_response();
            }
            let inside = seen.now.fetch_add(1, Ordering::SeqCst) + 1;
            seen.most.fetch_max(inside, Ordering::SeqCst);
            seen.asked
                .lock()
                .expect("no poisoned lock")
                .push(query.clone());
            tokio::time::sleep(Duration::from_millis(50)).await;
            seen.now.fetch_sub(1, Ordering::SeqCst);

            // The broker's scan ceiling: a `q` the view checks entity by entity is never counted
            // past 10 000 stored rows, and `q=past-ceiling` stands for a read over it (T-4324).
            let q = param(&query, "q");
            if q == Some("past-ceiling") || (q.is_some() && param(&query, "count") == Some("true"))
            {
                let problem = json!({
                    "type": "https://uri.etsi.org/ngsi-ld/errors/TooManyResults",
                    "title": "TooManyResults",
                    "detail": "query matched more than 10000 entities before filtering",
                });
                return (StatusCode::FORBIDDEN, axum::Json(problem)).into_response();
            }
            // The gateway asks every type in one query (`type=A,B`); the view answers them
            // one type after another, each in id order.
            let asked = param(&query, "type")
                .unwrap_or_default()
                .replace("%2C", ",");
            let asked: Vec<&str> = asked.split(',').collect();
            let all: Vec<(&str, usize)> = asked
                .iter()
                .flat_map(|name| {
                    let n = HELD
                        .iter()
                        .find(|(held, _)| held == name)
                        .map_or(0, |(_, n)| *n);
                    (0..n).map(move |i| (*name, i))
                })
                .collect();
            let total = all.len();
            let number = |key: &str| {
                param(&query, key)
                    .and_then(|value| value.parse::<usize>().ok())
                    .unwrap_or(0)
            };
            let rows: Vec<Value> = all
                .iter()
                .skip(number("offset"))
                .take(number("limit"))
                .map(|(name, n)| {
                    json!({
                        "id": format!("urn:ngsi-ld:{name}:praha.eu:praha-mesto:{n:05}"),
                        "type": name,
                        "location": {
                            "type": "GeoProperty",
                            "value": { "type": "Point", "coordinates": [14.42, 50.08] }
                        }
                    })
                })
                .collect();
            let mut response = axum::Json(Value::Array(rows)).into_response();
            if counts && param(&query, "count") == Some("true") {
                response.headers_mut().insert(
                    "NGSILD-Results-Count",
                    total.to_string().parse().expect("a header value"),
                );
            }
            response
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("a free port");
    let port = listener.local_addr().expect("an address").port();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (format!("http://127.0.0.1:{port}"), seen)
}

fn endpoint(max_file_rows: Option<u32>) -> Endpoint {
    Endpoint {
        declared_types: None,
        roles: Default::default(),
        slug: SLUG.to_owned(),
        title: std::collections::BTreeMap::new(),
        description: std::collections::BTreeMap::new(),
        space: "praha-mesto".to_owned(),
        project: "praha".to_owned(),
        audience: Audience::Public,
        allowed_projects: Vec::new(),
        representations: vec![Representation::NgsiLd, Representation::Csv],
        rate_limit: None,
        creates: None,
        file_limits: max_file_rows.map(|rows| FileLimits {
            max_file_rows: Some(rows),
            max_file_bytes: None,
        }),
        hidden_attributes: BTreeSet::new(),
        projection: None,
        base_path: format!("/api/endpoint/{SLUG}"),
        models: Vec::new(),
        view_mapping: None,
        catalog: None,
        policy_names: Vec::new(),
        policies: vec![serde_norway::from_str(
            "contextSpaceRef: praha-mesto\n\
             assigner: did:web:praha.eu\n\
             assignee: { kind: role, id: public }\n\
             operations: [queryEntity, retrieveEntity]\n",
        )
        .expect("the policy spec parses")],
    }
}

async fn download(broker: &str, endpoint: Endpoint) -> (StatusCode, String) {
    download_with(broker, endpoint, "").await
}

async fn download_with(broker: &str, endpoint: Endpoint, query: &str) -> (StatusCode, String) {
    let app = router(Arc::new(
        Gateway::new(Broker::new(broker), Box::new(PolicyPdp), "praha.eu").serve([endpoint]),
    ));
    let response = app
        .oneshot(
            HttpRequest::builder()
                .uri(format!("/api/endpoint/{SLUG}/file.csv{query}"))
                .body(Body::empty())
                .expect("a request"),
        )
        .await
        .expect("the gateway answers");
    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), 64 * 1024 * 1024)
        .await
        .expect("a readable body");
    (status, String::from_utf8_lossy(&body).into_owned())
}

/// The ids of the CSV's rows, in the order they were written.
fn ids(text: &str) -> Vec<String> {
    text.lines()
        .filter_map(|line| {
            line.split(',')
                .map(|cell| cell.trim_matches('"'))
                .find(|cell| cell.starts_with("urn:ngsi-ld:"))
                .map(str::to_owned)
        })
        .collect()
}

fn expected() -> Vec<String> {
    HELD.iter()
        .flat_map(|(name, n)| {
            (0..*n).map(move |i| format!("urn:ngsi-ld:{name}:praha.eu:praha-mesto:{i:05}"))
        })
        .collect()
}

/// Every entity, each type's in order and the types in the order they were listed, while the
/// view saw more than one page at once and never more than four.
#[tokio::test]
async fn a_large_download_reads_its_pages_a_few_at_a_time_and_misses_none() {
    let (broker, seen) = view(true).await;
    let (status, collection) = download(&broker, endpoint(None)).await;
    assert_eq!(status, StatusCode::OK, "{collection}");
    assert_eq!(ids(&collection), expected());
    let most = seen.most.load(Ordering::SeqCst);
    assert!((2..=4).contains(&most), "pages in flight at once: {most}");
    assert_eq!(
        seen.asked.lock().expect("no poisoned lock").len(),
        4,
        "3700 entities of both types asked in one query, 1000 a page"
    );
}

/// A view that sends no count is walked page by page, and the download is still whole.
#[tokio::test]
async fn without_a_count_the_pages_are_walked_and_none_is_missed() {
    let (broker, _) = view(false).await;
    let (status, collection) = download(&broker, endpoint(None)).await;
    assert_eq!(status, StatusCode::OK, "{collection}");
    assert_eq!(ids(&collection), expected());
}

/// A total past the Endpoint's row ceiling is refused once the counts are read, before the
/// further pages are asked for.
#[tokio::test]
async fn a_total_past_the_ceiling_is_refused_before_the_rest_is_read() {
    let (broker, seen) = view(true).await;
    let (status, _) = download(&broker, endpoint(Some(2000))).await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
    let asked = seen.asked.lock().expect("no poisoned lock").clone();
    assert!(
        asked.iter().all(|query| query.contains("offset=0")),
        "{asked:?}"
    );
}

/// A download filtered by `q` asks for no count, which the broker cannot give past its scan
/// ceiling, and walks the pages instead: on dev every file link of a catalogue dataset (`q` on
/// `dataset`) answered 403 "Access Denied by Policy" (T-4324).
#[tokio::test]
async fn a_download_with_a_q_walks_its_pages_without_a_count() {
    let (broker, seen) = view(true).await;
    let (status, collection) = download_with(
        &broker,
        endpoint(None),
        "?type=WasteContainer&q=dataset%3D%3D%22d%22",
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{collection}");
    assert_eq!(ids(&collection).len(), 2500);
    let asked = seen.asked.lock().expect("no poisoned lock").clone();
    assert!(
        asked.iter().all(|query| !query.contains("count=true")),
        "{asked:?}"
    );
}

/// The broker's TooManyResults is the download's 413, never a Policy's 403.
#[tokio::test]
async fn the_brokers_scan_ceiling_is_payload_too_large_not_forbidden() {
    let (broker, _) = view(true).await;
    let (status, problem) = download_with(
        &broker,
        endpoint(None),
        "?type=WasteContainer&q=past-ceiling",
    )
    .await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE, "{problem}");
    let problem: Value = serde_json::from_str(&problem).expect("a problem document");
    assert_eq!(problem["status"], 413, "{problem}");
}
