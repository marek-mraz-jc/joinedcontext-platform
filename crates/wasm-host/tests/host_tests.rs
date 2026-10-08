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
        std::fs::write(self.dir.join(format!("{}.wasm", &digest[7..])), bytes).expect("component");
        Placed {
            name: name.into(),
            id: format!("{name}-id"),
            tenant: "helsinki".into(),
            digest,
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
        world.dir.join(format!("{}.wasm", &app.digest[7..])),
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
            &format!("/api/fetch?{}/ngsi-ld/v1/entities", gateway.uri()),
            Some("tok-1"),
        )
        .await
        .unwrap();
    assert_eq!(answer, "status 200");
    let seen = gateway.received_requests().await.unwrap_or_default();
    assert_eq!(seen.len(), 1);
    assert_eq!(
        seen[0]
            .headers
            .get("authorization")
            .map(|v| v.to_str().unwrap_or("")),
        Some("Bearer tok-1")
    );
    assert!(seen[0].headers.get("cookie").is_none());

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
