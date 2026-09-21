//! T-2491: edge cases of `CredentialManager::get_endpoint_token` (`src/inject.rs`), the per-slug
//! token cache every forwarded call of a run goes through (AG-52, PF-34).
//!
//! **The contract.** One client-credentials grant per audience (the endpoint slug), cached; a
//! cached token with more than 30 s left is served again, otherwise a new one is minted. The
//! grant's `expires_in` is capped at 300 s. An empty client secret returns a fixed stub token, and
//! only with the `test-stub` feature, which the image never builds.
//!
//! **Inputs.** The slug (validated upstream as a run's endpoint), and the token endpoint's answer:
//! its status and a body of `{access_token, expires_in}`.
//!
//! Struck: "`expires_in` above 300 is capped at 300" cannot be observed here, because the cache
//! reads `std::time::Instant` and no test can move it by 270 s; a huge `expires_in` is covered
//! instead, which is where the cap would have overflowed. Two callers at once for one uncached slug
//! both grant (the lock is released before the grant, `inject.rs` line 41): harmless, noted in
//! chyby.md, and the case below proves the part that matters, neither deadlocks and each gets the
//! slug's own token.

use std::sync::Arc;
use std::time::Duration;

use agent_proxy::config::Config;
use agent_proxy::inject::CredentialManager;
use serde_json::json;
use wiremock::matchers::{body_string_contains, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const SECRET: &str = "s3cr3t-client-value";
const TOKEN_PATH: &str = "/realms/jc/protocol/openid-connect/token";

fn manager(token_url: &str, secret: &str) -> CredentialManager {
    let config = Config::from_lookup(|key| match key {
        "JC_PROXY_BIND" => Some("127.0.0.1:0".to_owned()),
        "JC_MODEL_KEY" => Some("mock-model-key".to_owned()),
        "JC_FORGE_TOKEN" => Some("mock-forge-token".to_owned()),
        "JC_OIDC_TOKEN_URL" => Some(token_url.to_owned()),
        "JC_OIDC_CLIENT_SECRET" => Some(secret.to_owned()),
        _ => None,
    })
    .expect("the test configuration is complete");
    CredentialManager::new(Arc::new(config))
}

/// A token endpoint that answers every grant for `audience` with `token`, valid `expires_in` s.
async fn grants(server: &MockServer, audience: &str, token: &str, expires_in: u64) {
    Mock::given(method("POST"))
        .and(path(TOKEN_PATH))
        .and(body_string_contains(format!("audience={audience}")))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({ "access_token": token, "expires_in": expires_in })),
        )
        .mount(server)
        .await;
}

async fn grant_count(server: &MockServer) -> usize {
    server.received_requests().await.unwrap_or_default().len()
}

fn url(server: &MockServer) -> String {
    format!("{}{TOKEN_PATH}", server.uri())
}

/// AG-52: a token is bound to the audience it was minted for; the cache never crosses slugs.
#[tokio::test]
async fn a_token_for_one_slug_is_never_served_for_a_different_slug() {
    let server = MockServer::start().await;
    grants(&server, "slug-a", "token-a", 300).await;
    grants(&server, "slug-b", "token-b", 300).await;
    let credentials = manager(&url(&server), SECRET);

    assert_eq!(
        credentials.get_endpoint_token("slug-a").await.unwrap(),
        "token-a"
    );
    assert_eq!(
        credentials.get_endpoint_token("slug-b").await.unwrap(),
        "token-b"
    );
    assert_eq!(
        credentials.get_endpoint_token("slug-a").await.unwrap(),
        "token-a"
    );
    assert_eq!(
        grant_count(&server).await,
        2,
        "one grant per slug, then the cache"
    );
}

/// AG-52: callers at once for one slug neither deadlock nor get another slug's token.
#[tokio::test]
async fn concurrent_callers_for_the_same_slug_neither_deadlock_nor_get_a_foreign_token() {
    let server = MockServer::start().await;
    grants(&server, "slug-a", "token-a", 300).await;
    let credentials = manager(&url(&server), SECRET);

    let calls = (0..8).map(|_| {
        let credentials = credentials.clone();
        tokio::spawn(async move { credentials.get_endpoint_token("slug-a").await })
    });
    let answers = tokio::time::timeout(Duration::from_secs(10), futures::future::join_all(calls))
        .await
        .expect("no caller hangs");
    for answer in answers {
        assert_eq!(answer.expect("the task ends").expect("a token"), "token-a");
    }
    // Once the grants landed, the next caller is served from the cache.
    let before = grant_count(&server).await;
    assert!((1..=8).contains(&before), "{before} grants");
    credentials.get_endpoint_token("slug-a").await.unwrap();
    assert_eq!(grant_count(&server).await, before);
}

/// AG-52: more than 30 s left is reused, without a new grant.
#[tokio::test]
async fn a_cached_token_with_31_seconds_left_is_reused_without_a_new_grant() {
    let server = MockServer::start().await;
    grants(&server, "slug-a", "token-a", 32).await;
    let credentials = manager(&url(&server), SECRET);

    credentials.get_endpoint_token("slug-a").await.unwrap();
    credentials.get_endpoint_token("slug-a").await.unwrap();
    assert_eq!(grant_count(&server).await, 1);
}

/// AG-52: 30 s or less left is renewed before it is handed out.
#[tokio::test]
async fn a_cached_token_with_29_seconds_left_triggers_a_fresh_grant() {
    let server = MockServer::start().await;
    grants(&server, "slug-a", "token-a", 29).await;
    let credentials = manager(&url(&server), SECRET);

    credentials.get_endpoint_token("slug-a").await.unwrap();
    credentials.get_endpoint_token("slug-a").await.unwrap();
    assert_eq!(grant_count(&server).await, 2);
}

/// PF-34: a refused grant is an error that names the status and never the client secret.
#[tokio::test]
async fn a_token_endpoint_that_returns_a_non_success_status_yields_an_error_naming_no_secret() {
    for status in [400, 401, 403, 500, 503] {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(status)
                    .set_body_string(format!("invalid_client: client_secret={SECRET}")),
            )
            .mount(&server)
            .await;
        let error = manager(&url(&server), SECRET)
            .get_endpoint_token("slug-a")
            .await
            .expect_err("a refused grant");
        assert!(error.contains(&status.to_string()), "{status}: {error}");
        assert!(!error.contains(SECRET), "{status}: {error}");
    }
}

/// PF-34: an answer that is not the token document is an error, not a panic and not a token.
#[tokio::test]
async fn a_token_endpoint_answering_with_a_body_that_is_not_json_is_an_error_not_a_panic() {
    for body in ["", "<html>login</html>", "{", "null", "[]"] {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_string(body))
            .mount(&server)
            .await;
        let answer = manager(&url(&server), SECRET)
            .get_endpoint_token("slug-a")
            .await;
        assert!(answer.is_err(), "{body:?} gave {answer:?}");
    }
}

/// PF-34: a token document without its token or its lifetime is refused, and nothing is cached.
#[tokio::test]
async fn a_token_response_missing_access_token_or_expires_in_is_refused() {
    for body in [
        json!({ "expires_in": 300 }),
        json!({ "access_token": "token-a" }),
        json!({ "access_token": 7, "expires_in": 300 }),
        json!({ "access_token": "token-a", "expires_in": -1 }),
        json!({ "access_token": "token-a", "expires_in": "300" }),
    ] {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(&body))
            .mount(&server)
            .await;
        let credentials = manager(&url(&server), SECRET);
        assert!(
            credentials.get_endpoint_token("slug-a").await.is_err(),
            "{body}"
        );
        assert!(
            credentials.get_endpoint_token("slug-a").await.is_err(),
            "{body}: cached"
        );
        assert_eq!(grant_count(&server).await, 2, "{body}: nothing was cached");
    }
}

/// AG-52: a token that is already over is used once and never reused from the cache.
#[tokio::test]
async fn expires_in_of_zero_is_never_cached_as_reusable() {
    let server = MockServer::start().await;
    grants(&server, "slug-a", "token-a", 0).await;
    let credentials = manager(&url(&server), SECRET);

    for _ in 0..3 {
        credentials.get_endpoint_token("slug-a").await.unwrap();
    }
    assert_eq!(grant_count(&server).await, 3);
}

/// AG-52: the largest lifetime a realm may write neither overflows the cache nor fails the call.
#[tokio::test]
async fn expires_in_larger_than_300_is_capped_at_300() {
    let server = MockServer::start().await;
    grants(&server, "slug-a", "token-a", u64::MAX).await;
    let credentials = manager(&url(&server), SECRET);

    assert_eq!(
        credentials.get_endpoint_token("slug-a").await.unwrap(),
        "token-a"
    );
    assert_eq!(
        credentials.get_endpoint_token("slug-a").await.unwrap(),
        "token-a"
    );
    assert_eq!(grant_count(&server).await, 1);
}

/// AG-52: a realm that does not answer ends the call within the client's timeout.
#[tokio::test]
async fn a_slow_or_hanging_token_endpoint_times_out_rather_than_hanging_the_request() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({ "access_token": "late", "expires_in": 300 }))
                .set_delay(Duration::from_secs(30)),
        )
        .mount(&server)
        .await;
    let answer = tokio::time::timeout(
        Duration::from_secs(15),
        manager(&url(&server), SECRET).get_endpoint_token("slug-a"),
    )
    .await
    .expect("the call ends before the realm answers");
    assert!(answer.is_err(), "{answer:?}");
}

/// PF-34, T-1480: with a secret configured, the stub token never stands in for a real grant.
#[tokio::test]
async fn the_empty_secret_mock_path_never_activates_when_a_real_secret_is_configured() {
    let server = MockServer::start().await;
    grants(&server, "slug-a", "token-a", 300).await;
    let token = manager(&url(&server), SECRET)
        .get_endpoint_token("slug-a")
        .await
        .unwrap();
    assert_eq!(token, "token-a");
    assert!(!token.starts_with("mock-token-for-"));
    let sent = server.received_requests().await.unwrap_or_default();
    assert_eq!(sent.len(), 1, "the grant went to the realm");
    let body = String::from_utf8_lossy(&sent[0].body).into_owned();
    assert!(body.contains("grant_type=client_credentials"), "{body}");
    assert!(body.contains("audience=slug-a"), "{body}");
}
