//! The visitor token of the anonymous channels (T-4653, API/05 §1.2.1): a browser proves a moment
//! of work on a signed challenge and gets a signed token with a random id, which the limits count
//! instead of its address, so people behind one address each have their own. Nothing is stored:
//! both are signed with a key that is random per process, so a restart only sends the widget for
//! a new token.
// ponytail: the key is per process, like the limits it serves; one from a secretRef when the
// assistant runs on more than one replica.

use ring::hmac;
use ring::rand::{SecureRandom, SystemRandom};
use sha2::{Digest, Sha256};

/// How long a challenge may be solved, in seconds.
pub const CHALLENGE_SECS: u64 = 120;
/// How long a token is good for, in seconds.
pub const TOKEN_SECS: u64 = 3_600;
/// The leading zero bits a proof needs: 16,000 hashes on average, a tenth of a second of
/// `crypto.subtle` on a laptop and well under a second on a phone.
pub const DIFFICULTY: u32 = 14;
/// The bits while the deployment's minute is busy: sixteen times the work per token.
pub const BUSY_DIFFICULTY: u32 = 18;

/// Signs challenges and tokens with this process's random key.
pub struct Signer {
    key: hmac::Key,
    random: SystemRandom,
}

impl Default for Signer {
    fn default() -> Self {
        let random = SystemRandom::new();
        let mut bytes = [0u8; 32];
        random
            .fill(&mut bytes)
            .expect("the system's random source answers");
        Self {
            key: hmac::Key::new(hmac::HMAC_SHA256, &bytes),
            random,
        }
    }
}

impl Signer {
    fn sign(&self, parts: &[&str]) -> String {
        hex::encode(hmac::sign(&self.key, parts.join("\n").as_bytes()))
    }

    fn signed(&self, parts: &[&str], signature: &str) -> bool {
        hex::decode(signature)
            .is_ok_and(|tag| hmac::verify(&self.key, parts.join("\n").as_bytes(), &tag).is_ok())
    }

    /// A challenge of `public_id` needing `difficulty` zero bits, good for [`CHALLENGE_SECS`]
    /// from `now` (Unix seconds): `{salt}.{expires}.{bits}.{signature}`.
    pub fn challenge(&self, public_id: &str, difficulty: u32, now: u64) -> String {
        let mut salt = [0u8; 12];
        self.random
            .fill(&mut salt)
            .expect("the system's random source answers");
        let (salt, expires, bits) = (
            hex::encode(salt),
            (now + CHALLENGE_SECS).to_string(),
            difficulty.to_string(),
        );
        let signature = self.sign(&["challenge", public_id, &salt, &expires, &bits]);
        format!("{salt}.{expires}.{bits}.{signature}")
    }

    /// The token a solved challenge of `public_id` earns, and when it expires; `None` for a
    /// challenge this process did not sign for `public_id`, an expired one, or a nonce whose
    /// `SHA-256("{challenge}:{nonce}")` falls short of its bits.
    pub fn redeem(
        &self,
        public_id: &str,
        challenge: &str,
        nonce: &str,
        now: u64,
    ) -> Option<(String, u64)> {
        let mut parts = challenge.split('.');
        let (salt, expires, bits, signature) =
            (parts.next()?, parts.next()?, parts.next()?, parts.next()?);
        if parts.next().is_some()
            || !self.signed(&["challenge", public_id, salt, expires, bits], signature)
            || now >= expires.parse::<u64>().ok()?
        {
            return None;
        }
        if nonce.is_empty() || nonce.len() > 20 || !nonce.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        if zero_bits(&Sha256::digest(format!("{challenge}:{nonce}"))) < bits.parse::<u32>().ok()? {
            return None;
        }
        // One proof, one visitor: the id is the challenge's salt signed, so a replayed proof is
        // the same visitor however often it is redeemed.
        let id = self.sign(&["visitor", public_id, salt])[..32].to_owned();
        let until = now + TOKEN_SECS;
        let signature = self.sign(&["token", public_id, &id, &until.to_string()]);
        Some((format!("{id}.{until}.{signature}"), until))
    }

    /// The visitor id of a token this process signed for `public_id`, still good at `now`.
    pub fn visitor(&self, public_id: &str, token: &str, now: u64) -> Option<String> {
        let mut parts = token.split('.');
        let (id, until, signature) = (parts.next()?, parts.next()?, parts.next()?);
        (parts.next().is_none()
            && id.len() == 32
            && self.signed(&["token", public_id, id, until], signature)
            && until.parse::<u64>().ok()? > now)
            .then(|| id.to_owned())
    }
}

/// The leading zero bits of `hash`.
fn zero_bits(hash: &[u8]) -> u32 {
    let mut bits = 0;
    for byte in hash {
        bits += byte.leading_zeros();
        if *byte != 0 {
            break;
        }
    }
    bits
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: u64 = 1_760_792_400;

    fn solve(challenge: &str, bits: u32) -> String {
        (0u64..)
            .map(|n| n.to_string())
            .find(|n| zero_bits(&Sha256::digest(format!("{challenge}:{n}"))) >= bits)
            .expect("a nonce")
    }

    #[test]
    fn a_solved_challenge_earns_a_token_that_names_one_visitor_for_an_hour() {
        let signer = Signer::default();
        let challenge = signer.challenge("bb", 8, NOW);
        let nonce = solve(&challenge, 8);
        let (token, until) = signer
            .redeem("bb", &challenge, &nonce, NOW + 5)
            .expect("redeemed");
        assert_eq!(until, NOW + 5 + TOKEN_SECS);
        let id = signer.visitor("bb", &token, NOW + 60).expect("a visitor");
        assert_eq!(
            signer.visitor("bb", &token, until - 1),
            Some(id.clone()),
            "reused within its hour"
        );
        assert_eq!(signer.visitor("bb", &token, until), None, "refused after");
        assert_eq!(
            signer.visitor("other", &token, NOW + 60),
            None,
            "another deployment's"
        );

        // A replayed proof is the same visitor; another proof another one.
        let (again, _) = signer
            .redeem("bb", &challenge, &nonce, NOW + 6)
            .expect("redeemed");
        assert_eq!(signer.visitor("bb", &again, NOW + 60), Some(id.clone()));
        let second = signer.challenge("bb", 8, NOW);
        let (other, _) = signer
            .redeem("bb", &second, &solve(&second, 8), NOW)
            .expect("redeemed");
        assert_ne!(signer.visitor("bb", &other, NOW), Some(id));
    }

    #[test]
    fn an_invalid_or_expired_proof_earns_nothing() {
        let signer = Signer::default();
        let challenge = signer.challenge("bb", 8, NOW);
        let nonce = solve(&challenge, 8);
        assert!(
            signer
                .redeem("bb", &challenge, &nonce, NOW + CHALLENGE_SECS)
                .is_none(),
            "expired"
        );
        assert!(
            signer.redeem("other", &challenge, &nonce, NOW).is_none(),
            "another deployment's"
        );
        let wrong = (0u64..)
            .map(|n| n.to_string())
            .find(|n| zero_bits(&Sha256::digest(format!("{challenge}:{n}"))) == 0)
            .expect("a miss");
        assert!(
            signer.redeem("bb", &challenge, &wrong, NOW).is_none(),
            "short of its bits"
        );
        for nonce in ["", "-1", "1e3", "123456789012345678901"] {
            assert!(
                signer.redeem("bb", &challenge, nonce, NOW).is_none(),
                "{nonce:?}"
            );
        }
        // Easier bits written in are a signature that no longer holds.
        let easier = challenge.replacen(".8.", ".0.", 1);
        assert!(signer.redeem("bb", &easier, "0", NOW).is_none());
        let foreign = Signer::default();
        assert!(
            foreign.redeem("bb", &challenge, &nonce, NOW).is_none(),
            "another process's key"
        );
        for token in ["", "a.b.c", "x.1.y.z"] {
            assert!(signer.visitor("bb", token, NOW).is_none(), "{token:?}");
        }
    }

    #[test]
    fn zero_bits_count_across_bytes() {
        assert_eq!(zero_bits(&[0, 0, 0x0f, 0xff]), 20);
        assert_eq!(zero_bits(&[0x80]), 0);
        assert_eq!(zero_bits(&[0, 0]), 16);
    }
}
