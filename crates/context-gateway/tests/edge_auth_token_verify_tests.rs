//! Edge cases of `auth::token::Verifier::verify` (T-1901, EP-26, MP-02).
//!
//! Contract, in one sentence: a token passes only when it names a key this realm published,
//! its signature verifies under **that key's own algorithm**, its issuer is this realm exactly,
//! one of the audiences the caller named is in its `aud`, and the moment is inside its
//! validity — and every refusal is the same 401 to the caller, so a probe learns nothing from
//! which one it hit (PF-46, R20).
//!
//! The algorithm is the verifier's, never the token's: a token whose header says `none` or
//! `HS256` is checked with the algorithm the realm published for that `kid`, which is what
//! closes the algorithm-confusion family of attacks.
//!
//! `token_tests.rs` covers the happy path, another resource, another realm, another key, a
//! forged algorithm and an expired token. These are the ones around them: the audience list
//! empty and of many, the leeway at its bound, `nbf`, the issuer one character off, and the
//! shapes of a string that is not a JWT at all.

mod common;

use common::Realm;
use context_gateway::auth::token::{bearer, Rejected, Verifier};
use serde_json::{json, Value};

const SLUG: &str = "d7m2xq9vkt4zc6wrb8shj5nfp3";

/// A token bound to this endpoint, `lifetime` seconds from now.
fn claims(audience: Value, lifetime: i64) -> Value {
    json!({
        "iss": common::ISSUER,
        "sub": "f:1:marek",
        "aud": audience,
        "azp": "portal",
        "exp": common::in_seconds(lifetime),
        "iat": common::in_seconds(-10),
    })
}

fn audiences() -> Vec<String> {
    vec![SLUG.to_owned()]
}

/// `Claims` carries no `PartialEq`, and a refusal is what every case below compares: this is
/// the verification with the claims dropped, so the error is comparable on its own.
fn refusal(
    verified: Result<context_gateway::auth::token::Claims, Rejected>,
) -> Result<(), Rejected> {
    verified.map(|_| ())
}

#[test]
fn a_caller_that_names_no_audience_binds_the_token_to_nothing_and_is_refused() {
    let realm = Realm::new();
    let verifier = realm.verifier();
    let token = realm.mint(&claims(json!(SLUG), 300));

    assert_eq!(
        refusal(verifier.verify(&token, &[])),
        Err(Rejected::WrongAudience),
        "an empty audience list would refuse nothing, so it refuses everything",
    );
    assert!(verifier.verify(&token, &audiences()).is_ok());
}

/// The caller may name several values for one resource — the slug, the full URL — and the
/// token has to carry one of them. One that carries none is refused however many were offered.
#[test]
fn one_of_the_named_audiences_is_enough_and_none_is_not() {
    let realm = Realm::new();
    let verifier = realm.verifier();
    let many = vec![
        "https://2.28.67.127.sslip.io/api/endpoint/".to_owned() + SLUG,
        SLUG.to_owned(),
        "account".to_owned(),
    ];

    assert!(verifier
        .verify(&realm.mint(&claims(json!(SLUG), 300)), &many)
        .is_ok());
    assert!(verifier
        .verify(&realm.mint(&claims(json!([SLUG, "account"]), 300)), &many)
        .is_ok());
    assert_eq!(
        refusal(verifier.verify(&realm.mint(&claims(json!("another-endpoint"), 300)), &many)),
        Err(Rejected::WrongAudience),
    );
    assert_eq!(
        refusal(verifier.verify(&realm.mint(&claims(json!([]), 300)), &many)),
        Err(Rejected::WrongAudience),
        "an empty `aud` names no resource",
    );
}

/// An audience is compared exactly: a slug with anything around it is another resource.
#[test]
fn the_audience_is_compared_exactly() {
    let realm = Realm::new();
    let verifier = realm.verifier();

    for near in [
        format!("{SLUG} "),
        format!(" {SLUG}"),
        SLUG.to_uppercase(),
        format!("{SLUG}/"),
        format!("{SLUG}\n"),
        SLUG[..SLUG.len() - 1].to_owned(),
        format!("{SLUG}x"),
    ] {
        assert_eq!(
            refusal(verifier.verify(&realm.mint(&claims(json!(near), 300)), &audiences())),
            Err(Rejected::WrongAudience),
            "{near:?} is not this endpoint",
        );
    }
}

/// The leeway is one minute of clock skew between Keycloak and this pod, and it is a bound
/// like any other: a token a minute and a second past its expiry is refused.
#[test]
fn the_expiry_leeway_is_one_minute_and_not_a_second_more() {
    let realm = Realm::new();
    let verifier = realm.verifier();

    assert!(
        verifier
            .verify(&realm.mint(&claims(json!(SLUG), -30)), &audiences())
            .is_ok(),
        "half a minute past expiry is inside the skew window",
    );
    assert_eq!(
        refusal(verifier.verify(&realm.mint(&claims(json!(SLUG), -120)), &audiences())),
        Err(Rejected::Expired),
        "two minutes past it is not",
    );
}

/// A token that is not valid yet is refused the same way one that is no longer valid is: the
/// caller cannot tell them apart, and neither can a probe.
#[test]
fn a_token_that_is_not_valid_yet_is_refused_like_an_expired_one() {
    let realm = Realm::new();
    let verifier = realm.verifier();
    let mut early = claims(json!(SLUG), 3_600);
    early["nbf"] = json!(common::in_seconds(600));

    assert_eq!(
        refusal(verifier.verify(&realm.mint(&early), &audiences())),
        Err(Rejected::Expired),
    );

    let mut soon = claims(json!(SLUG), 3_600);
    soon["nbf"] = json!(common::in_seconds(30));
    assert!(
        verifier.verify(&realm.mint(&soon), &audiences()).is_ok(),
        "half a minute early is inside the same skew window",
    );
}

/// The issuer is this realm, character for character: a realm URL with a trailing slash, or
/// one hosted on a domain that merely starts the same way, is another realm.
#[test]
fn the_issuer_is_this_realm_exactly() {
    let realm = Realm::new();
    let verifier = realm.verifier();

    for other in [
        format!("{}/", common::ISSUER),
        common::ISSUER.replace("https", "http"),
        common::ISSUER.to_uppercase(),
        format!("{}.evil.example", common::ISSUER),
        common::ISSUER.replace("/realms/", "/realms//"),
        String::new(),
    ] {
        let mut elsewhere = claims(json!(SLUG), 300);
        elsewhere["iss"] = json!(other);
        assert_eq!(
            refusal(verifier.verify(&realm.mint(&elsewhere), &audiences())),
            Err(Rejected::WrongIssuer),
            "{other:?} is not this realm",
        );
    }
}

/// A verifier with no keys accepts nothing. It is the state every gateway starts in, and the
/// state a realm that publishes an empty JWKS puts it back into (T-1898, T-1900).
#[test]
fn a_verifier_holding_no_key_accepts_nothing() {
    let realm = Realm::new();
    let empty = Verifier::new(common::ISSUER);
    assert_eq!(empty.key_count(), 0);

    assert_eq!(
        refusal(empty.verify(&realm.mint(&claims(json!(SLUG), 300)), &audiences())),
        Err(Rejected::UnknownKey),
    );
}

/// What is not a JWT at all. Each of these is `Malformed`, and none of them panics, allocates
/// without bound or reaches a key.
#[test]
fn a_string_that_is_not_a_jwt_is_malformed_and_nothing_more() {
    let realm = Realm::new();
    let verifier = realm.verifier();

    for not_a_token in [
        "",
        " ",
        ".",
        "..",
        "a.b",
        "a.b.c.d",
        "not a token",
        "Bearer eyJ",
        "eyJhbGciOiJFUzI1NiJ9",
        "\0",
        "\u{feff}eyJ.eyJ.x",
        &"a".repeat(64 * 1024),
        &common::b64("{\"alg\":\"none\"}"),
    ] {
        assert_eq!(
            refusal(verifier.verify(not_a_token, &audiences())),
            Err(Rejected::Malformed),
            "{not_a_token:.32?} is not a token",
        );
    }
}

/// A token signed by this realm but naming a key it never published is refused before any
/// signature is checked: an unknown `kid` never triggers an outbound fetch, so a caller cannot
/// make the gateway do work on its own schedule (the module says so in as many words).
#[test]
fn an_unknown_key_id_is_refused_without_the_gateway_going_anywhere() {
    let realm = Realm::new();
    let verifier = realm.verifier();

    for kid in [
        "",
        " ",
        "realm-key-2",
        "REALM-KEY-1",
        "realm-key-1 ",
        "../realm-key-1",
    ] {
        assert_eq!(
            refusal(verifier.verify(
                &realm.mint_with_kid(kid, &claims(json!(SLUG), 300)),
                &audiences()
            )),
            Err(Rejected::UnknownKey),
            "{kid:?} is not a key this realm published",
        );
    }

    assert_eq!(
        refusal(verifier.verify(
            &realm.mint_without_kid(&claims(json!(SLUG), 300)),
            &audiences()
        )),
        Err(Rejected::Malformed),
        "a token that names no key names no key",
    );
}

/// The order of the checks, as a probe sees it: a token that is wrong in several ways is
/// refused for the first thing looked at, and every one of those answers is the same 401 with
/// the same body — so the order tells the caller nothing either (R20).
#[test]
fn every_refusal_is_the_same_answer_to_the_caller() {
    let refusals = [
        Rejected::NoToken,
        Rejected::Malformed,
        Rejected::UnknownKey,
        Rejected::BadSignature,
        Rejected::Expired,
        Rejected::WrongIssuer,
        Rejected::WrongAudience,
    ];
    let answers: Vec<jc_core::ProblemDetails> = refusals.iter().cloned().map(Into::into).collect();

    for answer in &answers {
        assert_eq!(answer.status, 401);
        assert_eq!(answer.title, answers[0].title);
        assert_eq!(answer.detail, answers[0].detail);
        assert_eq!(answer.type_uri, answers[0].type_uri);
        assert!(answer.extensions.is_empty(), "nothing occurrence-specific");
    }

    let body = serde_json::to_string(&answers[2]).expect("the answer serialises");
    assert!(!body.contains("key"), "not even which check failed: {body}");
}

/// The `Authorization` header, which is what hands `verify` its string. A platform API key is
/// a different credential with a different verifier, and it must not be mistaken for a JWT.
#[test]
fn the_bearer_header_yields_a_token_or_nothing_at_all() {
    assert_eq!(bearer(Some("Bearer abc")), Ok("abc"));
    assert_eq!(
        bearer(Some("Bearer  abc  ")),
        Ok("abc"),
        "the token is trimmed"
    );

    for not_a_bearer in [
        None,
        Some(""),
        Some("Bearer"),
        Some("Bearer "),
        Some("Bearer    "),
        Some("bearer abc"),
        Some("BEARER abc"),
        Some("Basic abc"),
        Some("Bearer\tabc"),
        Some(" Bearer abc"),
    ] {
        assert_eq!(
            bearer(not_a_bearer),
            Err(Rejected::NoToken),
            "{not_a_bearer:?} presents no bearer token",
        );
    }

    let key = format!(
        "Bearer {}whatever",
        context_gateway::auth::api_key::KEY_PREFIX
    );
    assert_eq!(
        bearer(Some(&key)),
        Err(Rejected::NoToken),
        "an API key is another credential, not a malformed JWT",
    );
}

/// The same token twice is the same answer, and a refused token in between changes nothing:
/// `verify` reads the key table and holds no state of its own.
#[test]
fn the_same_token_twice_verifies_the_same_way() {
    let realm = Realm::new();
    let verifier = realm.verifier();
    let good = realm.mint(&claims(json!(SLUG), 300));
    let bad = realm.mint(&claims(json!("another-endpoint"), 300));

    let first = verifier.verify(&good, &audiences()).map(|c| c.sub);
    assert!(verifier.verify(&bad, &audiences()).is_err());
    let second = verifier.verify(&good, &audiences()).map(|c| c.sub);

    assert_eq!(first, second);
    assert_eq!(first, Ok("f:1:marek".to_owned()));
}

/// What the claims carry through. `verify` returns them whole, including the members a
/// transfer token adds — the checking of those is `dataspace_token::subject`'s (T-1896).
#[test]
fn the_claims_that_come_back_are_the_ones_the_realm_signed() {
    let realm = Realm::new();
    let verifier = realm.verifier();
    let mut rich = claims(json!(SLUG), 300);
    rich["preferred_username"] = json!("marek");
    rich["realm_access"] = json!({ "roles": ["viewer"] });
    rich["groups"] = json!(["/administrators"]);
    rich["agreementId"] = json!("urn:uuid:9a1f-air-quality");
    rich["participant"] = json!("did:web:helsinki.fi");

    let verified = verifier
        .verify(&realm.mint(&rich), &audiences())
        .expect("it verifies");
    assert_eq!(verified.preferred_username.as_deref(), Some("marek"));
    assert_eq!(verified.roles(), ["viewer"]);
    assert_eq!(verified.groups, ["/administrators"]);
    assert_eq!(
        verified.agreement_id.as_deref(),
        Some("urn:uuid:9a1f-air-quality")
    );
    assert_eq!(verified.azp.as_deref(), Some("portal"));
}
