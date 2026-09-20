//! Edge cases of `auth::api_key::verify` (T-1894, EP-26, MP-02).
//!
//! Contract, in one sentence: a presented key passes only when its id is the record's id exactly,
//! the key has not reached its expiry, the caller's address is on the allow list, and the secret
//! verifies against the stored Argon2id hash — and every failure is one answer to the caller, so a
//! probe cannot tell an unknown id from a wrong secret (R20).
//!
//! `api_key_tests.rs` covers the happy path, the expiry instant, an unknown address and a stored
//! hash that is not a PHC string. These are the ones around them.

use argon2::password_hash::{rand_core::OsRng, PasswordHasher, SaltString};
use argon2::Argon2;
use chrono::{DateTime, TimeZone, Utc};
use context_gateway::auth::api_key::{verify, KeyRecord, PresentedKey, Rejected};

fn at(year: i32, month: u32, day: u32) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(year, month, day, 12, 0, 0)
        .single()
        .expect("a real instant")
}

fn hash(secret: &str) -> String {
    let salt = SaltString::generate(&mut OsRng);
    Argon2::default()
        .hash_password(secret.as_bytes(), &salt)
        .expect("the secret hashes")
        .to_string()
}

fn record(allow: &[&str]) -> KeyRecord {
    KeyRecord {
        key_id: "k1".to_owned(),
        service_account: "collector".to_owned(),
        project: "helsinki".to_owned(),
        secret_hash: hash("s3cret"),
        expires_at: None,
        ip_allow_list: allow.iter().map(|a| (*a).to_owned()).collect(),
    }
}

fn presented<'a>(key_id: &'a str, secret: &'a str) -> PresentedKey<'a> {
    PresentedKey { key_id, secret }
}

#[test]
fn the_key_id_is_compared_exactly() {
    let stored = record(&[]);
    assert!(verify(&presented("k1", "s3cret"), &stored, at(2026, 1, 1), None).is_ok());
    for near in ["K1", "k1 ", " k1", "k", "k11", "k1\n", "k1\0", "k%31"] {
        assert_eq!(
            verify(&presented(near, "s3cret"), &stored, at(2026, 1, 1), None),
            Err(Rejected::UnknownKey),
            "{near:?} is not the key id `k1`",
        );
    }
}

#[test]
fn a_wrong_secret_never_passes_however_close_it_is() {
    let stored = record(&[]);
    for near in [
        "s3cre", "s3crett", "S3CRET", " s3cret", "s3cret ", "", "s3cret\0",
    ] {
        assert_eq!(
            verify(&presented("k1", near), &stored, at(2026, 1, 1), None),
            Err(Rejected::WrongSecret),
            "{near:?} is not the secret",
        );
    }
    // The stored hash itself is not a password: presenting what the database holds must not work.
    assert_eq!(
        verify(
            &presented("k1", &stored.secret_hash),
            &stored,
            at(2026, 1, 1),
            None
        ),
        Err(Rejected::WrongSecret),
        "a copy of the database is not a set of working credentials (PF-36)",
    );
}

#[test]
fn an_expired_key_is_refused_before_its_secret_is_even_hashed() {
    let mut stored = record(&[]);
    stored.expires_at = Some(at(2026, 1, 1));
    // Past the expiry, the right secret does not help.
    assert_eq!(
        verify(&presented("k1", "s3cret"), &stored, at(2026, 1, 2), None),
        Err(Rejected::Expired),
    );
    // Before it, it still works — and at the instant itself it does not (that is `>=`).
    assert!(verify(&presented("k1", "s3cret"), &stored, at(2025, 12, 31), None).is_ok());
    assert_eq!(
        verify(&presented("k1", "s3cret"), &stored, at(2026, 1, 1), None),
        Err(Rejected::Expired),
    );
}

#[test]
fn the_order_of_the_checks_is_the_cheap_ones_first() {
    // An unknown id, an expiry and a refused address are all decided before Argon2 runs, so a probe
    // cannot make the platform do the expensive work by guessing ids (R20). This pins the order by
    // what it answers: a key that fails two ways answers the earlier one.
    let mut stored = record(&["10.0.0.1"]);
    stored.expires_at = Some(at(2026, 1, 1));
    assert_eq!(
        verify(
            &presented("other", "wrong"),
            &stored,
            at(2026, 6, 1),
            Some("9.9.9.9")
        ),
        Err(Rejected::UnknownKey),
    );
    assert_eq!(
        verify(
            &presented("k1", "wrong"),
            &stored,
            at(2026, 6, 1),
            Some("9.9.9.9")
        ),
        Err(Rejected::Expired),
    );
    stored.expires_at = None;
    assert_eq!(
        verify(
            &presented("k1", "wrong"),
            &stored,
            at(2026, 6, 1),
            Some("9.9.9.9")
        ),
        Err(Rejected::AddressNotAllowed),
    );
}

#[test]
fn an_allow_list_that_cannot_be_evaluated_does_not_pass() {
    let stored = record(&["10.0.0.1"]);
    // No address known: an allow list that cannot be evaluated is not one that passes.
    assert_eq!(
        verify(&presented("k1", "s3cret"), &stored, at(2026, 1, 1), None),
        Err(Rejected::AddressNotAllowed),
    );
    // An address that is not the one named, in every near shape.
    for near in [
        "10.0.0.2",
        "10.0.0.10",
        "010.0.0.1",
        "10.0.0.1 ",
        "10.0.0.1:443",
        "::ffff:10.0.0.1",
        "",
    ] {
        assert_eq!(
            verify(
                &presented("k1", "s3cret"),
                &stored,
                at(2026, 1, 1),
                Some(near)
            ),
            Err(Rejected::AddressNotAllowed),
            "{near:?} is not 10.0.0.1",
        );
    }
    assert!(verify(
        &presented("k1", "s3cret"),
        &stored,
        at(2026, 1, 1),
        Some("10.0.0.1")
    )
    .is_ok());
}

#[test]
fn a_cidr_block_allows_its_own_addresses_and_no_others() {
    let stored = record(&["10.1.2.0/24"]);
    for inside in ["10.1.2.0", "10.1.2.1", "10.1.2.255"] {
        assert!(
            verify(
                &presented("k1", "s3cret"),
                &stored,
                at(2026, 1, 1),
                Some(inside)
            )
            .is_ok(),
            "{inside} is inside 10.1.2.0/24",
        );
    }
    for outside in [
        "10.1.3.1",
        "10.1.1.255",
        "11.1.2.1",
        "10.1.2",
        "10.1.2.1/32",
    ] {
        assert_eq!(
            verify(
                &presented("k1", "s3cret"),
                &stored,
                at(2026, 1, 1),
                Some(outside)
            ),
            Err(Rejected::AddressNotAllowed),
            "{outside} is not inside 10.1.2.0/24",
        );
    }
}

#[test]
fn the_bounds_of_a_prefix_are_the_whole_range_and_one_address() {
    let everything = record(&["0.0.0.0/0"]);
    assert!(verify(
        &presented("k1", "s3cret"),
        &everything,
        at(2026, 1, 1),
        Some("203.0.113.9")
    )
    .is_ok());

    let one = record(&["203.0.113.9/32"]);
    assert!(verify(
        &presented("k1", "s3cret"),
        &one,
        at(2026, 1, 1),
        Some("203.0.113.9")
    )
    .is_ok());
    assert_eq!(
        verify(
            &presented("k1", "s3cret"),
            &one,
            at(2026, 1, 1),
            Some("203.0.113.10")
        ),
        Err(Rejected::AddressNotAllowed),
    );
}

#[test]
fn an_allow_list_entry_that_is_not_an_address_allows_nothing() {
    // A prefix past the bound, a prefix that is not a number, a host name, an empty entry: each is
    // refused rather than read as "any address". A list that names something never widens.
    for nonsense in [
        "10.0.0.0/33",
        "10.0.0.0/999",
        "10.0.0.0/abc",
        "10.0.0.0/",
        "not-an-address",
        "*",
        "",
        "10.0.0.0/-1",
    ] {
        let stored = record(&[nonsense]);
        assert_eq!(
            verify(
                &presented("k1", "s3cret"),
                &stored,
                at(2026, 1, 1),
                Some("10.0.0.1")
            ),
            Err(Rejected::AddressNotAllowed),
            "the entry {nonsense:?} must allow nothing",
        );
    }
}

#[test]
fn one_entry_of_a_longer_list_is_enough_and_the_others_do_not_widen_it() {
    let stored = record(&["192.0.2.7", "10.1.2.0/24", "not-an-address"]);
    assert!(verify(
        &presented("k1", "s3cret"),
        &stored,
        at(2026, 1, 1),
        Some("192.0.2.7")
    )
    .is_ok());
    assert!(verify(
        &presented("k1", "s3cret"),
        &stored,
        at(2026, 1, 1),
        Some("10.1.2.3")
    )
    .is_ok());
    assert_eq!(
        verify(
            &presented("k1", "s3cret"),
            &stored,
            at(2026, 1, 1),
            Some("192.0.2.8")
        ),
        Err(Rejected::AddressNotAllowed),
    );
}

#[test]
fn an_ipv6_range_is_refused_rather_than_guessed() {
    // `matches_cidr` reads IPv4 only, so an IPv6 CIDR matches nothing and the key simply does not
    // work from such an address — fail closed. A literal IPv6 address in the list does work,
    // because a literal entry is compared as text.
    let block = record(&["2001:db8::/32"]);
    assert_eq!(
        verify(
            &presented("k1", "s3cret"),
            &block,
            at(2026, 1, 1),
            Some("2001:db8::1")
        ),
        Err(Rejected::AddressNotAllowed),
        "an IPv6 range grants nothing: it is a gap in reach, never in strictness",
    );
    let literal = record(&["2001:db8::1"]);
    assert!(verify(
        &presented("k1", "s3cret"),
        &literal,
        at(2026, 1, 1),
        Some("2001:db8::1")
    )
    .is_ok());
}

#[test]
fn a_stored_hash_that_is_not_a_hash_verifies_nothing() {
    for broken in [
        "",
        "not-a-phc-string",
        "$argon2id$v=19$m=1,t=1,p=1$",
        "$unknown$abc",
    ] {
        let mut stored = record(&[]);
        stored.secret_hash = broken.to_owned();
        assert_eq!(
            verify(&presented("k1", "s3cret"), &stored, at(2026, 1, 1), None),
            Err(Rejected::WrongSecret),
            "{broken:?}",
        );
    }
}
