//! Edge cases of `auth::token::Verifier::replace_keys` (T-1900, EP-26, MP-02).
//!
//! Contract, in one sentence: the verifier ends up holding exactly the asymmetric signing keys
//! of the JWKS it was handed — each under the `kid` the realm published, only when the key
//! names an algorithm that parses, and never a symmetric key, which would let anyone who can
//! read the JWKS sign their own tokens with a value the realm just published (PF-46).
//!
//! The table is replaced **whole**, not merged: this is what makes a rotated realm stop
//! accepting the key it rotated away from, and it is why the count it reports is the count of
//! what is now there rather than of what was added.
//!
//! `token_tests.rs` covers verification through the realm's own key. These are the JWKS
//! shapes around it: a key with no id, an algorithm the crate cannot parse, a symmetric key, a
//! key id twice, a JWKS with no key at all.

mod common;

use context_gateway::auth::token::Verifier;
use jsonwebtoken::jwk::JwkSet;
use serde_json::{json, Value};

/// A JWKS of exactly these keys.
fn jwks(keys: Value) -> JwkSet {
    serde_json::from_value(json!({ "keys": keys })).expect("a JWKS")
}

/// One usable P-256 key, as Keycloak publishes one.
fn ec(kid: &str) -> Value {
    json!({
        "kty": "EC",
        "crv": "P-256",
        "alg": "ES256",
        "use": "sig",
        "kid": kid,
        "x": "f83OJ3D2xF1Bg8vub9tLe1gHMzV76e8Tus9uPHvRVEU",
        "y": "x_FEzRu9m36HLN_tue659LNpXW6pCyStikYjKIWI5a0",
    })
}

#[test]
fn a_jwks_of_one_usable_key_leaves_exactly_that_key() {
    let verifier = Verifier::new(common::ISSUER);
    assert_eq!(verifier.key_count(), 0, "a new verifier holds none");

    assert_eq!(verifier.replace_keys(&jwks(json!([ec("realm-key-1")]))), 1);
    assert_eq!(verifier.key_count(), 1);
}

/// A symmetric key in a realm JWKS is a shared secret published to everybody who can read the
/// endpoint. Taking it would let any of them mint a token this verifier accepts.
#[test]
fn a_symmetric_key_is_never_taken_however_it_is_labelled() {
    let verifier = Verifier::new(common::ISSUER);
    for symmetric in [
        json!({ "kty": "oct", "alg": "HS256", "kid": "shared", "k": "c2VjcmV0LXNlY3JldC1zZWNyZXQtMzI" }),
        json!({ "kty": "oct", "alg": "ES256", "kid": "shared", "k": "c2VjcmV0LXNlY3JldC1zZWNyZXQtMzI" }),
        json!({ "kty": "oct", "use": "sig", "kid": "shared", "k": "c2VjcmV0LXNlY3JldC1zZWNyZXQtMzI" }),
    ] {
        assert_eq!(
            verifier.replace_keys(&jwks(json!([symmetric]))),
            0,
            "no symmetric key is a signing key here",
        );
        assert_eq!(verifier.key_count(), 0);
    }
}

/// A key with no `kid` cannot be found by a token that names one, and `verify` refuses a token
/// with no `kid` outright — so a key without an id could only ever be dead weight.
#[test]
fn a_key_without_a_key_id_is_left_out() {
    let verifier = Verifier::new(common::ISSUER);
    let mut anonymous = ec("realm-key-1");
    anonymous.as_object_mut().expect("an object").remove("kid");

    assert_eq!(verifier.replace_keys(&jwks(json!([anonymous]))), 0);
    assert_eq!(verifier.key_count(), 0);
}

/// The algorithm is taken from the key, never from the token that arrives later: a key that
/// does not name one is left out rather than defaulted, because a default would be an
/// algorithm the realm never chose.
#[test]
fn a_key_that_names_no_algorithm_or_an_unreadable_one_is_left_out() {
    let verifier = Verifier::new(common::ISSUER);

    let mut no_alg = ec("realm-key-1");
    no_alg.as_object_mut().expect("an object").remove("alg");
    assert_eq!(verifier.replace_keys(&jwks(json!([no_alg]))), 0);

    // An algorithm the crate knows as an *encryption* algorithm is not a signing one, so the
    // key is left out rather than installed under an algorithm nobody would verify with.
    let mut encryption = ec("realm-key-1");
    encryption["alg"] = json!("RSA-OAEP");
    assert_eq!(verifier.replace_keys(&jwks(json!([encryption]))), 0);

    // And an algorithm no version of the crate knows is not a JWKS this gateway can read at
    // all: serde refuses the whole set, which `jwks::refresh` reports as unreachable and which
    // leaves the keys the verifier already had (T-1898).
    let mut unknown = ec("realm-key-1");
    unknown["alg"] = json!("ES999");
    assert!(
        serde_json::from_value::<JwkSet>(json!({ "keys": [unknown] })).is_err(),
        "one unreadable algorithm costs the whole JWKS",
    );
}

#[test]
fn the_table_is_replaced_whole_so_a_rotated_key_stops_being_accepted() {
    let verifier = Verifier::new(common::ISSUER);
    assert_eq!(verifier.replace_keys(&jwks(json!([ec("old")]))), 1);

    assert_eq!(
        verifier.replace_keys(&jwks(json!([ec("new")]))),
        1,
        "the new JWKS is the whole of the table, not an addition to it",
    );
    assert_eq!(verifier.key_count(), 1);
}

/// A realm mid-rotation publishes both keys, and both have to work or every token minted in
/// the last few minutes is refused.
#[test]
fn a_realm_publishing_two_keys_leaves_both() {
    let verifier = Verifier::new(common::ISSUER);
    assert_eq!(
        verifier.replace_keys(&jwks(json!([ec("old"), ec("new")]))),
        2
    );
    assert_eq!(verifier.key_count(), 2);
}

/// One `kid` is one entry: a JWKS naming the same id twice cannot make the table grow, and the
/// count says what is really there rather than how many keys were offered.
#[test]
fn a_key_id_published_twice_is_one_entry_and_the_count_says_so() {
    let verifier = Verifier::new(common::ISSUER);
    let mut second = ec("realm-key-1");
    second["x"] = json!("x_FEzRu9m36HLN_tue659LNpXW6pCyStikYjKIWI5a0");

    assert_eq!(
        verifier.replace_keys(&jwks(json!([ec("realm-key-1"), second]))),
        1
    );
    assert_eq!(verifier.key_count(), 1);
}

/// The one that matters for availability: a JWKS with no usable key empties the table, and the
/// count reports it honestly as zero. `jwks::refresh` hands that zero to its caller, and a
/// verifier holding no key refuses every token — which is why a JWKS that parses but carries
/// nothing usable is a realm problem the count has to make visible (T-1898).
#[test]
fn a_jwks_with_nothing_usable_in_it_empties_the_table_and_reports_zero() {
    let verifier = Verifier::new(common::ISSUER);
    assert_eq!(verifier.replace_keys(&jwks(json!([ec("realm-key-1")]))), 1);

    assert_eq!(verifier.replace_keys(&jwks(json!([]))), 0);
    assert_eq!(verifier.key_count(), 0);

    assert_eq!(verifier.replace_keys(&jwks(json!([ec("realm-key-1")]))), 1);
    let unusable = json!({ "kty": "oct", "kid": "shared", "k": "c2VjcmV0LXNlY3JldC1zZWNyZXQtMzI" });
    assert_eq!(verifier.replace_keys(&jwks(json!([unusable]))), 0);
    assert_eq!(verifier.key_count(), 0);
}

/// A key id is a string the realm chose and is used as a map key: an unusual one is stored
/// under exactly what was published, and the count is still one.
#[test]
fn an_unusual_key_id_is_stored_under_exactly_what_was_published() {
    let verifier = Verifier::new(common::ISSUER);
    for kid in [
        "",
        " ",
        "REALM-KEY-1",
        "realm key 1",
        "réalm-kéy-1",
        "realm-key-1\n",
    ] {
        assert_eq!(
            verifier.replace_keys(&jwks(json!([ec(kid)]))),
            1,
            "{kid:?} is a key id like any other",
        );
        assert_eq!(verifier.key_count(), 1);
    }
}

/// Replacing the keys while the verifier is shared is what the refresh loop really does, and
/// it takes `&self`: the table swaps atomically, so a reader sees one table or the other and
/// never a half-built one.
#[test]
fn the_keys_can_be_replaced_through_a_shared_reference_while_others_read() {
    let verifier = std::sync::Arc::new(Verifier::new(common::ISSUER));
    verifier.replace_keys(&jwks(json!([ec("realm-key-1")])));

    let readers: Vec<_> = (0..4)
        .map(|_| {
            let verifier = std::sync::Arc::clone(&verifier);
            std::thread::spawn(move || {
                for _ in 0..200 {
                    // Never zero, never two: one table or the other, whole.
                    assert_eq!(verifier.key_count(), 1);
                }
            })
        })
        .collect();
    for _ in 0..200 {
        assert_eq!(verifier.replace_keys(&jwks(json!([ec("realm-key-1")]))), 1);
    }
    for reader in readers {
        reader.join().expect("the reader finishes");
    }
}

/// What the verifier prints. It holds decoding keys, which are public — but the debug line
/// still says what it is for and how many, never the material.
#[test]
fn the_verifier_prints_what_it_is_for_and_never_what_it_holds() {
    let verifier = Verifier::new(common::ISSUER);
    verifier.replace_keys(&jwks(json!([ec("realm-key-1")])));

    let printed = format!("{verifier:?}");
    assert!(printed.contains(common::ISSUER));
    assert!(printed.contains('1'));
    assert!(!printed.contains("f83OJ3D2"), "no key material: {printed}");
    assert!(
        !printed.contains("realm-key-1"),
        "not even a key id: {printed}"
    );
}
