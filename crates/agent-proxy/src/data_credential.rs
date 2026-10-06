//! A run's short-lived data credential (ADR-N-038 decision 6, T-3134).
//!
//! The editing agent tests an App's functions, and `jc-functions` runs them with no token of the
//! run's person. The Portal asks this proxy for a credential of the run instead; `jc-functions`
//! sends the function's data calls here with it, and they go exactly as the run's own do: its
//! slugs, its write rule, its person's delegated token. The credential opens the data routes and
//! nothing else, lives five minutes, and is kept here only as a digest.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use argon2::password_hash::rand_core::{OsRng, RngCore};
use axum::http::HeaderMap;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use sha2::{Digest, Sha256};

/// How a data credential is told apart from a run ticket in `Authorization: Bearer`.
pub const PREFIX: &str = "jcd_";
/// How long one lives: long enough for a function call, short enough to be worth nothing later.
pub const TTL: Duration = Duration::from_secs(300);
/// Live credentials one run may hold; the oldest makes room for a new one.
pub const MAX_PER_RUN: usize = 8;

struct Entry {
    digest: [u8; 32],
    expires: Instant,
}

/// The live credentials, by run, as digests.
#[derive(Clone, Default)]
pub struct DataCredentials {
    inner: Arc<Mutex<HashMap<String, Vec<Entry>>>>,
}

fn digest(secret: &str) -> [u8; 32] {
    Sha256::digest(secret.as_bytes()).into()
}

/// Equal in time that does not depend on where the two first differ.
fn same(a: &[u8; 32], b: &[u8; 32]) -> bool {
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

impl DataCredentials {
    /// A new credential of `run_id`, as the bearer token `jcd_<run>.<secret>`.
    pub fn mint(&self, run_id: &str) -> String {
        let mut bytes = [0u8; 32];
        OsRng.fill_bytes(&mut bytes);
        let secret = URL_SAFE_NO_PAD.encode(bytes);
        let now = Instant::now();
        let mut live = self
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        // Every run's expired entries go on each mint, so the map holds live credentials only.
        live.retain(|_, entries| {
            entries.retain(|entry| entry.expires > now);
            !entries.is_empty()
        });
        let entries = live.entry(run_id.to_owned()).or_default();
        if entries.len() >= MAX_PER_RUN {
            entries.remove(0);
        }
        entries.push(Entry {
            digest: digest(&secret),
            expires: now + TTL,
        });
        format!("{PREFIX}{run_id}.{secret}")
    }

    /// Whether `secret` is a live credential of `run_id`.
    pub fn verify(&self, run_id: &str, secret: &str) -> bool {
        let presented = digest(secret);
        let now = Instant::now();
        let live = self
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        live.get(run_id).is_some_and(|entries| {
            entries
                .iter()
                .any(|entry| entry.expires > now && same(&entry.digest, &presented))
        })
    }

    /// Drops every credential of a run, as its end does.
    pub fn revoke(&self, run_id: &str) {
        let mut live = self
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        live.remove(run_id);
    }
}

/// The run and secret of an `Authorization: Bearer jcd_<run>.<secret>`, when that is what came.
pub fn presented(headers: &HeaderMap) -> Option<(String, String)> {
    let bearer = headers.get("authorization")?.to_str().ok()?.trim();
    let token = bearer.strip_prefix("Bearer ")?.trim();
    let (run, secret) = token.strip_prefix(PREFIX)?.split_once('.')?;
    if run.is_empty() || secret.is_empty() {
        return None;
    }
    Some((run.to_owned(), secret.to_owned()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    fn bearer(token: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(
            "authorization",
            HeaderValue::from_str(&format!("Bearer {token}")).expect("a header"),
        );
        headers
    }

    #[test]
    fn a_minted_credential_verifies_for_its_run_alone() {
        let store = DataCredentials::default();
        let token = store.mint("run-1");
        let (run, secret) = presented(&bearer(&token)).expect("a data credential");
        assert_eq!(run, "run-1");
        assert!(store.verify("run-1", &secret));
        assert!(!store.verify("run-2", &secret), "another run's id");
        assert!(!store.verify("run-1", "guessed"), "a secret never minted");
        assert_ne!(store.mint("run-1"), token, "every mint is new");
    }

    #[test]
    fn a_run_holds_a_bounded_number_and_its_end_drops_them() {
        let store = DataCredentials::default();
        let first = presented(&bearer(&store.mint("run-1"))).expect("parsed").1;
        for _ in 0..MAX_PER_RUN {
            store.mint("run-1");
        }
        assert!(!store.verify("run-1", &first), "the oldest made room");
        let last = presented(&bearer(&store.mint("run-1"))).expect("parsed").1;
        assert!(store.verify("run-1", &last));
        store.revoke("run-1");
        assert!(!store.verify("run-1", &last));
    }

    #[test]
    fn only_the_data_credential_shape_is_read() {
        for header in [
            "Bearer jcr_run-1.ticket",
            "Bearer jcd_.secret",
            "Bearer jcd_run-1.",
            "Bearer jcd_run-1",
            "Basic jcd_run-1.x",
        ] {
            let mut headers = HeaderMap::new();
            headers.insert("authorization", HeaderValue::from_static(header));
            assert!(presented(&headers).is_none(), "{header}");
        }
        assert!(presented(&HeaderMap::new()).is_none());
    }
}
