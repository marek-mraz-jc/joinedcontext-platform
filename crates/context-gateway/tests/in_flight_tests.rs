//! T-4782: the gateway works on a bounded number of requests at once, in total and per caller. A
//! broker that answers slowly fills the slots and not the gateway's memory: a request past them is
//! answered 503 with `Retry-After` at once and never reaches the broker, and the slots come back
//! when the admitted requests are answered. T-5124: anonymous requests share half of the slots, so
//! an anonymous flood is answered 429 and a signed-in caller still gets in.

use axum::body::Body;
use axum::extract::Request;
use axum::http::StatusCode;
use axum::routing::any;
use axum::Router;
use context_gateway::app::{router, Gateway};
use context_gateway::middleware::in_flight::anonymous_share;
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

/// The binary's allocator (`main.rs`), so the memory test below measures what is deployed.
#[global_allocator]
static ALLOCATOR: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

const SLUG: &str = "k7r2m4xq9vbn3tdw6hcy5pajfe";
const SPACE: &str = "ovzdusie";
const DOMAIN: &str = "banskabystrica.sk";
/// Entities in the broker's page: a full page of the gateway's own paging (`PAGE`).
const ENTITIES: usize = 1_000;

/// A broker that holds every read until `release` hands it a permit, then answers a page of
/// [`ENTITIES`] entities of about 1 KiB each, of `total` it says it holds; it counts the reads it
/// holds at once.
struct Slow {
    url: String,
    release: Arc<Semaphore>,
    holding: Arc<AtomicUsize>,
    most: Arc<AtomicUsize>,
    reads: Arc<AtomicUsize>,
}

async fn slow_broker(total: usize) -> Slow {
    let page: Vec<_> = (0..ENTITIES)
        .map(|n| {
            let mut entity = json!({
                "id": format!("urn:ngsi-ld:AirQualityObserved:{DOMAIN}:{SPACE}:st-{n}"),
                "type": "AirQualityObserved",
            });
            // Twelve measured Properties of about 75 bytes each, as a real observation holds.
            for attribute in 0..12 {
                entity[format!("a{attribute}")] = json!({
                    "type": "Property", "value": n + attribute, "observedAt": "2026-10-19T22:00:00Z"
                });
            }
            entity
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
        let total = total.to_string();
        async move {
            reads.fetch_add(1, Ordering::SeqCst);
            let now = holding.fetch_add(1, Ordering::SeqCst) + 1;
            most.fetch_max(now, Ordering::SeqCst);
            gate.acquire().await.expect("the gate").forget();
            holding.fetch_sub(1, Ordering::SeqCst);
            (
                [
                    ("content-type", "application/json".to_owned()),
                    ("ngsild-results-count", total),
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
        deprecation: None,
        feeds: Vec::new(),
        calendars: Vec::new(),
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
        .uri(format!(
            "/api/endpoint/{SLUG}/file.csv?type=AirQualityObserved"
        ))
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

/// The 429 of a full anonymous share (T-5124).
async fn assert_too_many(answer: axum::response::Response) {
    assert_eq!(answer.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(answer.headers()["retry-after"], "1");
    let body = axum::body::to_bytes(answer.into_body(), 1 << 16)
        .await
        .expect("the body");
    let problem: serde_json::Value = serde_json::from_slice(&body).expect("a problem");
    assert_eq!(problem["status"], 429, "{problem}");
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

/// The load test: 128 anonymous callers of 4 reads each against a broker that holds every read. The
/// gateway forwards the anonymous half of its 16 slots and answers the other 504 429 at once, so its memory stays under a fixed bound
/// however many come in; without the cap all 512 reach the broker and their pages arrive together.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_slow_broker_fills_the_slots_and_not_the_memory() {
    const CAP: usize = 16;
    const SENT: usize = 512;
    let admitted = anonymous_share(CAP);
    let broker = slow_broker(ENTITIES).await;
    let app = gateway(&broker, CAP, 4);
    let before = peak_kib();

    let started = Instant::now();
    let answers: Vec<_> = (0..SENT)
        .map(|n| tokio::spawn(app.clone().oneshot(read(&format!("192.0.2.{}", n % 128)))))
        .collect();
    // Every request past the cap is answered while the broker still holds the admitted ones.
    holding(&broker, admitted).await;
    let deadline = Instant::now() + Duration::from_secs(30);
    while answers.iter().filter(|answer| answer.is_finished()).count() < SENT - admitted {
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
            assert_too_many(answer).await;
            busy += 1;
        }
    }
    let grown_mib = peak_kib().saturating_sub(before) / 1024;
    eprintln!("cap {CAP}: {busy} of {SENT} answered 429 in {shed:?}; peak memory grew {grown_mib} MiB; broker held at most {}", broker.most.load(Ordering::SeqCst));

    assert_eq!(
        broker.most.load(Ordering::SeqCst),
        admitted,
        "the broker held at most the anonymous share"
    );
    assert_eq!(
        broker.reads.load(Ordering::SeqCst),
        admitted,
        "no 429 reached the broker"
    );
    assert_eq!(busy, SENT - admitted);
    // Measured (debug build, 1 MiB pages): cap 16 grew 8 MiB and shed 496 in 12 ms, cap 64 grew
    // 15 MiB; without a cap all 512 reached the broker and the peak grew 83 MiB.
    assert!(grown_mib < 48, "peak memory grew {grown_mib} MiB");
}

/// T-5124: anonymous readers from as many addresses as they like hold the anonymous half and are
/// answered 429 past it, while a caller with a token is still admitted: here the handler answers
/// it 401, as no realm is configured, which it can only do once the gateway let it in.
#[tokio::test]
async fn an_anonymous_flood_is_answered_429_and_a_signed_in_caller_still_gets_in() {
    let broker = slow_broker(ENTITIES).await;
    let app = gateway(&broker, 8, 8);
    let flood: Vec<_> = (0..anonymous_share(8))
        .map(|n| tokio::spawn(app.clone().oneshot(read(&format!("192.0.2.{n}")))))
        .collect();
    holding(&broker, anonymous_share(8)).await;
    assert_too_many(
        app.clone()
            .oneshot(read("198.51.100.7"))
            .await
            .expect("an answer"),
    )
    .await;

    let mut signed = read("198.51.100.8");
    signed.headers_mut().insert(
        axum::http::header::AUTHORIZATION,
        axum::http::HeaderValue::from_static("Bearer a.b.c"),
    );
    let answer = app.clone().oneshot(signed).await.expect("an answer");
    assert_eq!(answer.status(), StatusCode::UNAUTHORIZED);

    broker.release.add_permits(flood.len());
    for answer in flood {
        assert_eq!(
            answer.await.expect("joined").expect("answered").status(),
            StatusCode::OK
        );
    }
    assert_eq!(broker.reads.load(Ordering::SeqCst), anonymous_share(8));
}

#[tokio::test]
async fn one_caller_past_its_share_is_refused_while_another_is_served() {
    let broker = slow_broker(ENTITIES).await;
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
    let broker = slow_broker(ENTITIES).await;
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

/// Entities the broker says it holds for the load below: a `file.csv` of 40 pages, 40 MiB of
/// broker JSON held whole while the table is built.
const DOWNLOADED: usize = 40 * ENTITIES;

/// Waves of full-page reads from 64 callers and of `file.csv` downloads through the gateway's
/// default caps (128 in flight, 32 per caller, 2 downloads), every answer read whole; returns the
/// MiB this process grew by at the peak and the MiB it still holds once it went quiet.
async fn a_load_of_reads_and_downloads() -> (u64, u64) {
    let broker = slow_broker(DOWNLOADED).await;
    broker.release.add_permits(Semaphore::MAX_PERMITS);
    let app = router(Arc::new(
        Gateway::new(Broker::new(broker.url.clone()), Box::new(PolicyPdp), DOMAIN)
            .limit_in_flight(128, 32, 2)
            .serve([endpoint()]),
    ));
    let before = resident_kib();
    // What a gateway keeps from a busy minute (connection pools, the rate limiter's buckets, a
    // reloaded Endpoint table): small blocks allocated between the answers that outlive them.
    let mut kept: Vec<Vec<u8>> = Vec::new();
    for wave in 0..6 {
        let answers: Vec<_> = (0..200)
            .map(|n| {
                let caller = format!("192.0.2.{}", (n + wave) % 64);
                let request = if n % 25 == 0 {
                    download(&caller)
                } else {
                    read(&caller)
                };
                tokio::spawn(app.clone().oneshot(request))
            })
            .collect();
        for answer in answers {
            let answer = answer.await.expect("joined").expect("answered");
            if answer.status() == StatusCode::OK {
                axum::body::to_bytes(answer.into_body(), 256 << 20)
                    .await
                    .expect("the answer");
            } else if answer.status() == StatusCode::TOO_MANY_REQUESTS {
                assert_too_many(answer).await;
            } else {
                assert_busy(answer).await;
            }
        }
        // A reload of the Endpoint table between two waves: 2 MiB of small blocks, kept.
        kept.extend((0..2_000).map(|_| vec![1; 1024]));
    }
    let peak = peak_kib().saturating_sub(before) / 1024;
    // jemalloc purges what was freed after `dirty_decay_ms`, on its background thread.
    // A quiet gateway still answers a probe now and then.
    for _ in 0..30 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        std::hint::black_box(vec![0u8; 4096]);
    }
    let idle = resident_kib().saturating_sub(before) / 1024;
    drop(kept);
    (peak, idle)
}

/// Set in the child process of the test below, which runs the load and prints what it left.
const LOAD_CHILD: &str = "JC_GATEWAY_TEST_LOAD_CHILD";

/// The allocator settings the image starts the gateway with (`_RJEM_MALLOC_CONF` in the
/// Dockerfile), read from there so this test measures what is deployed.
fn deployed_malloc_conf() -> String {
    let dockerfile = include_str!("../../../Dockerfile");
    dockerfile
        .split_whitespace()
        .find_map(|word| word.strip_prefix("_RJEM_MALLOC_CONF="))
        .expect("the Dockerfile sets _RJEM_MALLOC_CONF")
        .to_owned()
}

/// T-4782 (reopened twice): prod's gateway went 328 -> 966 Mi in the first minute of a 100 req/s
/// run and idled at 928 Mi of its 1 Gi after it, with glibc's malloc and a fixed mmap threshold.
/// A download held every entity of its 40 MiB of broker JSON parsed, about 600 MiB of small
/// blocks, until it built the table; glibc gives back only the top of its heap, so they stayed.
/// Now the table takes each page as it arrives, and the binary allocates with jemalloc
/// (`main.rs`, as this test binary does), whose background thread returns freed pages. The load
/// runs in a child process of its own (this test, `LOAD_CHILD` set) with the image's allocator
/// settings, so its memory is no other test's.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_memory_of_a_load_comes_back_once_it_ends() {
    const NAME: &str = "the_memory_of_a_load_comes_back_once_it_ends";
    if std::env::var_os(LOAD_CHILD).is_some() {
        let (peak, idle) = a_load_of_reads_and_downloads().await;
        eprintln!("load MiB {peak} {idle}");
        return;
    }
    assert!(
        include_str!("../src/main.rs").contains("static ALLOCATOR: tikv_jemallocator::Jemalloc"),
        "the binary no longer allocates with jemalloc, which this test measures"
    );
    assert_eq!(
        include_str!("../../../Dockerfile")
            .matches("cargo build --release --locked --workspace --exclude assistant --features context-gateway/jemalloc")
            .count(),
        2,
        "both workspace builds of the image turn on the gateway's jemalloc feature (T-5017)"
    );
    let output = std::process::Command::new(std::env::current_exe().expect("this test"))
        .args(["--exact", NAME, "--nocapture"])
        .env(LOAD_CHILD, "1")
        .env("_RJEM_MALLOC_CONF", deployed_malloc_conf())
        .output()
        .expect("the load ran");
    let printed = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{printed}");
    let (peak, idle) = printed
        .lines()
        .find_map(|line| line.strip_prefix("load MiB "))
        .and_then(|mib| mib.trim().split_once(' '))
        .and_then(|(peak, idle)| Some((peak.parse::<u64>().ok()?, idle.parse::<u64>().ok()?)))
        .unwrap_or_else(|| panic!("no measurement in {printed}"));
    eprintln!("the load grew the process by {peak} MiB at its peak and left {idle} MiB");
    // Measured (4 worker threads): this build 346-364 MiB at the peak and 70-72 MiB left in
    // release, 271-306 and 60-70 in debug; 12 MiB of what is left is `kept`.
    // A download's entities held whole until the table was built: 1,254 MiB at the peak.
    // glibc's malloc with the mmap threshold prod ran: 342-363 MiB at the peak, 304-313 left.
    assert!(
        peak > 64,
        "the load held only {peak} MiB: it no longer measures anything"
    );
    assert!(
        peak < 640,
        "the load grew the process by {peak} MiB: a download holds more than its table"
    );
    assert!(
        idle < 128,
        "the load left {idle} MiB of its {peak} MiB peak behind"
    );
}
