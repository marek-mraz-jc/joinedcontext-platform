//! Attack vector: a token from the wrong realm, client, audience or algorithm (T-1677, PF-46).
//!
//! The steps of the vector that the rest of the suite does not already play. `token_tests.rs`
//! covers another realm, another key, another resource, an expired token, a missing `kid` and
//! `alg: none`; `edge_auth_token_verify_tests.rs` covers the audience list, the leeway, `nbf`,
//! the issuer one character off and the shapes of a string that is not a JWT.
//!
//! What is left, and what this file is: **algorithm confusion**. The realm's JWKS is public, so
//! an attacker holds the verification key. The whole family of attacks is to present that key
//! back as if it were a signing secret — `HS256` with the published point, `RS256` over an
//! elliptic-curve key, an `alg` the realm never uses — and hope the verifier takes the
//! algorithm from the token header. The gateway takes it from the key table instead, so every
//! one of these is refused; this file is the proof.
//!
//! Two independent guards hold the property, and the tests are named for that:
//! `an_algorithm_the_key_is_not_of_is_refused_however_plausible_the_header_looks` is the one
//! that goes red the moment `Validation::new(*algorithm)` is built from `header.alg` again,
//! while the HMAC forgeries stay refused even then, because `jsonwebtoken` will not read an
//! elliptic-curve key as a shared secret either. Both are asserted, so the day the second guard
//! moves the first one is still written down.

mod common;

use common::{in_seconds, Realm, ISSUER, KID};
use context_gateway::auth::token::{Rejected, Verifier};
use jsonwebtoken::jwk::AlgorithmParameters;
use jsonwebtoken::{encode, Algorithm, EncodingKey, Header};
use serde_json::{json, Value};

const SLUG: &str = "d7m2xq9vkt4zc6wrb8shj5nfp3";

fn audiences() -> Vec<String> {
    vec![SLUG.to_owned()]
}

/// Claims that are correct in every respect but the signature: issuer, audience and validity
/// all check out, so nothing but the algorithm can be what refuses the token.
fn perfect_claims() -> Value {
    json!({
        "iss": ISSUER,
        "sub": "f:1:attacker",
        "aud": SLUG,
        "azp": "portal",
        "exp": in_seconds(300),
        "iat": in_seconds(-10),
    })
}

/// The bytes of the public point the realm publishes, which is what an attacker feeds back as a
/// shared secret. `x || y` is the uncompressed point without its `0x04` tag, the same value the
/// JWK carries.
fn published_key_material(realm: &Realm) -> Vec<u8> {
    let jwk = realm
        .published_jwks()
        .keys
        .first()
        .expect("the realm publishes one key");
    match &jwk.algorithm {
        AlgorithmParameters::EllipticCurve(ec) => {
            use base64::engine::general_purpose::URL_SAFE_NO_PAD;
            use base64::Engine;
            let mut material = URL_SAFE_NO_PAD.decode(&ec.x).expect("base64url x");
            material.extend(URL_SAFE_NO_PAD.decode(&ec.y).expect("base64url y"));
            material
        }
        other => panic!("the realm publishes an elliptic-curve key, not {other:?}"),
    }
}

/// Signs `claims` with `algorithm` and `secret`, naming the realm's own key id: the forgery
/// points at a key the verifier really holds, so the only thing standing between it and a
/// session is the refusal to believe the header.
fn forge(algorithm: Algorithm, secret: &[u8], claims: &Value) -> String {
    let mut header = Header::new(algorithm);
    header.kid = Some(KID.to_owned());
    encode(&header, claims, &EncodingKey::from_secret(secret)).expect("a forged token")
}

/// PF-46: the realm's public key, handed back as an HMAC secret, mints nothing.
///
/// This is the attack the module doc of `auth::token` names in as many words. `token_tests.rs`
/// plays `alg: none`; this is its harder sibling, where the signature genuinely verifies under
/// the algorithm the attacker chose and is still refused, because the gateway never asked the
/// token which algorithm to use.
#[test]
fn the_published_verification_key_signs_nothing_when_it_comes_back_as_an_hmac_secret() {
    let realm = Realm::new();
    let verifier = realm.verifier();
    let material = published_key_material(&realm);

    for secret in [
        material.clone(),
        // The same key in the shapes a forger reaches for first: the JWK as it was published,
        // and the base64url of the raw point.
        serde_json::to_vec(realm.published_jwks().keys.first().expect("a key")).expect("json"),
        {
            use base64::engine::general_purpose::URL_SAFE_NO_PAD;
            use base64::Engine;
            URL_SAFE_NO_PAD.encode(&material).into_bytes()
        },
    ] {
        for algorithm in [Algorithm::HS256, Algorithm::HS384, Algorithm::HS512] {
            let forged = forge(algorithm, &secret, &perfect_claims());
            assert_eq!(
                verifier.verify(&forged, &audiences()).err(),
                Some(Rejected::Malformed),
                "{algorithm:?} over the published key was taken for a signature",
            );
        }
    }
}

/// PF-46: naming an asymmetric algorithm the key is not of is refused too.
///
/// The realm publishes one elliptic-curve key. A header that says `RS256` while pointing at it
/// asks the verifier to read an EC key as an RSA one; a header that says `ES384` asks it to read
/// a P-256 signature as a P-384 one. An attacker writes these headers by hand — there is no
/// signing key to be had — so the token is assembled the same way, with the realm's own
/// signature over its own payload carried across unchanged.
#[test]
fn an_algorithm_the_key_is_not_of_is_refused_however_plausible_the_header_looks() {
    let realm = Realm::new();
    let verifier = realm.verifier();

    // The realm's own token, taken apart: the payload and the signature are genuine, and only
    // the header is rewritten. This is the strongest form the attacker can present.
    let honest = realm.mint(&perfect_claims());
    let mut parts = honest.split('.');
    let _header = parts.next().expect("a header");
    let payload = parts.next().expect("a payload");
    let signature = parts.next().expect("a signature");

    for algorithm in ["RS256", "RS384", "RS512", "PS256", "ES384", "HS256", "none"] {
        let header =
            common::b64(&json!({ "alg": algorithm, "typ": "JWT", "kid": KID }).to_string());
        let forged = format!("{header}.{payload}.{signature}");
        assert_eq!(
            verifier.verify(&forged, &audiences()).err(),
            Some(Rejected::Malformed),
            "a header claiming {algorithm} was honoured",
        );
    }
}

/// PF-46: the header's `alg` is not consulted at all — a token the realm really signed verifies
/// whatever its header claims to be, and a token the realm did not sign verifies never.
///
/// The pair is what makes the two tests above green *for the right reason*: they would also pass
/// if the verifier refused every token, and this one would not.
#[test]
fn the_algorithm_comes_from_the_key_table_and_the_realms_own_token_still_passes() {
    let realm = Realm::new();
    let verifier = realm.verifier();

    let honest = realm.mint(&perfect_claims());
    assert!(
        verifier.verify(&honest, &audiences()).is_ok(),
        "the realm's own token must still verify, or the two refusals above prove nothing",
    );

    // The same payload, the same key id, signed by nobody: only the signature differs.
    let (signed_part, _) = honest.rsplit_once('.').expect("a JWT has three parts");
    let swapped = format!("{signed_part}.{}", "A".repeat(86));
    assert_eq!(
        verifier.verify(&swapped, &audiences()).err(),
        Some(Rejected::BadSignature),
        "a replaced signature passed",
    );
}

/// PF-46: a forged algorithm is refused without a key ever being fetched.
///
/// A flood of algorithm-confusion attempts must not turn the gateway into a load generator
/// against the realm. The verifier holds its keys and has no way to fetch one on this path at
/// all — the assertion is that the table is untouched after the flood, and the check that keeps
/// it honest is that a verifier holding no keys refuses the same token for a different reason.
#[test]
fn a_flood_of_forgeries_never_makes_the_gateway_reach_for_a_key() {
    let realm = Realm::new();
    let verifier = realm.verifier();
    let secret = published_key_material(&realm);
    let before = verifier.key_count();

    for nonce in 0..64 {
        let mut claims = perfect_claims();
        claims["jti"] = json!(format!("forgery-{nonce}"));
        assert!(verifier
            .verify(&forge(Algorithm::HS256, &secret, &claims), &audiences())
            .is_err());
    }

    assert_eq!(
        verifier.key_count(),
        before,
        "the key table moved while refusing forgeries",
    );

    let empty = Verifier::new(ISSUER);
    assert_eq!(
        empty
            .verify(
                &forge(Algorithm::HS256, &secret, &perfect_claims()),
                &audiences()
            )
            .err(),
        Some(Rejected::UnknownKey),
        "a verifier with no keys refuses for want of a key, and fetches none either",
    );
    assert_eq!(empty.key_count(), 0, "a refusal installed a key");
}
