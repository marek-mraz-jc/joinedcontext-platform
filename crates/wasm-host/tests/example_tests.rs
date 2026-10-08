//! The SDK's server example, `examples/apps/rust-wasm-server` (T-3341), end to end on the host:
//! its component built for wasm32-wasip2, its migration run as the reconciler runs it, its notes
//! in its own schema and its file uploaded and downloaded by the browser through presigned URLs.
//! The same component placed as a second App sees none of the first one's notes. Needs the
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
    let migration = std::fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../examples/apps/rust-wasm-server/migrations/0001_notes.sql"),
    )
    .expect("migration");
    let mut statements = provision::app(&format!("s1_{}", db.suffix), id, "postgres").expect("app");
    statements.push(format!("set role {}", provision::owner_of(id)));
    statements.push(format!("set search_path = app_{id}"));
    statements.push(migration);
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
    async fn call(
        &self,
        app: &Placed,
        method: &str,
        path: &str,
        body: &str,
    ) -> (u16, serde_json::Value) {
        let request = http::Request::builder()
            .method(method)
            .uri(format!("http://apps.test/apps/{}{path}", app.name))
            .header("content-type", "application/json")
            .body(Bytes::from(body.to_owned()))
            .expect("request");
        let response = self
            .host
            .serve(app, request, None)
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

#[tokio::test(flavor = "multi_thread")]
async fn the_example_keeps_notes_and_files_of_its_own() {
    let db = Db::new().await;
    let store = Store::new().await;
    let bytes = example();
    let digest = format!("sha256:{}", hex::encode(Sha256::digest(&bytes)));
    let dir = std::env::temp_dir().join(format!("wasm-host-example-{}", db.suffix));
    std::fs::create_dir_all(&dir).expect("dir");
    std::fs::write(dir.join(format!("sha256-{}.wasm", &digest[7..])), &bytes).expect("component");

    let (one, two) = (db.id("ex1"), db.id("ex2"));
    place(&db, &one).await;
    place(&db, &two).await;
    let stores = Stores {
        sql: Some(PgStore::with_pool(
            db.pool("s1", 4).await,
            SqlLimits::default(),
        )),
        // The bucket's shard prefix is the database's shard, as one shard's pods hold both.
        blob: Some(S3Blob::new(store.bucket("s1"), &store.shard("s1"), 1 << 20)),
    };
    let host =
        Host::new(Limits::default(), Source::Dir(dir), Arc::new(stores), None).expect("host");
    let example = Example { host };
    let first = Placed {
        digest: digest.clone(),
        ..app(&one)
    };
    let second = Placed {
        digest,
        ..app(&two)
    };

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
