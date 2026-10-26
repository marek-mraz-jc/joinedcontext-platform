//! Subjects the Portal revoked: a person disabled, deleted or logged out, a ServiceAccount
//! removed (T-5568, TR-03187 AR-6).
//!
//! The gateway verifies tokens locally, so a disabled person's token would open every surface
//! until it expires. The Portal keeps the marks (`sub` and the second it was revoked) and lists
//! them on its internal listener (`JC_GATEWAY_REVOCATIONS_URL`); the gateway asks every ten
//! seconds, with its own workload token, and hands the list to the [`Verifier`], which refuses a
//! listed subject's token issued at or before its mark on every door that verifies one: REST,
//! the hub and Endpoint MCP, WebSocket, MQTT.
//!
//! A failed poll keeps the last list: a revocation must not lapse because the Portal was away
//! for a moment. The Portal drops a mark once no token issued before it can still be valid.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use jc_core::auth::token::Verifier;
use serde::Deserialize;

use crate::previews::WorkloadToken;

/// How often the Portal is asked, the pace of the preview list (CC-78).
pub const INTERVAL: Duration = Duration::from_secs(10);

/// The most of the list read: about 100 000 marks, so a Portal answering without end cannot
/// fill the gateway's memory (T-4984).
pub const MAX_LIST_BYTES: usize = 8 * 1024 * 1024;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Item {
    subject: String,
    revoked_at: i64,
}

#[derive(Deserialize)]
struct List {
    items: Vec<Item>,
}

/// The poller: where the list is, who asks, and the verifier it feeds.
pub struct Revocations {
    client: reqwest::Client,
    url: String,
    token: Option<Arc<WorkloadToken>>,
    verifier: Arc<Verifier>,
    etag: Option<String>,
}

impl Revocations {
    /// A poller of `url` that sends `token` and writes into `verifier`.
    pub fn new(
        url: String,
        token: Option<Arc<WorkloadToken>>,
        verifier: Arc<Verifier>,
    ) -> Result<Self, String> {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(5))
            .build()
            .map_err(|error| format!("no HTTP client for the revocations: {error}"))?;
        Ok(Self {
            client,
            url,
            token,
            verifier,
            etag: None,
        })
    }

    /// Asks once. `Ok(Some(n))` installed a list of `n` subjects, `Ok(None)` is a `304`: the
    /// list stands. An error leaves the verifier's list as it was.
    pub async fn poll(&mut self) -> Result<Option<usize>, String> {
        let mut request = self.client.get(&self.url);
        if let Some(token) = self.token.as_ref() {
            request = request.bearer_auth(token.get().await?);
        }
        if let Some(tag) = self.etag.as_deref() {
            request = request.header(reqwest::header::IF_NONE_MATCH, tag);
        }
        let response = request.send().await.map_err(|error| error.to_string())?;
        if response.status() == reqwest::StatusCode::NOT_MODIFIED {
            return Ok(None);
        }
        if !response.status().is_success() {
            self.etag = None;
            return Err(format!("the Portal answered {}", response.status()));
        }
        let tag = response
            .headers()
            .get(reqwest::header::ETAG)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        let list: List = crate::previews::json_within(response, MAX_LIST_BYTES)
            .await
            .map_err(|error| {
                self.etag = None;
                format!("the revocations are not readable: {error}")
            })?;
        let marks: HashMap<String, i64> = list
            .items
            .into_iter()
            .map(|item| (item.subject, item.revoked_at))
            .collect();
        let count = marks.len();
        self.verifier.replace_revocations(marks);
        self.etag = tag;
        Ok(Some(count))
    }

    /// Polls every [`INTERVAL`] for as long as the gateway runs.
    pub async fn follow(mut self) {
        let mut ticker = tokio::time::interval(INTERVAL);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            ticker.tick().await;
            if let Err(error) = self.poll().await {
                tracing::warn!(%error, "the Portal did not list the revoked subjects; the last list stands");
            }
        }
    }
}
