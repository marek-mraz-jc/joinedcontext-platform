//! Edge cases of `auth::jwks::refresh` (T-1898, EP-26, MP-02).
//!
//! Contract, in one sentence: `refresh` fetches the realm's JWKS over the in-cluster `http://`
//! address and installs its asymmetric keys, reporting how many are now there — and when
//! anything about the fetch fails it says so and **leaves the keys the verifier already has**,
//! because a realm that is briefly unreachable must not take every authenticated caller down
//! with it (T-0228, PF-46).
//!
//! The URL is checked before anything is fetched: an `https://` JWKS URL is refused rather
//! than silently downgraded, and nothing else is a URL this gateway fetches over.
//!
//! Every case here runs a throwaway server on `127.0.0.1:0` and asks `refresh` to read from
//! it, so what is exercised is the real client, the real 1 MiB body cap and the real parse.

mod common;

use axum::extract::Request;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::any;
use axum::Router;
use common::Realm;
use context_gateway::auth::jwks::{refresh, JwksError};
use context_gateway::auth::token::Verifier;
use serde_json::json;

/// A server that answers every request with the same thing.
async fn serving(
    answer: impl Fn() -> axum::response::Response + Clone + Send + Sync + 'static,
) -> String {
    let app = Router::new().fallback(any(move |_: Request| {
        let answer = answer.clone();
        async move { answer() }
    }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("a free port");
    let address = listener.local_addr().expect("the bound address");
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    format!("http://{address}/realms/joinedcontext/protocol/openid-connect/certs")
}

/// An address nothing is listening on: a closed port on the loopback interface.
async fn nothing_there() -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("a free port");
    let address = listener.local_addr().expect("the bound address");
    drop(listener);
    format!("http://{address}/certs")
}

/// A verifier that already holds this realm's one key, which is the state a running gateway
/// is in when a refresh comes round.
fn loaded(realm: &Realm) -> Verifier {
    let verifier = realm.verifier();
    assert_eq!(verifier.key_count(), 1);
    verifier
}

#[tokio::test]
async fn an_https_jwks_url_is_refused_rather_than_downgraded() {
    let realm = Realm::new();
    let verifier = loaded(&realm);

    let refused = refresh(
        &verifier,
        "https://keycloak/realms/jc/protocol/openid-connect/certs",
    )
    .await
    .expect_err("https is not the in-cluster address");
    assert!(matches!(refused, JwksError::Url(_)));
    assert_eq!(
        verifier.key_count(),
        1,
        "and the keys it had are still there"
    );
}

#[tokio::test]
async fn nothing_but_an_http_url_is_fetched_from() {
    let realm = Realm::new();
    let verifier = loaded(&realm);

    for not_a_url in [
        "",
        " ",
        "keycloak/certs",
        "//keycloak/certs",
        "HTTP://keycloak/certs",
        "Http://keycloak/certs",
        "ftp://keycloak/certs",
        "file:///etc/passwd",
        "http:/keycloak/certs",
        " http://keycloak/certs",
        "javascript:alert(1)",
    ] {
        let refused = refresh(&verifier, not_a_url)
            .await
            .expect_err("{not_a_url:?} is not a URL the gateway fetches over");
        assert!(
            matches!(refused, JwksError::Url(_)),
            "{not_a_url:?} is refused before anything is fetched",
        );
    }
    assert_eq!(verifier.key_count(), 1);
}

/// A string that starts `http://` and is still not a URL: the parse failure is a URL error,
/// and it happens before any connection is made.
#[tokio::test]
async fn a_string_that_only_starts_like_a_url_is_a_url_error() {
    let realm = Realm::new();
    let verifier = loaded(&realm);

    for broken in ["http://", "http:// ", "http://["] {
        let refused = refresh(&verifier, broken).await.expect_err("not a URL");
        assert!(matches!(refused, JwksError::Url(_)), "{broken:?}");
    }

    // A host that parses and cannot be reached is the other error, not this one: hyper's `Uri`
    // accepts more than a resolver does, so `http://host:notaport/certs` fails on the way out
    // rather than on the way in. What matters is that it is refused and costs no key.
    assert!(
        refresh(&verifier, "http://host:notaport/certs")
            .await
            .is_err(),
        "a host nothing resolves is still refused",
    );
    assert_eq!(verifier.key_count(), 1);
}

#[tokio::test]
async fn a_realm_that_answers_its_jwks_installs_exactly_those_keys() {
    // A verifier with no key at all, which is what a gateway starts with.
    let verifier = Verifier::new(common::ISSUER);
    let url = serving(|| {
        axum::Json(json!({
            "keys": [{
                "kty": "EC",
                "crv": "P-256",
                "alg": "ES256",
                "use": "sig",
                "kid": "realm-key-1",
                "x": "f83OJ3D2xF1Bg8vub9tLe1gHMzV76e8Tus9uPHvRVEU",
                "y": "x_FEzRu9m36HLN_tue659LNpXW6pCyStikYjKIWI5a0",
            }]
        }))
        .into_response()
    })
    .await;

    assert_eq!(refresh(&verifier, &url).await, Ok(1));
    assert_eq!(verifier.key_count(), 1);
}

/// Every answer that is not a success is the same kind of failure, and none of them costs the
/// keys the gateway is running on.
#[tokio::test]
async fn an_answer_that_is_not_a_success_leaves_the_keys_alone() {
    let realm = Realm::new();
    for status in [
        StatusCode::MOVED_PERMANENTLY,
        StatusCode::FOUND,
        StatusCode::UNAUTHORIZED,
        StatusCode::FORBIDDEN,
        StatusCode::NOT_FOUND,
        StatusCode::TOO_MANY_REQUESTS,
        StatusCode::INTERNAL_SERVER_ERROR,
        StatusCode::SERVICE_UNAVAILABLE,
    ] {
        let verifier = loaded(&realm);
        let url = serving(move || (status, "").into_response()).await;

        let refused = refresh(&verifier, &url).await.expect_err("not a JWKS");
        assert!(
            matches!(refused, JwksError::Unreachable(_)),
            "{status} is not an answer to read keys from",
        );
        assert_eq!(verifier.key_count(), 1, "{status} costs no key");
    }
}

/// A redirect is not followed: the client the gateway builds does not chase one, so a realm
/// answering 302 to somewhere else is an unreachable realm rather than a fetch from wherever
/// the header pointed.
#[tokio::test]
async fn a_redirect_is_not_followed() {
    let realm = Realm::new();
    let verifier = loaded(&realm);
    let url = serving(|| {
        (
            StatusCode::FOUND,
            [(
                axum::http::header::LOCATION,
                "http://elsewhere.invalid/certs",
            )],
            "",
        )
            .into_response()
    })
    .await;

    assert!(matches!(
        refresh(&verifier, &url).await.expect_err("not followed"),
        JwksError::Unreachable(_),
    ));
    assert_eq!(verifier.key_count(), 1);
}

/// A body that is not a JWKS: not JSON at all, JSON of the wrong shape, an empty body, and one
/// whose `keys` member is not a list. Each is unreachable-in-the-sense-of-unreadable, and each
/// leaves the keys.
#[tokio::test]
async fn a_body_that_is_not_a_jwks_leaves_the_keys_alone() {
    let realm = Realm::new();
    for body in [
        "",
        "not json",
        "<html>keycloak is starting</html>",
        "null",
        "[]",
        "{}",
        r#"{"keys": "realm-key-1"}"#,
        r#"{"keys": [{"kty": "EC"}]}"#,
        r#"{"keys": [{"kty": "EC", "crv": "P-256", "alg": "ES999", "kid": "k", "x": "a", "y": "b"}]}"#,
    ] {
        let verifier = loaded(&realm);
        let url = serving(move || body.into_response()).await;

        let refused = refresh(&verifier, &url).await.expect_err("not a JWKS");
        assert!(
            matches!(refused, JwksError::Unreachable(_)),
            "{body:.40?} is not a JWKS",
        );
        assert_eq!(verifier.key_count(), 1, "{body:.40?} costs no key");
    }
}

/// The body is read with a cap: a realm — or something sitting where the realm should be —
/// that answers a gigabyte does not become a gigabyte in this pod's memory.
#[tokio::test]
async fn a_body_past_the_cap_is_refused_rather_than_read() {
    let realm = Realm::new();
    let verifier = loaded(&realm);
    let url = serving(|| {
        // Well past the 1 MiB `to_bytes` limit, and valid JSON as far as it goes.
        let mut body = String::from("{\"keys\": [");
        body.push_str(&"\"x\",".repeat(400_000));
        body.push_str("\"x\"]}");
        body.into_response()
    })
    .await;

    let refused = refresh(&verifier, &url).await.expect_err("past the cap");
    assert!(matches!(refused, JwksError::Unreachable(_)));
    assert_eq!(verifier.key_count(), 1);
}

#[tokio::test]
async fn a_realm_that_is_not_listening_leaves_the_keys_alone() {
    let realm = Realm::new();
    let verifier = loaded(&realm);
    let url = nothing_there().await;

    let refused = refresh(&verifier, &url)
        .await
        .expect_err("nothing is there");
    assert!(matches!(refused, JwksError::Unreachable(_)));
    assert_eq!(verifier.key_count(), 1, "a realm that is away costs no key");
}

/// The one answer that does cost the keys: a JWKS that parses and carries nothing usable. It
/// is a **success** — `Ok(0)` — and it replaces the table with an empty one, after which every
/// token is refused until the next refresh brings keys back. See the note in T-1898: this is
/// the one path where a realm answering badly, rather than not at all, locks the gateway out.
#[tokio::test]
async fn a_jwks_that_parses_with_nothing_usable_in_it_empties_the_table() {
    let realm = Realm::new();
    let verifier = loaded(&realm);
    let url = serving(|| axum::Json(json!({ "keys": [] })).into_response()).await;

    assert_eq!(refresh(&verifier, &url).await, Ok(0));
    assert_eq!(
        verifier.key_count(),
        0,
        "an empty JWKS is applied whole, like any other",
    );
}

/// What a failure says. The message goes into a log line, never to a caller — but it still
/// must not carry anything of the realm's beyond the address the operator configured, and
/// nothing at all of a token.
#[tokio::test]
async fn a_failure_says_what_happened_and_nothing_about_a_caller() {
    let realm = Realm::new();
    let verifier = loaded(&realm);
    let url = serving(|| (StatusCode::UNAUTHORIZED, "realm secret: hunter2").into_response()).await;

    let refused = refresh(&verifier, &url)
        .await
        .expect_err("401 is not a JWKS");
    let said = refused.to_string();
    assert!(
        said.contains("401"),
        "it says what the realm answered: {said}"
    );
    assert!(!said.contains("hunter2"), "never the body: {said}");
}
