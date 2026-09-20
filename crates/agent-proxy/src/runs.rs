//! Resolves run parameters and permissions from Portal backend.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::RwLock;
use url::Url;

#[derive(Debug, Clone, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RunContext {
    pub id: String,
    pub project: String,
    pub app_name: String,
    pub endpoint_slug: String,
    /// Every endpoint slug of the run, the primary first (AP-44): what
    /// `/v1/data/endpoints/{slug}/…` may address. A Portal that sends none means the primary alone.
    #[serde(default)]
    pub endpoint_slugs: Vec<String>,
    pub allows_write: bool,
    pub branch: String,
    pub path_prefix: String,
    pub status: String,
    pub ticket_hash: String,
    pub max_tokens: u64,
    pub allowed_hosts: Vec<String>,
    pub requests_per_minute: u32,
    /// Model calls this run may make, from the profile's `limits.stepsPerRun` (AG-25, AG-51);
    /// `0` when the Portal does not send it.
    #[serde(default)]
    pub steps_per_run: u32,
    pub max_response_bytes: u64,
    /// Bytes this run may read from the allow-listed hosts in total, from the profile's
    /// `egress.maxBytesPerRun` (AG-65). `0` — a Portal that does not send it, or a profile that
    /// names no host — is a run that reaches nothing.
    #[serde(default)]
    pub max_egress_bytes_per_run: u64,
    pub created_by: String,
    pub model_name: String,
    /// The profile's `model.reasoningEffort`, absent when it names none (AG-72).
    #[serde(default)]
    pub reasoning_effort: Option<String>,
}

impl RunContext {
    /// The endpoints this run may read through the proxy, the primary first.
    pub fn slugs(&self) -> Vec<&str> {
        if self.endpoint_slugs.is_empty() {
            return vec![self.endpoint_slug.as_str()];
        }
        self.endpoint_slugs.iter().map(String::as_str).collect()
    }
}

/// Run records the proxy has fetched, with the moment each was fetched.
type RunCache = Arc<RwLock<HashMap<String, (Instant, Arc<RunContext>)>>>;

#[derive(Clone)]
pub struct RunResolver {
    http: reqwest::Client,
    portal_base: Url,
    /// How the resolver names itself to the Portal (AG-52, T-2271): its own client's token, minted
    /// and held by the credential manager. `None` is a resolver in a test, which asks nobody.
    credentials: Option<crate::inject::CredentialManager>,
    cache: RunCache,
}

impl std::fmt::Debug for RunResolver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RunResolver")
            .field("portal_base", &self.portal_base.as_str())
            .field("credentials", &self.credentials.is_some())
            .finish()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum RunError {
    #[error("run not found: {0}")]
    NotFound(String),
    #[error("run is in terminal state '{0}' and cannot execute")]
    NotActive(String),
    #[error("portal communication failure: {0}")]
    Transport(String),
}

impl RunResolver {
    pub fn new(portal_base: Url, credentials: crate::inject::CredentialManager) -> Self {
        Self {
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(5))
                .build()
                .unwrap_or_default(),
            portal_base,
            credentials: Some(credentials),
            cache: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    pub fn with_cached(run: RunContext) -> Self {
        let mut map = HashMap::new();
        map.insert(
            run.id.clone(),
            (Instant::now() + Duration::from_secs(3600), Arc::new(run)),
        );
        Self {
            http: reqwest::Client::new(),
            portal_base: Url::parse("http://portal").unwrap(),
            credentials: None,
            cache: Arc::new(RwLock::new(map)),
        }
    }

    /// A resolver that holds `run` for an hour and asks `portal_base` when it must look past it.
    pub fn with_cached_at(portal_base: Url, run: RunContext) -> Self {
        let mut resolver = Self::with_cached(run);
        resolver.portal_base = portal_base;
        resolver
    }

    pub async fn resolve(&self, run_id: &str) -> Result<Arc<RunContext>, RunError> {
        let now = Instant::now();
        {
            let cache = self.cache.read().await;
            if let Some((exp, run)) = cache.get(run_id) {
                if *exp > now {
                    Self::check_status(run)?;
                    return Ok(Arc::clone(run));
                }
            }
        }

        self.fetch(run_id).await
    }

    /// The run as the Portal holds it now, past the cache: a conversation gains an endpoint
    /// between two calls, and its first call through that endpoint must not wait out the cache
    /// (AG-75). A failure leaves the cached record as it was.
    pub async fn resolve_fresh(&self, run_id: &str) -> Result<Arc<RunContext>, RunError> {
        self.fetch(run_id).await
    }

    async fn fetch(&self, run_id: &str) -> Result<Arc<RunContext>, RunError> {
        let now = Instant::now();
        let mut url = self.portal_base.clone();
        url.set_path(&format!("internal/agent-runs/{run_id}"));

        let mut req = self.http.get(url);
        if let Some(credentials) = &self.credentials {
            let bearer = credentials
                .get_portal_token()
                .await
                .map_err(RunError::Transport)?;
            req = req.bearer_auth(bearer);
        }

        let resp = req
            .send()
            .await
            .map_err(|e| RunError::Transport(e.to_string()))?;
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Err(RunError::NotFound(run_id.to_string()));
        }
        if !resp.status().is_success() {
            return Err(RunError::Transport(format!("status {}", resp.status())));
        }

        let run: RunContext = resp
            .json()
            .await
            .map_err(|e| RunError::Transport(e.to_string()))?;
        Self::check_status(&run)?;

        let arc = Arc::new(run);
        let mut cache = self.cache.write().await;
        cache.insert(
            run_id.to_string(),
            (now + Duration::from_secs(5), Arc::clone(&arc)),
        );

        Ok(arc)
    }

    fn check_status(run: &RunContext) -> Result<(), RunError> {
        match run.status.as_str() {
            "failed" | "cancelled" | "expired" | "published" => {
                Err(RunError::NotActive(run.status.clone()))
            }
            _ => Ok(()),
        }
    }
}
