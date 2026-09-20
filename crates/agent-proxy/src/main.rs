//! Main binary entry point for `jc-agent-proxy`.

use agent_proxy::{
    config::Config, inject::CredentialManager, limits::LimitManager, router, runs::RunResolver,
    ProxyState,
};
use std::sync::Arc;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let config = Config::from_env()?;
    config.require_secrets()?;
    tracing::info!(bind = %config.bind, "starting jc-agent-proxy daemon");

    let config_arc = Arc::new(config.clone());
    // The credentials come first: the resolver asks the Portal with a token of this proxy's own
    // client now, not with a string both sides held (AG-52, T-2271).
    let credentials = CredentialManager::new(config_arc.clone());
    let runs = RunResolver::new(config.portal_base.clone(), credentials.clone());
    let limits = LimitManager::default();
    let http = reqwest::Client::new();
    // No redirect of its own: the fetch route checks every hop against the run's allow-list.
    let egress = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .unwrap_or_default();

    let state = Arc::new(ProxyState {
        config: config_arc,
        runs,
        credentials,
        limits,
        http,
        egress,
    });

    let listener = tokio::net::TcpListener::bind(config.bind).await?;
    axum::serve(listener, router(state))
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await?;

    Ok(())
}
