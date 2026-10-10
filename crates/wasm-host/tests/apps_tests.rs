//! Every server WASM App of a Portal checkout on the host (T-3351): each `apps/<name>/` with a
//! `server/host-test.json` is built for wasm32-wasip2, its migrations run twice as the
//! reconciler runs them (the second run must change nothing), and its scenario is played against
//! a real Postgres and RustFS, with the gateway a mock that answers the App's own Endpoint alone.
//! The same component placed as a second App then sees none of the first App's rows.
//!
//! ```text
//! JC_WASM_TEST_APPS=/path/to/joinedcontext-portal/apps   # plus the variables of tests/storage_tests.rs
//! ```
//!
//! A scenario, `server/host-test.json`:
//!
//! ```json
//! {
//!   "gateway": [{"path": "/ngsi-ld/v1/entities", "query": {"type": "Alert"}, "file": "host-test/alerts.json"}],
//!   "steps": [
//!     {"call": "POST /api/reports", "body": {"title": "t"}, "status": 201, "save": {"report": "/id"}},
//!     {"call": "GET /api/reports/{report}", "status": 200, "expect": {"/title": "t"}, "length": {"/places": 0}},
//!     {"call": "POST /api/reports/{report}/snapshot", "status": 200, "upload": "/url"},
//!     {"call": "GET /api/reports/{report}/snapshot", "status": 200, "download": "/url"},
//!     {"call": "GET /api/reports", "as": "second", "status": 200, "length": {"": 0}}
//!   ]
//! }
//! ```
//!
//! `gateway[].path` is below the Endpoint; a `file` is relative to `server/`. A step's `{name}`
//! is what an earlier step's `save` took; `upload` puts bytes to the URL at that pointer and
//! `download` reads them back; `as: "second"` calls as the second App.

mod common;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use bytes::Bytes;
use common::{app, Db, Store};
use http_body_util::BodyExt;
use serde::Deserialize;
use serde_json::Value;
use sha2::{Digest, Sha256};
use sqlx::{AssertSqlSafe, Connection, PgConnection};
use wasm_host::blob::S3Blob;
use wasm_host::host::Host;
use wasm_host::limits::Limits;
use wasm_host::placement::Placed;
use wasm_host::provision;
use wasm_host::source::Source;
use wasm_host::sql::{PgStore, SqlLimits};
use wasm_host::storage::Stores;
use wiremock::matchers::{header, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

const SLUG: &str = "ep1";
const TOKEN: &str = "tok-apps-test";
const BYTES: &str = "jc-host-test";

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Scenario {
    #[serde(default)]
    gateway: Vec<Answer>,
    steps: Vec<Step>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Answer {
    path: String,
    #[serde(default)]
    query: BTreeMap<String, String>,
    file: String,
    #[serde(default = "ok")]
    status: u16,
}

fn ok() -> u16 {
    200
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Step {
    call: String,
    #[serde(default)]
    body: Option<Value>,
    status: u16,
    #[serde(default)]
    expect: BTreeMap<String, Value>,
    #[serde(default)]
    length: BTreeMap<String, usize>,
    #[serde(default)]
    save: BTreeMap<String, String>,
    #[serde(default)]
    upload: Option<String>,
    #[serde(default)]
    download: Option<String>,
    #[serde(default, rename = "as")]
    caller: Option<String>,
}

fn build(server: &Path) -> Vec<u8> {
    let target = std::env::var("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target"))
        .join("wasm-host-apps");
    let status = Command::new(std::env::var("CARGO").unwrap_or_else(|_| "cargo".into()))
        .args([
            "build",
            "--release",
            "--locked",
            "--target",
            "wasm32-wasip2",
        ])
        .arg("--manifest-path")
        .arg(server.join("Cargo.toml"))
        .env("CARGO_TARGET_DIR", &target)
        .env_remove("RUSTFLAGS")
        .status()
        .expect("cargo runs");
    assert!(
        status.success(),
        "{} builds for wasm32-wasip2",
        server.display()
    );
    let manifest = std::fs::read_to_string(server.join("Cargo.toml")).expect("server/Cargo.toml");
    let name = package_name(&manifest).expect("server/Cargo.toml names its package");
    std::fs::read(target.join(format!(
        "wasm32-wasip2/release/{}.wasm",
        name.replace('-', "_")
    )))
    .expect("the component")
}

/// The `name` of a Cargo.toml's `[package]`, without a TOML parser.
fn package_name(text: &str) -> Option<String> {
    text.split("[package]")
        .nth(1)?
        .lines()
        .take_while(|line| !line.starts_with('['))
        .find(|line| line.trim_start().starts_with("name"))?
        .split('"')
        .nth(1)
        .map(str::to_owned)
}

/// The App placed on shard `s1` as `id`, then its migrations as its owner, in the order of their
/// names, twice: a second publish must change nothing.
async fn place(db: &Db, id: &str, migrations: &Path) {
    let mut conn = PgConnection::connect(&db.url).await.expect("db");
    let mut files: Vec<PathBuf> = std::fs::read_dir(migrations)
        .map(|dir| {
            dir.filter_map(|e| e.ok().map(|e| e.path()))
                .filter(|p| p.extension().is_some_and(|x| x == "sql"))
                .collect()
        })
        .unwrap_or_default();
    files.sort();
    let mut statements = provision::app(&format!("s1_{}", db.suffix), id, "postgres").expect("app");
    for _ in 0..2 {
        statements.push(format!("set role {}", provision::owner_of(id)));
        statements.push(format!("set search_path = app_{id}"));
        for file in &files {
            statements.push(std::fs::read_to_string(file).expect("migration"));
        }
        statements.push("reset role".into());
        statements.push("reset search_path".into());
    }
    for statement in statements {
        sqlx::raw_sql(AssertSqlSafe(statement.clone()))
            .execute(&mut conn)
            .await
            .unwrap_or_else(|err| panic!("{id}: {statement}: {err}"));
    }
}

async fn call(
    host: &Host,
    app: &Placed,
    verb: &str,
    target: &str,
    body: Option<&Value>,
) -> (u16, Value) {
    let request = http::Request::builder()
        .method(verb)
        .uri(format!("http://apps.test/apps/{}{target}", app.name))
        .header("content-type", "application/json")
        .body(Bytes::from(body.map(Value::to_string).unwrap_or_default()))
        .expect("request");
    let response = host
        .serve(app, request, Some(TOKEN.to_owned()))
        .await
        .unwrap_or_else(|failure| panic!("{} {verb} {target}: {failure:?}", app.name));
    let status = response.status().as_u16();
    let bytes = response
        .into_body()
        .collect()
        .await
        .map(|b| b.to_bytes())
        .unwrap_or_default();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

async fn play(apps: &Path, name: &str, db: &Db, store: &Store) {
    let server = apps.join(name).join("server");
    let scenario: Scenario = serde_json::from_str(
        &std::fs::read_to_string(server.join("host-test.json")).expect("host-test.json"),
    )
    .unwrap_or_else(|err| panic!("{name}/server/host-test.json: {err}"));

    let gateway = MockServer::start().await;
    for answer in &scenario.gateway {
        let body = std::fs::read(server.join(&answer.file))
            .unwrap_or_else(|err| panic!("{name}: {}: {err}", answer.file));
        let mut mock = Mock::given(method("GET"))
            .and(path(format!("/api/endpoint/{SLUG}{}", answer.path)))
            .and(header("authorization", format!("Bearer {TOKEN}").as_str()));
        for (key, value) in &answer.query {
            mock = mock.and(query_param(key.as_str(), value.as_str()));
        }
        mock.respond_with(
            ResponseTemplate::new(answer.status).set_body_raw(body, "application/json"),
        )
        .mount(&gateway)
        .await;
    }

    let bytes = build(&server);
    let digest = format!("sha256:{}", hex::encode(Sha256::digest(&bytes)));
    let dir = std::env::temp_dir().join(format!("wasm-host-apps-{}-{name}", db.suffix));
    std::fs::create_dir_all(&dir).expect("dir");
    std::fs::write(dir.join(format!("sha256-{}.wasm", &digest[7..])), &bytes).expect("component");

    let short: String = name
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .take(12)
        .collect();
    let (one, two) = (db.id(&format!("{short}1")), db.id(&format!("{short}2")));
    let migrations = apps.join(name).join("migrations");
    place(db, &one, &migrations).await;
    place(db, &two, &migrations).await;
    let stores = Stores {
        sql: Some(PgStore::with_pool(
            db.pool("s1", 4).await,
            SqlLimits::default(),
        )),
        blob: Some(S3Blob::new(store.bucket("s1"), &store.shard("s1"), 1 << 20)),
    };
    let host = Host::new(
        Limits::default(),
        Source::Dir(dir),
        Arc::new(stores),
        Some(&gateway.uri()),
    )
    .expect("host");
    let placed = |id: &str| Placed {
        name: name.to_owned(),
        digest: digest.clone(),
        endpoint: Some(SLUG.to_owned()),
        ..app(id)
    };
    let (first, second) = (placed(&one), placed(&two));

    let http = reqwest::Client::new();
    let mut saved: BTreeMap<String, String> = BTreeMap::new();
    let mut second_uploaded = false;
    for (n, step) in scenario.steps.iter().enumerate() {
        let (verb, mut target) = step
            .call
            .split_once(' ')
            .map(|(v, t)| (v.to_owned(), t.to_owned()))
            .unwrap_or_else(|| panic!("{name} step {n}: call is `VERB /path`"));
        for (key, value) in &saved {
            target = target.replace(&format!("{{{key}}}"), value);
        }
        let caller = match step.caller.as_deref() {
            None => &first,
            Some("second") => &second,
            Some(other) => panic!("{name} step {n}: unknown caller {other}"),
        };
        let (status, answer) = call(&host, caller, &verb, &target, step.body.as_ref()).await;
        let at = format!("{name} step {n} ({verb} {target})");
        assert_eq!(status, step.status, "{at}: {answer}");
        for (pointer, want) in &step.expect {
            assert_eq!(
                answer.pointer(pointer),
                Some(want),
                "{at} {pointer}: {answer}"
            );
        }
        for (pointer, want) in &step.length {
            let got = answer
                .pointer(pointer)
                .and_then(Value::as_array)
                .map(Vec::len);
            assert_eq!(got, Some(*want), "{at} length of {pointer}: {answer}");
        }
        for (key, pointer) in &step.save {
            let value = match answer.pointer(pointer) {
                Some(Value::String(text)) => text.clone(),
                Some(Value::Number(number)) => number.to_string(),
                other => panic!("{at}: nothing to save at {pointer}: {other:?}"),
            };
            saved.insert(key.clone(), value);
        }
        if let Some(pointer) = &step.upload {
            second_uploaded |= step.caller.is_some();
            let url = answer
                .pointer(pointer)
                .and_then(Value::as_str)
                .expect("an upload URL");
            let put = http.put(url).body(BYTES).send().await.expect("upload");
            assert_eq!(put.status(), 200, "{at}: the upload");
        }
        if let Some(pointer) = &step.download {
            let url = answer
                .pointer(pointer)
                .and_then(Value::as_str)
                .expect("a download URL");
            let got = http.get(url).send().await.expect("download");
            assert_eq!(got.text().await.expect("text"), BYTES, "{at}: the download");
        }
    }
    if !second_uploaded {
        let theirs = store
            .blob("s1", 1 << 20)
            .list(&second, "")
            .await
            .expect("list");
        assert!(
            theirs.is_empty(),
            "{name}: the second App's prefix holds {theirs:?}"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn every_server_app_keeps_its_own_data_on_the_host() {
    let apps = PathBuf::from(common::var("JC_WASM_TEST_APPS"));
    let mut names: Vec<String> = std::fs::read_dir(&apps)
        .unwrap_or_else(|err| panic!("JC_WASM_TEST_APPS={}: {err}", apps.display()))
        .filter_map(|e| e.ok())
        .filter(|e| e.path().join("server/host-test.json").is_file())
        .filter_map(|e| e.file_name().into_string().ok())
        .collect();
    names.sort();
    assert!(
        !names.is_empty(),
        "no apps/*/server/host-test.json in {}",
        apps.display()
    );
    let db = Db::new().await;
    let store = Store::new().await;
    for name in names {
        play(&apps, &name, &db, &store).await;
    }
}
