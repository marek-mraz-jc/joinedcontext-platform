//! Edge cases of `routes::diagnostics::handler` (T-1864).
//!
//! The diagnostics door lets a run read why its own step failed. Everything it is trusted with
//! is here: the component is one of two the proxy knows, the id is a name and never a path, the
//! run in the request is the ticket's, and what comes back is redacted before the workspace
//! reads it — because a pipeline's diagnostics are full of connection strings.

mod common;

use axum::body::Body;
use axum::http::StatusCode;
use common::{app, authed, body_of, sample_run, Bases, RUN_ID, TICKET};
use tower::ServiceExt;
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

fn proxy(portal: &str, per_minute: u32, max_response_bytes: u64) -> axum::Router {
    let mut run = sample_run(false);
    run.requests_per_minute = per_minute;
    run.max_response_bytes = max_response_bytes;
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
        .respond_with(ResponseTemplate::new(status).set_body_string(body))
        .mount(&portal)
        .await;
    portal
}

/// AG-57: the door is closed to a caller with no run credentials.
#[tokio::test]
async fn a_read_without_credentials_is_refused_before_the_portal_is_asked() {
    let portal = MockServer::start().await;
    let response = proxy(&portal.uri(), 100, 1_048_576)
        .oneshot(
            axum::http::Request::builder()
                .uri("/v1/diagnostics/pipeline/hsl-bikes")
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

/// AG-22: a ticket of another run reads no diagnostics.
///
/// The Portal answers `404` here rather than resolving nowhere: an id no run holds is a refusal of
/// the caller's (`401`), and a Portal that cannot be reached is a failure of the platform's
/// (`503`, T-2418). This case is about the first.
#[tokio::test]
async fn a_ticket_of_another_run_reads_no_diagnostics() {
    let portal = portal_answering(404, "").await;
    for (run, ticket) in [(RUN_ID, "another-runs-ticket"), ("nobody", TICKET)] {
        let response = proxy(&portal.uri(), 100, 1_048_576)
            .oneshot(
                axum::http::Request::builder()
                    .uri("/v1/diagnostics/pipeline/hsl-bikes")
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

/// AG-57: the door knows two components. Anything else is refused before a request leaves the
/// proxy, whatever case or spacing it is written in.
#[tokio::test]
async fn a_component_the_door_does_not_know_is_refused() {
    let portal = portal_answering(200, "{}").await;
    for component in [
        "Pipeline",
        "PIPELINE",
        "pipelines",
        "%20pipeline",
        "pipeline%20",
        "secret",
        "endpoint",
        "..",
        "%2e%2e",
    ] {
        let response = proxy(&portal.uri(), 100, 1_048_576)
            .oneshot(
                authed("GET", &format!("/v1/diagnostics/{component}/hsl-bikes"))
                    .body(Body::empty())
                    .expect("a request"),
            )
            .await
            .expect("an answer");
        assert!(
            matches!(
                response.status(),
                StatusCode::BAD_REQUEST | StatusCode::FORBIDDEN | StatusCode::NOT_FOUND
            ),
            "'{component}' answered {}",
            response.status()
        );
    }
    assert!(portal
        .received_requests()
        .await
        .unwrap_or_default()
        .is_empty());
}

/// AG-57, T-0817: an id is a resource name or a change id and never a path, so nothing a
/// workspace types can move the request off the run's own diagnostics.
#[tokio::test]
async fn an_id_that_is_not_a_name_is_refused() {
    let portal = portal_answering(200, "{}").await;
    for id in [
        "..",
        "%2e%2e",
        "..%2fsecrets",
        "HSL-Bikes",
        "-hsl",
        "hsl-",
        "hsl%20bikes",
        "hsl_bikes",
        "chg-XYZ12345",
        "chg-0000000A0",
        &"a".repeat(64),
    ] {
        let response = proxy(&portal.uri(), 100, 1_048_576)
            .oneshot(
                authed("GET", &format!("/v1/diagnostics/pipeline/{id}"))
                    .body(Body::empty())
                    .expect("a request"),
            )
            .await
            .expect("an answer");
        assert!(
            matches!(
                response.status(),
                StatusCode::BAD_REQUEST | StatusCode::FORBIDDEN | StatusCode::NOT_FOUND
            ),
            "'{id}' answered {}",
            response.status()
        );
    }
    assert!(portal
        .received_requests()
        .await
        .unwrap_or_default()
        .is_empty());
}

/// AG-57: the names and change ids that are legitimate still pass, at the bounds of both shapes.
#[tokio::test]
async fn a_name_and_a_change_id_pass_at_their_bounds() {
    let portal = portal_answering(200, "{}").await;
    for (component, id) in [
        ("pipeline", "a"),
        ("pipeline", "hsl-bikes"),
        ("pipeline", &"a".repeat(63)),
        ("change", "chg-0000000a"),
        ("change", "chg-ffffffff"),
    ] {
        let response = proxy(&portal.uri(), 100, 1_048_576)
            .oneshot(
                authed("GET", &format!("/v1/diagnostics/{component}/{id}"))
                    .body(Body::empty())
                    .expect("a request"),
            )
            .await
            .expect("an answer");
        assert_eq!(response.status(), StatusCode::OK, "{component}/{id}");
    }
}

/// AG-57, AG-52: the run in the path the Portal is asked with is the ticket's, so a workspace
/// reads the diagnostics of its own run and of no other.
#[tokio::test]
async fn the_run_in_the_portals_path_is_the_tickets() {
    let portal = portal_answering(200, "{}").await;
    let response = proxy(&portal.uri(), 100, 1_048_576)
        .oneshot(
            authed(
                "GET",
                "/v1/diagnostics/pipeline/hsl-bikes?run=11111111-2222-3333-4444-555555555555",
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
        format!("/internal/agent-runs/{RUN_ID}/diagnostics/pipeline/hsl-bikes")
    );
    assert!(
        !asked.url.as_str().contains("11111111"),
        "a run id from the query travelled: {}",
        asked.url
    );
}

/// AG-40, AG-56, T-0957: a credential in the diagnostics is scrubbed before the workspace reads
/// it, and what a change is made of is not taken for one.
#[tokio::test]
async fn a_credential_in_the_diagnostics_is_scrubbed_and_a_commit_sha_is_not() {
    // Assembled here, so this source holds no string a secret scanner would take for a real one.
    let github = format!("ghp_{}", "abc123def456ghi789jkl012");
    let sha = "9f1c2b7d4e6a8c0b2d4f6a8c0e2b4d6f8a0c2e4b";
    let answer = serde_json::json!({
        "error": format!("push with {github} to postgresql://jc:s3cr3t-pw@db:5432/jc failed"),
        "authorization": "Bearer abcdefghijklmnop.qrstuvwxyz",
        "commit": sha,
        "received": 1200
    })
    .to_string();
    let portal = portal_answering(200, &answer).await;

    let response = proxy(&portal.uri(), 100, 1_048_576)
        .oneshot(
            authed("GET", "/v1/diagnostics/pipeline/hsl-bikes")
                .body(Body::empty())
                .expect("a request"),
        )
        .await
        .expect("an answer");
    assert_eq!(response.status(), StatusCode::OK);

    let read = body_of(response).await;
    for secret in [github.as_str(), "s3cr3t-pw", "abcdefghijklmnop.qrstuvwxyz"] {
        assert!(!read.contains(secret), "'{secret}' reached the workspace");
    }
    assert!(read.contains(sha), "the commit sha was taken for a secret");
    assert!(read.contains("1200"), "the counters stay: {read}");
}

/// AG-41: the profile's response cap holds on this door, and it cuts on character boundaries, so
/// a document full of accented text comes back short rather than broken.
#[tokio::test]
async fn the_answer_is_cut_at_the_runs_response_cap() {
    let portal = portal_answering(200, &"\u{e4}".repeat(100)).await;
    let response = proxy(&portal.uri(), 100, 10)
        .oneshot(
            authed("GET", "/v1/diagnostics/pipeline/hsl-bikes")
                .body(Body::empty())
                .expect("a request"),
        )
        .await
        .expect("an answer");

    assert_eq!(response.status(), StatusCode::OK);
    let read = body_of(response).await;
    assert_eq!(
        read.chars().count(),
        10,
        "the cap counts characters and the text stays valid: {read:?}"
    );
}

/// AG-41: the run's per-minute limit holds here too, and it is checked after the component and
/// the id, so a workspace cannot spend its budget on requests the proxy would refuse anyway.
#[tokio::test]
async fn the_read_past_the_per_minute_limit_is_refused() {
    let portal = portal_answering(200, "{}").await;
    let app = proxy(&portal.uri(), 1, 1_048_576);

    let bad = app
        .clone()
        .oneshot(
            authed("GET", "/v1/diagnostics/pipeline/NOT-a-name")
                .body(Body::empty())
                .expect("a request"),
        )
        .await
        .expect("an answer");
    assert_eq!(bad.status(), StatusCode::BAD_REQUEST, "no budget is spent");

    for expected in [StatusCode::OK, StatusCode::TOO_MANY_REQUESTS] {
        let response = app
            .clone()
            .oneshot(
                authed("GET", "/v1/diagnostics/pipeline/hsl-bikes")
                    .body(Body::empty())
                    .expect("a request"),
            )
            .await
            .expect("an answer");
        assert_eq!(response.status(), expected);
    }
}

/// AG-52: the Portal is told the proxy's own audience-bound token, never the workspace's.
#[tokio::test]
async fn the_portal_is_told_the_proxys_own_token_and_nothing_of_the_workspace() {
    let portal = portal_answering(200, "{}").await;
    let response = proxy(&portal.uri(), 100, 1_048_576)
        .oneshot(
            authed("GET", "/v1/diagnostics/pipeline/hsl-bikes")
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

/// AG-57: a Portal that refuses says so to the run, as JSON, without its own address or the
/// proxy's token.
#[tokio::test]
async fn a_portal_refusal_is_relayed_as_json_without_its_address_or_token() {
    for upstream in [400u16, 403, 404, 409, 429, 500] {
        let portal = portal_answering(upstream, r#"{"detail":"no"}"#).await;
        let host = portal.address().to_string();

        let response = proxy(&portal.uri(), 100, 1_048_576)
            .oneshot(
                authed("GET", "/v1/diagnostics/pipeline/hsl-bikes")
                    .body(Body::empty())
                    .expect("a request"),
            )
            .await
            .expect("an answer");

        assert_eq!(response.status().as_u16(), upstream);
        assert_eq!(
            response
                .headers()
                .get("content-type")
                .and_then(|value| value.to_str().ok()),
            Some("application/json")
        );
        let body = body_of(response).await;
        assert!(!body.contains(&host), "the Portal's address leaked: {body}");
        assert!(!body.contains("mock-token-for"), "a token leaked: {body}");
    }
}

/// AG-57: the diagnostics door is a GET; every other method is refused by the router.
#[tokio::test]
async fn the_diagnostics_door_takes_no_write() {
    for verb in ["POST", "PUT", "PATCH", "DELETE"] {
        let response = proxy("http://portal.invalid:8080", 100, 1_048_576)
            .oneshot(
                authed(verb, "/v1/diagnostics/pipeline/hsl-bikes")
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
