//! The SDK's server example, `examples/apps/rust-wasm-server` (T-3341), end to end on the host:
//! its component built for wasm32-wasip2, its migration run as the reconciler runs it, its notes
//! in its own schema and its file uploaded and downloaded by the browser through presigned URLs.
//! The same component placed as a second App sees none of the first one's notes. Its roles and
//! edits (T-3825, API/07) are run with three callers: anonymous, a viewer and a steward, whose
//! roles come from a stand-in Portal and whose Entity edits reach a stand-in gateway. Needs the
//! variables of tests/storage_tests.rs and the wasm32-wasip2 target.

mod common;

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use bytes::Bytes;
use common::{app, Db, Store};
use http_body_util::BodyExt;
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
use wiremock::matchers::{header, method, path_regex};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn example() -> Vec<u8> {
    let root =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/apps/rust-wasm-server/server");
    let target = std::env::var("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target"))
        .join("wasm-host-example");
    let status = Command::new(std::env::var("CARGO").unwrap_or_else(|_| "cargo".into()))
        .args([
            "build",
            "--release",
            "--target",
            "wasm32-wasip2",
            "--manifest-path",
        ])
        .arg(root.join("Cargo.toml"))
        .env("CARGO_TARGET_DIR", &target)
        .env_remove("RUSTFLAGS")
        .status()
        .expect("cargo runs");
    assert!(status.success(), "the example builds for wasm32-wasip2");
    std::fs::read(target.join("wasm32-wasip2/release/rust_wasm_server.wasm")).expect("the example")
}

/// The example placed on shard `s1` as App `id`: its role and schema, then its migration as the
/// owner, as the reconciler does it.
async fn place(db: &Db, id: &str) {
    let mut conn = PgConnection::connect(&db.url).await.expect("db");
    let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../examples/apps/rust-wasm-server/migrations");
    let mut migrations: Vec<PathBuf> = std::fs::read_dir(&dir)
        .expect("migrations")
        .map(|entry| entry.expect("entry").path())
        .collect();
    migrations.sort();
    let mut statements = provision::app(&format!("s1_{}", db.suffix), id, "postgres").expect("app");
    statements.push(format!("set role {}", provision::owner_of(id)));
    statements.push(format!("set search_path = app_{id}"));
    for migration in migrations {
        statements.push(std::fs::read_to_string(migration).expect("migration"));
    }
    statements.push("reset role".into());
    statements.push("reset search_path".into());
    for statement in statements {
        sqlx::raw_sql(AssertSqlSafe(statement.clone()))
            .execute(&mut conn)
            .await
            .unwrap_or_else(|err| panic!("{statement}: {err}"));
    }
}

struct Example {
    host: Arc<Host>,
}

impl Example {
    /// As the viewer `tok-viewer`.
    async fn call(
        &self,
        app: &Placed,
        method: &str,
        path: &str,
        body: &str,
    ) -> (u16, serde_json::Value) {
        self.call_as(app, Some(VIEWER), method, path, body, &[])
            .await
    }

    async fn call_as(
        &self,
        app: &Placed,
        token: Option<&str>,
        method: &str,
        path: &str,
        body: &str,
        headers: &[(&str, &str)],
    ) -> (u16, serde_json::Value) {
        let mut request = http::Request::builder()
            .method(method)
            .uri(format!("http://apps.test/apps/{}{path}", app.name))
            .header("content-type", "application/json");
        for (name, value) in headers {
            request = request.header(*name, *value);
        }
        let request = request.body(Bytes::from(body.to_owned())).expect("request");
        let response = self
            .host
            .serve(app, request, token.map(str::to_owned))
            .await
            .unwrap_or_else(|failure| panic!("{method} {path}: {failure:?}"));
        let status = response.status().as_u16();
        let bytes = response
            .into_body()
            .collect()
            .await
            .map(|b| b.to_bytes())
            .unwrap_or_default();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null),
        )
    }
}

const VIEWER: &str = "tok-viewer";
const OTHER: &str = "tok-other";
const STEWARD: &str = "tok-steward";

/// A Portal that tells the host who holds each token and which roles of the App they have, and a
/// gateway whose Endpoint `ep1` lets the steward change an Entity and refuses the viewer.
async fn stand_ins() -> (MockServer, MockServer) {
    let portal = MockServer::start().await;
    for (token, id, roles) in [
        (VIEWER, "u-viewer", vec!["viewer"]),
        (OTHER, "u-other", vec!["viewer"]),
        (STEWARD, "u-steward", vec!["viewer", "steward"]),
    ] {
        Mock::given(method("GET"))
            .and(path_regex(
                r"^/api/v1/projects/helsinki/apps/[a-z0-9-]+/me$",
            ))
            .and(header("authorization", format!("Bearer {token}").as_str()))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": id, "name": format!("Person {id}"), "roles": roles
            })))
            .mount(&portal)
            .await;
    }
    let gateway = MockServer::start().await;
    let attrs = r"^/api/endpoint/ep1/ngsi-ld/v1/entities/[^/]+/attrs$";
    Mock::given(method("PATCH"))
        .and(path_regex(attrs))
        .and(header(
            "authorization",
            format!("Bearer {STEWARD}").as_str(),
        ))
        .respond_with(ResponseTemplate::new(204))
        .mount(&gateway)
        .await;
    Mock::given(method("PATCH"))
        .and(path_regex(attrs))
        .respond_with(ResponseTemplate::new(403))
        .mount(&gateway)
        .await;
    (portal, gateway)
}

/// The example built and placed twice on one host, as Apps `ex1` and `ex2` with Endpoint `ep1`.
async fn placed(
    db: &Db,
    store: &Store,
    portal: &MockServer,
    gateway: &MockServer,
) -> (Example, Placed, Placed) {
    let bytes = example();
    let digest = format!("sha256:{}", hex::encode(Sha256::digest(&bytes)));
    let dir = std::env::temp_dir().join(format!("wasm-host-example-{}", db.suffix));
    std::fs::create_dir_all(&dir).expect("dir");
    std::fs::write(dir.join(format!("sha256-{}.wasm", &digest[7..])), &bytes).expect("component");

    let (one, two) = (db.id("ex1"), db.id("ex2"));
    place(db, &one).await;
    place(db, &two).await;
    let stores = Stores {
        sql: Some(PgStore::with_pool(
            db.pool("s1", 4).await,
            SqlLimits::default(),
        )),
        // The bucket's shard prefix is the database's shard, as one shard's pods hold both.
        blob: Some(S3Blob::new(store.bucket("s1"), &store.shard("s1"), 1 << 20)),
    };
    let host = Host::new(
        Limits::default(),
        Source::Dir(dir),
        Arc::new(stores),
        Some(&gateway.uri()),
        Some(&portal.uri()),
    )
    .expect("host");
    let example = Example { host };
    let first = Placed {
        digest: digest.clone(),
        endpoint: Some("ep1".into()),
        ..app(&one)
    };
    let second = Placed {
        digest,
        endpoint: Some("ep1".into()),
        ..app(&two)
    };
    (example, first, second)
}

#[tokio::test(flavor = "multi_thread")]
async fn the_example_keeps_notes_and_files_of_its_own() {
    let db = Db::new().await;
    let store = Store::new().await;
    let (portal, gateway) = stand_ins().await;
    let (example, first, second) = placed(&db, &store, &portal, &gateway).await;
    let (status, note) = example
        .call(&first, "POST", "/api/notes", r#"{"body": "  buy milk "}"#)
        .await;
    assert_eq!(status, 201, "{note}");
    assert_eq!(note["body"], "buy milk");
    let id = note["id"].as_i64().expect("an id");
    assert_eq!(
        example
            .call(&first, "POST", "/api/notes", r#"{"body": ""}"#)
            .await
            .0,
        400
    );
    assert_eq!(
        example
            .call(&first, "POST", "/api/notes", r#"{"text": "x"}"#)
            .await
            .0,
        400,
        "an unknown field is refused"
    );

    let (status, upload) = example
        .call(
            &first,
            "POST",
            &format!("/api/notes/{id}/file"),
            r#"{"name": "list.txt"}"#,
        )
        .await;
    assert_eq!(status, 200, "{upload}");
    let http = reqwest::Client::new();
    let put = http
        .put(upload["url"].as_str().expect("url"))
        .body("milk, eggs")
        .send()
        .await
        .expect("upload");
    assert_eq!(put.status(), 200);
    let (_, download) = example
        .call(&first, "GET", &format!("/api/notes/{id}/file"), "")
        .await;
    let got = http
        .get(download["url"].as_str().expect("url"))
        .send()
        .await
        .expect("download");
    assert_eq!(got.text().await.expect("text"), "milk, eggs");

    assert_eq!(
        example
            .call(
                &first,
                "PUT",
                &format!("/api/notes/{id}"),
                r#"{"body": "buy oat milk"}"#
            )
            .await
            .0,
        204
    );
    let (_, notes) = example.call(&first, "GET", "/api/notes", "").await;
    assert_eq!(notes[0]["body"], "buy oat milk");
    assert_eq!(notes[0]["file"], format!("notes/{id}/list.txt"));

    // The same component as another App: its own empty table, and no way to the first one's note.
    let (_, theirs) = example.call(&second, "GET", "/api/notes", "").await;
    assert_eq!(theirs, serde_json::json!([]));
    assert_eq!(
        example
            .call(&second, "GET", &format!("/api/notes/{id}/file"), "")
            .await
            .0,
        404
    );
    assert_eq!(
        example
            .call(&second, "DELETE", &format!("/api/notes/{id}"), "")
            .await
            .0,
        404,
        "the note is not its own"
    );
    assert_eq!(
        example
            .call(&first, "GET", "/api/notes", "")
            .await
            .1
            .as_array()
            .map(Vec::len),
        Some(1)
    );

    assert_eq!(
        example
            .call(&first, "DELETE", &format!("/api/notes/{id}"), "")
            .await
            .0,
        204
    );
    assert_eq!(
        example.call(&first, "GET", "/api/notes", "").await.1,
        serde_json::json!([])
    );
    assert_eq!(
        example
            .call(&first, "GET", &format!("/api/notes/{id}/file"), "")
            .await
            .0,
        404
    );
    let blob = store.blob("s1", 1 << 20);
    assert!(
        blob.list(&first, "").await.expect("list").is_empty(),
        "the file went with its note"
    );
}

/// T-3825, API/07: who may change what is decided from the caller the Portal names, never from
/// the request; a note is its writer's, the notice a steward's, and every change leaves history.
#[tokio::test(flavor = "multi_thread")]
async fn roles_decide_who_changes_what_and_every_change_is_history() {
    let db = Db::new().await;
    let store = Store::new().await;
    let (portal, gateway) = stand_ins().await;
    let (example, app, _) = placed(&db, &store, &portal, &gateway).await;
    let call = |token, method, path: &str, body| {
        let path = path.to_owned();
        let (example, app) = (&example, &app);
        async move { example.call_as(app, token, method, &path, body, &[]).await }
    };

    // Anonymous: the notice, and nothing else.
    let (status, notice) = call(None, "GET", "/api/notice", "").await;
    assert_eq!(status, 200, "{notice}");
    for (method, path, body) in [
        ("GET", "/api/notes", ""),
        ("POST", "/api/notes", r#"{"body": "x"}"#),
        ("PUT", "/api/notice", r#"{"body": "x"}"#),
        ("GET", "/api/history", ""),
        (
            "PATCH",
            "/api/entities/urn:ngsi-ld:Station:1",
            r#"{"bikes": {"value": 3}}"#,
        ),
    ] {
        assert_eq!(
            call(None, method, path, body).await.0,
            401,
            "{method} {path}"
        );
    }

    // A viewer's note is theirs: another viewer neither sees nor changes it, a steward does both.
    let (status, note) = call(Some(VIEWER), "POST", "/api/notes", r#"{"body": "mine"}"#).await;
    assert_eq!(status, 201, "{note}");
    assert_eq!(note["owner_id"], "u-viewer");
    let id = note["id"].as_i64().expect("id");
    let at = format!("/api/notes/{id}");
    assert_eq!(
        call(Some(OTHER), "GET", "/api/notes", "").await.1,
        serde_json::json!([])
    );
    let (status, refused) = call(Some(OTHER), "PUT", &at, r#"{"body": "theirs"}"#).await;
    assert_eq!(status, 404, "{refused}");
    assert_eq!(refused["detail"], "no such record, or not yours to change");
    assert_eq!(call(Some(OTHER), "DELETE", &at, "").await.0, 404);
    assert_eq!(
        call(Some(STEWARD), "GET", "/api/notes", "").await.1[0]["id"],
        id
    );
    assert_eq!(
        call(Some(STEWARD), "PUT", &at, r#"{"body": "tidied"}"#)
            .await
            .0,
        204
    );
    assert_eq!(
        call(Some(VIEWER), "GET", "/api/notes", "").await.1[0]["body"],
        "tidied"
    );

    // The notice is a steward's to change; a role in the body or a header changes nothing, and the
    // refusal names the role needed, never the caller's own.
    let (status, refused) = example
        .call_as(
            &app,
            Some(VIEWER),
            "PUT",
            "/api/notice",
            r#"{"body": "hijacked", "roles": ["steward"]}"#,
            &[("x-roles", "steward"), ("x-jc-roles", "steward")],
        )
        .await;
    assert_eq!(status, 403, "{refused}");
    assert_eq!(refused["detail"], "this needs the role steward");
    assert!(!refused.to_string().contains("viewer"), "{refused}");
    assert_eq!(call(Some(VIEWER), "GET", "/api/history", "").await.0, 403);
    assert_eq!(
        call(None, "GET", "/api/notice", "").await.1,
        notice,
        "unchanged"
    );
    assert_eq!(
        call(
            Some(STEWARD),
            "PUT",
            "/api/notice",
            r#"{"body": "Docks 3 and 4 closed"}"#
        )
        .await
        .0,
        204
    );
    assert_eq!(
        call(None, "GET", "/api/notice", "").await.1["body"],
        "Docks 3 and 4 closed"
    );

    // Context data: the Endpoint's Policy decides, even where the App checks no role.
    let entity = "/api/entities/urn:ngsi-ld:Station:1";
    let bikes = r#"{"bikes": {"type": "Property", "value": 3}}"#;
    let (status, refused) = call(Some(VIEWER), "PATCH", entity, bikes).await;
    assert_eq!(status, 403, "{refused}");
    assert_eq!(call(Some(STEWARD), "PATCH", entity, bikes).await.0, 204);
    let edits = gateway.received_requests().await.unwrap_or_default();
    assert_eq!(edits.len(), 2);
    assert!(
        edits[1]
            .url
            .path()
            .ends_with("/entities/urn%3Angsi-ld%3AStation%3A1/attrs"),
        "{}",
        edits[1].url
    );
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&edits[1].body).expect("json"),
        serde_json::from_str::<serde_json::Value>(bikes).expect("json")
    );

    // A failed write leaves no history; the owner's delete does.
    assert_eq!(
        call(Some(VIEWER), "POST", "/api/notes", r#"{"body": ""}"#)
            .await
            .0,
        400
    );
    assert_eq!(call(Some(VIEWER), "DELETE", &at, "").await.0, 204);
    let (status, history) = call(Some(STEWARD), "GET", "/api/history", "").await;
    assert_eq!(status, 200, "{history}");
    let rows: Vec<(String, String, String)> = history
        .as_array()
        .expect("rows")
        .iter()
        .map(|row| {
            let text = |key: &str| row[key].as_str().unwrap_or_default().to_owned();
            (text("actor_id"), text("action"), text("target"))
        })
        .rev()
        .collect();
    let note = format!("notes/{id}");
    assert_eq!(
        rows,
        [
            ("u-viewer".into(), "insert".into(), note.clone()),
            ("u-steward".into(), "update".into(), note.clone()),
            ("u-steward".into(), "update".into(), "notice/1".into()),
            (
                "u-steward".into(),
                "edit".into(),
                "urn:ngsi-ld:Station:1".into()
            ),
            ("u-viewer".into(), "delete".into(), note),
        ]
    );
}
