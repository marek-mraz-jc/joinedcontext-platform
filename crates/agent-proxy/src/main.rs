//! Main binary entry point for `jc-agent-proxy`.

use agent_proxy::{
    config::Config, inject::CredentialManager, limits::LimitManager, router, runs::RunResolver,
    ProxyState,
};
use std::sync::Arc;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // OPS-15: JSON lines on stdout, nothing on disk.
    jc_core::logs::init("info")?;

    // A binary compiled with the suites' stub token would answer an empty client secret with a
    // made-up token; it is never a proxy anyone may run (T-1480).
    if cfg!(feature = "test-stub") {
        return Err(
            "jc-agent-proxy was built with the test-stub feature and does not start".into(),
        );
    }
    let config = Config::from_env()?;
    config.require_secrets()?;
    tracing::info!(bind = %config.bind, "starting jc-agent-proxy daemon");

    let config_arc = Arc::new(config.clone());
    // The credentials come first: the resolver asks the Portal with a token of this proxy's own
    // client now, not with a string both sides held (AG-52, T-2271).
    let credentials = CredentialManager::new(config_arc.clone());
    let runs = RunResolver::new(config.portal_base.clone(), credentials.clone());
    let limits = LimitManager::default();
    let state = Arc::new(ProxyState::new(config_arc, runs, credentials, limits));

    let listener = tokio::net::TcpListener::bind(config.bind).await?;
    axum::serve(listener, router(state))
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await?;

    Ok(())
}
