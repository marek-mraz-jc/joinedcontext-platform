//! Edge cases of `routes::events::handler` (T-1865).
//!
//! An event is one line of a conversation a person reads. It is the model's own words, so the
//! route bounds what one line may cost, refuses what is not an event, names the run from the
//! ticket, and scrubs a credential that slipped into the words before the Portal stores it.

mod common;

use axum::body::Body;
use axum::http::StatusCode;
use common::{app, authed, body_of, sample_run, Bases, RUN_ID, TICKET};
use tower::ServiceExt;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const CEILING: usize = 64 * 1024;

fn proxy(portal: &str, per_minute: u32) -> axum::Router {
    let mut run = sample_run(false);
    run.requests_per_minute = per_minute;
    app(
        run,
        Bases {
            portal: portal.to_owned(),
            ..Bases::default()
        },
    )
}

async fn portal_answering(status: u16, body: &str) -> MockServer {
    let portal = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/internal/agent-runs/events"))
        .respond_with(ResponseTemplate::new(status).set_body_string(body))
        .mount(&portal)
        .await;
    portal
}

/// What the Portal was handed, as JSON.
async fn last_posted(portal: &MockServer) -> serde_json::Value {
    let seen = portal.received_requests().await.expect("the mock recorded");
    serde_json::from_slice(&seen.last().expect("a request reached the Portal").body)
        .expect("the Portal is handed JSON")
}

/// AG-46: an event from a caller with no run credentials is not an event of any run.
#[tokio::test]
async fn an_event_without_credentials_is_refused_before_the_portal_is_asked() {
    let portal = MockServer::start().await;
    let response = proxy(&portal.uri(), 100)
        .oneshot(
            axum::http::Request::builder()
                .method("POST")
                .uri("/v1/runs/events")
                .body(Body::from(r#"{"kind":"log","payload":{}}"#))
                .expect("a request"),
        )
        .await
        .expect("an answer");

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert!(portal
        .received_requests()
        .await
        .unwrap_or_default()
        .is_empty());
}

/// AG-22: a ticket of another run writes into no run's conversation.
///
/// The Portal here holds no run and answers `404` to the lookup, which is what "no such run" is.
/// A Portal that cannot be reached at all is a failure of the platform's and answers `503`, not
/// `401` (T-2418); this case is about the caller's credential.
#[tokio::test]
async fn a_ticket_of_another_run_writes_no_event() {
    // A Portal that holds no run: every lookup is unmatched and answers 404.
    let portal = MockServer::start().await;
    for (run, ticket) in [(RUN_ID, "another-runs-ticket"), ("nobody", TICKET)] {
        let response = proxy(&portal.uri(), 100)
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/v1/runs/events")
                    .header("x-jc-run", run)
                    .header("x-jc-ticket", ticket)
                    .body(Body::from(r#"{"kind":"log","payload":{}}"#))
                    .expect("a request"),
            )
            .await
            .expect("an answer");
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED, "run '{run}'");
    }
}

/// AG-45, AG-46: the ceiling is counted on the bytes that arrived, before anything is parsed, so
/// a workspace cannot decide how much of the run store and of every reader's stream one line takes.
#[tokio::test]
async fn an_event_past_the_ceiling_is_refused_before_it_is_parsed() {
    let portal = portal_answering(200, "{}").await;
    // Past the ceiling and not even valid JSON: the size is what refuses it.
    let response = proxy(&portal.uri(), 100)
        .oneshot(
            authed("POST", "/v1/runs/events")
                .body(Body::from(vec![b'x'; CEILING + 1]))
                .expect("a request"),
        )
        .await
        .expect("an answer");

    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    let detail = body_of(response).await;
    assert!(detail.contains(&(CEILING + 1).to_string()), "{detail}");
    assert!(portal
        .received_requests()
        .await
        .unwrap_or_default()
        .is_empty());
}

/// AG-45: the event at the ceiling is still an event.
#[tokio::test]
async fn an_event_at_the_ceiling_is_accepted() {
    let portal = portal_answering(200, "{}").await;
    let filler = "a".repeat(CEILING - r#"{"kind":"log","payload":{"text":""}}"#.len());
    let body = format!(r#"{{"kind":"log","payload":{{"text":"{filler}"}}}}"#);
    assert_eq!(body.len(), CEILING);

    let response = proxy(&portal.uri(), 100)
        .oneshot(
            authed("POST", "/v1/runs/events")
                .body(Body::from(body))
                .expect("a request"),
        )
        .await
        .expect("an answer");
    assert_eq!(response.status(), StatusCode::OK);
}

/// AG-46: what is not an event is refused with the reason, and never forwarded as one.
#[tokio::test]
async fn a_body_that_is_not_an_event_is_refused_with_the_reason() {
    let portal = portal_answering(200, "{}").await;
    for sent in [
        "",
        "not json at all",
        "{}",
        "[]",
        "null",
        r#"{"kind":"log"}"#,
        r#"{"payload":{}}"#,
        r#"{"kind":null,"payload":{}}"#,
        r#"{"kind":7,"payload":{}}"#,
        r#"{"kind":["log"],"payload":{}}"#,
    ] {
        let response = proxy(&portal.uri(), 100)
            .oneshot(
                authed("POST", "/v1/runs/events")
                    .body(Body::from(sent))
                    .expect("a request"),
            )
            .await
            .expect("an answer");
        assert_eq!(
            response.status(),
            StatusCode::BAD_REQUEST,
            "'{sent}' passed"
        );
    }
    assert!(portal
        .received_requests()
        .await
        .unwrap_or_default()
        .is_empty());
}

/// AG-46, AG-52: the run the event belongs to is the ticket's. A `runId` in the body is not read,
/// so a workspace cannot write into another run's conversation.
#[tokio::test]
async fn the_run_comes_from_the_ticket_and_never_from_the_body() {
    let portal = portal_answering(200, "{}").await;
    let response = proxy(&portal.uri(), 100)
        .oneshot(
            authed("POST", "/v1/runs/events")
                .body(Body::from(
                    r#"{"runId":"11111111-2222-3333-4444-555555555555","kind":"log","payload":{"text":"hello"}}"#,
                ))
                .expect("a request"),
        )
        .await
        .expect("an answer");

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(last_posted(&portal).await["runId"], RUN_ID);
}

/// AG-40, AG-56, T-0957: a credential that slipped into the model's words is scrubbed before the
/// Portal stores it, streams it to every reader and reads it back on the next turn.
#[tokio::test]
async fn a_credential_in_the_words_is_scrubbed_before_the_portal_sees_it() {
    let portal = portal_answering(200, "{}").await;
    // Assembled here, so this source holds no string a secret scanner would take for a real one.
    let github = format!("ghp_{}", "abc123def456ghi789jkl012");
    let dsn = "postgresql://jc:s3cr3t-pw@db:5432/jc";
    let sent = serde_json::json!({
        "kind": "log",
        "payload": {
            "text": format!("cloned with {github}, then {dsn} refused the connection"),
            "authorization": "Bearer abcdefghijklmnop.qrstuvwxyz",
            "received": 42
        }
    })
    .to_string();

    let response = proxy(&portal.uri(), 100)
        .oneshot(
            authed("POST", "/v1/runs/events")
                .body(Body::from(sent))
                .expect("a request"),
        )
        .await
        .expect("an answer");
    assert_eq!(response.status(), StatusCode::OK);

    let stored = last_posted(&portal).await.to_string();
    for secret in [github.as_str(), "s3cr3t-pw", "abcdefghijklmnop.qrstuvwxyz"] {
        assert!(!stored.contains(secret), "'{secret}' reached the Portal");
    }
    assert!(
        stored.contains("refused the connection") && stored.contains("42"),
        "the shape of the event stays: {stored}"
    );
}

/// AG-40: an event that carries no credential reaches the Portal exactly as it was written, so
/// the redaction never costs a run its own words.
#[tokio::test]
async fn an_event_without_a_credential_travels_unchanged() {
    let portal = portal_answering(200, "{}").await;
    let payload = serde_json::json!({
        "text": "merge of 9f1c2b7d4e6a8c0b2d4f6a8c0e2b4d6f8a0c2e4b into main failed",
        "counters": { "received": 1200, "sent": 1190 },
        "list": [1, 2, 3],
        "flag": true,
        "nothing": null
    });
    let sent = serde_json::json!({ "kind": "log", "payload": payload }).to_string();

    let response = proxy(&portal.uri(), 100)
        .oneshot(
            authed("POST", "/v1/runs/events")
                .body(Body::from(sent))
                .expect("a request"),
        )
        .await
        .expect("an answer");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(last_posted(&portal).await["payload"], payload);
}

/// AG-46: a payload of any JSON shape is an event; the route does not decide what a kind means.
#[tokio::test]
async fn a_payload_of_any_shape_is_accepted() {
    let portal = portal_answering(200, "{}").await;
    for payload in ["{}", "[]", "\"text\"", "7", "true", "null"] {
        let sent = format!(r#"{{"kind":"log","payload":{payload}}}"#);
        let response = proxy(&portal.uri(), 100)
            .oneshot(
                authed("POST", "/v1/runs/events")
                    .body(Body::from(sent))
                    .expect("a request"),
            )
            .await
            .expect("an answer");
        assert_eq!(response.status(), StatusCode::OK, "payload {payload}");
    }
}

/// AG-41: the run's per-minute limit holds here too, and the refused event never reaches the
/// Portal — the limit is checked after the body is read and before anything is forwarded.
#[tokio::test]
async fn the_event_past_the_per_minute_limit_is_refused() {
    let portal = portal_answering(200, "{}").await;
    let app = proxy(&portal.uri(), 1);

    for expected in [StatusCode::OK, StatusCode::TOO_MANY_REQUESTS] {
        let response = app
            .clone()
            .oneshot(
                authed("POST", "/v1/runs/events")
                    .body(Body::from(r#"{"kind":"log","payload":{}}"#))
                    .expect("a request"),
            )
            .await
            .expect("an answer");
        assert_eq!(response.status(), expected);
    }
    assert_eq!(
        portal.received_requests().await.unwrap_or_default().len(),
        1
    );
}

/// AG-52: the Portal is told the proxy's own audience-bound token, never the workspace's.
#[tokio::test]
async fn the_portal_is_told_the_proxys_own_token() {
    let portal = portal_answering(200, "{}").await;
    let response = proxy(&portal.uri(), 100)
        .oneshot(
            authed("POST", "/v1/runs/events")
                .header("authorization", "Bearer the-workspaces-own-token")
                .body(Body::from(r#"{"kind":"log","payload":{}}"#))
                .expect("a request"),
        )
        .await
        .expect("an answer");
    assert_eq!(response.status(), StatusCode::OK);

    let seen = portal.received_requests().await.expect("the mock recorded");
    assert_eq!(
        seen.first()
            .expect("a request")
            .headers
            .get("authorization")
            .and_then(|value| value.to_str().ok()),
        Some("Bearer mock-token-for-portal-internal")
    );
}

/// AG-46: a Portal that refuses the event says so to the run, without its own address or the
/// proxy's token.
#[tokio::test]
async fn a_portal_refusal_is_relayed_without_its_address_or_token() {
    for upstream in [400u16, 401, 403, 404, 409, 429, 500] {
        let portal = portal_answering(upstream, r#"{"error":"no"}"#).await;
        let host = portal.address().to_string();

        let response = proxy(&portal.uri(), 100)
            .oneshot(
                authed("POST", "/v1/runs/events")
                    .body(Body::from(r#"{"kind":"log","payload":{}}"#))
                    .expect("a request"),
            )
            .await
            .expect("an answer");

        assert_eq!(response.status().as_u16(), upstream);
        let body = body_of(response).await;
        assert!(!body.contains(&host), "the Portal's address leaked: {body}");
        assert!(!body.contains("mock-token-for"), "a token leaked: {body}");
    }
}

/// AG-46: the events door is a POST; a GET on it is refused by the router.
#[tokio::test]
async fn the_events_door_takes_no_get() {
    let response = proxy("http://portal.invalid:8080", 100)
        .oneshot(
            authed("GET", "/v1/runs/events")
                .body(Body::empty())
                .expect("a request"),
        )
        .await
        .expect("an answer");
    assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
}
