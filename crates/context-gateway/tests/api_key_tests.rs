use argon2::password_hash::{PasswordHasher, SaltString};
use argon2::Argon2;
use chrono::{DateTime, Utc};
use context_gateway::auth::api_key::{parse, verify, KeyRecord, PresentedKey, Rejected};

const SECRET: &str = "9f4c1d2e_b7a3_4e51_8c0d_2f6a91be44d7";

fn at(instant: &str) -> DateTime<Utc> {
    instant.parse().expect("a fixed instant")
}

/// The stored hash of [`SECRET`]. A fixed salt keeps the test deterministic; a real key is
/// salted from the OS, which is the one thing a test must not do if it wants to compare
/// against a constant.
fn hash(secret: &str) -> String {
    let salt = SaltString::from_b64("Y2l2aXRhcy1zYWx0").expect("a valid b64 salt");
    Argon2::default()
        .hash_password(secret.as_bytes(), &salt)
        .expect("argon2 hashes")
        .to_string()
}

fn record() -> KeyRecord {
    KeyRecord {
        key_id: "k1".to_owned(),
        service_account: "etl-ovzdusie".to_owned(),
        project: "ovzdusie".to_owned(),
        secret_hash: hash(SECRET),
        expires_at: Some(at("2026-12-31T00:00:00Z")),
        ip_allow_list: Vec::new(),
    }
}

fn presented(secret: &str) -> PresentedKey<'_> {
    PresentedKey {
        key_id: "k1",
        secret,
    }
}

/// PF-36: the stored hash is what the secret is checked against, and a matching secret is
/// the only thing that passes.
#[test]
fn a_matching_secret_verifies_and_a_wrong_one_does_not() {
    verify(
        &presented(SECRET),
        &record(),
        at("2026-09-06T12:00:00Z"),
        None,
    )
    .expect("the right secret");

    for wrong in [
        "",
        "9f4c1d2e_b7a3_4e51_8c0d_2f6a91be44d8",
        &SECRET.to_uppercase(),
        &SECRET[..SECRET.len() - 1],
    ] {
        assert_eq!(
            verify(
                &presented(wrong),
                &record(),
                at("2026-09-06T12:00:00Z"),
                None
            ),
            Err(Rejected::WrongSecret),
            "{wrong:?} was accepted"
        );
    }

    assert_eq!(
        verify(
            &PresentedKey {
                key_id: "k2",
                secret: SECRET
            },
            &record(),
            at("2026-09-06T12:00:00Z"),
            None
        ),
        Err(Rejected::UnknownKey)
    );
}

/// A stored hash that is not a PHC string verifies nothing. A corrupt row must not become
/// a key that lets everything through.
#[test]
fn a_stored_hash_that_is_not_a_phc_string_verifies_nothing() {
    for corrupt in ["", "not-a-hash", SECRET, "$argon2id$v=19$m=19456"] {
        let mut broken = record();
        broken.secret_hash = corrupt.to_owned();
        assert_eq!(
            verify(
                &presented(SECRET),
                &broken,
                at("2026-09-06T12:00:00Z"),
                None
            ),
            Err(Rejected::WrongSecret),
            "{corrupt:?} was treated as a usable hash"
        );
    }
}

/// PF-37: the key stops working at its expiry, and the expiry instant itself is already
/// past it.
#[test]
fn expiry_is_enforced_at_the_instant_it_names() {
    let record = record();
    verify(
        &presented(SECRET),
        &record,
        at("2026-12-30T23:59:59Z"),
        None,
    )
    .expect("a second before expiry");

    for expired in ["2026-12-31T00:00:00Z", "2027-01-01T00:00:00Z"] {
        assert_eq!(
            verify(&presented(SECRET), &record, at(expired), None),
            Err(Rejected::Expired),
            "{expired} was accepted"
        );
    }

    let mut forever = record.clone();
    forever.expires_at = None;
    verify(
        &presented(SECRET),
        &forever,
        at("2099-01-01T00:00:00Z"),
        None,
    )
    .expect("a key with no expiry never expires");
}

/// PF-36: a key bound to an address is refused from anywhere else, and refused outright
/// when the gateway cannot tell where the caller is.
#[test]
fn the_ip_allow_list_is_enforced_including_when_the_address_is_unknown() {
    let mut bound = record();
    bound.ip_allow_list = vec!["10.4.0.7".to_owned(), "192.168.10.0/24".to_owned()];
    let now = at("2026-09-06T12:00:00Z");

    for allowed in ["10.4.0.7", "192.168.10.1", "192.168.10.255"] {
        verify(&presented(SECRET), &bound, now, Some(allowed))
            .unwrap_or_else(|e| panic!("{allowed}: {e}"));
    }

    for refused in ["10.4.0.8", "192.168.11.1", "not-an-address"] {
        assert_eq!(
            verify(&presented(SECRET), &bound, now, Some(refused)),
            Err(Rejected::AddressNotAllowed),
            "{refused} was accepted"
        );
    }

    assert_eq!(
        verify(&presented(SECRET), &bound, now, None),
        Err(Rejected::AddressNotAllowed),
        "an allow list that cannot be evaluated is not an allow list that passes"
    );

    // An empty list is not a list of nothing, it is no restriction at all.
    verify(&presented(SECRET), &record(), now, None).expect("no allow list, any address");
}

/// PF-37: the `jc_` prefix makes a leaked key recognisable, and the secret may carry the
/// separator itself, so only the first one is structural.
#[test]
fn parsing_splits_the_key_on_the_first_separator_after_the_prefix() {
    let header = format!("Bearer jc_k1_{SECRET}");
    let key = parse(&header).expect("a well-formed key");
    assert_eq!(key.key_id, "k1");
    assert_eq!(key.secret, SECRET, "the secret keeps its own underscores");

    for not_a_key in ["", "Bearer ", "Bearer eyJhbGciOiJSUzI1NiJ9.x.y", "jc_k1_s"] {
        assert_eq!(parse(not_a_key), Err(Rejected::NotAKey), "{not_a_key:?}");
    }

    for malformed in ["Bearer jc_k1", "Bearer jc__secret", "Bearer jc_k1_"] {
        assert_eq!(parse(malformed), Err(Rejected::Malformed), "{malformed:?}");
    }
}
