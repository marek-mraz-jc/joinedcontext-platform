//! Edge cases of `app::unauthorized` (T-2497, AG-32, R20).
//!
//! **The contract.** An unauthenticated call on a non-public MCP instance answers `401` with
//! `WWW-Authenticate: Bearer resource_metadata="…"`, whose URL names the endpoint's own
//! `/.well-known/oauth-protected-resource` under the configured public base, for every slug,
//! resolvable or not.
//!
//! **Inputs.** The `{slug}` path segment as axum decoded it, and the configured public base. The
//! slug is interpolated into a quoted-string of a header, so every case here is a shape of it.
//! Four of them were red and are fixed by T-2509: a slug outside the slug alphabet gets the 401
//! without a challenge.

mod common;

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use context_gateway::app::{router, Gateway};
use context_gateway::auth::accounts::ServiceAccounts;
use context_gateway::pdp::PolicyPdp;
use context_gateway::proxy::Broker;
use tower::ServiceExt;

const HOST: &str = "https://bb.example.sk";

fn app() -> axum::Router {
    let realm = common::Realm::new();
    router(std::sync::Arc::new(
        Gateway::new(
            Broker::new("http://broker.internal.svc:1026"),
            Box::new(PolicyPdp),
            "banskabystrica.sk",
        )
        .authenticate(
            std::sync::Arc::new(realm.verifier()),
            ServiceAccounts::new(),
            Some(HOST.to_owned()),
        ),
    ))
}

/// One unauthenticated MCP message to `raw_slug` (as written in the path): the status, every
/// `WWW-Authenticate` value, and the body.
async fn challenge(raw_slug: &str) -> (StatusCode, Vec<String>, String) {
    let response = app()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/api/endpoint/{raw_slug}/mcp"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#,
                ))
                .expect("a request"),
        )
        .await
        .expect("the gateway answers");
    let status = response.status();
    let challenges = response
        .headers()
        .get_all(header::WWW_AUTHENTICATE)
        .iter()
        .map(|value| String::from_utf8_lossy(value.as_bytes()).into_owned())
        .collect();
    let body = axum::body::to_bytes(response.into_body(), 64 * 1024)
        .await
        .expect("a readable body");
    (
        status,
        challenges,
        String::from_utf8_lossy(&body).into_owned(),
    )
}

/// The `resource_metadata` URL of a challenge, when the challenge is exactly one parameter.
fn metadata_url(challenge: &str) -> Option<&str> {
    let url = challenge
        .strip_prefix("Bearer resource_metadata=\"")?
        .strip_suffix('"')?;
    (!url.contains('"')).then_some(url)
}

/// What a client does with the URL: resolve its dot segments. The path must still be the one
/// endpoint's own metadata document.
fn names_one_endpoints_metadata(url: &str) -> bool {
    let Some(path) = url.strip_prefix(HOST) else {
        return false;
    };
    let mut segments: Vec<&str> = Vec::new();
    for segment in path.split('/').skip(1) {
        match segment {
            "." => {}
            ".." => {
                segments.pop();
            }
            other => segments.push(other),
        }
    }
    segments.len() == 5
        && segments[..2] == ["api", "endpoint"]
        && segments[3..] == [".well-known", "oauth-protected-resource"]
}

/// Every hostile shape answers 401, and a challenge it carries is one parameter naming this
/// endpoint's own metadata. A shape `HeaderValue` refuses answers 401 with no challenge.
async fn assert_contained(raw_slug: &str) {
    let (status, challenges, body) = challenge(raw_slug).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{raw_slug}: {body}");
    assert!(challenges.len() <= 1, "{raw_slug}: {challenges:?}");
    if let Some(challenge) = challenges.first() {
        let url = metadata_url(challenge).unwrap_or_else(|| {
            panic!("{raw_slug}: the challenge is not one parameter: {challenge}")
        });
        assert!(
            names_one_endpoints_metadata(url),
            "{raw_slug}: resource_metadata points elsewhere: {url}",
        );
    }
}

/// AG-32: a quote in the slug cannot end the quoted-string and add a parameter of its own.
#[tokio::test]
async fn a_slug_with_a_double_quote_cannot_close_the_resource_metadata_parameter() {
    assert_contained("x%22%2C%20error%3D%22invalid_token").await;
    assert_contained("x%22%2CBasic%20realm%3D%22evil").await;
    assert_contained("%22").await;
}

/// AG-32: CR and LF in the slug never make a second header or a second challenge.
#[tokio::test]
async fn a_slug_with_cr_lf_never_produces_a_second_header() {
    for raw in ["a%0D%0ASet-Cookie%3A%20x%3D1", "a%0Ab", "a%0Db"] {
        let (status, challenges, _) = challenge(raw).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{raw}");
        assert!(
            challenges
                .iter()
                .all(|c| !c.contains('\n') && !c.contains('\r')),
            "{raw}"
        );
        assert!(challenges.len() <= 1, "{raw}: {challenges:?}");
    }
}

/// AG-32: a NUL byte is still the refusal the client can act on, not a server error.
#[tokio::test]
async fn a_slug_with_a_nul_byte_answers_401_not_500() {
    assert_contained("a%00b").await;
}

/// AG-32: whatever `HeaderValue` refuses, the answer is the 401, with its challenge or without.
/// Struck: a slug that is not UTF-8 (`%FF`) never reaches the function, axum's `Path` answers 400.
#[tokio::test]
async fn a_slug_that_header_value_rejects_still_answers_401_with_a_challenge_or_none_never_a_panic()
{
    for raw in ["%7F", "%01", "a%09b"] {
        assert_contained(raw).await;
    }
}

/// AG-32: dot segments in the slug do not move the metadata URL to another path.
#[tokio::test]
async fn a_percent_encoded_dot_dot_slash_slug_does_not_point_resource_metadata_at_another_path() {
    for raw in [
        "%2e%2e",
        "%2e%2e%2f",
        "..%2f..%2fadmin",
        "%2E%2E%2F%2E%2E%2Fx",
        "a%2fb",
    ] {
        assert_contained(raw).await;
    }
}

/// AG-32: a slug outside ASCII reaches the header encoded, or not at all, never as raw bytes.
#[tokio::test]
async fn a_unicode_slug_is_encoded_not_passed_raw_into_the_header() {
    let (status, challenges, _) = challenge("%C5%BEiar-nad-hronom").await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    for challenge in &challenges {
        assert!(challenge.is_ascii(), "{challenge}");
    }
    assert_contained("%C5%BEiar-nad-hronom").await;
}

/// AG-32: a long slug makes a challenge of its own length plus the fixed text, no more.
#[tokio::test]
async fn a_slug_of_one_thousand_characters_is_bounded() {
    let slug = "a".repeat(1000);
    let (status, challenges, _) = challenge(&slug).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    for challenge in &challenges {
        assert!(
            challenge.len() < slug.len() + 200,
            "{} bytes",
            challenge.len()
        );
    }
    assert_contained(&slug).await;
}

/// AG-32: a backslash, the quoted-string escape, cannot escape the closing quote.
#[tokio::test]
async fn a_backslash_in_the_slug_is_escaped() {
    for raw in ["a%5C", "a%5C%22", "%5C%5C"] {
        assert_contained(raw).await;
    }
}

/// R20: the challenge names the public base the deployment is reached at, never the broker.
#[tokio::test]
async fn the_challenge_names_the_public_base_never_an_internal_host() {
    let (status, challenges, _) = challenge("k4y7pq2mzt6vhx3nbwrs5cjd8f").await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(
        challenges,
        [format!(
            "Bearer resource_metadata=\"{HOST}/api/endpoint/k4y7pq2mzt6vhx3nbwrs5cjd8f/.well-known/oauth-protected-resource\""
        )],
    );
    assert!(!challenges[0].contains("internal"), "{}", challenges[0]);
}

/// R20: the body is the problem document and nothing of a token or the broker.
#[tokio::test]
async fn the_401_body_carries_no_token_or_upstream_detail() {
    let (status, _, body) = challenge("k4y7pq2mzt6vhx3nbwrs5cjd8f").await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let problem: serde_json::Value = serde_json::from_str(&body).expect("a problem document");
    assert_eq!(problem["status"], 401);
    for word in ["broker", "internal", "1026", "eyJ", "Bearer "] {
        assert!(!body.contains(word), "{word} in {body}");
    }
}
