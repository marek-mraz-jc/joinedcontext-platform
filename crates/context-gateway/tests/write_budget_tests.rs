//! T-4077: the gateway holds at most its write budget of request bodies at once. A write past it
//! waits for its share in arrival order (T-4123) and, when the wait runs out, is answered 503 with
//! `Retry-After` before its body is read, and never reaches the broker; a read is not counted; the
//! budget comes back when a write is answered.

use axum::body::Body;
use axum::extract::Request;
use axum::http::{header::CONTENT_LENGTH, Method, StatusCode};
use axum::routing::any;
use axum::Router;
use context_gateway::app::{router, Gateway};
use context_gateway::pdp::PolicyPdp;
use context_gateway::proxy::Broker;
use context_gateway::resolver::Endpoint;
use jc_core::kinds::{Audience, PolicySpec, Representation};
use serde_json::json;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, Semaphore};
use tower::ServiceExt;

const SLUG: &str = "k7r2m4xq9vbn3tdw6hcy5pajfe";
const SPACE: &str = "ovzdusie";
const DOMAIN: &str = "banskabystrica.sk";

/// A broker that answers a read at once and holds every write until `release` hands it a
/// permit, telling `arrived` the body's size in KiB when one comes in.
struct Stalled {
    url: String,
    arrived: mpsc::UnboundedReceiver<usize>,
    release: Arc<Semaphore>,
    writes: Arc<AtomicUsize>,
}

async fn stalled_broker() -> Stalled {
    let (tx, arrived) = mpsc::unbounded_channel();
    let release = Arc::new(Semaphore::new(0));
    let writes = Arc::new(AtomicUsize::new(0));
    let (gate, count) = (Arc::clone(&release), Arc::clone(&writes));
    let app = Router::new().fallback(any(move |request: Request| {
        let (tx, gate, count) = (tx.clone(), Arc::clone(&gate), Arc::clone(&count));
        async move {
            if request.method() == Method::GET {
                return (StatusCode::OK, "[]");
            }
            let body = axum::body::to_bytes(request.into_body(), 8 << 20)
                .await
                .unwrap_or_default();
            count.fetch_add(1, Ordering::SeqCst);
            let _ = tx.send(body.len() / 1024);
            gate.acquire().await.expect("the gate").forget();
            (StatusCode::NO_CONTENT, "")
        }
    }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("a free port");
    let address = listener.local_addr().expect("the bound address");
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    Stalled {
        url: format!("http://{address}"),
        arrived,
        release,
        writes,
    }
}

fn endpoint() -> Endpoint {
    let policy: PolicySpec = serde_norway::from_str(&format!(
        "contextSpaceRef: {SPACE}\n\
         assigner: did:web:{DOMAIN}\n\
         assignee: {{ kind: role, id: public }}\n\
         operations: [upsertBatch, queryEntity, deleteEntity]\n\
         information:\n\
         \x20 - entities:\n\
         \x20     - type: AirQualityObserved\n"
    ))
    .expect("the policy spec parses");
    Endpoint {
        declared_types: None,
        catalog: None,
        deprecation: None,
        roles: Default::default(),
        slug: SLUG.to_owned(),
        title: Default::default(),
        description: Default::default(),
        space: SPACE.to_owned(),
        project: SPACE.to_owned(),
        audience: Audience::Public,
        allowed_projects: Vec::new(),
        representations: vec![Representation::NgsiLd],
        rate_limit: None,
        creates: None,
        file_limits: None,
        hidden_attributes: Default::default(),
        projection: None,
        view_mapping: None,
        base_path: format!("/api/endpoint/{SLUG}"),
        models: Vec::new(),
        policy_names: Vec::new(),
        manifest_name: String::new(),
        policies: vec![policy],
    }
}

/// An upsert of one entity padded to about `kib` KiB.
fn upsert(kib: usize, n: usize) -> Request<Body> {
    let body = json!([{
        "id": format!("urn:ngsi-ld:AirQualityObserved:{DOMAIN}:{SPACE}:st-{n}"),
        "type": "AirQualityObserved",
        "name": { "type": "Property", "value": "x".repeat(kib * 1024) }
    }])
    .to_string();
    Request::builder()
        .method(Method::POST)
        .uri(format!(
            "/api/endpoint/{SLUG}/ngsi-ld/v1/entityOperations/upsert"
        ))
        .header("content-type", "application/json")
        .header(CONTENT_LENGTH, body.len())
        .body(Body::from(body))
        .expect("a request")
}

fn read() -> Request<Body> {
    Request::builder()
        .uri(format!(
            "/api/endpoint/{SLUG}/ngsi-ld/v1/entities?type=AirQualityObserved"
        ))
        .body(Body::empty())
        .expect("a request")
}

#[tokio::test]
async fn a_write_past_the_budget_waits_then_is_answered_503_and_never_reaches_the_broker() {
    let mut broker = stalled_broker().await;
    let gateway = Arc::new(
        Gateway::new(Broker::new(broker.url.clone()), Box::new(PolicyPdp), DOMAIN)
            .budget_writes(1)
            .wait_for_writes(Duration::from_millis(100))
            .serve([endpoint()]),
    );
    let app = router(gateway);

    // The first write takes 700 of the 1024 KiB and waits on the broker.
    let held = tokio::spawn(app.clone().oneshot(upsert(700, 1)));
    broker
        .arrived
        .recv()
        .await
        .expect("the first write arrives");

    let over = app
        .clone()
        .oneshot(upsert(700, 2))
        .await
        .expect("an answer");
    assert_eq!(over.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(over.headers()["retry-after"], "1");
    let body = axum::body::to_bytes(over.into_body(), 1 << 16)
        .await
        .expect("the body");
    let problem: serde_json::Value = serde_json::from_slice(&body).expect("a problem");
    assert!(
        problem["detail"]
            .as_str()
            .is_some_and(|d| d.contains("Retry-After")),
        "{problem}"
    );

    // A read takes no share; a small write still fits beside the held one.
    let reading = app.clone().oneshot(read()).await.expect("an answer");
    assert_eq!(reading.status(), StatusCode::OK);
    let small = tokio::spawn(app.clone().oneshot(upsert(100, 3)));
    broker
        .arrived
        .recv()
        .await
        .expect("the small write arrives");
    assert_eq!(
        broker.writes.load(Ordering::SeqCst),
        2,
        "the 503 never reached the broker"
    );

    // Answered, the writes give their shares back.
    broker.release.add_permits(2);
    assert!(held
        .await
        .expect("joined")
        .expect("answered")
        .status()
        .is_success());
    assert!(small
        .await
        .expect("joined")
        .expect("answered")
        .status()
        .is_success());
    let again = tokio::spawn(app.clone().oneshot(upsert(700, 4)));
    broker.arrived.recv().await.expect("the budget is back");
    broker.release.add_permits(1);
    assert!(again
        .await
        .expect("joined")
        .expect("answered")
        .status()
        .is_success());
}

/// A DELETE carries no body and no Content-Length.
fn delete(n: usize) -> Request<Body> {
    Request::builder()
        .method(Method::DELETE)
        .uri(format!(
            "/api/endpoint/{SLUG}/ngsi-ld/v1/entities/urn:ngsi-ld:AirQualityObserved:{DOMAIN}:{SPACE}:st-{n}"
        ))
        // a delete names why (DM-113)
        .header("jc-reason", "test")
        .body(Body::empty())
        .expect("a request")
}

#[tokio::test]
async fn a_delete_without_content_length_takes_the_smallest_share() {
    let mut broker = stalled_broker().await;
    let gateway = Arc::new(
        Gateway::new(Broker::new(broker.url.clone()), Box::new(PolicyPdp), DOMAIN)
            .budget_writes(1)
            .serve([endpoint()]),
    );
    let app = router(gateway);

    // A write holds 700 of the 1024 KiB; a DELETE still fits beside it.
    let held = tokio::spawn(app.clone().oneshot(upsert(700, 1)));
    broker.arrived.recv().await.expect("the write arrives");
    let deleting = tokio::spawn(app.clone().oneshot(delete(1)));
    broker.arrived.recv().await.expect("the delete arrives");
    broker.release.add_permits(2);
    assert!(held
        .await
        .expect("joined")
        .expect("answered")
        .status()
        .is_success());
    let deleted = deleting.await.expect("joined").expect("answered");
    assert_eq!(deleted.status(), StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn a_write_larger_than_the_budget_still_goes_through_an_idle_gateway() {
    let mut broker = stalled_broker().await;
    let gateway = Arc::new(
        Gateway::new(Broker::new(broker.url.clone()), Box::new(PolicyPdp), DOMAIN)
            .budget_writes(1)
            .serve([endpoint()]),
    );
    let app = router(gateway);
    let big = tokio::spawn(app.clone().oneshot(upsert(1500, 1)));
    broker.arrived.recv().await.expect("the big write arrives");
    broker.release.add_permits(1);
    assert!(big
        .await
        .expect("joined")
        .expect("answered")
        .status()
        .is_success());
}

/// T-4123: on dev a stream of small writes kept part of the budget held at every instant, so a
/// page of most of the budget never found its share free and was answered 503 every second. A
/// page that waits now holds back the small writes that come after it, and goes first.
#[tokio::test]
async fn a_page_waiting_for_most_of_the_budget_goes_before_the_small_writes_behind_it() {
    let mut broker = stalled_broker().await;
    let gateway = Arc::new(
        Gateway::new(Broker::new(broker.url.clone()), Box::new(PolicyPdp), DOMAIN)
            .budget_writes(1)
            .wait_for_writes(Duration::from_secs(5))
            .serve([endpoint()]),
    );
    let app = router(gateway);

    // A small write holds 100 of the 1024 KiB at the broker.
    let small = tokio::spawn(app.clone().oneshot(upsert(100, 1)));
    broker
        .arrived
        .recv()
        .await
        .expect("the small write arrives");
    // A page of 1000 KiB does not fit beside it and waits; a 50 KiB write after it would fit
    // beside the small one, and still waits behind the page.
    let page = tokio::spawn(app.clone().oneshot(upsert(1000, 2)));
    tokio::time::sleep(Duration::from_millis(100)).await;
    let later = tokio::spawn(app.clone().oneshot(upsert(50, 3)));
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(
        broker.writes.load(Ordering::SeqCst),
        1,
        "the later small write overtook the waiting page"
    );

    // The small write answered, the page takes its share; the later write does not fit beside
    // the page and follows when the page is answered.
    broker.release.add_permits(1);
    let first = broker.arrived.recv().await.expect("the page arrives");
    assert!(
        first >= 1000,
        "the page goes first, not a {first} KiB write"
    );
    broker.release.add_permits(1);
    let second = broker
        .arrived
        .recv()
        .await
        .expect("the later write arrives");
    assert!(second < 100, "{second} KiB");
    broker.release.add_permits(1);
    for answer in [small, page, later] {
        assert!(answer
            .await
            .expect("joined")
            .expect("answered")
            .status()
            .is_success());
    }
}
