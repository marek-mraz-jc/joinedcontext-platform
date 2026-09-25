//! A run reads as the person who started it (ADR-N-038, AG-94).
//!
//! The Portal hands the person's access token over once, when the run starts. The proxy checks it
//! names the run's own person, exchanges it at once for a token of its own client whose subject is
//! that person (RFC 8693, Keycloak standard token exchange V2) with a refresh token of the same
//! session, and keeps both in memory for the run alone. Every data call of the run carries that
//! token; a run without one reads nothing. When the run ends the refresh token is revoked
//! (RFC 7009). Nothing here is written to disk or to a log line.

use crate::config::Config;
use crate::runs::{RunContext, RunError, RunResolver};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;

const TOKEN_EXCHANGE: &str = "urn:ietf:params:oauth:grant-type:token-exchange";
const ACCESS_TOKEN_TYPE: &str = "urn:ietf:params:oauth:token-type:access_token";
const REFRESH_TOKEN_TYPE: &str = "urn:ietf:params:oauth:token-type:refresh_token";
/// The audience of a run's token: the gateway's, which every endpoint accepts (Architecture/12
/// §4). V2 takes client IDs only, so an endpoint slug cannot be one; the run's own slug list
/// narrows the endpoints (AP-44).
const GATEWAY_AUDIENCE: &str = "context-gateway";
/// A token this close to its expiry is refreshed before it is sent.
const LEEWAY: Duration = Duration::from_secs(30);

/// One run's grant: the person's exchanged token and the refresh token of their session.
struct Grant {
    access: String,
    until: Instant,
    refresh: String,
}

/// Why a run has no token to send. The reasons never carry a token.
#[derive(Debug, thiserror::Error)]
pub enum GrantError {
    /// No person's token was handed over for this run, or its grant has ended.
    #[error("this run holds no identity of the person who started it")]
    Missing,
    /// The realm refused the grant: the person signed out, or their session expired.
    #[error("the realm refused the grant of the person who started this run: {0}")]
    Refused(String),
    /// The realm could not be reached or answered something unreadable.
    #[error("the realm could not be reached: {0}")]
    Transport(String),
}

/// Why a hand-over was refused.
#[derive(Debug, thiserror::Error)]
pub enum BindError {
    /// The token is not an active token of the run's own person.
    #[error("the token is not an active token of the person who started the run")]
    NotTheRunsPerson,
    /// The realm refused the exchange.
    #[error("the realm refused the exchange: {0}")]
    Refused(String),
    #[error("the realm could not be reached: {0}")]
    Transport(String),
}

/// What RFC 7662 answers, as far as the proxy reads it.
#[derive(serde::Deserialize)]
struct Introspection {
    #[serde(default)]
    active: bool,
    #[serde(default)]
    username: Option<String>,
    #[serde(default)]
    azp: Option<String>,
    #[serde(default)]
    aud: Option<Audience>,
}

#[derive(serde::Deserialize)]
#[serde(untagged)]
enum Audience {
    One(String),
    Many(Vec<String>),
}

impl Audience {
    fn contains(&self, name: &str) -> bool {
        match self {
            Audience::One(one) => one == name,
            Audience::Many(many) => many.iter().any(|a| a == name),
        }
    }
}

#[derive(serde::Deserialize)]
struct Tokens {
    access_token: String,
    expires_in: u64,
    #[serde(default)]
    refresh_token: Option<String>,
}

/// The grants of the runs this proxy holds, one per run id.
#[derive(Clone)]
pub struct Grants {
    config: Arc<Config>,
    http: reqwest::Client,
    // ponytail: every grant sits behind its own lock, so one run's refresh never waits on another
    // run's; the map lock is held for lookups only.
    held: Arc<Mutex<HashMap<String, Arc<Mutex<Grant>>>>>,
}

impl Grants {
    pub fn new(config: Arc<Config>) -> Self {
        Self {
            config,
            // The exchange carries the client secret and a person's token in its form body; a
            // redirect would carry both wherever the realm named (T-1695).
            http: crate::no_redirect_client(Some(Duration::from_secs(5))),
            held: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    async fn introspect(&self, token: &str) -> Result<Introspection, String> {
        let response = self
            .http
            .post(self.config.introspection_url())
            .form(&[
                ("client_id", self.config.oidc_client_id.as_str()),
                ("client_secret", self.config.oidc_client_secret.as_str()),
                ("token", token),
            ])
            .send()
            .await
            .map_err(|e| e.without_url().to_string())?;
        if !response.status().is_success() {
            return Err(format!("introspection answered {}", response.status()));
        }
        response
            .json()
            .await
            .map_err(|e| e.without_url().to_string())
    }

    /// Whether `bearer` is the Portal's own service-account token, issued for this proxy.
    ///
    /// A person's token of the Portal's client carries the same `azp`, so the subject must be
    /// the client's service-account user as well (the rule the gateway applies, AG-95).
    pub async fn is_portal(&self, bearer: &str) -> Result<bool, String> {
        let seen = self.introspect(bearer).await?;
        let portal = &self.config.portal_client_id;
        Ok(seen.active
            && seen.azp.as_deref() == Some(portal.as_str())
            && seen
                .username
                .is_some_and(|u| u.eq_ignore_ascii_case(&format!("service-account-{portal}")))
            && seen
                .aud
                .is_some_and(|aud| aud.contains(&self.config.oidc_client_id)))
    }

    /// Takes the person's token for `run`, exchanges it and holds the grant (ADR-N-038 §3.1–2).
    ///
    /// The token must be active and name the run's own person: the Portal handing over the wrong
    /// session would otherwise let one person's run read as another. A grant the run already
    /// held is replaced and revoked.
    pub async fn bind(&self, run: &RunContext, subject_token: &str) -> Result<(), BindError> {
        let seen = self
            .introspect(subject_token)
            .await
            .map_err(BindError::Transport)?;
        let theirs = seen
            .username
            .is_some_and(|u| u.eq_ignore_ascii_case(&run.created_by));
        if !seen.active || !theirs {
            return Err(BindError::NotTheRunsPerson);
        }

        let response = self
            .http
            .post(self.config.token_url())
            .form(&[
                ("grant_type", TOKEN_EXCHANGE),
                ("client_id", self.config.oidc_client_id.as_str()),
                ("client_secret", self.config.oidc_client_secret.as_str()),
                ("subject_token", subject_token),
                ("subject_token_type", ACCESS_TOKEN_TYPE),
                ("requested_token_type", REFRESH_TOKEN_TYPE),
                ("audience", GATEWAY_AUDIENCE),
            ])
            .send()
            .await
            .map_err(|e| BindError::Transport(e.without_url().to_string()))?;
        if !response.status().is_success() {
            return Err(BindError::Refused(format!(
                "token exchange answered {}",
                response.status()
            )));
        }
        let tokens: Tokens = response
            .json()
            .await
            .map_err(|e| BindError::Transport(e.without_url().to_string()))?;
        let refresh = tokens.refresh_token.ok_or_else(|| {
            BindError::Refused("the exchange returned no refresh token".to_owned())
        })?;
        let grant = Grant {
            access: tokens.access_token,
            until: Instant::now() + Duration::from_secs(tokens.expires_in),
            refresh,
        };

        let replaced = self
            .held
            .lock()
            .await
            .insert(run.id.clone(), Arc::new(Mutex::new(grant)));
        if let Some(old) = replaced {
            let refresh = old.lock().await.refresh.clone();
            self.revoke(&run.id, &refresh).await;
        }
        tracing::info!(run = %run.id, user = %run.created_by, "the run holds its person's grant");
        Ok(())
    }

    /// The run's current token, refreshed when it is about to expire (ADR-N-038 §3.3).
    ///
    /// A refused refresh ends the grant: the person's session is over, and the run's next read
    /// is a `401` rather than a retry against a realm that already said no.
    pub async fn token(&self, run_id: &str) -> Result<String, GrantError> {
        let grant = self
            .held
            .lock()
            .await
            .get(run_id)
            .cloned()
            .ok_or(GrantError::Missing)?;
        let mut grant = grant.lock().await;
        if grant.until > Instant::now() + LEEWAY {
            return Ok(grant.access.clone());
        }

        let response = self
            .http
            .post(self.config.token_url())
            .form(&[
                ("grant_type", "refresh_token"),
                ("client_id", self.config.oidc_client_id.as_str()),
                ("client_secret", self.config.oidc_client_secret.as_str()),
                ("refresh_token", grant.refresh.as_str()),
            ])
            .send()
            .await
            .map_err(|e| GrantError::Transport(e.without_url().to_string()))?;
        let status = response.status();
        if status.is_client_error() {
            drop(grant);
            self.held.lock().await.remove(run_id);
            tracing::info!(run = %run_id, %status, "the realm ended the run's grant");
            return Err(GrantError::Refused(format!("refresh answered {status}")));
        }
        if !status.is_success() {
            return Err(GrantError::Transport(format!("refresh answered {status}")));
        }
        let tokens: Tokens = response
            .json()
            .await
            .map_err(|e| GrantError::Transport(e.without_url().to_string()))?;
        grant.access = tokens.access_token;
        grant.until = Instant::now() + Duration::from_secs(tokens.expires_in);
        if let Some(refresh) = tokens.refresh_token {
            grant.refresh = refresh;
        }
        Ok(grant.access.clone())
    }

    /// Drops the run's grant and revokes its refresh token (ADR-N-038 §3.5). A run with no grant
    /// is nothing to do.
    pub async fn end(&self, run_id: &str) {
        let Some(grant) = self.held.lock().await.remove(run_id) else {
            return;
        };
        let refresh = grant.lock().await.refresh.clone();
        self.revoke(run_id, &refresh).await;
    }

    async fn revoke(&self, run_id: &str, refresh: &str) {
        let sent = self
            .http
            .post(self.config.revocation_url())
            .form(&[
                ("client_id", self.config.oidc_client_id.as_str()),
                ("client_secret", self.config.oidc_client_secret.as_str()),
                ("token", refresh),
                ("token_type_hint", "refresh_token"),
            ])
            .send()
            .await;
        match sent {
            Ok(response) if response.status().is_success() => {
                tracing::info!(run = %run_id, "the run's grant is revoked");
            }
            // The grant is dropped from memory either way; the realm's session lifespan bounds
            // what an unrevoked one could still do, and nothing here holds it any more.
            Ok(response) => {
                tracing::warn!(run = %run_id, status = %response.status(), "the realm refused to revoke the run's grant")
            }
            Err(error) => {
                tracing::warn!(run = %run_id, error = %error.without_url(), "the run's grant could not be revoked")
            }
        }
    }

    /// Ends the grant of every held run the Portal no longer holds as active: finished,
    /// cancelled, expired or reaped. A Portal that cannot be reached ends nothing.
    pub async fn end_finished(&self, runs: &RunResolver) {
        let held: Vec<String> = self.held.lock().await.keys().cloned().collect();
        for run_id in held {
            match runs.resolve_fresh(&run_id).await {
                Err(RunError::NotFound(_) | RunError::NotActive(_)) => self.end(&run_id).await,
                Err(RunError::Transport(_)) | Ok(_) => {}
            }
        }
    }
}
