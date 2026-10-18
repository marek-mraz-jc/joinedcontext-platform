//! Keeping the realm's signing keys current (T-0228, PF-46).
//!
//! The JWKS is fetched in the background and swapped in whole. A request never fetches:
//! an unknown key id is a rejection, because a token that names a key nobody published is
//! not a token, and letting a request trigger an outbound fetch is a way to make the
//! gateway do work on an attacker's schedule.
//!
//! The URL is the in-cluster Keycloak address, so plain HTTP is what the gateway speaks
//! here; the hop is inside the mesh, where Linkerd provides mTLS. An `https` JWKS URL is
//! refused at start-up rather than silently downgraded.

use crate::auth::token::Verifier;
use axum::body::Body;
use jsonwebtoken::jwk::JwkSet;
use std::sync::Arc;
use std::time::Duration;

/// How often the keys are re-fetched. Keycloak rotates on the order of days; a realm that
/// has just rotated must not lock every caller out until the next restart.
pub const REFRESH: Duration = Duration::from_secs(300);

/// Why the keys could not be loaded.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum JwksError {
    /// The URL is not one the gateway fetches over.
    #[error("{0}")]
    Url(String),
    /// The realm did not answer, or answered with something other than a JWKS.
    #[error("{0}")]
    Unreachable(String),
}

/// Fetches the JWKS at `url` and installs its keys, returning how many were usable.
pub async fn refresh(verifier: &Verifier, url: &str) -> Result<usize, JwksError> {
    if url.starts_with("https://") {
        return Err(JwksError::Url(
            "the JWKS URL must be the in-cluster http:// address; the gateway does not speak TLS to Keycloak"
                .to_owned(),
        ));
    }
    if !url.starts_with("http://") {
        return Err(JwksError::Url(
            "the JWKS URL must start with http://".to_owned(),
        ));
    }

    let client: hyper_util::client::legacy::Client<_, Body> =
        hyper_util::client::legacy::Client::builder(hyper_util::rt::TokioExecutor::new())
            .build_http();
    let response = client
        .get(url.parse().map_err(|e| JwksError::Url(format!("{e}")))?)
        .await
        .map_err(|e| JwksError::Unreachable(e.to_string()))?;

    if !response.status().is_success() {
        return Err(JwksError::Unreachable(format!(
            "the realm answered {}",
            response.status()
        )));
    }
    let bytes = axum::body::to_bytes(Body::new(response.into_body()), 1024 * 1024)
        .await
        .map_err(|e| JwksError::Unreachable(e.to_string()))?;
    let jwks: JwkSet =
        serde_json::from_slice(&bytes).map_err(|e| JwksError::Unreachable(e.to_string()))?;

    Ok(verifier.replace_keys(&jwks))
}

/// Refreshes the keys forever, starting now.
///
/// A failed refresh keeps the keys the verifier already has: a realm that is briefly
/// unreachable must not take every authenticated caller down with it.
pub async fn keep_current(verifier: Arc<Verifier>, url: String) {
    loop {
        match refresh(&verifier, &url).await {
            Ok(count) => tracing::info!(count, "realm signing keys loaded"),
            Err(error) => {
                tracing::error!(%error, url = %url, "cannot refresh the realm signing keys")
            }
        }
        tokio::time::sleep(REFRESH).await;
    }
}
