//! The run tickets that verified, kept so a run's later calls skip Argon2 (F-PLT1-007).

use crate::runs::RunContext;
use argon2::{Argon2, PasswordHash, PasswordVerifier};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

/// How many Argon2 verifies run at once. Each takes ~19 MiB and tens of milliseconds, so a flood of
/// wrong tickets queues here instead of exhausting the proxy's memory (F-PLT1-007).
const ARGON2_AT_ONCE: usize = 4;

/// The tickets that verified, per run, so each later call of a run costs one SHA-256 instead of
/// one Argon2 verify (F-PLT1-007). An entry is bound to the stored hash it verified against: a
/// Portal that re-mints the ticket changes `ticket_hash`, and the entry no longer applies.
/// Per run: the stored hash a ticket verified against, and that ticket's SHA-256.
type Verified = HashMap<String, (String, [u8; 32])>;

#[derive(Clone)]
pub struct TicketCache {
    verified: Arc<Mutex<Verified>>,
    argon2: Arc<tokio::sync::Semaphore>,
    hashed: Arc<AtomicU64>,
}

impl Default for TicketCache {
    fn default() -> Self {
        Self {
            verified: Arc::default(),
            argon2: Arc::new(tokio::sync::Semaphore::new(ARGON2_AT_ONCE)),
            hashed: Arc::default(),
        }
    }
}

impl TicketCache {
    /// Whether `ticket` is the run's: a constant-time compare with the digest of the ticket that
    /// last verified against the same stored hash, else one Argon2 verify (at most
    /// `ARGON2_AT_ONCE` at once). `Err` is a stored hash that is no PHC string.
    pub async fn verify(
        &self,
        run: &RunContext,
        ticket: &str,
    ) -> Result<bool, argon2::password_hash::Error> {
        let digest: [u8; 32] = Sha256::digest(ticket.as_bytes()).into();
        let known = self
            .verified
            .lock()
            .ok()
            .and_then(|map| map.get(&run.id).cloned());
        if let Some((hash, seen)) = known {
            if hash == run.ticket_hash && digests_equal(&seen, &digest) {
                return Ok(true);
            }
        }
        let parsed = PasswordHash::new(&run.ticket_hash)?;
        let Ok(_permit) = self.argon2.acquire().await else {
            return Ok(false);
        };
        self.hashed.fetch_add(1, Ordering::Relaxed);
        if Argon2::default()
            .verify_password(ticket.as_bytes(), &parsed)
            .is_err()
        {
            return Ok(false);
        }
        if let Ok(mut map) = self.verified.lock() {
            map.insert(run.id.clone(), (run.ticket_hash.clone(), digest));
        }
        Ok(true)
    }

    /// Drops what is held for a run that ended.
    pub fn forget(&self, run_id: &str) {
        if let Ok(mut map) = self.verified.lock() {
            map.remove(run_id);
        }
    }
}

/// Equal digests, compared in time independent of where they differ.
fn digests_equal(a: &[u8; 32], b: &[u8; 32]) -> bool {
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[cfg(test)]
mod tests {
    use super::TicketCache;
    use argon2::password_hash::{rand_core::OsRng, PasswordHasher, SaltString};
    use std::sync::atomic::Ordering;

    fn run_with_ticket(ticket: &str) -> crate::runs::RunContext {
        let salt = SaltString::generate(&mut OsRng);
        let hash = argon2::Argon2::default()
            .hash_password(ticket.as_bytes(), &salt)
            .map(|hash| hash.to_string())
            .unwrap_or_default();
        serde_json::from_value(serde_json::json!({
            "id": "run-1", "project": "p", "appName": "a", "endpointSlug": "e",
            "allowsWrite": false, "branch": "b", "pathPrefix": "", "status": "building",
            "ticketHash": hash, "maxTokens": 1, "allowedHosts": [], "requestsPerMinute": 1,
            "maxResponseBytes": 1, "createdBy": "u", "modelName": "m"
        }))
        .expect("a run")
    }

    /// F-PLT1-007: the right ticket costs one Argon2 verify, its later calls none; a wrong ticket
    /// after it is still refused, and a re-minted ticket (a new stored hash) is verified again.
    #[tokio::test]
    async fn a_verified_ticket_skips_argon2_and_nothing_else_does() {
        let tickets = TicketCache::default();
        let run = run_with_ticket("right");
        assert_eq!(tickets.verify(&run, "right").await, Ok(true));
        assert_eq!(tickets.verify(&run, "right").await, Ok(true));
        assert_eq!(tickets.hashed.load(Ordering::Relaxed), 1);
        assert_eq!(tickets.verify(&run, "wrong").await, Ok(false));
        assert_eq!(tickets.hashed.load(Ordering::Relaxed), 2);
        let reminted = run_with_ticket("new");
        assert_eq!(tickets.verify(&reminted, "right").await, Ok(false));
        assert_eq!(tickets.verify(&reminted, "new").await, Ok(true));
        tickets.forget("run-1");
        assert_eq!(tickets.verify(&reminted, "new").await, Ok(true));
        assert_eq!(tickets.hashed.load(Ordering::Relaxed), 5);
    }
}
