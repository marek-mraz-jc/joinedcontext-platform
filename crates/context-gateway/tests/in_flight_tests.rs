//! T-4782: the gateway works on a bounded number of requests at once, in total and per caller. A
//! broker that answers slowly fills the slots and not the gateway's memory: a request past them is
//! answered 503 with `Retry-After` at once and never reaches the broker, and the slots come back
//! when the admitted requests are answered.

use axum::body::Body;
use axum::extract::Request;
use axum::http::StatusCode;
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
use std::time::{Duration, Instant};
use tokio::sync::Semaphore;
use tower::ServiceExt;

const SLUG: &str = "k7r2m4xq9vbn3tdw6hcy5pajfe";
const SPACE: &str = "ovzdusie";
const DOMAIN: &str = "banskabystrica.sk";
/// Entities in the broker's page: a full page of the gateway's own paging (`PAGE`).
const ENTITIES: usize = 1_000;

/// A broker that holds every read until `release` hands it a permit, then answers a page of
/// [`ENTITIES`] entities of about 1 KiB each; it counts the reads it holds at once.
struct Slow {
    url: String,
    release: Arc<Semaphore>,
    holding: Arc<AtomicUsize>,
    most: Arc<AtomicUsize>,
    reads: Arc<AtomicUsize>,
}

async fn slow_broker() -> Slow {
    let page: Vec<_> = (0..ENTITIES)
        .map(|n| {
            json!({
                "id": format!("urn:ngsi-ld:AirQualityObserved:{DOMAIN}:{SPACE}:st-{n}"),
                "type": "AirQualityObserved",
                "name": { "type": "Property", "value": "x".repeat(900) },
                "pm10": { "type": "Property", "value": n, "observedAt": "2026-10-19T22:00:00Z" }
            })
        })
        .collect();
    let page = axum::body::Bytes::from(serde_json::to_vec(&page).expect("the page"));
    let release = Arc::new(Semaphore::new(0));
    let (holding, most, reads): (Arc<AtomicUsize>, Arc<AtomicUsize>, Arc<AtomicUsize>) =
        Default::default();
    let state = (
        Arc::clone(&release),
        Arc::clone(&holding),
        Arc::clone(&most),
        Arc::clone(&reads),
    );
    let app = Router::new().fallback(any(move |_: Request| {
        let (gate, holding, most, reads) = state.clone();
        let page = page.clone();
        async move {
            reads.fetch_add(1, Ordering::SeqCst);
            let now = holding.fetch_add(1, Ordering::SeqCst) + 1;
            most.fetch_max(now, Ordering::SeqCst);
            gate.acquire().await.expect("the gate").forget();
            holding.fetch_sub(1, Ordering::SeqCst);
            (
                [
                    ("content-type", "application/json"),
                    ("ngsild-results-count", "1000"),
                ],
                page,
            )
        }
    }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("a free port");
    let address = listener.local_addr().expect("the bound address");
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    Slow {
        url: format!("http://{address}"),
        release,
        holding,
        most,
        reads,
    }
}

fn endpoint() -> Endpoint {
    let policy: PolicySpec = serde_norway::from_str(&format!(
        "contextSpaceRef: {SPACE}\n\
         assigner: did:web:{DOMAIN}\n\
         assignee: {{ kind: role, id: public }}\n\
         operations: [queryEntity]\n\
         information:\n\
         \x20 - entities:\n\
         \x20     - type: AirQualityObserved\n"
    ))
    .expect("the policy spec parses");
    Endpoint {
        declared_types: None,
        catalog: None,
        roles: Default::default(),
        slug: SLUG.to_owned(),
        title: Default::default(),
        description: Default::default(),
        space: SPACE.to_owned(),
        project: SPACE.to_owned(),
        audience: Audience::Public,
        allowed_projects: Vec::new(),
        representations: vec![Representation::NgsiLd, Representation::Csv],
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

/// An anonymous read of a full page from the client at `address`.
fn read(address: &str) -> Request<Body> {
    Request::builder()
        .uri(format!(
            "/api/endpoint/{SLUG}/ngsi-ld/v1/entities?type=AirQualityObserved&limit=1000"
        ))
        .header("x-forwarded-for", address)
        .body(Body::empty())
        .expect("a request")
}

fn download(address: &str) -> Request<Body> {
    Request::builder()
        .uri(format!("/api/endpoint/{SLUG}/file.csv"))
        .header("x-forwarded-for", address)
        .body(Body::empty())
        .expect("a request")
}

fn gateway(broker: &Slow, total: usize, per_caller: usize) -> Router {
    router(Arc::new(
        Gateway::new(Broker::new(broker.url.clone()), Box::new(PolicyPdp), DOMAIN)
            .limit_in_flight(total, per_caller, 1)
            .serve([endpoint()]),
    ))
}

/// Waits until the broker holds `count` reads.
async fn holding(broker: &Slow, count: usize) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while broker.holding.load(Ordering::SeqCst) < count {
        assert!(Instant::now() < deadline, "the broker never held {count}");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

async fn assert_busy(answer: axum::response::Response) {
    assert_eq!(answer.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(answer.headers()["retry-after"], "1");
    let body = axum::body::to_bytes(answer.into_body(), 1 << 16)
        .await
        .expect("the body");
    let problem: serde_json::Value = serde_json::from_slice(&body).expect("a problem");
    assert!(
        problem["detail"]
            .as_str()
            .is_some_and(|detail| detail.contains("Retry-After")),
        "{problem}"
    );
}

/// The peak resident memory of this process, in KiB (Linux `VmHWM`).
fn peak_kib() -> u64 {
    std::fs::read_to_string("/proc/self/status")
        .expect("/proc/self/status")
        .lines()
        .find_map(|line| line.strip_prefix("VmHWM:"))
        .and_then(|value| value.trim().trim_end_matches("kB").trim().parse().ok())
        .expect("VmHWM")
}

/// The load test: 128 callers of 4 reads each against a broker that holds every read. The gateway
/// forwards 16 and answers the other 496 503 at once, so its memory stays under a fixed bound
/// however many come in; without the cap all 512 reach the broker and their pages arrive together.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_slow_broker_fills_the_slots_and_not_the_memory() {
    const CAP: usize = 16;
    const SENT: usize = 512;
    let broker = slow_broker().await;
    let app = gateway(&broker, CAP, 4);
    let before = peak_kib();

    let started = Instant::now();
    let answers: Vec<_> = (0..SENT)
        .map(|n| tokio::spawn(app.clone().oneshot(read(&format!("192.0.2.{}", n % 128)))))
        .collect();
    // Every request past the cap is answered while the broker still holds the admitted ones.
    holding(&broker, CAP).await;
    let deadline = Instant::now() + Duration::from_secs(30);
    while answers.iter().filter(|answer| answer.is_finished()).count() < SENT - CAP {
        assert!(
            Instant::now() < deadline,
            "the requests past the cap were not answered"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let shed = started.elapsed();
    // Then the broker answers every held read at once: the gateway holds CAP pages together.
    broker.release.add_permits(SENT);
    let mut busy = 0;
    for answer in answers {
        let answer = answer.await.expect("joined").expect("answered");
        if answer.status() == StatusCode::OK {
            let body = axum::body::to_bytes(answer.into_body(), 64 << 20)
                .await
                .expect("the page");
            assert!(body.len() > ENTITIES * 900, "a whole page");
        } else {
            assert_busy(answer).await;
            busy += 1;
        }
    }
    let grown_mib = peak_kib().saturating_sub(before) / 1024;
    eprintln!("cap {CAP}: {busy} of {SENT} answered 503 in {shed:?}; peak memory grew {grown_mib} MiB; broker held at most {}", broker.most.load(Ordering::SeqCst));

    assert_eq!(
        broker.most.load(Ordering::SeqCst),
        CAP,
        "the broker held at most the cap"
    );
    assert_eq!(
        broker.reads.load(Ordering::SeqCst),
        CAP,
        "no 503 reached the broker"
    );
    assert_eq!(busy, SENT - CAP);
    // Measured (debug build, 1 MiB pages): cap 16 grew 8 MiB and shed 496 in 12 ms, cap 64 grew
    // 15 MiB; without a cap all 512 reached the broker and the peak grew 83 MiB.
    assert!(grown_mib < 48, "peak memory grew {grown_mib} MiB");
}

#[tokio::test]
async fn one_caller_past_its_share_is_refused_while_another_is_served() {
    let broker = slow_broker().await;
    let app = gateway(&broker, 8, 2);
    let held: Vec<_> = (0..2)
        .map(|_| tokio::spawn(app.clone().oneshot(read("192.0.2.1"))))
        .collect();
    holding(&broker, 2).await;
    assert_busy(
        app.clone()
            .oneshot(read("192.0.2.1"))
            .await
            .expect("an answer"),
    )
    .await;

    let other = tokio::spawn(app.clone().oneshot(read("192.0.2.2")));
    holding(&broker, 3).await;
    broker.release.add_permits(3);
    assert_eq!(
        other.await.expect("joined").expect("answered").status(),
        StatusCode::OK
    );
    for answer in held {
        assert_eq!(
            answer.await.expect("joined").expect("answered").status(),
            StatusCode::OK
        );
    }
    // Answered, the caller's share is back.
    let again = tokio::spawn(app.clone().oneshot(read("192.0.2.1")));
    holding(&broker, 1).await;
    broker.release.add_permits(1);
    assert_eq!(
        again.await.expect("joined").expect("answered").status(),
        StatusCode::OK
    );
}

#[tokio::test]
async fn a_download_past_its_slots_is_refused_and_a_read_still_served() {
    let broker = slow_broker().await;
    let app = gateway(&broker, 8, 8);
    let first = tokio::spawn(app.clone().oneshot(download("192.0.2.1")));
    holding(&broker, 1).await;
    assert_busy(
        app.clone()
            .oneshot(download("192.0.2.2"))
            .await
            .expect("an answer"),
    )
    .await;
    assert_eq!(
        broker.reads.load(Ordering::SeqCst),
        1,
        "the refused download read nothing"
    );

    let reading = tokio::spawn(app.clone().oneshot(read("192.0.2.2")));
    holding(&broker, 2).await;
    broker.release.add_permits(2);
    assert_eq!(
        reading.await.expect("joined").expect("answered").status(),
        StatusCode::OK
    );
    assert_eq!(
        first.await.expect("joined").expect("answered").status(),
        StatusCode::OK
    );
}

/// The resident memory of this process now, in KiB (Linux `VmRSS`).
fn resident_kib() -> u64 {
    std::fs::read_to_string("/proc/self/status")
        .expect("/proc/self/status")
        .lines()
        .find_map(|line| line.strip_prefix("VmRSS:"))
        .and_then(|value| value.trim().trim_end_matches("kB").trim().parse().ok())
        .expect("VmRSS")
}

/// Waves of full-page reads from 64 callers through the gateway's default caps, every answer read
/// whole; returns how many KiB more this process holds once they are answered and it went idle.
async fn a_load_of_full_pages() -> u64 {
    let broker = slow_broker().await;
    broker.release.add_permits(Semaphore::MAX_PERMITS);
    let app = gateway(&broker, 128, 32);
    let before = resident_kib();
    for wave in 0..8 {
        let answers: Vec<_> = (0..400)
            .map(|n| {
                let caller = format!("192.0.2.{}", (n + wave) % 64);
                tokio::spawn(app.clone().oneshot(read(&caller)))
            })
            .collect();
        for answer in answers {
            let answer = answer.await.expect("joined").expect("answered");
            if answer.status() == StatusCode::OK {
                axum::body::to_bytes(answer.into_body(), 64 << 20)
                    .await
                    .expect("the page");
            } else {
                assert_busy(answer).await;
            }
        }
    }
    tokio::time::sleep(Duration::from_secs(1)).await;
    resident_kib().saturating_sub(before)
}

/// Set in the child processes of the test below, which run the load and print what it left.
const LOAD_CHILD: &str = "JC_GATEWAY_TEST_LOAD_CHILD";

/// T-4782 (reopened): after prod's 100 req/s run the gateway idled at 929 Mi of its 1 Gi. glibc
/// raises its mmap threshold to the largest block freed, so every later page body comes from the
/// heap and stays there. The deployment fixes the threshold at 128 KiB
/// (`MALLOC_MMAP_THRESHOLD_` in the gateway's base values, beside `MALLOC_ARENA_MAX`): the same
/// load then leaves almost nothing behind. Without it the growth stays, which makes this test red
/// when the setting goes. The load runs in a child process of its own (this test, `LOAD_CHILD`
/// set), so neither the setting nor its memory reaches the other tests of this binary.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_memory_of_a_load_comes_back_with_the_deployed_malloc_setting() {
    const NAME: &str = "the_memory_of_a_load_comes_back_with_the_deployed_malloc_setting";
    if std::env::var_os(LOAD_CHILD).is_some() {
        eprintln!("idle growth KiB {}", a_load_of_full_pages().await);
        return;
    }
    let idle_growth_mib = |mmap_threshold: Option<&str>| {
        let mut child = std::process::Command::new(std::env::current_exe().expect("this test"));
        child
            .args(["--exact", NAME, "--nocapture"])
            .env(LOAD_CHILD, "1")
            .env("MALLOC_ARENA_MAX", "2")
            .env_remove("MALLOC_MMAP_THRESHOLD_");
        if let Some(threshold) = mmap_threshold {
            child.env("MALLOC_MMAP_THRESHOLD_", threshold);
        }
        let output = child.output().expect("the load ran");
        let printed = String::from_utf8_lossy(&output.stderr);
        assert!(output.status.success(), "{printed}");
        printed
            .lines()
            .find_map(|line| line.strip_prefix("idle growth KiB "))
            .and_then(|kib| kib.trim().parse::<u64>().ok())
            .unwrap_or_else(|| panic!("no measurement in {printed}"))
            / 1024
    };
    let deployed = idle_growth_mib(Some("131072"));
    let dynamic = idle_growth_mib(None);
    eprintln!("idle growth after the load: {deployed} MiB with the deployed threshold, {dynamic} MiB with glibc's dynamic one");
    // Measured (debug build, 3,200 reads of 1 MiB pages, MALLOC_ARENA_MAX=2): 9 MiB with the
    // deployed threshold, 98-103 MiB without; release build 9 and 102 MiB.
    assert!(deployed < 32, "the load left {deployed} MiB behind");
    assert!(
        dynamic > deployed + 32,
        "glibc's dynamic threshold kept only {dynamic} MiB: the setting may no longer be needed"
    );
}
