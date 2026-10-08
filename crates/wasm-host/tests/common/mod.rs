//! The test servers of jc-wasm-host's storage suites: a fresh Postgres database with two shards
//! and their Apps, and RustFS with a key per shard. See tests/storage_tests.rs for the variables.

use sqlx::postgres::PgPoolOptions;
use sqlx::{AssertSqlSafe, Connection, PgConnection, PgPool};
use time::OffsetDateTime;
use wasm_host::blob::S3Blob;
use wasm_host::placement::Placed;
use wasm_host::provision;
use wasm_host::s3::Bucket;

pub fn var(name: &str) -> String {
    std::env::var(name)
        .unwrap_or_else(|_| panic!("set {name} (see the top of tests/storage_tests.rs)"))
}

pub fn unique() -> String {
    format!(
        "{}{}",
        std::process::id(),
        OffsetDateTime::now_utc().unix_timestamp_nanos() % 1_000_000_000
    )
}

pub fn app(id: &str) -> Placed {
    Placed {
        name: id.replace('_', "-"),
        id: id.into(),
        tenant: "helsinki".into(),
        digest: String::new(),
        jobs: Vec::new(),
    }
}

/// A fresh database with two shards and three Apps (`a`, `b` on s1; `c` on s2), each with a
/// `notes` table its migration made as the owner; returns its URL for a given login and password.
pub struct Db {
    pub url: String,
    pub suffix: String,
}

impl Db {
    pub async fn new() -> Self {
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
        let mut statements = Vec::new();
        statements.extend(provision::database(&name));
        for shard in [format!("s1_{suffix}"), format!("s2_{suffix}")] {
            statements.extend(provision::shard(&shard).expect("shard"));
            statements.push(format!(
                "alter role wasm_host_{shard} password 'pw-{shard}'"
            ));
        }
        for (shard, id) in [("s1", "a"), ("s1", "b"), ("s2", "c")] {
            let (shard, id) = (format!("{shard}_{suffix}"), format!("{id}_{suffix}"));
            statements.extend(provision::app(&shard, &id, "postgres").expect("app"));
            // The App's migration, as the reconciler runs it: as the App's owner, never the App.
            statements.push(format!("set role {}", provision::owner_of(&id)));
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

    pub fn login(&self, shard: &str) -> String {
        let shard = format!("{shard}_{}", self.suffix);
        let rest = self
            .url
            .split_once("://")
            .map(|(_, rest)| rest)
            .expect("url");
        let host_and_db = rest.rsplit_once('@').map_or(rest, |(_, h)| h);
        format!("postgres://wasm_host_{shard}:pw-{shard}@{host_and_db}")
    }

    pub fn id(&self, id: &str) -> String {
        format!("{id}_{}", self.suffix)
    }

    pub async fn pool(&self, shard: &str, size: u32) -> PgPool {
        PgPoolOptions::new()
            .max_connections(size)
            .connect(&self.login(shard))
            .await
            .expect("the shard logs in")
    }
}

/// RustFS with one bucket and one key per shard whose policy reaches `apps/<shard>/*` alone, as
/// the deployment provisions it.
pub struct Store {
    pub root: Bucket,
    pub suffix: String,
}

impl Store {
    pub async fn new() -> Self {
        let root = Bucket {
            endpoint: var("JC_WASM_TEST_S3"),
            bucket: "apps".into(),
            region: "us-east-1".into(),
            key_id: var("JC_WASM_TEST_S3_KEY"),
            secret: var("JC_WASM_TEST_S3_SECRET"),
            public_endpoint: None,
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

    pub fn shard(&self, shard: &str) -> String {
        format!("{shard}{}", self.suffix)
    }

    /// RustFS's admin API, signed with the root key.
    pub async fn admin(&self, method: &str, path: &str, query: &str, body: &serde_json::Value) {
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

    pub fn bucket(&self, shard: &str) -> Bucket {
        let shard = self.shard(shard);
        Bucket {
            key_id: format!("key{shard}"),
            secret: format!("secret-{shard}"),
            ..self.root.clone()
        }
    }

    pub fn blob(&self, shard: &str, quota: u64) -> S3Blob {
        S3Blob::new(self.bucket(shard), &self.shard(shard), quota)
    }
}
