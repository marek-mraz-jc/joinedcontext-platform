//! `jc-wasm-host`: one shard of the shared host of server WASM Apps (ADR-N-044).
//!
//! | Variable | Meaning |
//! |---|---|
//! | `JC_WASM_SHARD` | this shard's id; the placement file must name it |
//! | `JC_WASM_PLACEMENTS` | the placement file the reconciler renders for this shard |
//! | `JC_WASM_LISTEN` | the address to serve on, default `0.0.0.0:8080` |
//! | `JC_GATEWAY_URL` | the one origin an App may call, `https://host[:port]` |
//! | `JC_WASM_S3_ENDPOINT`, `JC_WASM_S3_BUCKET` | the bucket of `components/sha256-<64 hex>.wasm` and `apps/<shard>/<id>/` |
//! | `JC_WASM_S3_KEY_FILE`, `JC_WASM_S3_SECRET_FILE` | the shard's key, as mounted files, never variables |
//! | `JC_WASM_S3_PUBLIC_ENDPOINT` | the store's address for a browser, which presigned URLs name |
//! | `JC_WASM_COMPONENTS_DIR` | components from a directory instead of the bucket |
//! | `JC_WASM_DB_URL_FILE` | the apps database URL as the shard's login role, a mounted file |
//! | `JC_WASM_DB_URL`, `JC_WASM_DB_PASSWORD_FILE` | instead: the URL without a password, and the password as a mounted file |
//! | `JC_WASM_DB_POOL` | connections of the shard's pool, default 20 |
//! | `JC_WASM_SQL_QUOTA_BYTES`, `JC_WASM_BLOB_QUOTA_BYTES` | per App, default 100 MiB and 1 GiB |
//! | `JC_WASM_*` | the limits, each only lowered (`limits.rs`) |
//!
//! Without either an App's SQL answers `unavailable`; without a bucket, its files.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};

use hyper::server::conn::http1;
use hyper_util::rt::TokioIo;
use tokio::net::TcpListener;
use wasm_host::blob::S3Blob;
use wasm_host::host::Host;
use wasm_host::limits::Limits;
use wasm_host::metrics::Metrics;
use wasm_host::s3::Bucket;
use wasm_host::server::Shard;
use wasm_host::source::Source;
use wasm_host::sql::{connect_options, PgStore, SqlLimits};
use wasm_host::storage::{Storage, Stores, Unconfigured};

fn var(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.trim().is_empty())
}

fn secret(name: &str) -> Result<String, String> {
    let path = var(name).ok_or_else(|| format!("{name} is not set"))?;
    std::fs::read_to_string(&path)
        .map(|s| s.trim().to_owned())
        .map_err(|err| format!("{name}: {err}"))
}

fn bytes(name: &str, default: u64) -> Result<u64, String> {
    var(name).map_or(Ok(default), |v| {
        v.trim()
            .parse()
            .map_err(|_| format!("{name} is a number of bytes"))
    })
}

/// The shard's bucket, when the store is configured.
fn bucket() -> Result<Option<Bucket>, String> {
    let Some(endpoint) = var("JC_WASM_S3_ENDPOINT") else {
        return Ok(None);
    };
    let http = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|err| format!("the object store client: {err}"))?;
    Ok(Some(Bucket {
        endpoint,
        bucket: var("JC_WASM_S3_BUCKET").ok_or("JC_WASM_S3_BUCKET is not set")?,
        region: var("JC_WASM_S3_REGION").unwrap_or_else(|| "us-east-1".into()),
        key_id: secret("JC_WASM_S3_KEY_FILE")?,
        secret: secret("JC_WASM_S3_SECRET_FILE")?,
        public_endpoint: var("JC_WASM_S3_PUBLIC_ENDPOINT"),
        http,
    }))
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .json()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();
    if let Err(why) = run().await {
        tracing::error!(%why, "jc-wasm-host stopped");
        std::process::exit(1);
    }
}

async fn run() -> Result<(), String> {
    let id = var("JC_WASM_SHARD").ok_or("JC_WASM_SHARD is not set")?;
    let placements =
        PathBuf::from(var("JC_WASM_PLACEMENTS").ok_or("JC_WASM_PLACEMENTS is not set")?);
    let limits = Limits::from_env(var)?;
    let bucket = bucket()?;
    let source = match (var("JC_WASM_COMPONENTS_DIR"), &bucket) {
        (Some(dir), _) => Source::Dir(PathBuf::from(dir)),
        (None, Some(bucket)) => Source::Store(bucket.clone()),
        (None, None) => return Err("set JC_WASM_S3_ENDPOINT or JC_WASM_COMPONENTS_DIR".into()),
    };
    let database = match (var("JC_WASM_DB_URL_FILE"), var("JC_WASM_DB_URL")) {
        (Some(_), Some(_)) => {
            return Err("set JC_WASM_DB_URL_FILE or JC_WASM_DB_URL, not both".into())
        }
        (Some(_), None) => Some(connect_options(&secret("JC_WASM_DB_URL_FILE")?, None)?),
        (None, Some(url)) => {
            let password = match var("JC_WASM_DB_PASSWORD_FILE") {
                Some(_) => Some(secret("JC_WASM_DB_PASSWORD_FILE")?),
                None => None,
            };
            Some(connect_options(&url, password.as_deref())?)
        }
        (None, None) => None,
    };
    let sql = match database {
        None => None,
        Some(options) => {
            let size = var("JC_WASM_DB_POOL").map_or(Ok(20), |v| {
                v.trim()
                    .parse::<u32>()
                    .map_err(|_| "JC_WASM_DB_POOL is a number".to_owned())
            })?;
            let sql_limits = SqlLimits {
                quota_bytes: bytes("JC_WASM_SQL_QUOTA_BYTES", 100 << 20)?,
                ..SqlLimits::default()
            };
            Some(PgStore::connect(options, size, sql_limits).await?)
        }
    };
    let blob = match bucket {
        None => None,
        Some(bucket) => Some(S3Blob::new(
            bucket,
            &id,
            bytes("JC_WASM_BLOB_QUOTA_BYTES", 1 << 30)?,
        )),
    };
    let storage: Arc<dyn Storage> = if sql.is_none() && blob.is_none() {
        Arc::new(Unconfigured)
    } else {
        Arc::new(Stores { sql, blob })
    };
    let host = Host::new(limits, source, storage, var("JC_GATEWAY_URL").as_deref())
        .map_err(|err| format!("{err:#}"))?;
    let shard = Arc::new(Shard {
        id,
        host,
        apps: RwLock::new(HashMap::new()),
        metrics: Metrics::new(20),
    });
    shard.watch(placements);
    let listen = var("JC_WASM_LISTEN").unwrap_or_else(|| "0.0.0.0:8080".into());
    let listener = TcpListener::bind(&listen)
        .await
        .map_err(|err| format!("{listen}: {err}"))?;
    tracing::info!(%listen, shard = %shard.id, "serving");
    loop {
        let (stream, _) = match listener.accept().await {
            Ok(accepted) => accepted,
            Err(err) => {
                tracing::warn!(%err, "accept failed");
                continue;
            }
        };
        let shard = shard.clone();
        tokio::spawn(async move {
            let service = hyper::service::service_fn(move |request| {
                let shard = shard.clone();
                async move { Ok::<_, std::convert::Infallible>(shard.handle(request).await) }
            });
            if let Err(err) = http1::Builder::new()
                .serve_connection(TokioIo::new(stream), service)
                .await
            {
                tracing::debug!(%err, "connection ended");
            }
        });
    }
}
