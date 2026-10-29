//! The gateway's own workload identity when it reads the Portal's preview list (T-1500; PF-46,
//! AG-52).
//!
//! The Portal's internal listener answers a caller it can name: the gateway's confidential client,
//! with a token audience-bound to that listener. Here the realm is a small server of its own, so
//! the grant, the caching and a refusal are asserted rather than assumed.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use axum::extract::Form;
use axum::routing::post;
use axum::Router;
use serde::Deserialize;
use serde_json::{json, Value};

use context_gateway::previews::WorkloadToken;

const CLIENT: &str = "context-gateway";
const SECRET: &str = "not-a-real-secret";

#[derive(Deserialize)]
struct Grant {
    grant_type: String,
    client_id: String,
    client_secret: String,
}

/// A realm whose token endpoint answers `lifetime` seconds of validity, counting the grants it is
/// asked for and what it was asked with.
async fn realm(
    lifetime: u64,
    refuse: bool,
) -> (String, Arc<AtomicUsize>, Arc<std::sync::Mutex<Vec<String>>>) {
    let asked = Arc::new(AtomicUsize::new(0));
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let counter = Arc::clone(&asked);
    let grants = Arc::clone(&seen);
    let app = Router::new().route(
        "/realms/dev/protocol/openid-connect/token",
        post(move |Form(grant): Form<Grant>| {
            let counter = Arc::clone(&counter);
            let grants = Arc::clone(&grants);
            async move {
                counter.fetch_add(1, Ordering::SeqCst);
                if let Ok(mut seen) = grants.lock() {
                    seen.push(format!(
                        "{} {} {}",
                        grant.grant_type, grant.client_id, grant.client_secret
                    ));
                }
                if refuse {
                    // A realm that refuses names the reason in its body; the gateway must not put
                    // that body in a log line, so the test only needs the status here.
                    return (
                        axum::http::StatusCode::UNAUTHORIZED,
                        axum::Json(json!({ "error": "invalid_client" })),
                    );
                }
                (
                    axum::http::StatusCode::OK,
                    axum::Json(json!({
                        "access_token": "the-gateways-token",
                        "token_type": "Bearer",
                        "expires_in": lifetime,
                    })),
                )
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("a free port");
    let address = listener.local_addr().expect("the bound address");
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (
        format!("http://{address}/realms/dev/protocol/openid-connect/token"),
        asked,
        seen,
    )
}

/// PF-46, AG-52: the gateway obtains its own token with `client_credentials` on its own client,
/// and holds it instead of asking the realm for every poll — the list is read every ten seconds.
#[tokio::test]
async fn the_gateway_mints_its_own_token_once_and_holds_it() {
    let (token_url, asked, seen) = realm(300, false).await;
    let token = WorkloadToken::new(token_url, CLIENT.to_owned(), SECRET.to_owned());

    assert_eq!(token.get().await.expect("a token"), "the-gateways-token");
    assert_eq!(token.get().await.expect("a token"), "the-gateways-token");

    assert_eq!(asked.load(Ordering::SeqCst), 1, "the realm was asked once");
    let grants = seen.lock().expect("the grants");
    assert_eq!(grants[0], format!("client_credentials {CLIENT} {SECRET}"));
}

/// A token that dies before the next poll is replaced rather than presented: the Portal would
/// answer 401 and the previews would stop being served.
#[tokio::test]
async fn a_token_about_to_expire_is_minted_again() {
    let (token_url, asked, _) = realm(5, false).await;
    let token = WorkloadToken::new(token_url, CLIENT.to_owned(), SECRET.to_owned());

    token.get().await.expect("a token");
    token.get().await.expect("a token");

    assert_eq!(asked.load(Ordering::SeqCst), 2, "the realm was asked again");
}

/// A refused grant is an error the caller logs and retries on the next tick, not a panic and not
/// a poll without a credential.
#[tokio::test]
async fn a_refused_grant_is_an_error_and_names_no_secret() {
    let (token_url, _, _) = realm(300, true).await;
    let token = WorkloadToken::new(token_url, CLIENT.to_owned(), SECRET.to_owned());

    let error = token.get().await.expect_err("no token");
    assert!(error.contains("401"), "{error}");
    assert!(
        !error.contains(SECRET),
        "the reason carries no secret: {error}"
    );
    assert!(
        !error.contains("invalid_client"),
        "the realm's own body is not logged: {error}"
    );
}

/// The list the Portal answers is read with that token in the `Authorization` header, which is
/// what the internal listener names its caller by.
#[tokio::test]
async fn the_preview_list_is_read_with_the_token_in_the_header() {
    let (token_url, _, _) = realm(300, false).await;
    let token = Arc::new(WorkloadToken::new(
        token_url,
        CLIENT.to_owned(),
        SECRET.to_owned(),
    ));
    let bearer = token.get().await.expect("a token");

    // What `follow` sends, asserted where it can be read: the header a Portal sees.
    let seen: Arc<std::sync::Mutex<Vec<String>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
    let headers = Arc::clone(&seen);
    let app = Router::new().route(
        "/internal/previews",
        axum::routing::get(move |request: axum::extract::Request| {
            let headers = Arc::clone(&headers);
            async move {
                let value = request
                    .headers()
                    .get(axum::http::header::AUTHORIZATION)
                    .and_then(|value| value.to_str().ok())
                    .unwrap_or_default()
                    .to_owned();
                if let Ok(mut seen) = headers.lock() {
                    seen.push(value);
                }
                axum::Json(json!({ "items": [] }) as Value)
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("a free port");
    let address = listener.local_addr().expect("the bound address");
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    let answer = reqwest::Client::new()
        .get(format!("http://{address}/internal/previews"))
        .bearer_auth(&bearer)
        .send()
        .await
        .expect("the list");
    assert!(answer.status().is_success());
    assert_eq!(
        seen.lock().expect("the headers")[0],
        format!("Bearer {bearer}")
    );
}
