//! API keys for the callers that cannot do OAuth (T-0155, PF-36, PF-37).
//!
//! A device or a legacy ETL job presents `Authorization: Bearer jc_{keyId}_{secret}`. The
//! platform never stores the secret: it stores an Argon2id hash of it, so a copy of the
//! database is not a set of working credentials. Verification is constant-time in the
//! comparison and deliberately slow in the hashing, which is the point of Argon2id.
//!
//! The key is one of three things a caller must satisfy: the secret has to verify, the key
//! has to be unexpired, and the caller's address has to be on the allow list the account
//! declared. Any of them failing is the same answer, so a probe learns nothing about which.

use argon2::password_hash::{PasswordHash, PasswordVerifier};
use argon2::Argon2;
use chrono::{DateTime, Utc};

/// The prefix every platform API key carries, so a key is recognisable in a log or a leak
/// scan before it is ever used (PF-37).
pub const KEY_PREFIX: &str = "jc_";

/// What the platform stores about one key. Never the secret (PF-36).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyRecord {
    /// The key identifier, the middle segment of the presented key.
    pub key_id: String,
    /// The service account the key authenticates.
    pub service_account: String,
    /// The project the service account belongs to.
    pub project: String,
    /// The PHC-string Argon2id hash of the secret.
    pub secret_hash: String,
    /// When the key stops working, if it ever does.
    pub expires_at: Option<DateTime<Utc>>,
    /// The addresses allowed to use it; empty means any address.
    pub ip_allow_list: Vec<String>,
}

/// Why a presented key was refused.
///
/// The caller is told none of this: every variant answers the same 401, so a probe cannot
/// tell an unknown key id from a wrong secret (R20).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Rejected {
    /// The header is absent, or not a `jc_` key.
    #[error("no platform API key presented")]
    NotAKey,
    /// The key is not `jc_{keyId}_{secret}`.
    #[error("malformed API key")]
    Malformed,
    /// No key with this identifier is known.
    #[error("unknown key id")]
    UnknownKey,
    /// The secret does not verify against the stored hash.
    #[error("secret does not verify")]
    WrongSecret,
    /// The key is past its expiry (PF-37).
    #[error("key expired")]
    Expired,
    /// The caller's address is not on the key's allow list (PF-36).
    #[error("address not allowed for this key")]
    AddressNotAllowed,
}

/// The two halves of a presented key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PresentedKey<'a> {
    /// Which stored key the caller claims to hold.
    pub key_id: &'a str,
    /// The secret to verify against that key's hash.
    pub secret: &'a str,
}

/// Splits `Authorization: Bearer jc_{keyId}_{secret}` into its parts.
///
/// The secret may itself contain `_`, so only the first two separators are structural.
pub fn parse(authorization: &str) -> Result<PresentedKey<'_>, Rejected> {
    let token = authorization
        .strip_prefix("Bearer ")
        .ok_or(Rejected::NotAKey)?
        .trim();
    let rest = token.strip_prefix(KEY_PREFIX).ok_or(Rejected::NotAKey)?;

    let (key_id, secret) = rest.split_once('_').ok_or(Rejected::Malformed)?;
    if key_id.is_empty() || secret.is_empty() {
        return Err(Rejected::Malformed);
    }
    Ok(PresentedKey { key_id, secret })
}

/// Verifies a presented key against its stored record (PF-36, PF-37).
///
/// `now` and `caller_ip` are passed in rather than read from the environment, so the whole
/// decision is a pure function of its inputs and can be tested at its boundaries.
pub fn verify(
    presented: &PresentedKey<'_>,
    record: &KeyRecord,
    now: DateTime<Utc>,
    caller_ip: Option<&str>,
) -> Result<(), Rejected> {
    if presented.key_id != record.key_id {
        return Err(Rejected::UnknownKey);
    }
    if record.expires_at.is_some_and(|expiry| now >= expiry) {
        return Err(Rejected::Expired);
    }
    if !allows(record, caller_ip) {
        return Err(Rejected::AddressNotAllowed);
    }

    let hash = PasswordHash::new(&record.secret_hash).map_err(|_| Rejected::WrongSecret)?;
    Argon2::default()
        .verify_password(presented.secret.as_bytes(), &hash)
        .map_err(|_| Rejected::WrongSecret)
}

/// Whether the caller's address is on the key's allow list.
///
/// An empty list allows any address. A list that names anything and a caller whose address
/// the gateway does not know is refused: an allow list that cannot be evaluated is not an
/// allow list that passes.
fn allows(record: &KeyRecord, caller_ip: Option<&str>) -> bool {
    if record.ip_allow_list.is_empty() {
        return true;
    }
    let Some(caller) = caller_ip else {
        return false;
    };
    record
        .ip_allow_list
        .iter()
        .any(|allowed| matches_cidr(allowed, caller))
}

/// Whether an address matches an allow-list entry, which is either a literal address or a
/// CIDR block.
fn matches_cidr(allowed: &str, caller: &str) -> bool {
    let Some((network, prefix)) = allowed.split_once('/') else {
        return allowed == caller;
    };
    let Ok(prefix) = prefix.parse::<u32>() else {
        return false;
    };
    match (
        network.parse::<std::net::Ipv4Addr>(),
        caller.parse::<std::net::Ipv4Addr>(),
    ) {
        (Ok(network), Ok(caller)) if prefix <= 32 => {
            let mask = if prefix == 0 {
                0
            } else {
                u32::MAX << (32 - prefix)
            };
            network.to_bits() & mask == caller.to_bits() & mask
        }
        _ => false,
    }
}
