//! Edge cases of `routes::mcp::handler` (T-1870).
//!
//! The operations registry is where a run changes the platform itself. The one thing that
//! decides whose permissions the call runs under is the run id in the path, and that comes from
//! the ticket — never from the message the workspace composed.

mod common;

use axum::body::Body;
use axum::http::StatusCode;
use common::{app, authed, body_of, sample_run, Bases, RUN_ID, TICKET};
use tower::ServiceExt;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const CEILING: usize = 256 * 1024;

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
        .and(path(format!("/internal/agent-runs/{RUN_ID}/mcp")))
        .respond_with(ResponseTemplate::new(status).set_body_string(body))
        .mount(&portal)
        .await;
    portal
}

/// AG-64: a message with no run credentials reaches no registry.
#[tokio::test]
async fn a_message_without_credentials_is_refused_before_the_portal_is_asked() {
    let portal = MockServer::start().await;
    let response = proxy(&portal.uri(), 100)
        .oneshot(
            axum::http::Request::builder()
                .method("POST")
                .uri("/v1/mcp")
                .body(Body::from(
                    r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#,
                ))
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

/// AG-22: a ticket of another run, and one nobody holds, are the same refusal.
#[tokio::test]
async fn a_ticket_of_another_run_calls_no_operation() {
    for (run, ticket) in [(RUN_ID, "another-runs-ticket"), ("nobody", TICKET)] {
        let response = proxy("http://portal.invalid:8080", 100)
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/v1/mcp")
                    .header("x-jc-run", run)
                    .header("x-jc-ticket", ticket)
                    .body(Body::from("{}"))
                    .expect("a request"),
            )
            .await
            .expect("an answer");
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED, "run '{run}'");
    }
}

/// AG-64, AG-70: the run in the path is the ticket's. A `runId` the message names is not read,
/// so a workspace cannot call the registry as another run.
#[tokio::test]
async fn the_run_comes_from_the_ticket_and_never_from_the_message() {
    let portal = portal_answering(200, r#"{"result":{}}"#).await;
    let response = proxy(&portal.uri(), 100)
        .oneshot(
            authed("POST", "/v1/mcp")
                .body(Body::from(
                    r#"{"jsonrpc":"2.0","id":1,"method":"tools/list","runId":"11111111-2222-3333-4444-555555555555","params":{"runId":"11111111-2222-3333-4444-555555555555"}}"#,
                ))
                .expect("a request"),
        )
        .await
        .expect("an answer");

    assert_eq!(response.status(), StatusCode::OK);
    let seen = portal.received_requests().await.expect("the mock recorded");
    let asked = seen.first().expect("one request reached the Portal");
    assert_eq!(
        asked.url.path(),
        format!("/internal/agent-runs/{RUN_ID}/mcp"),
        "the path names the ticket's run"
    );
}

/// AG-45: a message past the route's ceiling is refused on the bytes that arrived, and the one
/// at the ceiling is not.
#[tokio::test]
async fn a_message_past_the_ceiling_is_refused_and_the_one_at_it_is_not() {
    let portal = portal_answering(200, "{}").await;
    for (size, expected) in [
        (CEILING, StatusCode::OK),
        (CEILING + 1, StatusCode::PAYLOAD_TOO_LARGE),
    ] {
        let response = proxy(&portal.uri(), 100)
            .oneshot(
                authed("POST", "/v1/mcp")
                    .body(Body::from(vec![b'x'; size]))
                    .expect("a request"),
            )
            .await
            .expect("an answer");
        assert_eq!(response.status(), expected, "a message of {size} bytes");
    }
}

/// AG-41: the run's per-minute limit holds on this door too, and the refused call never reaches
/// the Portal.
#[tokio::test]
async fn the_call_past_the_per_minute_limit_is_refused() {
    let portal = portal_answering(200, "{}").await;
    let app = proxy(&portal.uri(), 1);

    for expected in [StatusCode::OK, StatusCode::TOO_MANY_REQUESTS] {
        let response = app
            .clone()
            .oneshot(
                authed("POST", "/v1/mcp")
                    .body(Body::from("{}"))
                    .expect("a request"),
            )
            .await
            .expect("an answer");
        assert_eq!(response.status(), expected);
    }
    assert_eq!(
        portal.received_requests().await.unwrap_or_default().len(),
        1,
        "the refused call never reached the Portal"
    );
}

/// AG-52: the Portal is told the proxy's own audience-bound token; nothing the workspace set in
/// that slot reaches it.
#[tokio::test]
async fn the_portal_is_told_the_proxys_own_token_and_nothing_of_the_workspace() {
    let portal = portal_answering(200, "{}").await;
    let response = proxy(&portal.uri(), 100)
        .oneshot(
            authed("POST", "/v1/mcp")
                .header("authorization", "Bearer the-workspaces-own-token")
                .header("cookie", "session=stolen")
                .body(Body::from("{}"))
                .expect("a request"),
        )
        .await
        .expect("an answer");
    assert_eq!(response.status(), StatusCode::OK);

    let seen = portal.received_requests().await.expect("the mock recorded");
    let headers = &seen
        .first()
        .expect("one request reached the Portal")
        .headers;
    assert_eq!(
        headers
            .get("authorization")
            .and_then(|value| value.to_str().ok()),
        Some("Bearer mock-token-for-portal-internal"),
        "the Portal is told the proxy's own token"
    );
    for forbidden in ["cookie", "x-jc-run", "x-jc-ticket"] {
        assert!(
            headers.get(forbidden).is_none(),
            "'{forbidden}' was relayed to the Portal"
        );
    }
}

/// AG-64: the bytes of the message travel as they arrived — the proxy authenticates the run and
/// does not rewrite what the run said.
#[tokio::test]
async fn the_message_reaches_the_portal_as_it_arrived() {
    let portal = portal_answering(200, "{}").await;
    for sent in [
        r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#,
        "not json at all",
        "",
        "[]",
        "null",
    ] {
        let response = proxy(&portal.uri(), 100)
            .oneshot(
                authed("POST", "/v1/mcp")
                    .body(Body::from(sent))
                    .expect("a request"),
            )
            .await
            .expect("an answer");
        assert_eq!(response.status(), StatusCode::OK, "'{sent}'");

        let seen = portal.received_requests().await.expect("the mock recorded");
        assert_eq!(
            seen.last().expect("a request").body,
            sent.as_bytes(),
            "'{sent}' was rewritten"
        );
    }
}

/// AG-64: every answer the Portal gives is relayed as itself, and none of them carries the
/// Portal's address or the proxy's token.
#[tokio::test]
async fn a_portal_refusal_is_relayed_without_the_portals_address_or_token() {
    for upstream in [400u16, 401, 403, 404, 409, 429, 500] {
        let portal = portal_answering(upstream, r#"{"error":"no"}"#).await;
        let host = portal.address().to_string();

        let response = proxy(&portal.uri(), 100)
            .oneshot(
                authed("POST", "/v1/mcp")
                    .body(Body::from("{}"))
                    .expect("a request"),
            )
            .await
            .expect("an answer");

        assert_eq!(response.status().as_u16(), upstream);
        let body = body_of(response).await;
        assert!(!body.contains(&host), "the Portal's address leaked: {body}");
        assert!(
            !body.contains("mock-token-for"),
            "the proxy's token leaked: {body}"
        );
    }
}

/// AG-64: the answer is labelled JSON whatever the Portal labelled it, so a workspace cannot be
/// handed a document its client would render instead of read.
#[tokio::test]
async fn the_answer_is_labelled_json_whatever_the_portal_said() {
    let portal = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/html")
                .set_body_string("<script>alert(1)</script>"),
        )
        .mount(&portal)
        .await;

    let response = proxy(&portal.uri(), 100)
        .oneshot(
            authed("POST", "/v1/mcp")
                .body(Body::from("{}"))
                .expect("a request"),
        )
        .await
        .expect("an answer");

    assert_eq!(
        response
            .headers()
            .get("content-type")
            .and_then(|value| value.to_str().ok()),
        Some("application/json")
    );
}

/// AG-64: the registry door is a POST; a GET on it is refused by the router.
#[tokio::test]
async fn the_registry_door_takes_no_get() {
    let response = proxy("http://portal.invalid:8080", 100)
        .oneshot(
            authed("GET", "/v1/mcp")
                .body(Body::empty())
                .expect("a request"),
        )
        .await
        .expect("an answer");
    assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
}
