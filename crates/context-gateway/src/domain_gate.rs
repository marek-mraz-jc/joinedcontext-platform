//! Writes wait for a verified domain (PF-41, Architecture/03 §3, T-2572).
//!
//! The Portal checks that the Organization owns the domain it declares and keeps the state in its
//! database; the gateway reads that state from the Portal's internal listener every ten seconds
//! and, under `JC_GATEWAY_DOMAIN_VERIFICATION=enforce`, refuses a write to a space unless the
//! Organization the gateway serves is `verified`. `report`, the default, refuses nothing. The gate
//! fails closed: before the first answer, when the Portal cannot be reached, and when its list
//! does not name the Organization, the state is unknown and a write is refused. Reads never are.

use std::sync::Arc;
use std::time::{Duration, Instant};

use arc_swap::ArcSwapOption;
use jc_core::ProblemDetails;
use serde::Deserialize;

use crate::previews::WorkloadToken;

/// How often the Portal is asked, the same pace as the preview list (CC-78).
const INTERVAL: Duration = Duration::from_secs(10);
/// How old the last answer may be before the state counts as unknown: six missed polls. One
/// failed fetch does not stop every write; a Portal gone for a minute does.
pub const MAX_AGE: Duration = Duration::from_secs(60);

/// Whether an unverified domain only shows, or also stops writes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// The state is recorded and shown; nothing is refused (the default).
    Report,
    /// A write to a space of an Organization that is not `verified` answers `403`.
    Enforce,
}

impl Mode {
    /// `JC_GATEWAY_DOMAIN_VERIFICATION`: unset or empty is `report`; anything but the two
    /// words is an error, so a typo never runs as the weaker setting.
    pub fn parse(value: Option<&str>) -> Result<Self, String> {
        match value.map(str::trim) {
            None | Some("") | Some("report") => Ok(Self::Report),
            Some("enforce") => Ok(Self::Enforce),
            Some(other) => Err(format!("`{other}` is neither `report` nor `enforce`")),
        }
    }
}

/// One Organization as the Portal lists it.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Item {
    /// The Organization's name.
    pub organization: String,
    /// The domain it declares, the `{orgDomain}` of its entity ids.
    pub domain: String,
    /// `pending`, `verified` or `failed`; anything but `verified` refuses under `enforce`.
    pub state: String,
}

#[derive(Deserialize)]
struct List {
    items: Vec<Item>,
}

/// The gate: its mode, and the last list the Portal answered with when it last confirmed it.
pub struct DomainGate {
    mode: Mode,
    states: ArcSwapOption<(Instant, Vec<Item>)>,
}

impl DomainGate {
    /// A gate that knows no state yet.
    pub fn new(mode: Mode) -> Self {
        Self {
            mode,
            states: ArcSwapOption::empty(),
        }
    }

    /// Takes the Portal's latest list, confirmed at `at`.
    pub fn replace_at(&self, items: Vec<Item>, at: Instant) {
        self.states.store(Some(Arc::new((at, items))));
    }

    /// The Portal answered `304`: the list stands, confirmed now.
    fn confirm(&self) {
        if let Some(current) = self.states.load_full() {
            self.replace_at(current.1.clone(), Instant::now());
        }
    }

    /// The refusal of a write to a space of the Organization whose domain is `org_domain`, or
    /// `None` when the write may go on.
    pub fn refusal(&self, org_domain: &str) -> Option<ProblemDetails> {
        self.refusal_at(org_domain, Instant::now())
    }

    /// [`Self::refusal`] at `now`.
    pub fn refusal_at(&self, org_domain: &str, now: Instant) -> Option<ProblemDetails> {
        if self.mode == Mode::Report {
            return None;
        }
        let states = self.states.load();
        let item = states
            .as_deref()
            .filter(|(at, _)| now.saturating_duration_since(*at) <= MAX_AGE)
            .and_then(|(_, items)| items.iter().find(|item| item.domain == org_domain));
        let detail = match item {
            Some(item) if item.state == "verified" => return None,
            Some(item) => format!(
                "writes to the spaces of the organization {} are refused until its domain {} is \
                 verified (it is {}): publish the TXT record shown on the Portal's Access page, \
                 and the next check picks it up (PF-41)",
                item.organization, item.domain, item.state
            ),
            None => format!(
                "writes to the spaces of {org_domain} are refused: this gateway does not know \
                 yet whether the organization's domain is verified, and it refuses rather than \
                 guesses (PF-41)"
            ),
        };
        Some(ProblemDetails::new(403, "domain-not-verified", "Forbidden").with_detail(detail))
    }

    /// Asks the Portal for the list every ten seconds, with the gateway's own token (PF-46).
    /// A `304` confirms the list; a refusal, an error or an unreadable answer leaves it to age,
    /// so a Portal that stopped answering stops writes after [`MAX_AGE`] rather than freezing the
    /// last state.
    pub async fn follow(self: Arc<Self>, url: String, token: Option<Arc<WorkloadToken>>) {
        let client = match reqwest::Client::builder()
            .timeout(Duration::from_secs(20))
            .build()
        {
            Ok(client) => client,
            Err(error) => {
                tracing::error!(%error, "no HTTP client for the domain verifications");
                return;
            }
        };
        let mut ticker = tokio::time::interval(INTERVAL);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut etag: Option<String> = None;
        loop {
            ticker.tick().await;
            let mut request = client.get(&url);
            if let Some(token) = token.as_ref() {
                match token.get().await {
                    Ok(bearer) => request = request.bearer_auth(bearer),
                    Err(error) => {
                        tracing::warn!(%error, "no token for the Portal's domain verifications");
                        continue;
                    }
                }
            }
            if let Some(tag) = etag.as_deref() {
                request = request.header(reqwest::header::IF_NONE_MATCH, tag);
            }
            match request.send().await {
                Ok(response) if response.status() == reqwest::StatusCode::NOT_MODIFIED => {
                    self.confirm()
                }
                Ok(response) if response.status().is_success() => {
                    let tag = response
                        .headers()
                        .get(reqwest::header::ETAG)
                        .and_then(|value| value.to_str().ok())
                        .map(str::to_owned);
                    match response.json::<List>().await {
                        Ok(list) => {
                            self.replace_at(list.items, Instant::now());
                            etag = tag;
                        }
                        Err(error) => {
                            tracing::warn!(%error, "the domain verifications are not readable");
                            etag = None;
                        }
                    }
                }
                Ok(response) => {
                    tracing::warn!(status = %response.status(), "the Portal did not list the domain verifications");
                    etag = None;
                }
                Err(error) => {
                    tracing::warn!(%error, "the Portal did not list the domain verifications");
                    etag = None;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(state: &str) -> Item {
        Item {
            organization: "hel".into(),
            domain: "hel.fi".into(),
            state: state.into(),
        }
    }

    /// PF-41: `report` refuses nothing whatever the state, and the setting admits only its two
    /// words, so a typo never runs as the weaker one.
    #[test]
    fn report_refuses_nothing_and_the_setting_is_one_of_two_words() {
        let gate = DomainGate::new(Mode::Report);
        assert!(gate.refusal("hel.fi").is_none());
        gate.replace_at(vec![item("failed")], Instant::now());
        assert!(gate.refusal("hel.fi").is_none());

        assert_eq!(Mode::parse(None), Ok(Mode::Report));
        assert_eq!(Mode::parse(Some("")), Ok(Mode::Report));
        assert_eq!(Mode::parse(Some("enforce")), Ok(Mode::Enforce));
        assert!(Mode::parse(Some("enforced")).is_err());
        assert!(Mode::parse(Some("off")).is_err());
    }

    /// PF-41: under `enforce` only a verified domain lets a write through; pending, failed, an
    /// Organization the list does not name, no list yet and a list older than a minute are all
    /// refused, with the Organization named.
    #[test]
    fn enforce_lets_a_write_through_only_for_a_verified_domain_and_fails_closed() {
        let gate = DomainGate::new(Mode::Enforce);
        let now = Instant::now();
        let unknown = gate
            .refusal_at("hel.fi", now)
            .expect("refused before the first answer");
        assert_eq!(unknown.status, 403);
        assert!(unknown
            .detail
            .as_deref()
            .unwrap_or_default()
            .contains("hel.fi"));

        gate.replace_at(vec![item("verified")], now);
        assert!(gate.refusal_at("hel.fi", now).is_none());
        assert!(
            gate.refusal_at("espoo.fi", now).is_some(),
            "another domain is not this one"
        );

        for state in ["pending", "failed"] {
            gate.replace_at(vec![item(state)], now);
            let refused = gate.refusal_at("hel.fi", now).expect("refused");
            assert_eq!(refused.status, 403);
            let detail = refused.detail.unwrap_or_default();
            assert!(
                detail.contains("organization hel") && detail.contains(state),
                "{detail}"
            );
            assert!(refused.type_uri.ends_with("/domain-not-verified"));
        }

        // A verified answer holds through a missed poll and stops holding after a minute.
        gate.replace_at(vec![item("verified")], now);
        assert!(gate
            .refusal_at("hel.fi", now + Duration::from_secs(25))
            .is_none());
        assert!(
            gate.refusal_at("hel.fi", now + MAX_AGE + Duration::from_secs(1))
                .is_some(),
            "a Portal gone for a minute stops writes"
        );
    }
}
