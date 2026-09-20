//! Edge cases of `auth::api_key::parse` (T-1893, EP-26, MP-02).
//!
//! Contract, in one sentence: only `Authorization: Bearer jc_{keyId}_{secret}` with both halves
//! non-empty is a presented key, the split is on the FIRST separator after the prefix so a secret
//! may contain `_`, and everything else is refused without the refusal saying which part was wrong.
//!
//! `parsing_splits_the_key_on_the_first_separator_after_the_prefix` in `api_key_tests.rs` has the
//! happy path. These are the shapes an attacker sends.

use context_gateway::auth::api_key::{parse, Rejected, KEY_PREFIX};

#[test]
fn nothing_that_is_not_a_bearer_platform_key_is_one() {
    for header in [
        "",
        " ",
        "Bearer",
        "Bearer ",
        "Basic jc_abc_secret",
        "bearer jc_abc_secret", // the scheme is compared exactly, as the gateway writes it
        "BEARER jc_abc_secret",
        "Bearer  jc",                          // the prefix alone is not a key
        "Bearer xjc_abc_secret",               // the prefix has to start the token
        "Bearer JC_abc_secret",                // and it is lower case
        "jc_abc_secret",                       // without the scheme it is not presented as one
        "Bearer eyJhbGciOiJSUzI1NiJ9.e30.sig", // an OAuth token is not an API key
    ] {
        assert_eq!(parse(header), Err(Rejected::NotAKey), "{header:?}");
    }
}

#[test]
fn a_key_missing_one_of_its_two_halves_is_malformed() {
    for header in [
        "Bearer jc_",        // no separator after the prefix's own
        "Bearer jc_abc",     // no second separator
        "Bearer jc__secret", // empty key id
        "Bearer jc_abc_",    // empty secret
        "Bearer jc___",      // both empty, two separators
    ] {
        assert_eq!(parse(header), Err(Rejected::Malformed), "{header:?}");
    }
}

#[test]
fn a_secret_may_contain_the_separator_and_is_kept_whole() {
    let key = parse("Bearer jc_k1_a_b_c").expect("a key with underscores in the secret");
    assert_eq!(key.key_id, "k1");
    assert_eq!(
        key.secret, "a_b_c",
        "only the first separator after the prefix is structural"
    );
}

#[test]
fn surrounding_whitespace_is_not_part_of_the_key() {
    // The header value arrives with whatever a client put around it; what verifies is the key.
    for header in [
        "Bearer  jc_k1_s3cret",
        "Bearer jc_k1_s3cret ",
        "Bearer jc_k1_s3cret\t",
        "Bearer \tjc_k1_s3cret\n",
    ] {
        let key = parse(header).expect(header);
        assert_eq!((key.key_id, key.secret), ("k1", "s3cret"), "{header:?}");
    }
}

#[test]
fn nothing_inside_the_key_is_decoded_or_unescaped() {
    // A key is compared as bytes: `parse` must not percent-decode, lower-case or normalise, or two
    // different keys would become one.
    let key = parse("Bearer jc_k%31_%73ecret").expect("a key with percent signs");
    assert_eq!(key.key_id, "k%31");
    assert_eq!(key.secret, "%73ecret");

    let unicode = parse("Bearer jc_kü_şecret").expect("a key with non-ASCII");
    assert_eq!(unicode.key_id, "kü");
    assert_eq!(unicode.secret, "şecret");

    let mixed = parse("Bearer jc_K1_S3CRET").expect("a key in upper case");
    assert_eq!(
        mixed.key_id, "K1",
        "the key id keeps its case, so `K1` is not `k1`"
    );
}

#[test]
fn a_control_character_inside_the_key_stays_inside_the_key() {
    // Nothing here reaches a log or a header on its own; what matters is that the parser does not
    // split on it and hand a shorter key id to the lookup.
    let key = parse("Bearer jc_k1_sec\0ret").expect("a NUL inside the secret");
    assert_eq!(key.secret, "sec\0ret");
    let newline = parse("Bearer jc_k1_sec\nret").expect("a newline inside the secret");
    assert_eq!(newline.secret, "sec\nret");
    let cr = parse("Bearer jc_k1_sec\rret").expect("a carriage return inside the secret");
    assert_eq!(cr.secret, "sec\rret");
}

#[test]
fn a_very_long_key_is_parsed_rather_than_truncated() {
    let secret = "s".repeat(4096);
    let header = format!("Bearer {KEY_PREFIX}k1_{secret}");
    let key = parse(&header).expect("a long key");
    assert_eq!(key.key_id, "k1");
    assert_eq!(
        key.secret.len(),
        4096,
        "nothing is cut short before it is verified"
    );
}

#[test]
fn the_prefix_is_exactly_the_one_the_platform_publishes() {
    assert_eq!(
        KEY_PREFIX, "jc_",
        "leak scanners and log redaction are written against this"
    );
    // A key id that itself starts with the prefix is still one key, split once.
    let key = parse("Bearer jc_jc_secret").expect("a key id that looks like the prefix");
    assert_eq!((key.key_id, key.secret), ("jc", "secret"));
}
