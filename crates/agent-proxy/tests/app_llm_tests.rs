//! An App's `ai` service as a caller of the model door (AP-169, T-3584): the Portal's service
//! account, naming the App, on the completion endpoints alone. A stub realm answers introspection
//! for the Portal's, the assistant's and a person's token; a stub provider counts what reaches it.

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::{app, body_of, sample_run, Bases, PROXY_CLIENT};
use serde_json::json;
use tower::ServiceExt;
use wiremock::matchers::{body_string_contains, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const INTROSPECT_PATH: &str = "/realms/jc/protocol/openid-connect/token/introspect";
const PORTAL_TOKEN: &str = "the-portals-service-token";
const ASSISTANT_TOKEN: &str = "assistant-service-token";
const PERSON_TOKEN: &str = "a-person-of-the-portal-client";

async fn realm() -> MockServer {
    let realm = MockServer::start().await;
    for (token, azp, username) in [
        (PORTAL_TOKEN, "portal-api", "service-account-portal-api"),
        (
            ASSISTANT_TOKEN,
            "jc-assistant",
            "service-account-jc-assistant",
        ),
        (PERSON_TOKEN, "portal-api", "maria"),
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
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "choices": [{"message": {"role": "assistant", "content": "Pothole, severity 3."}}],
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

fn call(token: &str, app: &str, rest: &str) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(format!("/v1/llm/{rest}"))
        .header("authorization", format!("Bearer {token}"))
        .header("x-jc-app", app)
        .body(Body::from(r#"{"model":"m","messages":[]}"#))
        .expect("a request")
}

async fn reached(provider: &MockServer) -> usize {
    provider.received_requests().await.unwrap_or_default().len()
}

#[tokio::test]
async fn the_portal_calls_the_model_for_an_app() {
    let (realm, provider) = (realm().await, provider().await);
    let response = proxy(&realm, &provider)
        .oneshot(call(
            PORTAL_TOKEN,
            "roads/road-defects",
            "v1/chat/completions",
        ))
        .await
        .expect("an answer");
    assert_eq!(response.status(), StatusCode::OK);
    let text = body_of(response).await;
    assert!(
        text.contains("Pothole") && text.contains("\"total_tokens\":100"),
        "{text}"
    );
    assert_eq!(reached(&provider).await, 1);
}

#[tokio::test]
async fn nobody_but_the_portal_and_no_path_but_a_completion_reaches_the_provider() {
    let (realm, provider) = (realm().await, provider().await);
    let proxy = proxy(&realm, &provider);
    for (token, app, rest, status) in [
        (
            ASSISTANT_TOKEN,
            "roads/road-defects",
            "v1/chat/completions",
            StatusCode::UNAUTHORIZED,
        ),
        (
            PERSON_TOKEN,
            "roads/road-defects",
            "v1/chat/completions",
            StatusCode::UNAUTHORIZED,
        ),
        (
            "not-a-token",
            "roads/road-defects",
            "v1/chat/completions",
            StatusCode::UNAUTHORIZED,
        ),
        (
            PORTAL_TOKEN,
            "road-defects",
            "v1/chat/completions",
            StatusCode::BAD_REQUEST,
        ),
        (
            PORTAL_TOKEN,
            "Roads/x y",
            "v1/chat/completions",
            StatusCode::BAD_REQUEST,
        ),
        (
            PORTAL_TOKEN,
            "roads/road-defects",
            "v1/models",
            StatusCode::FORBIDDEN,
        ),
    ] {
        let response = proxy
            .clone()
            .oneshot(call(token, app, rest))
            .await
            .expect("an answer");
        assert_eq!(response.status(), status, "{token} {app} {rest}");
    }
    assert_eq!(reached(&provider).await, 0);
}
