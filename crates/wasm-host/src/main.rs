//! `jc-wasm-host`: one shard of the shared host of server WASM Apps (ADR-N-044).
//!
//! | Variable | Meaning |
//! |---|---|
//! | `JC_WASM_SHARD` | this shard's id; the placement file must name it |
//! | `JC_WASM_PLACEMENTS` | the placement file the reconciler renders for this shard |
//! | `JC_WASM_LISTEN` | the address to serve on, default `0.0.0.0:8080` |
//! | `JC_GATEWAY_URL` | the one origin an App may call, `https://host[:port]` |
//! | `JC_WASM_COMPONENTS_DIR` | components as `<64 hex>.wasm` in a directory, or else: |
//! | `JC_WASM_S3_ENDPOINT`, `JC_WASM_S3_BUCKET` | the object store holding `components/<64 hex>.wasm` |
//! | `JC_WASM_S3_KEY_FILE`, `JC_WASM_S3_SECRET_FILE` | the shard's read key, as mounted files, never variables |
//! | `JC_WASM_*` | the limits, each only lowered (`limits.rs`) |

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};

use hyper::server::conn::http1;
use hyper_util::rt::TokioIo;
use tokio::net::TcpListener;
use wasm_host::host::Host;
use wasm_host::limits::Limits;
use wasm_host::metrics::Metrics;
use wasm_host::server::Shard;
use wasm_host::source::Source;
use wasm_host::storage::Unconfigured;

fn var(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.trim().is_empty())
}

fn secret(name: &str) -> Result<String, String> {
    let path = var(name).ok_or_else(|| format!("{name} is not set"))?;
    std::fs::read_to_string(&path)
        .map(|s| s.trim().to_owned())
        .map_err(|err| format!("{name}: {err}"))
}

fn source() -> Result<Source, String> {
    if let Some(dir) = var("JC_WASM_COMPONENTS_DIR") {
        return Ok(Source::Dir(PathBuf::from(dir)));
    }
    let endpoint =
        var("JC_WASM_S3_ENDPOINT").ok_or("set JC_WASM_COMPONENTS_DIR or JC_WASM_S3_ENDPOINT")?;
    let bucket = var("JC_WASM_S3_BUCKET").ok_or("JC_WASM_S3_BUCKET is not set")?;
    let http = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|err| format!("the object store client: {err}"))?;
    Ok(Source::Store(wasm_host::s3::Bucket {
        endpoint,
        bucket,
        region: var("JC_WASM_S3_REGION").unwrap_or_else(|| "us-east-1".into()),
        key_id: secret("JC_WASM_S3_KEY_FILE")?,
        secret: secret("JC_WASM_S3_SECRET_FILE")?,
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
    let host = Host::new(
        limits,
        source()?,
        Arc::new(Unconfigured),
        var("JC_GATEWAY_URL").as_deref(),
    )
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
