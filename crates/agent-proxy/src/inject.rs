//! Credential management and dynamic bearer token acquisition.

use crate::config::Config;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;

/// What a token for the Portal's internal listener must be issued for; the Portal refuses any other
/// audience on those routes.
pub const PORTAL_INTERNAL_AUDIENCE: &str = "portal-internal";

#[derive(Clone)]
pub struct CredentialManager {
    config: Arc<Config>,
    http: reqwest::Client,
    endpoint_tokens: Arc<Mutex<HashMap<String, (Instant, String)>>>,
}

impl CredentialManager {
    pub fn new(config: Arc<Config>) -> Self {
        Self {
            config,
            // The grant carries the OIDC client secret in its form body. A redirect would put
            // that body on the wire to whatever the realm named, so this client follows none
            // (T-1695).
            http: crate::no_redirect_client(Some(Duration::from_secs(5))),
            endpoint_tokens: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub async fn get_endpoint_token(&self, endpoint_slug: &str) -> Result<String, String> {
        let now = Instant::now();
        {
            let cache = self.endpoint_tokens.lock().await;
            if let Some((exp, token)) = cache.get(endpoint_slug) {
                if *exp > now + Duration::from_secs(30) {
                    return Ok(token.clone());
                }
            }
        }

        // Only reachable in tests: `Config::require_secrets` makes an empty secret a startup
        // failure, so a deployed proxy never mints a stub token.
        if self.config.oidc_client_secret.is_empty() {
            return Ok(format!("mock-token-for-{endpoint_slug}"));
        }

        let token_url = self.config.token_url();

        let params = [
            ("grant_type", "client_credentials"),
            ("client_id", &self.config.oidc_client_id),
            ("client_secret", &self.config.oidc_client_secret),
            ("audience", endpoint_slug),
        ];

        let resp = self
            .http
            .post(token_url)
            .form(&params)
            .send()
            .await
            .map_err(|e| e.to_string())?;

        if !resp.status().is_success() {
            return Err(format!("token endpoint rejected grant: {}", resp.status()));
        }

        #[derive(serde::Deserialize)]
        struct TokenResp {
            access_token: String,
            expires_in: u64,
        }

        let data: TokenResp = resp.json().await.map_err(|e| e.to_string())?;
        let ttl = Duration::from_secs(data.expires_in.min(300));

        let mut cache = self.endpoint_tokens.lock().await;
        cache.insert(
            endpoint_slug.to_string(),
            (now + ttl, data.access_token.clone()),
        );

        Ok(data.access_token)
    }

    pub fn get_forge_token(&self) -> &str {
        &self.config.forge_token
    }

    pub fn get_model_key(&self) -> &str {
        &self.config.model_key
    }

    /// The token this proxy presents on the Portal's internal listener (AG-52, T-2271): its own
    /// client's, audience-bound to that listener, minted and held like every other one. It was a
    /// string shared with the Portal, which is the static key between cluster services CLAUDE.md
    /// rules out.
    pub async fn get_portal_token(&self) -> Result<String, String> {
        self.get_endpoint_token(PORTAL_INTERNAL_AUDIENCE).await
    }
}
