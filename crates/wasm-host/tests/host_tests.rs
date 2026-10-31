//! jc-wasm-host against real components (T-3339, AP-143, AP-146, AP-147): the test Apps of
//! `tests/guest` (a `jc:app@0.1.0` component) and `tests/services-guest` (`jc-app-sdk` over
//! `jc:app@0.1.1`, T-3622), built for wasm32-wasip2 here (`rustup target add wasm32-wasip2`),
//! served by a shard whose components sit in a directory. The gateway and the Portal are mocks that
//! record what reached them.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use bytes::Bytes;
use http_body_util::BodyExt;
use sha2::{Digest, Sha256};
use wasm_host::host::{Failure, Host};
use wasm_host::limits::Limits;
use wasm_host::placement::Placed;
use wasm_host::source::Source;
use wasm_host::storage::Unconfigured;
use wiremock::matchers::{any, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// The test App, a component of `jc:app@0.1.0` as published Apps are, built once per test binary.
fn guest() -> &'static [u8] {
    static BYTES: OnceLock<Vec<u8>> = OnceLock::new();
    BYTES.get_or_init(|| build("guest", "wasm_host_test_guest"))
}

/// The services test App on `jc-app-sdk` over `jc:app@0.1.1` (T-3622), built once per test binary.
fn services_guest() -> &'static [u8] {
    static BYTES: OnceLock<Vec<u8>> = OnceLock::new();
    BYTES.get_or_init(|| build("services-guest", "wasm_host_services_guest"))
}

fn build(dir: &str, lib: &str) -> Vec<u8> {
    {
        let manifest =
            Path::new(env!("CARGO_MANIFEST_DIR")).join(format!("tests/{dir}/Cargo.toml"));
        let target = std::env::var("CARGO_TARGET_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|_| Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target"))
            .join(format!("wasm-host-{dir}"));
        let status = Command::new(std::env::var("CARGO").unwrap_or_else(|_| "cargo".into()))
            .args([
                "build",
                "--release",
                "--target",
                "wasm32-wasip2",
                "--manifest-path",
            ])
            .arg(&manifest)
            .env("CARGO_TARGET_DIR", &target)
            .env_remove("RUSTFLAGS")
            .status()
            .expect("cargo runs");
        assert!(
            status.success(),
            "the test App builds for wasm32-wasip2 (rustup target add wasm32-wasip2)"
        );
        std::fs::read(target.join(format!("wasm32-wasip2/release/{lib}.wasm")))
            .expect("the test App")
    }
}

fn digest(bytes: &[u8]) -> String {
    format!("sha256:{}", hex::encode(Sha256::digest(bytes)))
}

/// The same component with a custom section of its own, so its digest is its own.
fn variant(n: u32) -> Vec<u8> {
    let mut bytes = guest().to_vec();
    let name = b"jc-test";
    let payload = n.to_le_bytes();
    let content = 1 + name.len() + payload.len();
    bytes.push(0);
    bytes.push(content as u8);
    bytes.push(name.len() as u8);
    bytes.extend_from_slice(name);
    bytes.extend_from_slice(&payload);
    bytes
}

struct World {
    dir: PathBuf,
    host: Arc<Host>,
}

impl World {
    fn new(case: &str, limits: Limits, gateway: Option<&str>) -> Self {
        Self::with_portal(case, limits, gateway, None)
    }

    fn with_portal(
        case: &str,
        limits: Limits,
        gateway: Option<&str>,
        portal: Option<&str>,
    ) -> Self {
        let dir = std::env::temp_dir().join(format!("wasm-host-{case}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("dir");
        let host = Host::new(
            limits,
            Source::Dir(dir.clone()),
            Arc::new(Unconfigured),
            gateway,
            portal,
        )
        .expect("host");
        Self { dir, host }
    }

    /// Places `bytes` as App `name` and returns its placement.
    fn place(&self, name: &str, bytes: &[u8]) -> Placed {
        let digest = digest(bytes);
        std::fs::write(
            self.dir.join(format!("sha256-{}.wasm", &digest[7..])),
            bytes,
        )
        .expect("component");
        Placed {
            name: name.into(),
            id: format!("{name}-id"),
            tenant: "helsinki".into(),
            digest,
            endpoint: Some("ep1".into()),
            jobs: Vec::new(),
        }
    }

    async fn get(
        &self,
        app: &Placed,
        path: &str,
        token: Option<&str>,
    ) -> Result<(u16, String), Failure> {
        self.call("GET", app, path, token).await
    }

    async fn call(
        &self,
        method: &str,
        app: &Placed,
        path: &str,
        token: Option<&str>,
    ) -> Result<(u16, String), Failure> {
        let request = http::Request::builder()
            .method(method)
            .uri(format!("http://apps.test/apps/{}{path}", app.name))
            .header("authorization", "Bearer from-the-caller")
            .header("cookie", "session=secret")
            .body(Bytes::new())
            .expect("request");
        let response = self
            .host
            .serve(app, request, token.map(str::to_owned))
            .await?;
        let status = response.status().as_u16();
        let body = response
            .into_body()
            .collect()
            .await
            .map(|b| b.to_bytes())
            .unwrap_or_default();
        Ok((status, String::from_utf8_lossy(&body).into_owned()))
    }
}

fn fast() -> Limits {
    Limits {
        wall_time: Duration::from_secs(1),
        ..Limits::default()
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn an_app_answers_below_its_own_path_and_never_sees_the_callers_credentials() {
    let world = World::new("hello", Limits::default(), None);
    let app = world.place("notes", guest());
    assert_eq!(
        world.get(&app, "/api/hello?x=1", None).await.unwrap(),
        (200, "hello /api/hello?x=1".into())
    );
    let (_, headers) = world.get(&app, "/api/headers", None).await.unwrap();
    assert!(
        !headers.contains("authorization") && !headers.contains("cookie"),
        "{headers}"
    );
    assert_eq!(world.get(&app, "/api/nothing", None).await.unwrap().0, 404);
    // The storage interfaces are linked and answer as the shard's stores do.
    let (_, sql) = world.get(&app, "/api/sql", None).await.unwrap();
    assert!(
        sql.starts_with("sql: ") && sql.contains("Unavailable"),
        "{sql}"
    );
}

/// AP-147: a component reaches nothing but HTTP through the gateway. No environment variable,
/// no directory, no name lookup, and no socket even to a port that is listening on the host
/// (T-3588 path 3: an App reaching a socket, the environment or the file system).
#[tokio::test(flavor = "multi_thread")]
async fn a_component_reaches_no_environment_file_or_socket() {
    let listening = std::net::TcpListener::bind("127.0.0.1:0").expect("a free port");
    let port = listening.local_addr().expect("the bound address").port();
    let world = World::new("reach", Limits::default(), None);
    let app = world.place("prober", guest());
    let (status, reached) = world
        .get(&app, &format!("/api/reach?{port}"), None)
        .await
        .unwrap();
    assert_eq!(status, 200, "{reached}");
    assert!(reached.starts_with("env=0 dirs=0 "), "{reached}");
    for (what, granted) in [
        ("lookup", "resolved"),
        ("tcp", "connected"),
        ("udp", "bound"),
    ] {
        let got = reached
            .split(' ')
            .find_map(|part| part.strip_prefix(&format!("{what}=")))
            .unwrap_or_default();
        assert!(
            got.starts_with("refused"),
            "{what} answered {got:?} ({reached}), not {granted}"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn every_request_is_a_fresh_instance_and_two_apps_share_nothing() {
    let world = World::new("state", Limits::default(), None);
    let a = world.place("a", &variant(1));
    let b = world.place("b", &variant(2));
    for app in [&a, &a, &b, &a] {
        assert_eq!(
            world.get(app, "/api/state", None).await.unwrap(),
            (200, "1".into())
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn an_app_that_never_ends_is_stopped_at_its_wall_time_and_others_go_on() {
    let world = World::new("loop", Limits::default(), None);
    let app = world.place("spin", guest());
    let started = Instant::now();
    let stopped = world.get(&app, "/api/loop", None).await.unwrap_err();
    let took = started.elapsed();
    assert_eq!(stopped, Failure::Timeout);
    // Only the lower bound: the App had its whole wall time. A loaded host makes the run
    // longer, never shorter, so this cannot fail for the machine's sake (T-3538).
    assert!(took >= Duration::from_secs(5), "stopped after {took:?}");
    assert_eq!(world.get(&app, "/api/hello", None).await.unwrap().0, 200);
}

/// T-4591: spinning guest code never yields, and on one worker thread a tokio clock would stop
/// with it, so the wall time never fired and fuel ended the run instead. The clock is a thread of
/// its own and each tick yields: the spin is stopped as a timeout, and another request is answered
/// on the same worker while it spins.
#[tokio::test(flavor = "current_thread")]
async fn a_spin_on_the_only_worker_thread_is_stopped_at_its_wall_time_and_yields_meanwhile() {
    let world = World::new("loop-one-thread", Limits::default(), None);
    let app = world.place("spin", guest());
    let spinning = world.get(&app, "/api/loop", None);
    let other = async {
        tokio::time::sleep(Duration::from_millis(200)).await;
        let started = Instant::now();
        let answer = world.get(&app, "/api/hello", None).await.unwrap().0;
        (answer, started.elapsed())
    };
    let (stopped, (answer, waited)) = tokio::join!(spinning, other);
    assert_eq!(stopped.unwrap_err(), Failure::Timeout);
    assert_eq!(answer, 200);
    assert!(
        waited < Duration::from_secs(4),
        "the other request waited {waited:?} behind the spin"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn an_app_asking_for_more_than_its_memory_fails_alone() {
    let world = World::new("alloc", fast(), None);
    let app = world.place("greedy", guest());
    let other = world.place("calm", &variant(3));
    assert!(matches!(
        world.get(&app, "/api/alloc", None).await,
        Err(Failure::Failed(_))
    ));
    assert_eq!(world.get(&other, "/api/hello", None).await.unwrap().0, 200);
    assert_eq!(world.get(&app, "/api/hello", None).await.unwrap().0, 200);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_component_that_does_not_match_its_digest_is_refused() {
    let world = World::new("digest", Limits::default(), None);
    let mut app = world.place("swapped", guest());
    // The store holds other bytes under the recorded digest: a component swapped after its build.
    std::fs::write(
        world.dir.join(format!("sha256-{}.wasm", &app.digest[7..])),
        variant(9),
    )
    .expect("swap");
    match world.get(&app, "/api/hello", None).await {
        Err(Failure::Unavailable(why)) => assert!(why.contains("its placement names"), "{why}"),
        other => panic!("{other:?}"),
    }
    app.digest = format!("sha256:{}", "0".repeat(64));
    assert!(matches!(
        world.get(&app, "/api/hello", None).await,
        Err(Failure::Unavailable(_))
    ));
}

#[tokio::test(flavor = "multi_thread")]
async fn outgoing_http_reaches_the_gateway_alone_with_the_callers_token() {
    let gateway = MockServer::start().await;
    Mock::given(any())
        .respond_with(ResponseTemplate::new(200))
        .mount(&gateway)
        .await;
    let elsewhere = MockServer::start().await;
    Mock::given(any())
        .respond_with(ResponseTemplate::new(200))
        .mount(&elsewhere)
        .await;
    let world = World::new("fetch", Limits::default(), Some(&gateway.uri()));
    let app = world.place("caller", guest());

    let (_, answer) = world
        .get(
            &app,
            "/api/fetch?http://gateway/ngsi-ld/v1/entities?type=Alert",
            Some("tok-1"),
        )
        .await
        .unwrap();
    assert_eq!(answer, "status 200");
    let seen = gateway.received_requests().await.unwrap_or_default();
    assert_eq!(seen.len(), 1);
    assert_eq!(
        seen[0].url.path(),
        "/api/endpoint/ep1/ngsi-ld/v1/entities",
        "the App's own Endpoint and no other"
    );
    assert_eq!(seen[0].url.query(), Some("type=Alert"));
    assert_eq!(
        seen[0]
            .headers
            .get("authorization")
            .map(|v| v.to_str().unwrap_or("")),
        Some("Bearer tok-1")
    );
    assert!(seen[0].headers.get("cookie").is_none());

    for path in [
        format!(
            "/api/fetch?{}/api/endpoint/ep1/ngsi-ld/v1/entities",
            gateway.uri()
        ),
        "/api/fetch?http://gateway/ngsi-ld/v1/../../other/ngsi-ld/v1/entities".to_owned(),
    ] {
        let (_, answer) = world.get(&app, &path, Some("tok-1")).await.unwrap();
        assert!(answer.starts_with("refused"), "{path}: {answer}");
    }
    assert_eq!(
        gateway.received_requests().await.unwrap_or_default().len(),
        1,
        "the gateway's own address and a step out of the Endpoint are refused"
    );
    let (_, answer) = world
        .get(
            &app,
            &format!("/api/fetch?{}/x", elsewhere.uri()),
            Some("tok-1"),
        )
        .await
        .unwrap();
    assert!(answer.starts_with("refused"), "{answer}");
    let (_, answer) = world
        .get(
            &app,
            "/api/fetch?http://169.254.169.254/latest/meta-data",
            None,
        )
        .await
        .unwrap();
    assert!(answer.starts_with("refused"), "{answer}");
    assert!(
        elsewhere
            .received_requests()
            .await
            .unwrap_or_default()
            .is_empty(),
        "nothing reached another host"
    );
}

/// AP-147: a component has no environment, so it calls the gateway as `http://gateway` and the
/// host sends the call to the App's own Endpoint on the gateway it is configured with, in the long
/// form (`/api/endpoint/<slug>/…`, T-3346) or the short one (T-3351); a look-alike origin and
/// another Endpoint go nowhere.
#[tokio::test(flavor = "multi_thread")]
async fn the_gateway_alias_reaches_the_apps_own_endpoint_alone() {
    let gateway = MockServer::start().await;
    Mock::given(any())
        .respond_with(ResponseTemplate::new(200))
        .mount(&gateway)
        .await;
    let world = World::new("alias", Limits::default(), Some(&gateway.uri()));
    let app = world.place("aliased", guest());

    let (_, answer) = world
        .get(
            &app,
            "/api/fetch?http://gateway/ngsi-ld/v1/entities?type=Road",
            Some("tok-2"),
        )
        .await
        .unwrap();
    assert_eq!(answer, "status 200");
    let (_, answer) = world
        .get(
            &app,
            "/api/fetch?http://gateway/api/endpoint/ep1/ngsi-ld/v1/entities?type=Road",
            Some("tok-2"),
        )
        .await
        .unwrap();
    assert_eq!(answer, "status 200");
    let seen = gateway.received_requests().await.unwrap_or_default();
    assert_eq!(seen.len(), 2);
    for request in &seen {
        assert_eq!(request.url.path(), "/api/endpoint/ep1/ngsi-ld/v1/entities");
        assert_eq!(request.url.query(), Some("type=Road"));
    }
    assert_eq!(
        seen[0]
            .headers
            .get("authorization")
            .map(|v| v.to_str().unwrap_or("")),
        Some("Bearer tok-2")
    );

    for target in [
        "https://gateway/ngsi-ld/v1/entities",
        "http://gateway.evil/ngsi-ld/v1/entities",
        "http://gateway:9999/ngsi-ld/v1/entities",
        "http://gateway/api/endpoint/other/ngsi-ld/v1/entities",
        "http://gateway/x",
    ] {
        let (_, answer) = world
            .get(&app, &format!("/api/fetch?{target}"), None)
            .await
            .unwrap();
        assert!(answer.starts_with("refused"), "{target}: {answer}");
    }
    assert_eq!(
        gateway.received_requests().await.unwrap_or_default().len(),
        2
    );

    let unconfigured = World::new("alias-none", Limits::default(), None);
    let app = unconfigured.place("aliased", guest());
    let (_, answer) = unconfigured
        .get(&app, "/api/fetch?http://gateway/x", None)
        .await
        .unwrap();
    assert!(answer.starts_with("refused"), "{answer}");
}

/// How long the mock gateway holds its answer: far beyond the 1 s wall time and its grace.
const GATEWAY_DELAY: Duration = Duration::from_secs(60);

/// T-3342: the wall time holds while the App waits on the host, not only while it computes.
#[tokio::test(flavor = "multi_thread")]
async fn a_slow_gateway_call_ends_with_the_requests_wall_time() {
    let gateway = MockServer::start().await;
    Mock::given(any())
        .respond_with(ResponseTemplate::new(200).set_delay(GATEWAY_DELAY))
        .mount(&gateway)
        .await;
    let world = World::new("slow", fast(), Some(&gateway.uri()));
    let app = world.place("waiter", guest());
    let started = Instant::now();
    let path = "/api/fetch?http://gateway/ngsi-ld/v1/entities".to_owned();
    assert_eq!(
        world.get(&app, &path, None).await.unwrap_err(),
        Failure::Timeout
    );
    let took = started.elapsed();
    // Ordered against the gateway's own answer, not a budget: the call ended before the
    // gateway would have answered (T-3538).
    assert!(
        took < GATEWAY_DELAY,
        "the 1 s wall time, not the gateway's {GATEWAY_DELAY:?}: {took:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn an_app_past_its_concurrency_is_busy_and_a_large_body_is_refused() {
    let world = World::new(
        "busy",
        Limits {
            per_app_concurrency: 1,
            request_bytes: 16,
            ..fast()
        },
        None,
    );
    let app = world.place("one", guest());
    let host = world.host.clone();
    let spin = app.clone();
    let first = tokio::spawn(async move {
        host.serve(
            &spin,
            http::Request::builder()
                .uri("/apps/one/api/loop")
                .body(Bytes::new())
                .unwrap(),
            None,
        )
        .await
        .map(|_| ())
    });
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(
        world.get(&app, "/api/hello", None).await.unwrap_err(),
        Failure::Busy
    );
    assert_eq!(first.await.unwrap(), Err(Failure::Timeout));
    let big = http::Request::builder()
        .uri("/apps/one/api/hello")
        .body(Bytes::from(vec![0u8; 17]))
        .unwrap();
    assert_eq!(
        world.host.serve(&app, big, None).await.unwrap_err(),
        Failure::TooLarge
    );
}

/// AP-143: one shard compiles, keeps and serves a thousand different Apps. Prints what it cost.
#[tokio::test(flavor = "multi_thread")]
async fn a_thousand_different_components_load_and_answer_on_one_shard() {
    let world = World::new("thousand", Limits::default(), None);
    let rss = || {
        std::fs::read_to_string("/proc/self/status")
            .ok()
            .and_then(|s| {
                s.lines()
                    .find(|l| l.starts_with("VmRSS:"))
                    .map(str::to_owned)
            })
            .unwrap_or_default()
    };
    let before = rss();
    let apps: Vec<Placed> = (0..1000)
        .map(|n| world.place(&format!("app{n}"), &variant(10_000 + n)))
        .collect();
    let started = Instant::now();
    let mut cold = Vec::new();
    for app in &apps {
        let t = Instant::now();
        assert_eq!(
            world.get(app, "/api/hello", None).await.expect("answers").0,
            200
        );
        cold.push(t.elapsed());
    }
    let loaded = started.elapsed();
    let mut warm = Vec::new();
    for app in apps.iter().step_by(10) {
        let t = Instant::now();
        assert_eq!(world.get(app, "/api/hello", None).await.unwrap().0, 200);
        warm.push(t.elapsed());
    }
    warm.sort();
    cold.sort();
    assert_eq!(world.host.cached(), 1000);
    println!(
        "1000 components: loaded and answered in {loaded:?}; RSS before {before}, after {}; cold p50 {:?} p99 {:?}; warm p50 {:?} p99 {:?}",
        rss(),
        cold[cold.len() / 2],
        cold[cold.len() * 99 / 100],
        warm[warm.len() / 2],
        warm[warm.len() * 99 / 100]
    );
}

/// AP-154, AP-159 (T-3372): a job's export runs in a fresh instance, and its calls reach the
/// App's own Endpoint with the job principal's token, which the component never holds.
#[tokio::test(flavor = "multi_thread")]
async fn a_job_runs_its_export_and_writes_with_the_job_principals_token_alone() {
    let gateway = MockServer::start().await;
    Mock::given(any())
        .respond_with(ResponseTemplate::new(200))
        .mount(&gateway)
        .await;
    let world = World::new("job", Limits::default(), Some(&gateway.uri()));
    let app = world.place("kpi", guest());
    let wall = Duration::from_secs(5);

    assert_eq!(
        world
            .host
            .run_job(&app, "tick", "job-at".into(), wall)
            .await,
        Ok(())
    );
    assert_eq!(
        world
            .host
            .run_job(&app, "call-gateway", "job-at".into(), wall)
            .await,
        Ok(())
    );
    let seen = gateway.received_requests().await.unwrap_or_default();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].url.path(), "/api/endpoint/ep1/ngsi-ld/v1/entities");
    assert_eq!(
        seen[0]
            .headers
            .get("authorization")
            .and_then(|v| v.to_str().ok()),
        Some("Bearer job-at")
    );
}

/// AP-155: a run's failure is the export's own sentence; a missing export or one of another shape
/// is named, not a trap.
#[tokio::test(flavor = "multi_thread")]
async fn a_failed_run_says_why_and_a_missing_or_misshapen_export_is_named() {
    let world = World::new("job-fail", Limits::default(), None);
    let app = world.place("kpi", guest());
    let wall = Duration::from_secs(5);
    assert_eq!(
        world.host.run_job(&app, "fail", "t".into(), wall).await,
        Err("the indicator could not be computed: no readings in the last hour".into())
    );
    for export in ["hourly", "wrong-shape"] {
        let why = world
            .host
            .run_job(&app, export, "t".into(), wall)
            .await
            .expect_err("refused");
        assert!(
            why.contains(&format!("`{export}: func() -> result<_, string>`")),
            "{why}"
        );
    }
}

/// F-R2PLT4-010: a run past its wall time while it waits on the gateway is stopped there, its
/// call dropped, instead of living on outside the App's cap until the call gives up.
#[tokio::test(flavor = "multi_thread")]
async fn a_job_waiting_on_the_gateway_past_its_wall_time_is_dropped() {
    use tokio::io::AsyncReadExt;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("a port");
    let url = format!("http://{}", listener.local_addr().expect("addr"));
    let world = World::new("job-waits", Limits::default(), Some(&url));
    let app = world.place("kpi", guest());
    let host = world.host.clone();
    let run = tokio::spawn(async move {
        host.run_job(&app, "call-gateway", "t".into(), Duration::from_secs(1))
            .await
    });
    // The gateway takes the call and never answers.
    let (mut socket, _) = listener.accept().await.expect("the call");
    let why = run.await.expect("joined").expect_err("stopped");
    assert!(why.contains("wall time"), "{why}");
    let mut rest = Vec::new();
    let closed = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            match socket.read_buf(&mut rest).await {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
        }
    })
    .await;
    assert!(closed.is_ok(), "the stopped run's call is still open");
}

/// AP-154: a run past its wall time is stopped, and the App still answers requests after it.
#[tokio::test(flavor = "multi_thread")]
async fn a_job_past_its_wall_time_is_stopped() {
    let world = World::new("job-spin", Limits::default(), None);
    let app = world.place("kpi", guest());
    let started = Instant::now();
    let why = world
        .host
        .run_job(&app, "spin", "t".into(), Duration::from_secs(1))
        .await
        .expect_err("stopped");
    assert!(why.contains("wall time"), "{why}");
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "{:?}",
        started.elapsed()
    );
    assert_eq!(world.get(&app, "/api/hello", None).await.unwrap().0, 200);
}

/// AP-159: the runner asks the token service for `{project}/{app}` and runs with its answer; with
/// no service, or a refusal, the run fails saying so and the component is never started.
#[tokio::test(flavor = "multi_thread")]
async fn a_run_takes_its_token_from_the_token_service_or_does_not_run() {
    use wasm_host::jobs::{HostRunner, JobTokens, Runner};
    use wiremock::matchers::{body_string_contains, method, path};

    let sidecar = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/token"))
        .and(body_string_contains("client_id=helsinki%2Fkpi"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "access_token": "job-at", "token_type": "Bearer", "expires_in": 300
        })))
        .mount(&sidecar)
        .await;
    let world = World::new("job-token", Limits::default(), None);
    let app = world.place("kpi", guest());
    let job = wasm_host::placement::PlacedJob {
        name: "hourly".into(),
        schedule: "0 * * * *".into(),
        export: "tick".into(),
    };
    let runner = HostRunner {
        host: world.host.clone(),
        tokens: Some(JobTokens::new(format!("{}/token", sidecar.uri())).expect("client")),
    };
    assert_eq!(runner.run(&app, &job).await, Ok(()));

    let other = Placed {
        name: "other".into(),
        ..app.clone()
    };
    let refused = runner.run(&other, &job).await.expect_err("no token");
    assert!(refused.contains("got no token"), "{refused}");

    let none = HostRunner {
        host: world.host.clone(),
        tokens: None,
    };
    let why = none.run(&app, &job).await.expect_err("no service");
    assert!(why.contains("JC_WASM_JOB_TOKEN_URL"), "{why}");
}

/// T-3345: the shard counts how its compiled-component cache answered, so a load test reads the
/// hit rate from `/metrics` instead of guessing it.
#[tokio::test(flavor = "multi_thread")]
async fn the_cache_counts_its_hits_and_misses() {
    let world = World::new("cache-stats", Limits::default(), None);
    let one = world.place("one", &variant(1));
    let two = world.place("two", &variant(2));
    for app in [&one, &one, &one, &two] {
        assert_eq!(
            world.get(app, "/api/hello", None).await.expect("answers").0,
            200
        );
    }
    let stats = world.host.cache_stats();
    assert_eq!((stats.cached, stats.hits, stats.misses), (2, 2, 2));
}

/// F-PLT3-022: a burst of first requests to a cold App fetches and compiles its component once.
#[tokio::test(flavor = "multi_thread")]
async fn a_burst_on_a_cold_app_compiles_its_component_once() {
    let world = Arc::new(World::new("cache-burst", Limits::default(), None));
    let one = Arc::new(world.place("one", &variant(1)));
    let burst: Vec<_> = (0..16)
        .map(|_| {
            let (world, one) = (world.clone(), one.clone());
            tokio::spawn(async move { world.get(&one, "/api/hello", None).await })
        })
        .collect();
    for call in burst {
        assert_eq!(call.await.expect("joins").expect("answers").0, 200);
    }
    let stats = world.host.cache_stats();
    assert_eq!((stats.cached, stats.hits, stats.misses), (1, 15, 1));
}

/// A Portal that answers `GET …/me`, `email/send` and `ai/complete` of App `desk` with the given
/// answers, recording what reached it.
async fn portal(
    caller: ResponseTemplate,
    email: ResponseTemplate,
    ai: ResponseTemplate,
) -> MockServer {
    let portal = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/projects/helsinki/apps/desk/me"))
        .respond_with(caller)
        .mount(&portal)
        .await;
    Mock::given(method("POST"))
        .and(path("/apps/desk/api/services/email/send"))
        .respond_with(email)
        .mount(&portal)
        .await;
    Mock::given(method("POST"))
        .and(path("/apps/desk/api/services/ai/complete"))
        .respond_with(ai)
        .mount(&portal)
        .await;
    portal
}

fn problem(status: u16, body: serde_json::Value) -> ResponseTemplate {
    ResponseTemplate::new(status).set_body_raw(body.to_string(), "application/problem+json")
}

/// T-3622, API/06 §5: a component asks for its caller, sends a mail to them and asks the model,
/// and the host asks the Portal each time as the request's person, on the App's own host; the
/// component never sees the token.
#[tokio::test(flavor = "multi_thread")]
async fn identity_email_and_ai_ask_the_portal_as_the_requests_person() {
    let portal = portal(
        ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "u-1", "name": "Eero Laine", "email": "eero@hel.fi", "roles": ["steward"]
        })),
        ResponseTemplate::new(202).set_body_json(serde_json::json!({ "id": "msg-7" })),
        ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "text": "{\"severity\":4}", "json": { "severity": 4 }, "tokens": { "in": 21, "out": 6 }
        })),
    )
    .await;
    let world = World::with_portal("services", Limits::default(), None, Some(&portal.uri()));
    let app = world.place("desk", services_guest());

    let (status, me) = world.get(&app, "/api/caller", Some("tok-1")).await.unwrap();
    assert_eq!(status, 200, "{me}");
    let me: serde_json::Value = serde_json::from_str(&me).expect("json");
    assert_eq!(me["name"], "Eero Laine");
    assert_eq!(me["roles"], serde_json::json!(["steward"]));
    assert_eq!(
        world
            .call("POST", &app, "/api/email", Some("tok-1"))
            .await
            .unwrap(),
        (202, "msg-7".into())
    );
    assert_eq!(
        world
            .call("POST", &app, "/api/ai", Some("tok-1"))
            .await
            .unwrap(),
        (200, "{\"severity\":4} 21 6".into())
    );

    let seen = portal.received_requests().await.unwrap_or_default();
    assert_eq!(seen.len(), 3);
    for request in &seen {
        assert_eq!(
            request
                .headers
                .get("authorization")
                .and_then(|v| v.to_str().ok()),
            Some("Bearer tok-1"),
            "{}",
            request.url
        );
        assert!(request.headers.get("cookie").is_none());
    }
    let mail: serde_json::Value = serde_json::from_slice(&seen[1].body).expect("json");
    assert_eq!(
        mail,
        serde_json::json!({ "to": "me", "subject": "Today's defects", "text": "Three new." })
    );
    assert_eq!(
        seen[1].headers.get("host").and_then(|v| v.to_str().ok()),
        Some("apps.test"),
        "the service routes answer on the App's own host"
    );
    let asked: serde_json::Value = serde_json::from_slice(&seen[2].body).expect("json");
    assert_eq!(asked["maxTokens"], 20);
    assert_eq!(
        asked["schema"],
        serde_json::json!({ "type": "object", "required": ["severity"] })
    );
    assert_eq!(asked["messages"][0]["role"], "user");
}

/// T-3622, SDK-43: a service a layer keeps off and a used-up quota reach the component with their
/// members, which `Response::from_service` answers as the Portal's problem.
#[tokio::test(flavor = "multi_thread")]
async fn a_refusal_reaches_the_component_with_its_layer_or_its_quota() {
    for layer in ["organization", "project", "app"] {
        let portal = portal(
            ResponseTemplate::new(401),
            problem(403, serde_json::json!({ "type": "https://joinedcontext.com/errors/service-off", "service": "email", "layer": layer })),
            problem(429, serde_json::json!({ "type": "https://joinedcontext.com/errors/quota", "service": "ai", "quota": "aiTokensPerDay", "resetAt": "2026-10-12T00:00:00Z" })),
        )
        .await;
        let world = World::with_portal(
            &format!("refused-{layer}"),
            Limits::default(),
            None,
            Some(&portal.uri()),
        );
        let app = world.place("desk", services_guest());
        let (status, off) = world
            .call("POST", &app, "/api/email", Some("tok-1"))
            .await
            .unwrap();
        assert_eq!(status, 403, "{off}");
        let off: serde_json::Value = serde_json::from_str(&off).expect("json");
        assert_eq!(off["type"], "https://joinedcontext.com/errors/service-off");
        assert_eq!(off["layer"], layer);
        assert_eq!(off["service"], "email");

        let (status, spent) = world
            .call("POST", &app, "/api/ai", Some("tok-1"))
            .await
            .unwrap();
        assert_eq!(status, 429, "{spent}");
        let spent: serde_json::Value = serde_json::from_str(&spent).expect("json");
        assert_eq!(spent["quota"], "aiTokensPerDay");
        assert_eq!(spent["resetAt"], "2026-10-12T00:00:00Z");

        assert_eq!(
            world.get(&app, "/api/caller", Some("tok-1")).await.unwrap(),
            (200, "nobody".into()),
            "a 401 is nobody"
        );
    }
}

/// T-3622: without a person nothing is asked; without a Portal every call says so.
#[tokio::test(flavor = "multi_thread")]
async fn an_anonymous_request_or_a_shard_without_a_portal_asks_nothing() {
    let portal = portal(
        ResponseTemplate::new(500),
        ResponseTemplate::new(500),
        ResponseTemplate::new(500),
    )
    .await;
    let world = World::with_portal("anonymous", Limits::default(), None, Some(&portal.uri()));
    let app = world.place("desk", services_guest());
    assert_eq!(
        world.get(&app, "/api/caller", None).await.unwrap(),
        (200, "nobody".into())
    );
    let (status, said) = world.call("POST", &app, "/api/email", None).await.unwrap();
    assert_eq!(status, 422, "{said}");
    assert!(said.contains("in a person's name"), "{said}");
    assert!(portal
        .received_requests()
        .await
        .unwrap_or_default()
        .is_empty());

    let world = World::new("no-portal", Limits::default(), None);
    let app = world.place("desk", services_guest());
    let (status, said) = world
        .call("POST", &app, "/api/ai", Some("tok-1"))
        .await
        .unwrap();
    assert_eq!(status, 503, "{said}");
    assert!(said.contains("JC_WASM_PORTAL_URL"), "{said}");
    assert_eq!(
        world.get(&app, "/api/caller", Some("tok-1")).await.unwrap(),
        (200, "nobody".into())
    );
}

/// T-3857: every App's job records and the shard's counters answer on the admin port alone. The
/// apps port, the one the edge reaches, answers `/jobs` and `/metrics` with a 404 whatever token
/// the caller holds, and still serves the placed Apps; the admin port serves no App.
#[tokio::test(flavor = "multi_thread")]
async fn job_records_and_counters_answer_on_the_admin_port_alone() {
    use std::collections::HashMap;
    use std::sync::RwLock;
    use wasm_host::jobs::{HostRunner, Scheduler, WALL_TIME};
    use wasm_host::metrics::Metrics;
    use wasm_host::server::{Port, Shard};

    let world = World::new("admin-port", Limits::default(), None);
    let app = world.place("one", guest());
    let shard = Arc::new(Shard {
        id: "0".into(),
        host: world.host.clone(),
        apps: RwLock::new(HashMap::from([("one".to_owned(), app)])),
        metrics: Metrics::new(20),
        jobs: Arc::new(Scheduler::new(
            HostRunner {
                host: world.host.clone(),
                tokens: None,
            },
            WALL_TIME,
        )),
        files: None,
    });
    let bind = || tokio::net::TcpListener::bind("127.0.0.1:0");
    let (apps, admin) = (bind().await.expect("apps"), bind().await.expect("admin"));
    let apps_url = format!("http://{}", apps.local_addr().expect("addr"));
    let admin_url = format!("http://{}", admin.local_addr().expect("addr"));
    tokio::spawn(shard.clone().serve(apps, Port::Apps));
    tokio::spawn(shard.clone().serve(admin, Port::Admin));
    let http = reqwest::Client::new();
    let get = |url: String, token: Option<&'static str>| {
        let mut request = http.get(url);
        if let Some(token) = token {
            request = request.bearer_auth(token);
        }
        async move {
            let answer = request.send().await.expect("answers");
            (answer.status().as_u16(), answer.text().await.expect("body"))
        }
    };

    for token in [None, Some("a-strangers-token"), Some("the-portals-token")] {
        for path in ["/jobs", "/metrics", "/jobs/", "/apps/one/../../jobs"] {
            let (status, body) = get(format!("{apps_url}{path}"), token).await;
            assert_eq!(status, 404, "apps port {path} {token:?}");
            assert!(!body.contains("\"jobs\""), "apps port {path}: {body}");
        }
    }
    assert_eq!(
        get(format!("{apps_url}/apps/one/api/hello"), None).await.0,
        200
    );
    assert_eq!(get(format!("{apps_url}/healthz"), None).await.0, 200);

    let (status, body) = get(format!("{admin_url}/jobs"), None).await;
    assert_eq!(status, 200);
    let records: serde_json::Value = serde_json::from_str(&body).expect("json");
    assert!(records["jobs"].is_array(), "{records}");
    let (status, body) = get(format!("{admin_url}/metrics"), None).await;
    assert_eq!(status, 200);
    assert!(
        body.contains("one"),
        "the counters name the App served: {body}"
    );
    assert_eq!(get(format!("{admin_url}/healthz"), None).await.0, 200);
    assert_eq!(
        get(format!("{admin_url}/apps/one/api/hello"), None).await.0,
        404
    );
}

/// T-4777: hostile Apps, each stopped or refused within a second of its own, the host answering
/// the next request: unbounded recursion overflows the guest stack, a spin under a small fuel budget
/// runs out of fuel even before its wall time, and a host file by path is no file at all.
#[tokio::test(flavor = "multi_thread")]
async fn a_hostile_app_is_trapped_or_refused_within_a_second_and_the_host_stays_up() {
    let world = World::new(
        "hostile",
        Limits {
            wall_time: Duration::from_secs(1),
            fuel: 50_000_000,
            ..Limits::default()
        },
        None,
    );
    let app = world.place("hostile", guest());
    assert_eq!(world.get(&app, "/api/hello", None).await.unwrap().0, 200);
    for (path, expect) in [("/api/recurse", "wasm trap"), ("/api/loop", "out of fuel")] {
        let started = Instant::now();
        let stopped = world.get(&app, path, None).await;
        let took = started.elapsed();
        match stopped {
            Err(Failure::Failed(why)) => assert!(why.contains(expect), "{path}: {why}"),
            other => panic!("{path} answered {other:?}, not a trap"),
        }
        assert!(took < Duration::from_secs(1), "{path} took {took:?}");
        assert_eq!(world.get(&app, "/api/hello", None).await.unwrap().0, 200);
    }
    let (status, answer) = world.get(&app, "/api/passwd", None).await.unwrap();
    assert_eq!(status, 200);
    assert!(answer.starts_with("refused"), "{answer}");
}

/// T-4777: an App that answers and then runs on still holds its place among the requests its App
/// may run at once, until its instance ends, so answering early is no way past the cap.
#[tokio::test(flavor = "multi_thread")]
async fn an_app_that_answers_and_runs_on_holds_its_place_until_its_instance_ends() {
    let world = World::new(
        "answer-then-spin",
        Limits {
            wall_time: Duration::from_secs(1),
            per_app_concurrency: 1,
            ..Limits::default()
        },
        None,
    );
    let app = world.place("lingers", guest());
    // No warm-up call first: its instance, too, keeps the one place until it ends, a moment after
    // its answer, so the next call raced it and read Busy on a loaded runner (ci-full 39365195786).
    assert_eq!(
        world
            .get(&app, "/api/answer-then-spin", None)
            .await
            .unwrap(),
        (200, "answered".into())
    );
    assert_eq!(
        world.get(&app, "/api/hello", None).await.unwrap_err(),
        Failure::Busy,
        "the first instance still runs"
    );
    tokio::time::sleep(Duration::from_millis(2500)).await;
    assert_eq!(world.get(&app, "/api/hello", None).await.unwrap().0, 200);
}

/// T-4777: the body of an App's call to the gateway is held to the request cap, so a component
/// streaming without end cannot fill the shard's memory; a body within the cap goes through.
#[tokio::test(flavor = "multi_thread")]
async fn an_outgoing_body_past_the_request_cap_is_refused_and_never_sent() {
    let gateway = MockServer::start().await;
    Mock::given(any())
        .respond_with(ResponseTemplate::new(200))
        .mount(&gateway)
        .await;
    let world = World::new(
        "upload",
        Limits {
            request_bytes: 2 << 20,
            ..Limits::default()
        },
        Some(&gateway.uri()),
    );
    let app = world.place("uploader", guest());
    let (_, answer) = world.get(&app, "/api/upload?1", Some("tok")).await.unwrap();
    assert_eq!(answer, "status 200");
    let (_, answer) = world
        .get(&app, "/api/upload?64", Some("tok"))
        .await
        .unwrap();
    assert!(answer.starts_with("refused"), "{answer}");
    let seen = gateway.received_requests().await.unwrap_or_default();
    assert_eq!(
        seen.len(),
        1,
        "only the body within the cap reached the gateway"
    );
    assert_eq!(seen[0].body.len(), 1 << 20);
}

/// T-6195: a presigned URL names the App's own host, and the shard serves it there: a GET link
/// streams the object as an attachment, a sized PUT link stores exactly its length, and a link
/// altered, for another method or with another body length is refused before the store is asked.
#[tokio::test(flavor = "multi_thread")]
async fn a_file_link_opens_on_the_apps_own_host_for_its_method_and_length_alone() {
    use std::collections::HashMap;
    use std::sync::RwLock;
    use wasm_host::blob::S3Blob;
    use wasm_host::files::FileUrls;
    use wasm_host::jobs::{HostRunner, Scheduler, WALL_TIME};
    use wasm_host::metrics::Metrics;
    use wasm_host::s3::Bucket;
    use wasm_host::server::{Port, Shard};
    use wasm_host::storage::Method as BlobMethod;
    use wiremock::matchers::query_param;

    let store = MockServer::start().await;
    let world = World::new("file-links", Limits::default(), None);
    let app = world.place("one", guest());
    let object = format!("/apps/apps/0/{}/tiles/a%20b.geojson", app.id);
    Mock::given(method("GET"))
        .and(path(object.as_str()))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_raw("{\"type\":\"FeatureCollection\"}", "application/geo+json"),
        )
        .mount(&store)
        .await;
    Mock::given(method("GET"))
        .and(path("/apps"))
        .and(query_param("list-type", "2"))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            "<ListBucketResult><IsTruncated>false</IsTruncated></ListBucketResult>",
        ))
        .mount(&store)
        .await;
    let upload = format!("/apps/apps/0/{}/up.png", app.id);
    Mock::given(method("PUT"))
        .and(path(upload.as_str()))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&store)
        .await;
    let bucket = Bucket {
        endpoint: store.uri(),
        bucket: "apps".into(),
        region: "us-east-1".into(),
        key_id: "key0".into(),
        secret: "secret-0".into(),
        public_endpoint: None,
        http: reqwest::Client::new(),
    };
    let files = Arc::new(
        S3Blob::new(bucket, "0", 1 << 20)
            .with_file_urls(FileUrls::new("apps.dev.example", "secret-0").expect("urls")),
    );
    let shard = Arc::new(Shard {
        id: "0".into(),
        host: world.host.clone(),
        apps: RwLock::new(HashMap::from([("one".to_owned(), app.clone())])),
        metrics: Metrics::new(20),
        jobs: Arc::new(Scheduler::new(
            HostRunner {
                host: world.host.clone(),
                tokens: None,
            },
            WALL_TIME,
        )),
        files: Some(files.clone()),
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let apps_url = format!("http://{}", listener.local_addr().expect("addr"));
    tokio::spawn(shard.clone().serve(listener, Port::Apps));
    // What the edge does: the App's host, `/api/*` rewritten to `/apps/{name}/api/*`.
    let routed = |url: &str| {
        assert!(
            url.starts_with("https://one.apps.dev.example/api/_jc/files/"),
            "{url}"
        );
        url.replace(
            "https://one.apps.dev.example/api/",
            &format!("{apps_url}/apps/one/api/"),
        )
    };
    let http = reqwest::Client::new();

    let get = routed(
        &files
            .presign(&app, "tiles/a b.geojson", BlobMethod::Get, 60)
            .await
            .expect("presign"),
    );
    let answer = http.get(&get).send().await.expect("get");
    assert_eq!(answer.status(), 200);
    let headers = answer.headers().clone();
    assert_eq!(headers["content-type"], "application/geo+json");
    assert_eq!(headers["content-disposition"], "attachment");
    assert_eq!(headers["x-content-type-options"], "nosniff");
    assert_eq!(
        answer.text().await.expect("body"),
        "{\"type\":\"FeatureCollection\"}"
    );

    let status = |answer: reqwest::Response| answer.status().as_u16();
    let tampered = get.replace("signature=", "signature=00");
    assert_eq!(
        status(http.get(&tampered).send().await.expect("tampered")),
        403
    );
    let elsewhere = get.replace("tiles/a%20b.geojson", "tiles/other.geojson");
    assert_eq!(status(http.get(&elsewhere).send().await.expect("key")), 403);
    assert_eq!(
        status(http.put(&get).body("x").send().await.expect("put")),
        403
    );
    assert_eq!(status(http.post(&get).send().await.expect("post")), 405);

    let put = routed(
        &files
            .presign_put(&app, "up.png", 3, 60)
            .await
            .expect("presign put"),
    );
    assert_eq!(
        status(http.put(&put).body("abcd").send().await.expect("long")),
        403
    );
    assert_eq!(
        status(http.get(&put).send().await.expect("get on put")),
        403
    );
    assert_eq!(
        status(
            http.put(&put)
                .header("content-type", "image/png")
                .body("abc")
                .send()
                .await
                .expect("put")
        ),
        200
    );
    assert!(
        files.presign_put(&app, "big", 26 << 20, 60).await.is_err(),
        "a link moves at most 25 MiB"
    );
}
