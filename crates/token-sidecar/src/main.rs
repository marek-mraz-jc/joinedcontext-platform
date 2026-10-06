//! `jc-token-sidecar`: the pipeline runner's token service (PL-19, T-1508).
//!
//! It runs in a pod of its own beside the runner, the one pod of the runner's namespace that may
//! reach the Kubernetes API, so the runner keeps none; the deployment's NetworkPolicy and mesh
//! authorization admit the runner alone to its port.
//!
//! Configuration is environment only:
//! - `JC_SIDECAR_LISTEN`: where to listen, `0.0.0.0:4180` by default.
//! - `JC_TOKEN_URL`: the realm's token endpoint.
//! - `JC_TOKEN_AUDIENCE`: the realm issuer, the audience Keycloak's federated client authentication expects.
//! - `KUBERNETES_SERVICE_HOST`/`KUBERNETES_SERVICE_PORT`: set by the kubelet; the API the TokenRequests go to.
//!
//! The runner's own ServiceAccount token, the cluster CA and the namespace are read from the
//! standard mount, `/var/run/secrets/kubernetes.io/serviceaccount/`.

use std::path::Path;
use token_sidecar::{router, Config, Sidecar};

const MOUNT: &str = "/var/run/secrets/kubernetes.io/serviceaccount";

fn required(name: &str) -> Result<String, String> {
    std::env::var(name)
        .ok()
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| format!("{name} is not set"))
}

async fn run() -> Result<(), String> {
    let listen = std::env::var("JC_SIDECAR_LISTEN").unwrap_or_else(|_| "127.0.0.1:4180".to_owned());
    let address: std::net::SocketAddr = listen
        .parse()
        .map_err(|_| format!("JC_SIDECAR_LISTEN {listen} is no address"))?;
    if !address.ip().is_loopback() {
        return Err(format!("JC_SIDECAR_LISTEN {listen} is not a loopback address: the sidecar answers its own pod alone"));
    }
    let host = required("KUBERNETES_SERVICE_HOST")?;
    let port = std::env::var("KUBERNETES_SERVICE_PORT").unwrap_or_else(|_| "443".to_owned());
    let mount = Path::new(MOUNT);
    let namespace = std::fs::read_to_string(mount.join("namespace"))
        .map_err(|error| format!("the namespace file: {error}"))?
        .trim()
        .to_owned();
    let ca =
        std::fs::read(mount.join("ca.crt")).map_err(|error| format!("the cluster CA: {error}"))?;
    let ca =
        reqwest::Certificate::from_pem(&ca).map_err(|error| format!("the cluster CA: {error}"))?;
    let http = reqwest::Client::builder()
        .add_root_certificate(ca)
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .map_err(|error| format!("the HTTP client: {error}"))?;
    let config = Config {
        kubernetes_api: format!("https://{host}:{port}"),
        namespace,
        own_token_file: mount.join("token"),
        token_url: required("JC_TOKEN_URL")?,
        audience: required("JC_TOKEN_AUDIENCE")?,
    };
    let listener = tokio::net::TcpListener::bind(address)
        .await
        .map_err(|error| format!("listening on {address}: {error}"))?;
    tracing::info!(%address, namespace = %config.namespace, "token sidecar listening");
    axum::serve(listener, router(Sidecar::new(config, http)))
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await
        .map_err(|error| error.to_string())
}

#[tokio::main]
async fn main() {
    let _ = jc_core::logs::init("info");
    if let Err(problem) = run().await {
        tracing::error!(%problem, "the token sidecar stopped");
        std::process::exit(1);
    }
}
