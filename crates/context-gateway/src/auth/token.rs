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

use arc_swap::ArcSwap;
use jsonwebtoken::jwk::{AlgorithmParameters, JwkSet};
use jsonwebtoken::{decode, decode_header, Algorithm, DecodingKey, Validation};
use serde::Deserialize;
use std::collections::HashMap;
use std::sync::Arc;

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
}

/// Keycloak's realm role container.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct RealmAccess {
    /// The realm roles.
    #[serde(default)]
    pub roles: Vec<String>,
}

impl Claims {
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
}

impl From<Rejected> for jc_core::ProblemDetails {
    fn from(rejected: Rejected) -> Self {
        tracing::info!(%rejected, "token refused");
        jc_core::ProblemDetails::unauthorized()
    }
}

/// The key table of one realm, by key id.
type VerifierKeys = HashMap<String, (DecodingKey, Algorithm)>;

/// The realm's signing keys, replaced whole when the JWKS is refreshed.
pub struct Verifier {
    issuer: String,
    keys: ArcSwap<VerifierKeys>,
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
            keys: ArcSwap::new(Arc::new(HashMap::new())),
        }
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
        self.keys.store(Arc::new(table));
        count
    }

    /// How many keys the verifier currently holds.
    pub fn key_count(&self) -> usize {
        self.keys.load().len()
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
        let header = decode_header(token).map_err(|_| Rejected::Malformed)?;
        let kid = header.kid.ok_or(Rejected::Malformed)?;
        let keys = self.keys.load();
        let (key, algorithm) = keys.get(&kid).ok_or(Rejected::UnknownKey)?;

        let mut validation = Validation::new(*algorithm);
        validation.set_issuer(&[&self.issuer]);
        validation.set_audience(audiences);
        validation.validate_exp = true;
        validation.validate_nbf = true;
        // Clock skew between Keycloak and this pod, and nothing more: a token is short
        // lived, so a generous window here is a generous window for a stolen one.
        validation.leeway = 60;

        decode::<Claims>(token, key, &validation)
            .map(|data| data.claims)
            .map_err(|error| match error.kind() {
                jsonwebtoken::errors::ErrorKind::ExpiredSignature
                | jsonwebtoken::errors::ErrorKind::ImmatureSignature => Rejected::Expired,
                jsonwebtoken::errors::ErrorKind::InvalidIssuer => Rejected::WrongIssuer,
                jsonwebtoken::errors::ErrorKind::InvalidAudience => Rejected::WrongAudience,
                jsonwebtoken::errors::ErrorKind::InvalidSignature => Rejected::BadSignature,
                _ => Rejected::Malformed,
            })
    }
}

/// The token in an `Authorization` header, if the header carries one that is not a
/// platform API key.
///
/// An API key is a different credential with a different verifier (PF-36), so it is not
/// this function's business, only its business to not mistake one for a JWT.
pub fn bearer(header: Option<&str>) -> Result<&str, Rejected> {
    let token = header
        .and_then(|value| value.strip_prefix("Bearer "))
        .map(str::trim)
        .filter(|token| !token.is_empty())
        .ok_or(Rejected::NoToken)?;
    if token.starts_with(crate::auth::api_key::KEY_PREFIX) {
        return Err(Rejected::NoToken);
    }
    Ok(token)
}
