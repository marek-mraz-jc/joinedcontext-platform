//! `jc-functions`: reads its configuration, keeps the realm's keys current and serves `/invoke`.

use std::sync::Arc;

use context_gateway::auth::{jwks, token::Verifier};
use functions::{router, AppState, SLOTS};
use tokio::sync::Semaphore;

fn required(name: &str) -> Result<String, String> {
    std::env::var(name)
        .ok()
        .filter(|v| !v.is_empty())
        .ok_or_else(|| format!("{name} is not set"))
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let bind = std::env::var("JC_FUNCTIONS_BIND").unwrap_or_else(|_| "0.0.0.0:8080".to_owned());
    let issuer = required("JC_OIDC_ISSUER")?;
    let jwks_url = required("JC_OIDC_JWKS_URL")?;
    let caller = required("JC_FUNCTIONS_CALLER")?;
    let audience =
        std::env::var("JC_FUNCTIONS_AUDIENCE").unwrap_or_else(|_| "jc-functions".to_owned());
    let gateway = required("JC_GATEWAY_URL")?.trim_end_matches('/').to_owned();
    // Scheme and authority only: a path here would make the endpoint check and the fetched URL differ.
    let parsed = reqwest::Url::parse(&gateway)?;
    if parsed.path() != "/" || parsed.query().is_some() {
        return Err("JC_GATEWAY_URL must be scheme and authority only".into());
    }

    let verifier = Arc::new(Verifier::new(issuer));
    if let Err(error) = jwks::refresh(&verifier, &jwks_url).await {
        tracing::error!(%error, "cannot load the realm signing keys yet; retrying in the background");
    }
    tokio::spawn(jwks::keep_current(Arc::clone(&verifier), jwks_url));

    let state = Arc::new(AppState {
        verifier,
        audience,
        caller,
        gateway,
        http: reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()?,
        slots: Arc::new(Semaphore::new(SLOTS)),
    });
    tracing::info!(%bind, "starting jc-functions");
    let listener = tokio::net::TcpListener::bind(&bind).await?;
    axum::serve(listener, router(state))
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await?;
    Ok(())
}
