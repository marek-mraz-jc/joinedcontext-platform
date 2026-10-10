//! jc-wasm-host against real components (T-3339, AP-143, AP-146, AP-147): the test App of
//! `tests/guest`, built for wasm32-wasip2 here (`rustup target add wasm32-wasip2`), served by a
//! shard whose components sit in a directory. The gateway is a mock that records what reached it.

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
use wiremock::matchers::any;
use wiremock::{Mock, MockServer, ResponseTemplate};

/// The test App, built once per test binary.
fn guest() -> &'static [u8] {
    static BYTES: OnceLock<Vec<u8>> = OnceLock::new();
    BYTES.get_or_init(|| {
        let manifest = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/guest/Cargo.toml");
        let target = std::env::var("CARGO_TARGET_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|_| Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target"))
            .join("wasm-host-guest");
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
        std::fs::read(target.join("wasm32-wasip2/release/wasm_host_test_guest.wasm"))
            .expect("the test App")
    })
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
        let dir = std::env::temp_dir().join(format!("wasm-host-{case}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("dir");
        let host = Host::new(
            limits,
            Source::Dir(dir.clone()),
            Arc::new(Unconfigured),
            gateway,
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
        let request = http::Request::builder()
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
    assert!(
        took >= Duration::from_secs(5) && took < Duration::from_secs(7),
        "stopped after {took:?}"
    );
    assert_eq!(world.get(&app, "/api/hello", None).await.unwrap().0, 200);
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

/// T-3342: the wall time holds while the App waits on the host, not only while it computes.
#[tokio::test(flavor = "multi_thread")]
async fn a_slow_gateway_call_ends_with_the_requests_wall_time() {
    let gateway = MockServer::start().await;
    Mock::given(any())
        .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_secs(8)))
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
    assert!(
        took < Duration::from_secs(3),
        "the 1 s wall time and its grace, not the gateway's 8 s: {took:?}"
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
