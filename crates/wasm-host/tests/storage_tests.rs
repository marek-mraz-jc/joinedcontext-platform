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

use sqlx::postgres::PgPoolOptions;
use sqlx::{AssertSqlSafe, Connection, PgConnection, PgPool};
use time::OffsetDateTime;
use wasm_host::blob::S3Blob;
use wasm_host::placement::Placed;
use wasm_host::provision;
use wasm_host::s3::Bucket;
use wasm_host::sql::{Kind, PgStore, SqlLimits};
use wasm_host::storage::{BlobError, Method, SqlError, Value};

fn var(name: &str) -> String {
    std::env::var(name)
        .unwrap_or_else(|_| panic!("set {name} (see the top of tests/storage_tests.rs)"))
}

fn unique() -> String {
    format!(
        "{}{}",
        std::process::id(),
        OffsetDateTime::now_utc().unix_timestamp_nanos() % 1_000_000_000
    )
}

fn app(id: &str) -> Placed {
    Placed {
        name: id.replace('_', "-"),
        id: id.into(),
        tenant: "helsinki".into(),
        digest: String::new(),
    }
}

/// A fresh database with two shards and three Apps (`a`, `b` on s1; `c` on s2), each with a
/// `notes` table its migration made as the owner; returns its URL for a given login and password.
struct Db {
    url: String,
    suffix: String,
}

impl Db {
    async fn new() -> Self {
        let admin = var("JC_WASM_TEST_DATABASE_URL");
        let suffix = unique();
        let name = format!("wasmhost_{suffix}");
        let mut conn = PgConnection::connect(&admin)
            .await
            .expect("the test server");
        sqlx::query(AssertSqlSafe(format!("create database {name}")))
            .execute(&mut conn)
            .await
            .expect("create database");
        let url = admin
            .rsplit_once('/')
            .map(|(base, _)| format!("{base}/{name}"))
            .expect("a URL with a database");
        let mut db = PgConnection::connect(&url).await.expect("the new database");
        let owner = format!("apps_owner_{suffix}");
        let mut statements = vec![format!("create role {owner} nologin")];
        statements.extend(provision::database(&name));
        for shard in [format!("s1_{suffix}"), format!("s2_{suffix}")] {
            statements.extend(provision::shard(&shard).expect("shard"));
            statements.push(format!(
                "alter role wasm_host_{shard} password 'pw-{shard}'"
            ));
        }
        for (shard, id) in [("s1", "a"), ("s1", "b"), ("s2", "c")] {
            let (shard, id) = (format!("{shard}_{suffix}"), format!("{id}_{suffix}"));
            statements.extend(provision::app(&shard, &id, &owner).expect("app"));
            // The App's migration, as the reconciler runs it: as the owner, never as the App.
            statements.push(format!("set role {owner}"));
            statements.push(format!(
                "create table app_{id}.notes (id serial primary key, body text not null)"
            ));
            statements.push(format!(
                "insert into app_{id}.notes (body) values ('{id} first')"
            ));
            statements.push("reset role".into());
        }
        for statement in statements {
            sqlx::query(AssertSqlSafe(statement.clone()))
                .execute(&mut db)
                .await
                .unwrap_or_else(|err| panic!("{statement}: {err}"));
        }
        Self { url, suffix }
    }

    fn login(&self, shard: &str) -> String {
        let shard = format!("{shard}_{}", self.suffix);
        let rest = self
            .url
            .split_once("://")
            .map(|(_, rest)| rest)
            .expect("url");
        let host_and_db = rest.rsplit_once('@').map_or(rest, |(_, h)| h);
        format!("postgres://wasm_host_{shard}:pw-{shard}@{host_and_db}")
    }

    fn id(&self, id: &str) -> String {
        format!("{id}_{}", self.suffix)
    }

    async fn pool(&self, shard: &str, size: u32) -> PgPool {
        PgPoolOptions::new()
            .max_connections(size)
            .connect(&self.login(shard))
            .await
            .expect("the shard logs in")
    }
}

fn limits() -> SqlLimits {
    SqlLimits {
        statement_timeout: Duration::from_millis(500),
        row_cap: 5,
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
    // A write through query is refused; a role switch never reaches Postgres.
    assert!(matches!(
        store
            .run(&a, "delete from notes".into(), vec![], Kind::Read)
            .await,
        Err(SqlError::Refused(_))
    ));
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

/// RustFS with one bucket and one key per shard whose policy reaches `apps/<shard>/*` alone, as
/// the deployment provisions it.
struct Store {
    root: Bucket,
    suffix: String,
}

impl Store {
    async fn new() -> Self {
        let root = Bucket {
            endpoint: var("JC_WASM_TEST_S3"),
            bucket: "apps".into(),
            region: "us-east-1".into(),
            key_id: var("JC_WASM_TEST_S3_KEY"),
            secret: var("JC_WASM_TEST_S3_SECRET"),
            http: reqwest::Client::new(),
        };
        let store = Self {
            root,
            suffix: unique(),
        };
        let bucket = Bucket {
            bucket: String::new(),
            ..store.root.clone()
        };
        let (status, _) = bucket
            .object(reqwest::Method::PUT, "apps", None)
            .await
            .expect("bucket");
        assert!(status == 200 || status == 409, "the bucket: {status}");
        for shard in ["s1", "s2"] {
            let shard = store.shard(shard);
            let policy = serde_json::json!({"Version": "2012-10-17", "Statement": [
                {"Effect": "Allow", "Action": ["s3:GetObject", "s3:PutObject", "s3:DeleteObject"], "Resource": [format!("arn:aws:s3:::apps/apps/{shard}/*")]},
                {"Effect": "Allow", "Action": ["s3:ListBucket"], "Resource": ["arn:aws:s3:::apps"], "Condition": {"StringLike": {"s3:prefix": [format!("apps/{shard}/*")]}}}
            ]});
            store.admin("PUT", "/rustfs/admin/v3/add-user", &format!("accessKey=key{shard}"), &serde_json::json!({"secretKey": format!("secret-{shard}"), "status": "enabled"})).await;
            store
                .admin(
                    "PUT",
                    "/rustfs/admin/v3/add-canned-policy",
                    &format!("name=p{shard}"),
                    &policy,
                )
                .await;
            store.admin("POST", "/rustfs/admin/v3/idp/builtin/policy/attach", "", &serde_json::json!({"policies": [format!("p{shard}")], "user": format!("key{shard}")})).await;
        }
        store
    }

    fn shard(&self, shard: &str) -> String {
        format!("{shard}{}", self.suffix)
    }

    /// RustFS's admin API, signed with the root key.
    async fn admin(&self, method: &str, path: &str, query: &str, body: &serde_json::Value) {
        let body = body.to_string();
        let sha = hex::encode(<sha2::Sha256 as sha2::Digest>::digest(body.as_bytes()));
        let (authorization, amz_date) =
            self.root
                .authorization(method, path, query, &[], &sha, OffsetDateTime::now_utc());
        let url = format!(
            "{}{path}{}{query}",
            self.root.endpoint,
            if query.is_empty() { "" } else { "?" }
        );
        let response = self
            .root
            .http
            .request(method.parse().expect("method"), url)
            .header("authorization", authorization)
            .header("x-amz-date", amz_date)
            .header("x-amz-content-sha256", sha)
            .body(body)
            .send()
            .await
            .expect("admin");
        assert!(
            response.status().is_success(),
            "{path}: {} {}",
            response.status(),
            response.text().await.unwrap_or_default()
        );
    }

    fn bucket(&self, shard: &str) -> Bucket {
        let shard = self.shard(shard);
        Bucket {
            key_id: format!("key{shard}"),
            secret: format!("secret-{shard}"),
            ..self.root.clone()
        }
    }

    fn blob(&self, shard: &str, quota: u64) -> S3Blob {
        S3Blob::new(self.bucket(shard), &self.shard(shard), quota)
    }
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
