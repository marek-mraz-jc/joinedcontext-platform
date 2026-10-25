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
    /// The runs' delegated grants: a run's data calls carry its person's token and no token of
    /// this proxy's own (ADR-N-038, AG-94).
    grants: crate::delegation::Grants,
}

impl CredentialManager {
    pub fn new(config: Arc<Config>) -> Self {
        Self {
            config: config.clone(),
            // The grant carries the OIDC client secret in its form body. A redirect would put
            // that body on the wire to whatever the realm named, so this client follows none
            // (T-1695).
            http: crate::no_redirect_client(Some(Duration::from_secs(5))),
            endpoint_tokens: Arc::new(Mutex::new(HashMap::new())),
            grants: crate::delegation::Grants::new(config.clone()),
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

        // The suites' stub: compiled only with `test-stub`, which the image never builds with, and
        // a binary that has it refuses to start (see `main`), T-1480.
        #[cfg(feature = "test-stub")]
        if !self.config.has_client_credential() {
            return Ok(format!("mock-token-for-{endpoint_slug}"));
        }

        let token_url = self.config.token_url();

        let mut params = self.config.client_auth()?;
        params.push(("grant_type", "client_credentials".to_owned()));
        params.push(("audience", endpoint_slug.to_owned()));

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

        let body = jc_core::outbound::read_capped(resp, crate::routes::MODEL_ANSWER_BYTES)
            .await
            .map_err(|e| e.to_string())?;
        let data: TokenResp = serde_json::from_slice(&body).map_err(|e| e.to_string())?;
        let ttl = Duration::from_secs(data.expires_in.min(300));

        let mut cache = self.endpoint_tokens.lock().await;
        cache.insert(
            endpoint_slug.to_string(),
            (now + ttl, data.access_token.clone()),
        );

        Ok(data.access_token)
    }

    /// The token a run's data call carries: its person's, exchanged when the run started
    /// (ADR-N-038 §3.3). There is no fallback to this proxy's own token.
    pub async fn get_run_token(
        &self,
        run_id: &str,
    ) -> Result<String, crate::delegation::GrantError> {
        // The suites' stub, on the terms of `get_endpoint_token` (T-1480).
        #[cfg(feature = "test-stub")]
        if !self.config.has_client_credential() {
            return Ok(format!("mock-token-for-run-{run_id}"));
        }
        self.grants.token(run_id).await
    }

    /// The runs' delegated grants.
    pub fn grants(&self) -> &crate::delegation::Grants {
        &self.grants
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
