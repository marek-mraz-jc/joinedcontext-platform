//! The model key's state, reported to the Portal (AG-96): a probe of the provider's key endpoint
//! that makes no completion, and every model call the provider refuses for the key. Neither the
//! report nor a log line carries the key.

use crate::config::Config;
use crate::ProxyState;
use serde::Serialize;
use serde_json::Value;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// What the provider last said about the key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum KeyState {
    Valid,
    Invalid,
    OutOfCredit,
    Unreachable,
}

/// One report, as `POST /internal/model-key` takes it (API/04 §7.2).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Report {
    pub state: KeyState,
    /// `probe` or `call`.
    pub source: &'static str,
    pub limit: Option<f64>,
    pub usage: Option<f64>,
    pub remaining: Option<f64>,
}

impl Report {
    fn bare(state: KeyState, source: &'static str) -> Self {
        Self {
            state,
            source,
            limit: None,
            usage: None,
            remaining: None,
        }
    }
}

/// How long a probe waits for the provider before the key counts as `unreachable`.
const PROBE_TIMEOUT: Duration = Duration::from_secs(20);
/// A refused call reports its state at most this often: a burst of refusals is one report.
const REFUSAL_EVERY: Duration = Duration::from_secs(60);

/// What one answer of `GET {base}/key` says. OpenRouter answers `data.limit` (`null` for a key
/// without one), `data.usage` and `data.limit_remaining`, in its credits.
pub fn read_key_answer(status: u16, body: &[u8]) -> Report {
    match status {
        200 => {
            let Some(data) = serde_json::from_slice::<Value>(body)
                .ok()
                .and_then(|answer| answer.get("data").cloned())
                .filter(Value::is_object)
            else {
                return Report::bare(KeyState::Unreachable, "probe");
            };
            Report {
                state: KeyState::Valid,
                source: "probe",
                limit: data.get("limit").and_then(Value::as_f64),
                usage: data.get("usage").and_then(Value::as_f64),
                remaining: data.get("limit_remaining").and_then(Value::as_f64),
            }
        }
        401 => Report::bare(KeyState::Invalid, "probe"),
        402 => Report::bare(KeyState::OutOfCredit, "probe"),
        _ => Report::bare(KeyState::Unreachable, "probe"),
    }
}

/// What a refused model call says about the key: `401` is a key the provider does not take,
/// `402` one without credit; any other status says nothing about the key.
pub fn of_refusal(status: u16) -> Option<KeyState> {
    match status {
        401 => Some(KeyState::Invalid),
        402 => Some(KeyState::OutOfCredit),
        _ => None,
    }
}

/// The account's balance from one answer of `GET {base}/credits`: OpenRouter's
/// `data.total_credits − data.total_usage`, or nothing when the answer says no such thing.
pub fn read_credits_answer(status: u16, body: &[u8]) -> Option<f64> {
    if status != 200 {
        return None;
    }
    let data = serde_json::from_slice::<Value>(body)
        .ok()?
        .get("data")?
        .clone();
    let total = data.get("total_credits")?.as_f64()?;
    let used = data.get("total_usage")?.as_f64()?;
    Some((total - used).max(0.0))
}

/// What is left to spend: the key's remaining limit, or the account's balance when that is less.
/// A key with credit left under its own limit stops all the same when the account is empty: on
/// dev the key read 45.80 of 50 while the account held 6.01 (T-3065).
pub fn with_balance(mut report: Report, balance: Option<f64>) -> Report {
    if report.state == KeyState::Valid {
        report.remaining = match (report.remaining, balance) {
            (Some(key), Some(account)) => Some(key.min(account)),
            (key, account) => key.or(account),
        };
    }
    report
}

/// Whether this proxy has a key endpoint to probe: OpenRouter's, and the probe not turned off.
pub fn probes(config: &Config) -> bool {
    config.model_probe_secs > 0
        && config
            .model_base
            .host_str()
            .is_some_and(|host| host == "openrouter.ai" || host.ends_with(".openrouter.ai"))
}

/// Asks the provider about the key, without a completion.
pub async fn probe(state: &ProxyState) -> Report {
    let report = match ask(state, "key").await {
        Some((status, body)) => read_key_answer(status, &body),
        None => return Report::bare(KeyState::Unreachable, "probe"),
    };
    if report.state != KeyState::Valid {
        return report;
    }
    let balance = ask(state, "credits")
        .await
        .and_then(|(status, body)| read_credits_answer(status, &body));
    with_balance(report, balance)
}

/// One GET of the provider's `path` with the key: its status and body, or nothing when the
/// provider did not answer.
async fn ask(state: &ProxyState, path: &str) -> Option<(u16, axum::body::Bytes)> {
    let url = format!(
        "{}/{path}",
        state.config.model_base.as_str().trim_end_matches('/')
    );
    let response = state
        .http
        .get(url)
        .bearer_auth(state.credentials.get_model_key())
        .timeout(PROBE_TIMEOUT)
        .send()
        .await
        .ok()?;
    let status = response.status().as_u16();
    Some((status, response.bytes().await.unwrap_or_default()))
}

/// Probes the key every `JC_MODEL_PROBE_SECS` and reports each answer, for as long as the proxy
/// runs; nothing when [`probes`] says there is nothing to ask.
pub async fn watch(state: ProxyState) {
    if !probes(&state.config) {
        return;
    }
    let mut every = tokio::time::interval(Duration::from_secs(state.config.model_probe_secs));
    loop {
        every.tick().await;
        let report = probe(&state).await;
        tracing::info!(
            state = ?report.state,
            limit = ?report.limit,
            remaining = ?report.remaining,
            "the model key, as the provider describes it"
        );
        send(&state, report).await;
    }
}

/// A model call the provider refused: a `401` or `402` is reported as the key's state, at most
/// once a minute for the same state.
pub fn refused(state: &ProxyState, status: u16) {
    let Some(key) = of_refusal(status) else {
        return;
    };
    static LAST: Mutex<Option<(KeyState, Instant)>> = Mutex::new(None);
    {
        let mut last = LAST.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if matches!(*last, Some((said, at)) if said == key && at.elapsed() < REFUSAL_EVERY) {
            return;
        }
        *last = Some((key, Instant::now()));
    }
    tracing::warn!(state = ?key, "the model provider refused the key");
    let state = state.clone();
    tokio::spawn(async move { send(&state, Report::bare(key, "call")).await });
}

/// Sends one report to the Portal's internal listener with this proxy's own token. A report that
/// cannot be sent is a log line: the next probe or refusal says it again.
async fn send(state: &ProxyState, report: Report) {
    let Ok(bearer) = state.credentials.get_portal_token().await else {
        tracing::warn!("no token to report the model key's state with");
        return;
    };
    let mut url = state.config.portal_base.clone();
    url.set_path("internal/model-key");
    let sent = state
        .http
        .post(url)
        .bearer_auth(bearer)
        .json(&report)
        .send()
        .await;
    match sent {
        Ok(response) if response.status().is_success() => {}
        Ok(response) => {
            tracing::warn!(status = %response.status(), "the Portal refused the model key's state");
        }
        Err(err) => tracing::warn!(error = %err, "the model key's state did not reach the Portal"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_key_answer_with_a_limit_is_valid_with_its_credits() {
        let body = br#"{"data":{"label":"sk-or-v1-abc...","limit":10,"usage":2.41,"limit_remaining":7.59,"is_free_tier":false}}"#;
        assert_eq!(
            read_key_answer(200, body),
            Report {
                state: KeyState::Valid,
                source: "probe",
                limit: Some(10.0),
                usage: Some(2.41),
                remaining: Some(7.59),
            }
        );
    }

    #[test]
    fn a_key_without_a_limit_is_valid_with_no_limit_and_no_remaining() {
        let body = br#"{"data":{"limit":null,"usage":0.5,"limit_remaining":null}}"#;
        let report = read_key_answer(200, body);
        assert_eq!(report.state, KeyState::Valid);
        assert_eq!((report.limit, report.remaining), (None, None));
        assert_eq!(report.usage, Some(0.5));
    }

    #[test]
    fn what_is_left_is_the_smaller_of_the_keys_limit_and_the_accounts_balance() {
        let body = br#"{"data":{"total_credits":109.1632707,"total_usage":103.148872267}}"#;
        let balance = read_credits_answer(200, body).expect("a balance");
        assert!((balance - 6.0144).abs() < 0.001, "{balance}");
        assert_eq!(read_credits_answer(403, body), None);
        assert_eq!(read_credits_answer(200, br#"{"data":{}}"#), None);

        let key = read_key_answer(
            200,
            br#"{"data":{"limit":50,"usage":4.2,"limit_remaining":45.8}}"#,
        );
        let spendable = with_balance(key.clone(), Some(balance));
        assert_eq!(spendable.remaining, Some(balance));
        assert_eq!(spendable.limit, Some(50.0));
        // No balance known: the key's own figure stands. No limit: the balance is what is left.
        assert_eq!(with_balance(key, None).remaining, Some(45.8));
        let unlimited = read_key_answer(200, br#"{"data":{"limit":null,"usage":1}}"#);
        assert_eq!(with_balance(unlimited, Some(3.0)).remaining, Some(3.0));
        // A refused key keeps no figures.
        let dead = read_key_answer(401, b"");
        assert_eq!(with_balance(dead, Some(3.0)).remaining, None);
    }

    #[test]
    fn refusals_and_nonsense_say_what_they_say_about_the_key() {
        assert_eq!(read_key_answer(401, b"").state, KeyState::Invalid);
        assert_eq!(read_key_answer(402, b"").state, KeyState::OutOfCredit);
        assert_eq!(read_key_answer(503, b"").state, KeyState::Unreachable);
        assert_eq!(read_key_answer(200, b"<html>").state, KeyState::Unreachable);
        assert_eq!(
            read_key_answer(200, br#"{"data":[]}"#).state,
            KeyState::Unreachable
        );
        assert_eq!(of_refusal(401), Some(KeyState::Invalid));
        assert_eq!(of_refusal(402), Some(KeyState::OutOfCredit));
        assert_eq!(of_refusal(429), None);
    }

    #[test]
    fn the_report_names_its_state_in_the_wire_spelling() {
        let wire = serde_json::to_value(Report::bare(KeyState::OutOfCredit, "call")).expect("json");
        assert_eq!(
            wire,
            serde_json::json!({ "state": "out_of_credit", "source": "call", "limit": null, "usage": null, "remaining": null })
        );
    }

    #[test]
    fn only_openrouter_is_probed_and_zero_turns_the_probe_off() {
        let config = |base: &str, secs: &str| {
            let (base, secs) = (base.to_owned(), secs.to_owned());
            Config::from_lookup(move |key| match key {
                "JC_MODEL_BASE" => Some(base.clone()),
                "JC_MODEL_PROBE_SECS" => Some(secs.clone()),
                "JC_MODEL_KEY" | "JC_FORGE_TOKEN" => Some("x".to_owned()),
                _ => None,
            })
            .expect("a configuration")
        };
        assert!(probes(&config("https://openrouter.ai/api/v1", "900")));
        assert!(!probes(&config("https://openrouter.ai/api/v1", "0")));
        assert!(!probes(&config("https://api.anthropic.com", "900")));
        assert!(!probes(&config(
            "https://openrouter.ai.evil.example/api/v1",
            "900"
        )));
    }
}
