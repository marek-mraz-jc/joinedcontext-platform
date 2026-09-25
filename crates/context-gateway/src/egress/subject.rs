//! Who a subscription delivers for, carried with it and signed (GW27, R48, T-2383).
//!
//! A delivery carries no caller: the broker calls the gateway with no token. So the subject the
//! gateway established when the subscription was written travels in the routed notification
//! endpoint, and every delivery decides the subscription again for that subject against the
//! policies in force. The broker holds the stored form, so the subject is sealed with a key only
//! the gateway holds, over the endpoint, the target and the granted areas beside it: a subject
//! copied onto another endpoint, or a `to` swapped under it, does not verify.

use crate::pdp::evaluator::Subject;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use ring::hmac;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

/// The shortest key the gateway accepts: HMAC-SHA256 is as strong as its key up to 32 bytes.
pub const MIN_KEY_BYTES: usize = 32;

/// The gateway's own key for sealing a subscription's subject (`JC_GATEWAY_DELIVERY_KEY`).
pub struct DeliveryKey(hmac::Key);

impl std::fmt::Debug for DeliveryKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("DeliveryKey([redacted])")
    }
}

impl DeliveryKey {
    /// A key from the configured secret; `None` when it is shorter than [`MIN_KEY_BYTES`].
    pub fn new(secret: &[u8]) -> Option<Self> {
        (secret.len() >= MIN_KEY_BYTES).then(|| Self(hmac::Key::new(hmac::HMAC_SHA256, secret)))
    }

    /// The `sub` parameter a routed notification endpoint carries.
    pub fn seal(
        &self,
        base_path: &str,
        target: &str,
        areas: &[String],
        subject: &Subject,
    ) -> String {
        let wire = Wire::from(subject);
        // Serializing a struct of strings and string sets cannot fail; an empty payload would
        // only fail to verify, never verify as someone else.
        let payload = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&wire).unwrap_or_default());
        let tag = hmac::sign(&self.0, &signed_bytes(base_path, target, areas, &payload));
        format!("{payload}.{}", URL_SAFE_NO_PAD.encode(tag.as_ref()))
    }

    /// The subject a `sub` parameter carries, when it was sealed by this key for exactly this
    /// endpoint, target and areas; `None` for anything else.
    pub fn open(
        &self,
        base_path: &str,
        target: &str,
        areas: &[String],
        sealed: &str,
    ) -> Option<Subject> {
        let (payload, tag) = sealed.split_once('.')?;
        let tag = URL_SAFE_NO_PAD.decode(tag).ok()?;
        hmac::verify(
            &self.0,
            &signed_bytes(base_path, target, areas, payload),
            &tag,
        )
        .ok()?;
        let wire: Wire = serde_json::from_slice(&URL_SAFE_NO_PAD.decode(payload).ok()?).ok()?;
        Some(wire.into())
    }
}

/// What the tag covers: a version, then every part with its length before it, so no two
/// different sets of parts sign the same bytes whatever a caller writes into its target.
fn signed_bytes(base_path: &str, target: &str, areas: &[String], payload: &str) -> Vec<u8> {
    let mut bytes = format!("jc-delivery-v1\n{}\n", areas.len());
    for part in [base_path, target]
        .into_iter()
        .chain(areas.iter().map(String::as_str))
        .chain([payload])
    {
        bytes.push_str(&format!("{}:{part}\n", part.len()));
    }
    bytes.into_bytes()
}

/// The subject as it is sealed: short names, because it rides in a URI the broker stores.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Wire {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    u: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    sa: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    g: BTreeSet<String>,
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    r: BTreeSet<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    d: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    a: Option<String>,
}

impl From<&Subject> for Wire {
    fn from(subject: &Subject) -> Self {
        Self {
            u: subject.user.clone(),
            sa: subject.service_account.clone(),
            g: subject.groups.clone(),
            r: subject.roles.clone(),
            d: subject.did.clone(),
            a: subject.agreement.clone(),
        }
    }
}

impl From<Wire> for Subject {
    fn from(wire: Wire) -> Self {
        Self {
            user: wire.u,
            service_account: wire.sa,
            groups: wire.g,
            roles: wire.r,
            did: wire.d,
            agreement: wire.a,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(byte: u8) -> DeliveryKey {
        DeliveryKey::new(&[byte; MIN_KEY_BYTES]).expect("a long enough key")
    }

    fn subject() -> Subject {
        Subject {
            user: Some("f:1:ana".into()),
            groups: BTreeSet::from(["air-readers".to_owned()]),
            roles: BTreeSet::from(["steward".to_owned()]),
            ..Subject::default()
        }
    }

    const PATH: &str = "/api/endpoint/k7m2";
    const TO: &str = "https://hooks.example.org/air";

    #[test]
    fn a_sealed_subject_opens_as_itself_for_the_same_endpoint_target_and_areas() {
        let areas = vec!["georel=within;geometry=Polygon".to_owned()];
        let sealed = key(1).seal(PATH, TO, &areas, &subject());
        assert_eq!(key(1).open(PATH, TO, &areas, &sealed), Some(subject()));
    }

    #[test]
    fn anything_moved_or_altered_does_not_open() {
        let areas = vec!["a".to_owned()];
        let sealed = key(1).seal(PATH, TO, &areas, &subject());
        assert_eq!(key(2).open(PATH, TO, &areas, &sealed), None, "another key");
        assert_eq!(
            key(1).open("/api/endpoint/other", TO, &areas, &sealed),
            None,
            "another endpoint"
        );
        assert_eq!(
            key(1).open(PATH, "https://evil.example", &areas, &sealed),
            None,
            "another target"
        );
        assert_eq!(key(1).open(PATH, TO, &[], &sealed), None, "an area dropped");
        let (payload, tag) = sealed.split_once('.').expect("two parts");
        let widened = URL_SAFE_NO_PAD.encode(br#"{"r":["admin"]}"#);
        assert_eq!(
            key(1).open(PATH, TO, &areas, &format!("{widened}.{tag}")),
            None,
            "a new subject"
        );
        assert_eq!(key(1).open(PATH, TO, &areas, payload), None, "no tag");
        assert_eq!(key(1).open(PATH, TO, &areas, ""), None, "nothing");
    }

    #[test]
    fn a_short_key_is_refused_and_a_key_never_prints() {
        assert!(DeliveryKey::new(&[7; MIN_KEY_BYTES - 1]).is_none());
        assert_eq!(format!("{:?}", key(9)), "DeliveryKey([redacted])");
    }
}
