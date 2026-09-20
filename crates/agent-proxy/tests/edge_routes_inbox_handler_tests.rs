//! Edge cases of `routes::inbox::handler` (T-1868).
//!
//! The inbox is what the person said, relayed to the workspace. A workspace asks "what was said
//! to me" and must not be able to ask it about another run: the run comes from the ticket and
//! nothing in the query can name one.

mod common;

use axum::body::Body;
use axum::http::StatusCode;
use common::{app, authed, body_of, sample_run, Bases, RUN_ID, TICKET};
use tower::ServiceExt;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// The clamp the route puts on how long a workspace may hold the call open.
const MAX_WAIT: u64 = 25;

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
    Mock::given(method("GET"))
        .and(path(format!("/internal/agent-runs/{RUN_ID}/inbox")))
        .respond_with(ResponseTemplate::new(status).set_body_string(body))
        .mount(&portal)
        .await;
    portal
}

/// The query the Portal was asked with, as pairs.
async fn last_query(portal: &MockServer) -> Vec<(String, String)> {
    let seen = portal.received_requests().await.expect("the mock recorded");
    seen.last()
        .expect("a request reached the Portal")
        .url
        .query_pairs()
        .map(|(name, value)| (name.into_owned(), value.into_owned()))
        .collect()
}

/// AG-46: an inbox read with no run credentials reads nobody's inbox.
#[tokio::test]
async fn an_inbox_read_without_credentials_is_refused_before_the_portal_is_asked() {
    let portal = MockServer::start().await;
    let response = proxy(&portal.uri(), 100)
        .oneshot(
            axum::http::Request::builder()
                .uri("/v1/runs/inbox")
                .body(Body::empty())
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

/// AG-22: a ticket of another run reads no inbox.
///
/// The Portal here holds no run and answers `404` to the lookup, which is what "no such run" is.
/// A Portal that cannot be reached at all is a failure of the platform's and answers `503`, not
/// `401` (T-2418); this case is about the caller's credential.
#[tokio::test]
async fn a_ticket_of_another_run_reads_no_inbox() {
    // A Portal that holds no run: every lookup is unmatched and answers 404.
    let portal = MockServer::start().await;
    for (run, ticket) in [(RUN_ID, "another-runs-ticket"), ("nobody", TICKET)] {
        let response = proxy(&portal.uri(), 100)
            .oneshot(
                axum::http::Request::builder()
                    .uri("/v1/runs/inbox")
                    .header("x-jc-run", run)
                    .header("x-jc-ticket", ticket)
                    .body(Body::empty())
                    .expect("a request"),
            )
            .await
            .expect("an answer");
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED, "run '{run}'");
    }
}

/// AG-46, AG-52: the inbox read is built from the ticket's run id. A `run`, `runId` or `id` in
/// the query is not read, so a workspace cannot read what was said to another run.
#[tokio::test]
async fn nothing_in_the_query_can_name_another_run() {
    let portal = portal_answering(200, "[]").await;
    let other = "11111111-2222-3333-4444-555555555555";

    let response = proxy(&portal.uri(), 100)
        .oneshot(
            authed(
                "GET",
                &format!("/v1/runs/inbox?run={other}&runId={other}&id={other}"),
            )
            .body(Body::empty())
            .expect("a request"),
        )
        .await
        .expect("an answer");
    assert_eq!(response.status(), StatusCode::OK);

    let seen = portal.received_requests().await.expect("the mock recorded");
    let asked = seen.first().expect("a request reached the Portal");
    assert_eq!(
        asked.url.path(),
        format!("/internal/agent-runs/{RUN_ID}/inbox")
    );
    assert!(
        !asked.url.as_str().contains(other),
        "the other run's id travelled: {}",
        asked.url
    );
}

/// AG-46: the wait a workspace asks for is clamped, so a run cannot hold a socket of the proxy
/// for longer than the route allows — however large the number and whatever the shape.
#[tokio::test]
async fn the_wait_is_clamped_to_the_routes_ceiling() {
    let portal = portal_answering(200, "[]").await;
    for (asked, expected) in [
        ("", MAX_WAIT),
        ("?wait=0", 0),
        ("?wait=1", 1),
        ("?wait=25", MAX_WAIT),
        ("?wait=26", MAX_WAIT),
        ("?wait=3600", MAX_WAIT),
        ("?wait=18446744073709551615", MAX_WAIT),
    ] {
        let response = proxy(&portal.uri(), 100)
            .oneshot(
                authed("GET", &format!("/v1/runs/inbox{asked}"))
                    .body(Body::empty())
                    .expect("a request"),
            )
            .await
            .expect("an answer");
        assert_eq!(response.status(), StatusCode::OK, "'{asked}'");
        assert!(
            last_query(&portal)
                .await
                .contains(&("wait".to_owned(), expected.to_string())),
            "'{asked}' asked for {:?}",
            last_query(&portal).await
        );
    }
}

/// AG-46: a `wait` or `after` that is not a number is refused rather than read as a default, so
/// a typo in the workspace's client is an error it can see.
#[tokio::test]
async fn a_cursor_or_a_wait_that_is_not_a_number_is_refused() {
    let portal = portal_answering(200, "[]").await;
    for query in [
        "?wait=soon",
        "?wait=-1",
        "?wait=1.5",
        "?after=first",
        "?after=1.5",
        "?after=",
    ] {
        let response = proxy(&portal.uri(), 100)
            .oneshot(
                authed("GET", &format!("/v1/runs/inbox{query}"))
                    .body(Body::empty())
                    .expect("a request"),
            )
            .await
            .expect("an answer");
        assert_eq!(response.status(), StatusCode::BAD_REQUEST, "'{query}'");
    }
    assert!(portal
        .received_requests()
        .await
        .unwrap_or_default()
        .is_empty());
}

/// AG-46: the cursor travels as the number it is, at zero and at the bounds of the type, and an
/// unknown parameter beside it changes nothing.
#[tokio::test]
async fn the_cursor_travels_as_the_number_it_is() {
    let portal = portal_answering(200, "[]").await;
    for (query, expected) in [
        ("", "0"),
        ("?after=0", "0"),
        ("?after=1", "1"),
        ("?after=-1", "-1"),
        ("?after=9223372036854775807", "9223372036854775807"),
        ("?after=7&unknown=x", "7"),
    ] {
        let response = proxy(&portal.uri(), 100)
            .oneshot(
                authed("GET", &format!("/v1/runs/inbox{query}"))
                    .body(Body::empty())
                    .expect("a request"),
            )
            .await
            .expect("an answer");
        assert_eq!(response.status(), StatusCode::OK, "'{query}'");
        let pairs = last_query(&portal).await;
        assert!(
            pairs.contains(&("after".to_owned(), expected.to_owned())),
            "'{query}' asked for {pairs:?}"
        );
        assert_eq!(pairs.len(), 2, "'{query}' asked for {pairs:?}");
    }
}

/// AG-41: the run's per-minute limit holds on the inbox too, and the refused read never reaches
/// the Portal — a workspace that polls in a loop is bounded here, not by the Portal.
#[tokio::test]
async fn the_read_past_the_per_minute_limit_is_refused() {
    let portal = portal_answering(200, "[]").await;
    let app = proxy(&portal.uri(), 1);

    for expected in [StatusCode::OK, StatusCode::TOO_MANY_REQUESTS] {
        let response = app
            .clone()
            .oneshot(
                authed("GET", "/v1/runs/inbox")
                    .body(Body::empty())
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
async fn the_portal_is_told_the_proxys_own_token_and_nothing_of_the_workspace() {
    let portal = portal_answering(200, "[]").await;
    let response = proxy(&portal.uri(), 100)
        .oneshot(
            authed("GET", "/v1/runs/inbox")
                .header("authorization", "Bearer the-workspaces-own-token")
                .header("cookie", "session=stolen")
                .body(Body::empty())
                .expect("a request"),
        )
        .await
        .expect("an answer");
    assert_eq!(response.status(), StatusCode::OK);

    let seen = portal.received_requests().await.expect("the mock recorded");
    let headers = &seen.first().expect("a request").headers;
    assert_eq!(
        headers
            .get("authorization")
            .and_then(|value| value.to_str().ok()),
        Some("Bearer mock-token-for-portal-internal")
    );
    for forbidden in ["cookie", "x-jc-run", "x-jc-ticket"] {
        assert!(
            headers.get(forbidden).is_none(),
            "'{forbidden}' was relayed to the Portal"
        );
    }
}

/// AG-46: what the Portal answers is relayed as itself — the status and the bytes — and it
/// carries neither the Portal's address nor the proxy's token.
#[tokio::test]
async fn a_portal_answer_is_relayed_without_its_address_or_token() {
    for (upstream, answer) in [
        (200u16, "[]"),
        (204, ""),
        (404, r#"{"detail":"no such run"}"#),
        (409, "not json at all"),
        (429, "{}"),
        (500, "{}"),
    ] {
        let portal = portal_answering(upstream, answer).await;
        let host = portal.address().to_string();

        let response = proxy(&portal.uri(), 100)
            .oneshot(
                authed("GET", "/v1/runs/inbox")
                    .body(Body::empty())
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

/// AG-46: the inbox door is a GET; every other method is refused by the router.
#[tokio::test]
async fn the_inbox_door_takes_no_write() {
    for verb in ["POST", "PUT", "PATCH", "DELETE"] {
        let response = proxy("http://portal.invalid:8080", 100)
            .oneshot(
                authed(verb, "/v1/runs/inbox")
                    .body(Body::empty())
                    .expect("a request"),
            )
            .await
            .expect("an answer");
        assert_eq!(
            response.status(),
            StatusCode::METHOD_NOT_ALLOWED,
            "{verb} passed"
        );
    }
}
