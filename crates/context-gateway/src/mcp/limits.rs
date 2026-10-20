//! The broker's page defaults and ceilings, as its `/q/health` publishes them (T-4804): the
//! hub and the named servers advertise them in the `limit` and `lastN` schemas and the read
//! tools' descriptions, the numbers the per-Endpoint server forwards from the broker itself.

use crate::proxy::Broker;
use axum::body::Body;
use axum::http::{HeaderMap, Method};
use serde_json::Value;
use std::sync::OnceLock;
use std::time::Duration;

/// What one call reads at most and by default: Entities per page (`limit`) and instances of
/// one Attribute per history answer (`lastN`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// Entities a page answers when the call names no `limit`.
    pub default_limit: u64,
    /// The largest `limit`.
    pub max_limit: u64,
    /// Instances of one Attribute a history answer serves when the call names no `lastN`.
    pub temporal_default: u64,
    /// The largest `lastN`.
    pub temporal_max: u64,
}

/// The broker's own defaults (Antares `ANTARES_DEFAULT_LIMIT`, `ANTARES_MAX_LIMIT`,
/// `ANTARES_TEMPORAL_DEFAULT_INSTANCES`, `ANTARES_TEMPORAL_INSTANCE_LIMIT`), until it answers.
impl Default for Limits {
    fn default() -> Self {
        Self {
            default_limit: 100,
            max_limit: 1000,
            temporal_default: 100,
            temporal_max: 1000,
        }
    }
}

static LIMITS: OnceLock<Limits> = OnceLock::new();

impl Limits {
    /// The JSON schema of `limit` (`temporal` false) or `lastN` (`temporal` true).
    pub fn schema(self, temporal: bool) -> Value {
        let (max, default) = if temporal {
            (self.temporal_max, self.temporal_default)
        } else {
            (self.max_limit, self.default_limit)
        };
        serde_json::json!({"type": "integer", "minimum": 1, "maximum": max, "default": default})
    }
}

/// The page part of `tools::anonymous_bounds`: `lastN` within the broker's maximum, its default
/// when absent unless the read is `averaged`, and `limit` within the broker's page maximum.
pub fn anonymous_page(
    arguments: &mut serde_json::Map<String, Value>,
    averaged: bool,
) -> Result<(), String> {
    let limits = current();
    match arguments.get("lastN") {
        None if averaged => {}
        None => {
            arguments.insert("lastN".to_owned(), Value::from(limits.temporal_default));
        }
        Some(n)
            if n.as_u64()
                .is_some_and(|n| (1..=limits.temporal_max).contains(&n)) => {}
        Some(_) => {
            return Err(format!(
                "`lastN` without a token is 1 to {}",
                limits.temporal_max
            ))
        }
    }
    match arguments.get("limit") {
        Some(limit)
            if !limit
                .as_u64()
                .is_some_and(|n| (1..=limits.max_limit).contains(&n)) =>
        {
            Err(format!("`limit` is 1 to {}", limits.max_limit))
        }
        _ => Ok(()),
    }
}

/// The broker's limits once read, its defaults before.
///
/// ponytail: read once at start, as the broker reads its settings; a broker redeployed with
/// other numbers shows them after the gateway's next restart.
pub fn current() -> Limits {
    LIMITS.get().copied().unwrap_or_default()
}

/// The limits in a `/q/health` body; a member the broker does not publish keeps its default,
/// and a default is never above its ceiling.
pub fn parse(health: &Value) -> Option<Limits> {
    let limits = health.get("limits")?;
    let base = Limits::default();
    let number = |name: &str, fallback: u64| {
        limits
            .get(name)
            .and_then(Value::as_u64)
            .filter(|n| *n > 0)
            .unwrap_or(fallback)
    };
    let max_limit = number("maxLimit", base.max_limit);
    let temporal_max = number("temporalInstanceLimit", base.temporal_max);
    Some(Limits {
        default_limit: number("defaultLimit", base.default_limit).min(max_limit),
        max_limit,
        temporal_default: number("temporalDefaultInstances", base.temporal_default)
            .min(temporal_max),
        temporal_max,
    })
}

/// The most bytes of a `/q/health` answer read.
const HEALTH_BYTES: usize = 64 * 1024;

/// Reads the broker's limits from its `/q/health`.
pub async fn read(broker: &Broker) -> Result<Limits, String> {
    let answer = broker
        .send(Method::GET, "/q/health", HeaderMap::new(), Body::empty())
        .await
        .map_err(|error| error.to_string())?;
    if !answer.status().is_success() {
        return Err(format!("the broker's health answered {}", answer.status()));
    }
    let bytes = axum::body::to_bytes(answer.into_body(), HEALTH_BYTES)
        .await
        .map_err(|error| error.to_string())?;
    let health: Value = serde_json::from_slice(&bytes).map_err(|error| error.to_string())?;
    parse(&health).ok_or_else(|| "the broker's health publishes no limits".to_owned())
}

/// Reads the broker's limits at start, again every 10 s while it does not answer, at most an
/// hour; the defaults hold meanwhile.
pub async fn follow(broker: Broker) {
    for _ in 0..360 {
        match read(&broker).await {
            Ok(limits) => {
                let _ = LIMITS.set(limits);
                tracing::info!(?limits, "the broker's page limits are read");
                return;
            }
            Err(reason) => {
                tracing::warn!(%reason, "the broker's page limits are not read yet");
                tokio::time::sleep(Duration::from_secs(10)).await;
            }
        }
    }
}

/// The sentence the read tools add to their description, from these limits.
pub fn note(tool: &str, limits: Limits) -> Option<String> {
    let entities = || {
        format!(
            "Default {} Entities per call, maximum {} (limit); more pages with nextCursor.",
            thousands(limits.default_limit),
            thousands(limits.max_limit)
        )
    };
    let history = || {
        format!(
            "Default {} values per Attribute per call, maximum {} (lastN).",
            thousands(limits.temporal_default),
            thousands(limits.temporal_max)
        )
    };
    match tool {
        "query_entities" | "batch_query" => Some(entities()),
        "query_temporal" | "retrieve_temporal" | "batch_query_temporal" => Some(history()),
        _ => None,
    }
}

/// `1000` as `1,000`.
fn thousands(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn health_limits_are_read_and_a_default_stays_under_its_ceiling() {
        let health = json!({"limits": {"maxLimit": 500, "defaultLimit": 900,
            "temporalInstanceLimit": 9, "temporalDefaultInstances": 100}});
        assert_eq!(
            parse(&health),
            Some(Limits {
                default_limit: 500,
                max_limit: 500,
                temporal_default: 9,
                temporal_max: 9
            })
        );
        // a broker that publishes only its ceilings: the defaults are 100
        let older = json!({"limits": {"maxLimit": 1000, "temporalInstanceLimit": 1000}});
        assert_eq!(parse(&older), Some(Limits::default()));
        assert_eq!(parse(&json!({"status": "UP"})), None);
    }

    #[test]
    fn the_notes_name_default_and_maximum() {
        let l = Limits::default();
        assert_eq!(
            note("query_entities", l).as_deref(),
            Some(
                "Default 100 Entities per call, maximum 1,000 (limit); more pages with nextCursor."
            )
        );
        assert_eq!(
            note("retrieve_temporal", l).as_deref(),
            Some("Default 100 values per Attribute per call, maximum 1,000 (lastN).")
        );
        assert_eq!(note("get_entity", l), None);
        assert_eq!(thousands(12_345_678), "12,345,678");
    }
}
