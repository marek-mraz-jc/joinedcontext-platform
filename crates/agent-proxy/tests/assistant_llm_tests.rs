//! The knowledge assistant as a caller of the model door (AG-109, AG-110, AG-111, T-3055).
//!
//! A stub realm answers introspection for the assistant's service-account token, for a person's
//! token of the same client and for a token of another client; a stub provider counts what
//! reaches it.

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::{app, authed, body_of, sample_run, Bases, PROXY_CLIENT};
use serde_json::json;
use tower::ServiceExt;
use wiremock::matchers::{body_string_contains, header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const INTROSPECT_PATH: &str = "/realms/jc/protocol/openid-connect/token/introspect";
const ASSISTANT_TOKEN: &str = "assistant-service-token";
const PERSON_TOKEN: &str = "a-person-of-the-assistant-client";
const PORTAL_TOKEN: &str = "the-portals-service-token";

async fn realm() -> MockServer {
    let realm = MockServer::start().await;
    for (token, azp, username) in [
        (
            ASSISTANT_TOKEN,
            "jc-assistant",
            "service-account-jc-assistant",
        ),
        (PERSON_TOKEN, "jc-assistant", "maria"),
        (PORTAL_TOKEN, "portal-api", "service-account-portal-api"),
    ] {
        Mock::given(method("POST"))
            .and(path(INTROSPECT_PATH))
            .and(body_string_contains(format!("token={token}")))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "active": true, "azp": azp, "username": username, "aud": [PROXY_CLIENT],
            })))
            .mount(&realm)
            .await;
    }
    // Any other token, as Keycloak answers one it does not know.
    Mock::given(method("POST"))
        .and(path(INTROSPECT_PATH))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"active": false})))
        .with_priority(10)
        .mount(&realm)
        .await;
    realm
}

async fn provider() -> MockServer {
    let provider = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .and(header("x-api-key", "mock-model-key"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "choices": [{"message": {"role": "assistant", "content": "Dobrý deň."}}],
            "usage": {"prompt_tokens": 70, "completion_tokens": 30, "total_tokens": 100}
        })))
        .mount(&provider)
        .await;
    provider
}

fn proxy(realm: &MockServer, provider: &MockServer) -> axum::Router {
    app(
        sample_run(false),
        Bases {
            model: provider.uri(),
            realm: Some(format!("{}/realms/jc", realm.uri())),
            ..Bases::default()
        },
    )
}

fn call(token: &str, deployment: Option<&str>, cap: Option<&str>, rest: &str) -> Request<Body> {
    let mut request = Request::builder()
        .method("POST")
        .uri(format!("/v1/llm/{rest}"))
        .header("authorization", format!("Bearer {token}"));
    if let Some(deployment) = deployment {
        request = request.header("x-jc-assistant-deployment", deployment);
    }
    if let Some(cap) = cap {
        request = request.header("x-jc-assistant-tokens-per-day", cap);
    }
    request
        .body(Body::from(r#"{"model":"m","messages":[]}"#))
        .expect("a request")
}

async fn reached(provider: &MockServer) -> usize {
    provider.received_requests().await.unwrap_or_default().len()
}

/// AG-109, AG-110: the assistant's service account buys model time for a deployment until the
/// deployment's day is spent, and another deployment's day is its own.
#[tokio::test]
async fn the_assistant_calls_the_model_for_a_deployment_until_its_day_is_spent() {
    let (realm, provider) = (realm().await, provider().await);
    let proxy = proxy(&realm, &provider);
    for _ in 0..2 {
        let response = proxy
            .clone()
            .oneshot(call(
                ASSISTANT_TOKEN,
                Some("bb/public"),
                Some("200"),
                "v1/chat/completions",
            ))
            .await
            .expect("an answer");
        assert_eq!(response.status(), StatusCode::OK);
        assert!(body_of(response).await.contains("Dobrý deň."));
    }
    let spent = proxy
        .clone()
        .oneshot(call(
            ASSISTANT_TOKEN,
            Some("bb/public"),
            Some("200"),
            "v1/chat/completions",
        ))
        .await
        .expect("an answer");
    assert_eq!(spent.status(), StatusCode::TOO_MANY_REQUESTS);
    let text = body_of(spent).await;
    assert!(
        text.contains("bb/public") && text.contains("tokensPerDay"),
        "{text}"
    );
    assert_eq!(
        reached(&provider).await,
        2,
        "the spent call never reached the provider"
    );

    let other = proxy
        .oneshot(call(
            ASSISTANT_TOKEN,
            Some("bb/ckan"),
            Some("200"),
            "v1/chat/completions",
        ))
        .await
        .expect("an answer");
    assert_eq!(other.status(), StatusCode::OK);
}

/// AG-109: a person's token of the assistant's client, another client's service account, or an
/// unknown token buys nothing, and the provider is never asked.
#[tokio::test]
async fn only_the_assistants_service_account_is_served() {
    let (realm, provider) = (realm().await, provider().await);
    for token in [PERSON_TOKEN, PORTAL_TOKEN, "made-up"] {
        let response = proxy(&realm, &provider)
            .oneshot(call(
                token,
                Some("bb/public"),
                Some("1000"),
                "v1/chat/completions",
            ))
            .await
            .expect("an answer");
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED, "{token}");
    }
    let no_bearer = proxy(&realm, &provider)
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/llm/v1/chat/completions")
                .header("x-jc-assistant-deployment", "bb/public")
                .header("x-jc-assistant-tokens-per-day", "1000")
                .body(Body::from("{}"))
                .expect("a request"),
        )
        .await
        .expect("an answer");
    assert_eq!(no_bearer.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(reached(&provider).await, 0);
}

/// AG-110: a deployment that is not `{project}/{name}`, and a missing, zero or unreadable cap,
/// are refused: no deployment runs without a day's budget.
#[tokio::test]
async fn a_call_without_a_deployment_or_a_cap_is_refused() {
    let (realm, provider) = (realm().await, provider().await);
    for (deployment, cap) in [
        ("bb", Some("1000")),
        ("bb/", Some("1000")),
        ("BB/public", Some("1000")),
        ("bb/public/x", Some("1000")),
        ("bb/public", None),
        ("bb/public", Some("0")),
        ("bb/public", Some("-5")),
        ("bb/public", Some("lots")),
    ] {
        let response = proxy(&realm, &provider)
            .oneshot(call(
                ASSISTANT_TOKEN,
                Some(deployment),
                cap,
                "v1/chat/completions",
            ))
            .await
            .expect("an answer");
        assert_eq!(
            response.status(),
            StatusCode::BAD_REQUEST,
            "{deployment} {cap:?}"
        );
    }
    assert_eq!(reached(&provider).await, 0);
}

/// AG-111: the assistant's door is the completion endpoints alone, refused before the realm is
/// asked; and a realm that cannot be reached is the platform's failure, not the caller's.
#[tokio::test]
async fn the_assistant_reaches_the_completion_endpoints_alone() {
    let (realm, provider) = (realm().await, provider().await);
    let response = proxy(&realm, &provider)
        .oneshot(call(
            ASSISTANT_TOKEN,
            Some("bb/public"),
            Some("1000"),
            "v1/models",
        ))
        .await
        .expect("an answer");
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    assert!(realm
        .received_requests()
        .await
        .unwrap_or_default()
        .is_empty());

    let gone = app(
        sample_run(false),
        Bases {
            model: provider.uri(),
            realm: Some("http://127.0.0.1:9/realms/jc".to_owned()),
            ..Bases::default()
        },
    )
    .oneshot(call(
        ASSISTANT_TOKEN,
        Some("bb/public"),
        Some("1000"),
        "v1/chat/completions",
    ))
    .await
    .expect("an answer");
    assert_eq!(gone.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(reached(&provider).await, 0);
}

/// AG-109: naming a deployment does not let a run spend its day: the run's ticket is not the
/// assistant's token.
#[tokio::test]
async fn a_run_naming_a_deployment_is_not_the_assistant() {
    let (realm, provider) = (realm().await, provider().await);
    let response = proxy(&realm, &provider)
        .oneshot(
            authed("POST", "/v1/llm/v1/chat/completions")
                .header("x-jc-assistant-deployment", "bb/public")
                .header("x-jc-assistant-tokens-per-day", "1000")
                .body(Body::from("{}"))
                .expect("a request"),
        )
        .await
        .expect("an answer");
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(reached(&provider).await, 0);
}
