//! Verifying a Keycloak token, and only then believing anything it says (T-0228, PF-45,
//! PF-46).
//!
//! Three claims decide, and nothing else about the caller is read from the request:
//! `iss` must be the realm the gateway was configured with, the signature and expiry must
//! verify against that realm's JWKS, and `aud` must name the resource being called. A
//! token that fails any of them is the same 401, so a probe cannot tell which.
//!
//! The algorithm comes from the key, never from the token header: a header is attacker
//! input, and honouring it is how `alg: none` and HMAC-with-the-public-key happen.
//!
//! A signature is checked once per token: the claims of a token that verified are remembered
//! until the key table changes, so the rate limiter's count and the handler's admission of one
//! request cost one signature check, and a client reusing its token for minutes costs one in
//! all (T-5290). Expiry and audience are checked on every call.

use arc_swap::ArcSwap;
use jsonwebtoken::jwk::{AlgorithmParameters, JwkSet};
use jsonwebtoken::{decode, decode_header, Algorithm, DecodingKey, Validation};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

/// Clock skew between Keycloak and this pod, and nothing more: a token is short lived, so a
/// generous window here is a generous window for a stolen one.
const LEEWAY: i64 = 60;

/// The most verified tokens a verifier remembers, a few MiB of claims. Past it the memo starts
/// over, which costs one signature check per token again and nothing else (T-5290).
const REMEMBERED: usize = 4_096;

/// The most refused tokens a verifier remembers, apart from the verified ones so a flood of
/// forgeries cannot push a genuine token out. A forged token is checked by the rate limiter
/// and then by the handler of the same request; this makes that one signature check, not two.
const REFUSED: usize = 1_024;

/// The claims the gateway acts on. A token carries many more; none of them decide.
#[derive(Debug, Clone, Deserialize)]
pub struct Claims {
    /// The token's subject.
    pub sub: String,
    /// The issuer, verified to be the configured realm.
    pub iss: String,
    /// The Keycloak client that obtained the token (PF-46).
    #[serde(default)]
    pub azp: Option<String>,
    /// The human's username, present on a token issued to a person.
    #[serde(default)]
    pub preferred_username: Option<String>,
    /// The realm roles Keycloak asserts.
    #[serde(default)]
    pub realm_access: Option<RealmAccess>,
    /// The groups Keycloak asserts.
    #[serde(default)]
    pub groups: Vec<String>,
    /// The client roles Keycloak asserts, by client id (ADR-N-030, AP-97).
    #[serde(default)]
    pub resource_access: HashMap<String, RealmAccess>,
    /// When the token expires. Always present: the verifier refuses a token without it.
    pub exp: i64,
    /// When the token was issued, which is what makes its lifetime measurable (DS-11).
    #[serde(default)]
    pub iat: Option<i64>,
    /// The data space agreement a transfer token acts under (DS-11, DS-13).
    #[serde(default, rename = "agreementId")]
    pub agreement_id: Option<String>,
    /// The DID of the participant a transfer token was issued to (DS-04, DS-11).
    #[serde(default)]
    pub participant: Option<String>,
    /// The audiences the token names (RFC 7519 `aud`, one string or a list). The verifier has
    /// already required one of the accepted ones; this says which, which is what tells a hub
    /// token from an Endpoint's own (ADR-N-025 section 4).
    #[serde(default, deserialize_with = "one_or_many")]
    pub aud: Vec<String>,
    /// The scopes the token was granted, space-separated (RFC 6749 section 3.3).
    #[serde(default)]
    pub scope: Option<String>,
}

/// `aud` as RFC 7519 section 4.1.3 allows it: a single string or an array of them.
fn one_or_many<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<Vec<String>, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Audience {
        One(String),
        Many(Vec<String>),
    }
    Ok(match Audience::deserialize(deserializer)? {
        Audience::One(audience) => vec![audience],
        Audience::Many(audiences) => audiences,
    })
}

/// Keycloak's realm role container.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct RealmAccess {
    /// The realm roles.
    #[serde(default)]
    pub roles: Vec<String>,
}

impl Claims {
    /// The roles of `client` this token carries, when `client` obtained it (`azp`); nothing for a
    /// token another client obtained, whatever its `resource_access` says (ADR-N-030, AP-97).
    pub fn client_roles(&self, client: &str) -> Vec<String> {
        if self.azp.as_deref() != Some(client) {
            return Vec::new();
        }
        self.resource_access
            .get(client)
            .map(|access| access.roles.clone())
            .unwrap_or_default()
    }

    /// Whether the token names any of `audiences`.
    pub fn names_audience(&self, audiences: &[String]) -> bool {
        self.aud.iter().any(|audience| audiences.contains(audience))
    }

    /// The Endpoint slugs the token's `endpoint:{slug}` scopes name: what the person picked at
    /// connect time for a hub connector (ADR-N-025 section 4, EP-88).
    pub fn endpoint_scopes(&self) -> impl Iterator<Item = &str> {
        self.scope
            .as_deref()
            .unwrap_or_default()
            .split_ascii_whitespace()
            .filter_map(|scope| scope.strip_prefix("endpoint:"))
            .filter(|slug| !slug.is_empty())
    }

    /// The realm roles the token asserts.
    pub fn roles(&self) -> &[String] {
        self.realm_access
            .as_ref()
            .map_or(&[][..], |access| &access.roles)
    }
}

/// Why a presented token was refused. Every variant answers the same 401 (R20).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Rejected {
    /// No `Authorization: Bearer` header, or one the gateway does not handle.
    #[error("no bearer token presented")]
    NoToken,
    /// The token is not a JWT the gateway can read.
    #[error("malformed token")]
    Malformed,
    /// The token names a key the realm's JWKS does not contain.
    #[error("token signed by an unknown key")]
    UnknownKey,
    /// The signature does not verify.
    #[error("signature does not verify")]
    BadSignature,
    /// The token is past its expiry, or not yet valid.
    #[error("token expired")]
    Expired,
    /// The token was issued by another realm.
    #[error("token issued by another realm")]
    WrongIssuer,
    /// The token does not name the resource being called (RFC 8707).
    #[error("token is not bound to this resource")]
    WrongAudience,
    /// The subject was disabled, deleted or logged out after the token was issued (AR-6).
    #[error("token issued before its subject was revoked")]
    Revoked,
}

impl From<Rejected> for crate::ProblemDetails {
    fn from(rejected: Rejected) -> Self {
        tracing::info!(%rejected, "token refused");
        crate::ProblemDetails::unauthorized()
    }
}

/// The key table of one realm, by key id.
type VerifierKeys = HashMap<String, (DecodingKey, Algorithm)>;

/// The claims of tokens whose signature and issuer verified, by the token's SHA-256.
type Remembered = HashMap<[u8; 32], Claims>;

/// One key table and the tokens verified against it. A refresh replaces both at once, so a
/// token of a key the realm withdrew is checked again, and refused.
#[derive(Default)]
struct Table {
    keys: VerifierKeys,
    remembered: Mutex<Remembered>,
    /// Tokens whose signature or issuer was refused: neither changes until the keys do.
    refused: Mutex<HashMap<[u8; 32], Rejected>>,
}

/// The realm's signing keys, replaced whole when the JWKS is refreshed.
pub struct Verifier {
    issuer: String,
    keys: ArcSwap<Table>,
    /// `sub` -> unix second of the subject's revocation, as the Portal lists it (T-5568).
    revoked: ArcSwap<HashMap<String, i64>>,
}

impl std::fmt::Debug for Verifier {
    /// Prints what the verifier is for, never what it holds.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Verifier")
            .field("issuer", &self.issuer)
            .field("keys", &self.key_count())
            .finish()
    }
}

impl Verifier {
    /// A verifier for one realm, with no keys until a JWKS is loaded.
    pub fn new(issuer: impl Into<String>) -> Self {
        Self {
            issuer: issuer.into(),
            keys: ArcSwap::new(Arc::new(Table::default())),
            revoked: ArcSwap::new(Arc::new(HashMap::new())),
        }
    }

    /// Replaces the revoked subjects: `sub` -> the unix second it was revoked at. A token of a
    /// listed subject issued at or before its mark is refused by [`Self::verify`] from now on;
    /// one issued later (the person enabled again and signed in anew) is not.
    pub fn replace_revocations(&self, marks: HashMap<String, i64>) {
        self.revoked.store(Arc::new(marks));
    }

    /// The realm this verifier accepts tokens from.
    pub fn issuer(&self) -> &str {
        &self.issuer
    }

    /// Replaces the key table with the keys of a JWKS, and reports how many were usable.
    ///
    /// Only asymmetric signing keys are taken. A symmetric key in a realm JWKS would let a
    /// caller sign its own tokens with a value the JWKS just published.
    pub fn replace_keys(&self, jwks: &JwkSet) -> usize {
        let mut table: VerifierKeys = HashMap::new();
        for jwk in &jwks.keys {
            let Some(kid) = jwk.common.key_id.clone() else {
                continue;
            };
            let algorithm = match &jwk.algorithm {
                AlgorithmParameters::RSA(_) | AlgorithmParameters::EllipticCurve(_) => {
                    match jwk
                        .common
                        .key_algorithm
                        .and_then(|a| a.to_string().parse().ok())
                    {
                        Some(algorithm) => algorithm,
                        None => continue,
                    }
                }
                _ => continue,
            };
            if let Ok(key) = DecodingKey::from_jwk(jwk) {
                table.insert(kid, (key, algorithm));
            }
        }
        let count = table.len();
        self.keys.store(Arc::new(Table {
            keys: table,
            ..Table::default()
        }));
        count
    }

    /// How many keys the verifier currently holds.
    pub fn key_count(&self) -> usize {
        self.keys.load().keys.len()
    }

    /// Verifies a token and returns its claims (PF-46).
    ///
    /// `audiences` is every value that names the resource being called; the token must
    /// contain one of them.
    pub fn verify(&self, token: &str, audiences: &[String]) -> Result<Claims, Rejected> {
        if audiences.is_empty() {
            // Nothing to bind the token to means nothing would refuse it.
            return Err(Rejected::WrongAudience);
        }
        let claims = self.decode(token, Some(audiences))?;
        unrevoked(claims, &self.revoked.load())
    }

    /// Who a token names: signature, issuer and lifetime checked, the audience not (T-5290).
    ///
    /// For counting a caller only, never for admitting one: a rate limiter keys a quota by the
    /// principal a genuine token names, whichever resource it is bound to, so a client cannot
    /// buy a fresh bucket with a made-up token. Every admission goes through [`Self::verify`].
    pub fn identify(&self, token: &str) -> Result<Claims, Rejected> {
        self.decode(token, None)
    }

    /// How many verified tokens are remembered, which is what a memo test looks at.
    pub fn remembered(&self) -> usize {
        remember(&self.keys.load().remembered).len()
    }

    fn decode(&self, token: &str, audiences: Option<&[String]>) -> Result<Claims, Rejected> {
        let table = self.keys.load();
        let digest: [u8; 32] = Sha256::digest(token.as_bytes()).into();
        let known = remember(&table.remembered).get(&digest).cloned();
        let claims = match known {
            Some(claims) => claims,
            None => self.check_once(&table, digest, token)?,
        };
        admit(claims, audiences, now())
    }

    /// The signature and issuer of a token this table has not verified yet, remembered either
    /// way. Only a refusal that cost a signature check is remembered: the cheap ones would only
    /// push the expensive ones out, and an expiry is the clock's, not the token's.
    fn check_once(&self, table: &Table, digest: [u8; 32], token: &str) -> Result<Claims, Rejected> {
        if let Some(rejected) = remember(&table.refused).get(&digest).cloned() {
            return Err(rejected);
        }
        match self.check_signature(&table.keys, token) {
            Ok(claims) => Ok(keep(&table.remembered, REMEMBERED, digest, claims)),
            Err(rejected @ (Rejected::BadSignature | Rejected::WrongIssuer)) => {
                Err(keep(&table.refused, REFUSED, digest, rejected))
            }
            Err(rejected) => Err(rejected),
        }
    }

    /// The signature under the key the token names, by that key's algorithm, and the issuer;
    /// the lifetime as of now. The audience is [`admit`]'s.
    fn check_signature(&self, keys: &VerifierKeys, token: &str) -> Result<Claims, Rejected> {
        let header = decode_header(token).map_err(|_| Rejected::Malformed)?;
        let kid = header.kid.ok_or(Rejected::Malformed)?;
        let (key, algorithm) = keys.get(&kid).ok_or(Rejected::UnknownKey)?;

        let mut validation = Validation::new(*algorithm);
        validation.set_issuer(&[&self.issuer]);
        validation.validate_aud = false;
        validation.validate_exp = true;
        validation.validate_nbf = true;
        validation.leeway = LEEWAY.unsigned_abs();

        decode::<Claims>(token, key, &validation)
            .map(|data| data.claims)
            .map_err(|error| match error.kind() {
                jsonwebtoken::errors::ErrorKind::ExpiredSignature
                | jsonwebtoken::errors::ErrorKind::ImmatureSignature => Rejected::Expired,
                jsonwebtoken::errors::ErrorKind::InvalidIssuer => Rejected::WrongIssuer,
                jsonwebtoken::errors::ErrorKind::InvalidSignature => Rejected::BadSignature,
                _ => Rejected::Malformed,
            })
    }
}

/// Claims whose signature verified, admitted at `now` (seconds since the epoch): still inside
/// their lifetime and, when `audiences` is given, naming one of them. A token without an `aud`
/// names nothing, so it is refused wherever an audience is asked for (RFC 8707); jsonwebtoken's
/// own check let it through.
fn admit(claims: Claims, audiences: Option<&[String]>, now: i64) -> Result<Claims, Rejected> {
    if claims.exp < now.saturating_sub(LEEWAY) {
        return Err(Rejected::Expired);
    }
    match audiences {
        Some(audiences) if !claims.names_audience(audiences) => Err(Rejected::WrongAudience),
        _ => Ok(claims),
    }
}

/// Claims whose subject was not revoked after they were issued. A token without `iat` cannot
/// show it came later, so it counts as issued before the mark.
fn unrevoked(claims: Claims, revoked: &HashMap<String, i64>) -> Result<Claims, Rejected> {
    match revoked.get(&claims.sub) {
        Some(mark) if claims.iat.unwrap_or(i64::MIN) <= *mark => Err(Rejected::Revoked),
        _ => Ok(claims),
    }
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| {
            i64::try_from(since.as_secs()).unwrap_or(i64::MAX)
        })
}

/// Remembers `value` under `digest`, starting the memo over when it holds `cap` entries.
fn keep<V: Clone>(memo: &Mutex<HashMap<[u8; 32], V>>, cap: usize, digest: [u8; 32], value: V) -> V {
    let mut memo = remember(memo);
    if memo.len() >= cap {
        memo.clear();
    }
    memo.insert(digest, value.clone());
    value
}

fn remember<T>(remembered: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    // A panicking holder poisoned the lock; the memo only saves work, so going on with it is
    // safe: every entry in it verified.
    remembered
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Whether the request presents a bearer token: what makes a caller not anonymous on every
/// surface, and the only credential a quota is keyed by (T-4590). `Authorization: Basic …` or
/// any other scheme is no token, so it neither leaves the anonymous allowance nor buys a
/// bucket of its own.
pub fn presents(headers: &axum::http::HeaderMap) -> bool {
    bearer(
        headers
            .get(axum::http::header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok()),
    )
    .is_ok()
}

/// The token in an `Authorization: Bearer` header. The platform has no other bearer
/// credential: a static API key is no token and fails verification like any other (PF-37).
pub fn bearer(header: Option<&str>) -> Result<&str, Rejected> {
    crate::http::bearer(header).ok_or(Rejected::NoToken)
}

#[cfg(test)]
mod tests {
    use super::{admit, unrevoked, Claims, Rejected, LEEWAY};
    use std::collections::HashMap;

    const NOW: i64 = 1_800_000_000;

    fn claims(aud: serde_json::Value, exp: i64) -> Claims {
        let mut claims = serde_json::json!({"sub": "f:1:a", "iss": "realm", "exp": exp});
        if !aud.is_null() {
            claims["aud"] = aud;
        }
        serde_json::from_value(claims).expect("claims")
    }

    fn slug() -> Vec<String> {
        vec!["slug".to_owned()]
    }

    #[test]
    fn a_remembered_token_expires_at_the_leeway_bound_and_not_before() {
        let at_bound = admit(claims("slug".into(), NOW - LEEWAY), Some(&slug()), NOW);
        assert_eq!(at_bound.map(|c| c.sub), Ok("f:1:a".to_owned()));
        let past = admit(claims("slug".into(), NOW - LEEWAY - 1), Some(&slug()), NOW);
        assert_eq!(past.map(|c| c.sub), Err(Rejected::Expired));
    }

    #[test]
    fn the_audience_is_one_of_those_asked_for_and_a_missing_one_is_none() {
        let other = admit(claims("other".into(), NOW), Some(&slug()), NOW);
        assert_eq!(other.map(|c| c.sub), Err(Rejected::WrongAudience));
        let missing = admit(claims(serde_json::Value::Null, NOW), Some(&slug()), NOW);
        assert_eq!(missing.map(|c| c.sub), Err(Rejected::WrongAudience));
        let many = admit(
            claims(serde_json::json!(["x", "slug"]), NOW),
            Some(&slug()),
            NOW,
        );
        assert_eq!(many.map(|c| c.sub), Ok("f:1:a".to_owned()));
    }

    #[test]
    fn identifying_asks_for_no_audience() {
        let missing = admit(claims(serde_json::Value::Null, NOW), None, NOW);
        assert_eq!(missing.map(|c| c.sub), Ok("f:1:a".to_owned()));
    }

    #[test]
    fn a_revoked_subject_is_refused_for_tokens_issued_up_to_its_mark() {
        let marks = HashMap::from([("f:1:a".to_owned(), NOW)]);
        let issued = |iat: Option<i64>| {
            let mut c = claims("slug".into(), NOW + 300);
            c.iat = iat;
            unrevoked(c, &marks).map(|c| c.sub)
        };
        assert_eq!(issued(Some(NOW - 1)), Err(Rejected::Revoked));
        assert_eq!(issued(Some(NOW)), Err(Rejected::Revoked));
        assert_eq!(issued(None), Err(Rejected::Revoked));
        assert_eq!(issued(Some(NOW + 1)), Ok("f:1:a".to_owned()));
        let other = unrevoked(claims("slug".into(), NOW + 300), &HashMap::new());
        assert_eq!(other.map(|c| c.sub), Ok("f:1:a".to_owned()));
    }
}
