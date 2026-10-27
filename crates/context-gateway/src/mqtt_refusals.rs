//! The CONNECTs the MQTT brokers' check refused, kept for the Portal's test-publish watch
//! (T-5534, API/01 §44b).
//!
//! Each refusal of a broker this repository declares counts as
//! `jc_mqtt_auth_refused_total{broker,reason}` and joins that broker's newest
//! [`KEPT_PER_BROKER`] refusals in memory: the time, the reason code and the broker's own
//! ServiceAccount the client named, nothing else. The token, the client id and the address never
//! enter the record. `GET /internal/mqtt/refusals/{broker}` reads them, on the internal listener,
//! for the Portal's mesh identity alone. A restart forgets them; the counter is the history.

use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use chrono::{DateTime, Utc};
use serde::Serialize;
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};

use crate::auth::accounts::ServiceAccounts;
use crate::mqtt::{Broker, MESH_IDENTITY};

/// The refusals kept per broker, newest first.
pub const KEPT_PER_BROKER: usize = 50;

/// The counter of refused CONNECTs, by broker release name and [`Reason`].
pub const REFUSED: &str = "jc_mqtt_auth_refused_total";

/// Why a CONNECT was refused, as a code the Portal words for the person.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Reason {
    /// No password, and not the runner or the capture from one of their pods.
    NoToken,
    /// The gateway has no realm to verify a token with.
    NoRealm,
    /// The token is past its `exp`, or not yet valid.
    Expired,
    /// The token is bound to another broker, or to none.
    WrongAudience,
    /// Not a token of this realm: malformed, an unknown key, a bad signature or another issuer.
    InvalidToken,
    /// The username or client id is not the token's client, or the token names none.
    WrongClient,
    /// The token's client is no ServiceAccount.
    UnknownAccount,
    /// The ServiceAccount belongs to another project.
    OtherProject,
    /// The ServiceAccount holds no role of this broker.
    NoRole,
    /// The runner or the capture has nothing to subscribe to on this broker.
    NoSubscription,
    /// The device's address lies outside the broker's `allowedSources` (PF-129).
    SourceAddress,
}

impl Reason {
    /// The code, as the counter's `reason` label and the record's `reason` carry it.
    pub fn code(self) -> &'static str {
        match self {
            Self::NoToken => "no_token",
            Self::NoRealm => "no_realm",
            Self::Expired => "expired",
            Self::WrongAudience => "wrong_audience",
            Self::InvalidToken => "invalid_token",
            Self::WrongClient => "wrong_client",
            Self::UnknownAccount => "unknown_account",
            Self::OtherProject => "other_project",
            Self::NoRole => "no_role",
            Self::NoSubscription => "no_subscription",
            Self::SourceAddress => "source_address",
        }
    }
}

/// One refused CONNECT.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Refusal {
    /// When the gateway refused it.
    pub at: DateTime<Utc>,
    /// Why.
    pub reason: Reason,
    /// The broker's own ServiceAccount the username named (`{project}-{account}`), when it names
    /// one of the broker's roles; `null` for any other name, which the client chose freely.
    pub account: Option<String>,
}

/// The ServiceAccount of `broker`'s project that `username` names (`{project}-{account}`): one
/// the repository declares, or one a deprecated role list names; `None` for any other name,
/// which the client chose freely.
pub fn account_of(broker: &Broker, accounts: &ServiceAccounts, username: &str) -> Option<String> {
    if let Some(account) = accounts
        .resolve(username)
        .filter(|account| account.project == broker.project)
    {
        return Some(account.name.clone());
    }
    let account = username
        .strip_prefix(broker.project.as_str())?
        .strip_prefix('-')?;
    broker
        .roles
        .iter()
        .any(|role| role.service_accounts.iter().any(|name| name == account))
        .then(|| account.to_owned())
}

/// The newest refusals of every broker.
#[derive(Debug, Default)]
pub struct Refusals {
    by_broker: Mutex<HashMap<String, VecDeque<Refusal>>>,
}

impl Refusals {
    /// Counts `refusal` for `release` and keeps it, the oldest dropped past the limit. Only a
    /// broker the repository declares is passed in, so the map holds one entry per broker.
    pub fn record(&self, release: &str, refusal: Refusal) {
        metrics::counter!(REFUSED, "broker" => release.to_owned(), "reason" => refusal.reason.code())
            .increment(1);
        let mut by_broker = self
            .by_broker
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let kept = by_broker.entry(release.to_owned()).or_default();
        kept.push_front(refusal);
        kept.truncate(KEPT_PER_BROKER);
    }

    /// The kept refusals of `release`, newest first.
    pub fn of(&self, release: &str) -> Vec<Refusal> {
        self.by_broker
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(release)
            .map(|kept| kept.iter().cloned().collect())
            .unwrap_or_default()
    }
}

/// The answer of the read route.
#[derive(Debug, Serialize)]
pub struct Kept {
    /// Newest first.
    pub refusals: Vec<Refusal>,
}

/// Whether the mesh identity in `headers` is the Portal's: `{serviceaccount}.{namespace}` then
/// Linkerd's `.serviceaccount.identity.…`. No configured identity admits nobody.
pub fn is_portal(headers: &HeaderMap, portal: Option<&str>) -> bool {
    let (Some(portal), Some(caller)) = (
        portal,
        headers
            .get(MESH_IDENTITY)
            .and_then(|value| value.to_str().ok()),
    ) else {
        return false;
    };
    caller
        .strip_prefix(portal)
        .is_some_and(|rest| rest.starts_with(".serviceaccount.identity."))
}

/// What the read route needs: the record and the Portal's identity.
#[derive(Clone)]
pub struct Reader {
    /// The record the CONNECT check writes.
    pub refusals: Arc<Refusals>,
    /// The Portal's `{serviceaccount}.{namespace}`.
    pub portal_identity: Option<Arc<str>>,
}

/// `GET /internal/mqtt/refusals/{broker}`: the kept refusals of the broker named by its release
/// name, for the Portal's mesh identity alone; any other caller gets 403 and nothing.
pub async fn read(
    State(reader): State<Reader>,
    headers: HeaderMap,
    Path(broker): Path<String>,
) -> Response {
    if !is_portal(&headers, reader.portal_identity.as_deref()) {
        return StatusCode::FORBIDDEN.into_response();
    }
    Json(Kept {
        refusals: reader.refusals.of(&broker),
    })
    .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;
    use jc_core::kinds::mqtt_broker::MqttRole;

    fn broker() -> Broker {
        Broker {
            name: "cameras".into(),
            project: "ovzdusie".into(),
            roles: vec![MqttRole {
                name: "camera".into(),
                service_accounts: vec!["cam-north".into()],
                publish: vec!["cameras/${clientid}/#".into()],
                subscribe: Vec::new(),
                shared_topics: false,
                reason: None,
            }],
            runner_topics: Vec::new(),
            sources: Vec::new(),
            capture_topics: Vec::new(),
            messages_per_second: 10,
        }
    }

    fn refusal(minute: u32, reason: Reason) -> Refusal {
        Refusal {
            at: DateTime::parse_from_rfc3339(&format!("2026-10-24T10:{minute:02}:00Z"))
                .expect("a time")
                .with_timezone(&Utc),
            reason,
            account: None,
        }
    }

    #[test]
    fn a_username_names_an_account_only_of_the_brokers_own_roles() {
        let broker = broker();
        assert_eq!(
            account_of(&broker, &ServiceAccounts::new(), "ovzdusie-cam-north"),
            Some("cam-north".to_owned())
        );
        for other in [
            "ovzdusie-cam-south",
            "espoo-cam-north",
            "ovzdusiecam-north",
            "cam-north",
            "",
        ] {
            assert_eq!(
                account_of(&broker, &ServiceAccounts::new(), other),
                None,
                "{other}"
            );
        }
    }

    #[test]
    fn the_record_keeps_the_newest_fifty_per_broker_apart() {
        let refusals = Refusals::default();
        for minute in 0..55 {
            refusals.record("mqtt-ovzdusie-cameras", refusal(minute, Reason::Expired));
        }
        refusals.record("mqtt-ovzdusie-meters", refusal(0, Reason::NoRole));
        let kept = refusals.of("mqtt-ovzdusie-cameras");
        assert_eq!(kept.len(), KEPT_PER_BROKER);
        assert_eq!(kept.first(), Some(&refusal(54, Reason::Expired)));
        assert_eq!(kept.last(), Some(&refusal(5, Reason::Expired)));
        assert_eq!(
            refusals.of("mqtt-ovzdusie-meters"),
            [refusal(0, Reason::NoRole)]
        );
        assert!(refusals.of("mqtt-other").is_empty());
    }

    #[test]
    fn a_refusal_serializes_to_time_reason_and_account_only() {
        let mut kept = refusal(1, Reason::WrongAudience);
        kept.account = Some("cam-north".into());
        assert_eq!(
            serde_json::to_value(&kept).expect("serializes"),
            serde_json::json!({"at": "2026-10-24T10:01:00Z", "reason": "wrong_audience", "account": "cam-north"})
        );
    }

    #[test]
    fn only_the_portals_mesh_identity_reads() {
        let with = |identity: &str| {
            let mut headers = HeaderMap::new();
            headers.insert(
                MESH_IDENTITY,
                HeaderValue::from_str(identity).expect("a header"),
            );
            headers
        };
        let portal = "portal.portal.serviceaccount.identity.linkerd.cluster.local";
        assert!(is_portal(&with(portal), Some("portal.portal")));
        for other in [
            "mqtt-ovzdusie-cameras.jc-mqtt.serviceaccount.identity.linkerd.cluster.local",
            "portal.portal-evil.serviceaccount.identity.linkerd.cluster.local",
            "portal.portal",
            "evil-portal.portal.serviceaccount.identity.linkerd.cluster.local",
        ] {
            assert!(!is_portal(&with(other), Some("portal.portal")), "{other}");
        }
        assert!(!is_portal(&with(portal), None));
        assert!(!is_portal(&HeaderMap::new(), Some("portal.portal")));
    }
}
