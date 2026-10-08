//! `jc:app/sql` and `jc:app/blob` against a real Postgres and a real RustFS (T-3340, AP-144,
//! AP-145, AP-146), each isolation layer proven on its own: with the host's checks out of the way,
//! Postgres still refuses one App on another's schema and a shard on another shard's App, and
//! RustFS still refuses one shard's key on another shard's prefix.
//!
//! ```text
//! JC_WASM_TEST_DATABASE_URL=postgres://postgres@127.0.0.1:5433/postgres   # a superuser
//! JC_WASM_TEST_S3=http://127.0.0.1:19000 JC_WASM_TEST_S3_KEY=… JC_WASM_TEST_S3_SECRET=…   # RustFS root
//! ```
//!
//! A missing variable is a failure that says what to set, never a test that passes by skipping.

use std::time::Duration;

mod common;

use common::{app, Db, Store};
use sqlx::{AssertSqlSafe, Connection, PgConnection};
use wasm_host::sql::{Kind, PgStore, SqlLimits};
use wasm_host::storage::{BlobError, Method, SqlError, Value};

fn limits() -> SqlLimits {
    SqlLimits {
        statement_timeout: Duration::from_millis(500),
        row_cap: 5,
        result_bytes: 8 << 20,
        per_app_statements: 4,
        quota_bytes: 100 << 20,
    }
}

#[tokio::test]
async fn an_app_reads_and_writes_its_own_tables_and_no_one_elses() {
    let db = Db::new().await;
    let store = PgStore::with_pool(db.pool("s1", 4).await, limits());
    let a = app(&db.id("a"));

    let (_, added) = store
        .run(
            &a,
            "insert into notes (body) values ($1)".into(),
            vec![Value::Text("second".into())],
            Kind::Write,
        )
        .await
        .expect("insert");
    assert_eq!(added, 1);
    let (rows, _) = store
        .run(
            &a,
            "select body from notes order by id".into(),
            vec![],
            Kind::Read,
        )
        .await
        .expect("select");
    let rows = rows.expect("rows");
    assert_eq!(rows.columns, ["body"]);
    assert_eq!(
        rows.values,
        [
            vec![Value::Text(format!("{} first", db.id("a")))],
            vec![Value::Text("second".into())]
        ]
    );

    // Another App's table, named outright: Postgres says no (the host never had a rule for it).
    let other = format!("select * from app_{}.notes", db.id("b"));
    assert_eq!(
        store.run(&a, other, vec![], Kind::Read).await.unwrap_err(),
        SqlError::Refused("permission denied".into())
    );
    let copy = "insert into notes (body) select body from notes".to_owned();
    assert!(store.run(&a, copy, vec![], Kind::Write).await.is_ok());
    // A write that reads back its rows goes through query; a role switch never reaches Postgres.
    let (gone, _) = store
        .run(
            &a,
            "delete from notes where body = $1 returning id".into(),
            vec![Value::Text("second".into())],
            Kind::Read,
        )
        .await
        .expect("delete returning");
    assert_eq!(gone.expect("rows").values.len(), 2, "the note and its copy");
    assert!(matches!(
        store
            .run(
                &a,
                "select set_config('role', 'postgres', true)".into(),
                vec![],
                Kind::Read
            )
            .await,
        Err(SqlError::Refused(_))
    ));
}

#[tokio::test]
async fn postgres_itself_refuses_another_apps_schema_and_another_shards_app() {
    let db = Db::new().await;
    let mut conn = PgConnection::connect(&db.login("s1"))
        .await
        .expect("shard 1");
    // Exactly what the host does, by hand, and then what the host would never let through.
    let mut tx = conn.begin().await.expect("tx");
    sqlx::query(AssertSqlSafe(format!(
        "select set_config('role', 'app_{}', true)",
        db.id("a")
    )))
    .execute(&mut *tx)
    .await
    .expect("own App");
    let err = sqlx::query(AssertSqlSafe(format!(
        "select * from app_{}.notes",
        db.id("b")
    )))
    .fetch_all(&mut *tx)
    .await
    .unwrap_err();
    assert!(err.to_string().contains("permission denied"), "{err}");
    tx.rollback().await.expect("rollback");

    // Shard 1's login is no member of shard 2's App: no role switch reaches it.
    let mut tx = conn.begin().await.expect("tx");
    let err = sqlx::query(AssertSqlSafe(format!(
        "select set_config('role', 'app_{}', true)",
        db.id("c")
    )))
    .execute(&mut *tx)
    .await
    .unwrap_err();
    assert!(err.to_string().contains("permission denied"), "{err}");
    tx.rollback().await.expect("rollback");

    // The login role alone reads no App's table: it has no right of its own.
    let err = sqlx::query(AssertSqlSafe(format!(
        "select * from app_{}.notes",
        db.id("a")
    )))
    .fetch_all(&mut conn)
    .await
    .unwrap_err();
    assert!(err.to_string().contains("permission denied"), "{err}");
    // And an App's role creates nothing, not even in its own schema.
    let mut tx = conn.begin().await.expect("tx");
    sqlx::query(AssertSqlSafe(format!(
        "select set_config('role', 'app_{}', true)",
        db.id("a")
    )))
    .execute(&mut *tx)
    .await
    .expect("own App");
    let err = sqlx::query(AssertSqlSafe(format!(
        "create table app_{}.x (i int)",
        db.id("a")
    )))
    .execute(&mut *tx)
    .await
    .unwrap_err();
    assert!(err.to_string().contains("permission denied"), "{err}");
}

/// T-3364: the shard's login may take every App role of its shard, so no grant refuses a role
/// switch from App A to App B on the same shard: the host's parser does, in every spelling, and
/// B's table stays unread.
#[tokio::test]
async fn no_spelling_of_a_role_switch_reaches_another_app_on_the_same_shard() {
    let db = Db::new().await;
    let store = PgStore::with_pool(db.pool("s1", 4).await, limits());
    let a = app(&db.id("a"));
    let b = db.id("b");
    for statement in [
        format!("SET ROLE app_{b}"),
        format!("set local role app_{b}"),
        format!("RESET ROLE; SET ROLE app_{b}"),
        format!("select 1; set role app_{b}"),
        format!("SET SESSION AUTHORIZATION app_{b}"),
        format!("select E'\\' ', set_config('role', 'app_{b}', true) --'"),
        format!("select * from pg_catalog.set_config('role', 'app_{b}', true)"),
    ] {
        for kind in [Kind::Read, Kind::Write] {
            assert!(
                matches!(
                    store.run(&a, statement.clone(), vec![], kind).await,
                    Err(SqlError::Refused(_))
                ),
                "{statement}"
            );
        }
        // Whatever was sent, A still reads only its own notes, and B's table stays out of reach.
        let (rows, _) = store
            .run(&a, "select body from notes".into(), vec![], Kind::Read)
            .await
            .expect("own notes");
        assert_eq!(
            rows.expect("rows").values,
            [vec![Value::Text(format!("{} first", db.id("a")))]]
        );
        assert_eq!(
            store
                .run(
                    &a,
                    format!("select * from app_{b}.notes"),
                    vec![],
                    Kind::Read
                )
                .await
                .unwrap_err(),
            SqlError::Refused("permission denied".into())
        );
    }
}

/// T-3342: a migration runs as the App's own owner, so a view (or a function) it defines reads
/// with that App's rights: one App's migration cannot publish another App's table.
#[tokio::test]
async fn a_migration_cannot_reach_another_apps_tables() {
    let db = Db::new().await;
    let mut admin = PgConnection::connect(&db.url).await.expect("db");
    let (a, b) = (db.id("a"), db.id("b"));
    sqlx::raw_sql(AssertSqlSafe(format!(
        "set role {}",
        wasm_host::provision::owner_of(&a)
    )))
    .execute(&mut admin)
    .await
    .expect("as a's owner");
    let err = sqlx::raw_sql(AssertSqlSafe(format!(
        "create view app_{a}.leak as select * from app_{b}.notes"
    )))
    .execute(&mut admin)
    .await
    .unwrap_err();
    assert!(err.to_string().contains("permission denied"), "{err}");
}

#[tokio::test]
async fn nothing_one_app_set_reaches_the_next_on_the_same_connection() {
    let db = Db::new().await;
    let pool = db.pool("s1", 1).await;
    let store = PgStore::with_pool(pool.clone(), limits());
    let who = "select current_user::text, current_setting('jc.app_id', true), current_setting('search_path')".to_owned();
    for id in ["a", "b", "a"] {
        let placed = app(&db.id(id));
        let (rows, _) = store
            .run(&placed, who.clone(), vec![], Kind::Read)
            .await
            .expect("who");
        let role = format!("app_{}", db.id(id));
        assert_eq!(
            rows.expect("rows").values[0],
            [
                Value::Text(role.clone()),
                Value::Text(db.id(id)),
                Value::Text(role)
            ]
        );
    }
    // After the transaction the connection is the shard's login again, with nothing set.
    let (user, app_id): (String, Option<String>) =
        sqlx::query_as("select current_user::text, nullif(current_setting('jc.app_id', true), '')")
            .fetch_one(&pool)
            .await
            .expect("after");
    assert_eq!(user, format!("wasm_host_s1_{}", db.suffix));
    assert_eq!(app_id, None);
}

#[tokio::test]
async fn the_row_cap_the_timeout_and_the_quota_stop_a_call_with_a_reason() {
    let db = Db::new().await;
    let a = app(&db.id("a"));
    let store = PgStore::with_pool(db.pool("s1", 2).await, limits());
    for n in 0..6 {
        store
            .run(
                &a,
                "insert into notes (body) values ($1)".into(),
                vec![Value::Text(n.to_string())],
                Kind::Write,
            )
            .await
            .expect("insert");
    }
    match store
        .run(&a, "select * from notes".into(), vec![], Kind::Read)
        .await
    {
        Err(SqlError::Refused(why)) => assert!(why.contains("more than 5 rows"), "{why}"),
        other => panic!("{other:?}"),
    }
    assert_eq!(
        store
            .run(&a, "select pg_sleep(2)".into(), vec![], Kind::Read)
            .await
            .unwrap_err(),
        SqlError::Timeout
    );
    // A result larger than its byte cap, with fewer rows than the row cap (T-3342).
    let heavy = PgStore::with_pool(
        db.pool("s1", 1).await,
        SqlLimits {
            result_bytes: 1_000,
            ..limits()
        },
    );
    match heavy
        .run(
            &a,
            "select repeat('x', 600) from notes limit 2".into(),
            vec![],
            Kind::Read,
        )
        .await
    {
        Err(SqlError::Refused(why)) => assert!(why.contains("larger than 1000 bytes"), "{why}"),
        other => panic!("{other:?}"),
    }
    let full = PgStore::with_pool(
        db.pool("s1", 1).await,
        SqlLimits {
            quota_bytes: 1,
            ..limits()
        },
    );
    assert!(matches!(
        full.run(
            &a,
            "insert into notes (body) values ('x')".into(),
            vec![],
            Kind::Write
        )
        .await,
        Err(SqlError::Quota(_))
    ));
    assert!(
        full.run(
            &a,
            "select count(*)::int8 from notes".into(),
            vec![],
            Kind::Read
        )
        .await
        .is_ok(),
        "reads go on past the quota"
    );
}

#[tokio::test]
async fn an_app_keeps_its_files_under_its_own_prefix() {
    let store = Store::new().await;
    let blob = store.blob("s1", 1 << 20);
    let (a, b) = (app("a"), app("b"));
    blob.put(
        &a,
        "notes/1.txt",
        b"one".to_vec(),
        Some("text/plain".into()),
    )
    .await
    .expect("put");
    assert_eq!(blob.get(&a, "notes/1.txt").await.expect("get"), b"one");
    assert_eq!(
        blob.list(&a, "notes/").await.expect("list"),
        ["notes/1.txt"]
    );
    // App b on the same shard has its own prefix: a's key is not there, and no key reaches it.
    assert_eq!(
        blob.get(&b, "notes/1.txt").await.unwrap_err(),
        BlobError::NotFound
    );
    for escape in [
        "../a/notes/1.txt",
        "/apps/s1/a/notes/1.txt",
        "notes/../../a/notes/1.txt",
    ] {
        assert!(
            matches!(blob.get(&b, escape).await, Err(BlobError::Refused(_))),
            "{escape}"
        );
    }
    assert!(blob.list(&b, "").await.expect("list").is_empty());
    blob.delete(&a, "notes/1.txt").await.expect("delete");
    assert_eq!(
        blob.get(&a, "notes/1.txt").await.unwrap_err(),
        BlobError::NotFound
    );
}

#[tokio::test]
async fn rustfs_itself_refuses_one_shards_key_on_another_shards_prefix() {
    let store = Store::new().await;
    let s1 = store.bucket("s1");
    let own = format!("apps/{}/a/x", store.shard("s1"));
    let other = format!("apps/{}/c/x", store.shard("s2"));
    assert_eq!(
        s1.object(reqwest::Method::PUT, &own, Some((b"x".to_vec(), None)))
            .await
            .expect("own")
            .0,
        200
    );
    assert_eq!(
        s1.object(reqwest::Method::PUT, &other, Some((b"x".to_vec(), None)))
            .await
            .expect("other")
            .0,
        403
    );
    assert_eq!(
        s1.object(
            reqwest::Method::PUT,
            "elsewhere/x",
            Some((b"x".to_vec(), None))
        )
        .await
        .expect("elsewhere")
        .0,
        403
    );
    assert!(
        s1.list(&format!("apps/{}/", store.shard("s2")))
            .await
            .is_err(),
        "shard 1 lists nothing of shard 2"
    );
}

#[tokio::test]
async fn a_presigned_url_opens_one_key_for_one_method_for_minutes() {
    let store = Store::new().await;
    let blob = store.blob("s1", 1 << 20);
    let a = app("a");
    blob.put(&a, "photo.png", b"png".to_vec(), None)
        .await
        .expect("put");
    let url = blob
        .presign(&a, "photo.png", Method::Get, 86_400)
        .expect("presign");
    assert!(
        url.contains("X-Amz-Expires=300&"),
        "clamped to five minutes: {url}"
    );
    let http = reqwest::Client::new();
    assert_eq!(
        http.get(&url)
            .send()
            .await
            .expect("get")
            .bytes()
            .await
            .expect("bytes")
            .as_ref(),
        b"png"
    );
    // The same signature for another key or another method is refused by the store.
    assert_eq!(
        http.get(url.replace("photo.png", "other.png"))
            .send()
            .await
            .expect("other")
            .status(),
        403
    );
    assert_eq!(
        http.put(&url).body("x").send().await.expect("put").status(),
        403
    );
    assert!(matches!(
        blob.presign(&a, "../b/photo.png", Method::Put, 60),
        Err(BlobError::Refused(_))
    ));
}

#[tokio::test]
async fn the_blob_quota_stops_a_write_with_a_reason() {
    let store = Store::new().await;
    let blob = store.blob("s1", 4);
    let a = app("q");
    blob.put(&a, "small", b"abc".to_vec(), None)
        .await
        .expect("within the quota");
    match blob.put(&a, "more", b"de".to_vec(), None).await {
        Err(BlobError::Quota(why)) => assert!(why.contains("4 bytes"), "{why}"),
        other => panic!("{other:?}"),
    }
}
