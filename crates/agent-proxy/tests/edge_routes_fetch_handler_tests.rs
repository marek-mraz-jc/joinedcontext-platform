//! Edge cases of `routes::fetch::handler` (T-1866).
//!
//! The fetch route is the only way out of a workspace that has no route of its own. What keeps
//! it safe is checked here: the run comes from the ticket, the host comes from the profile, the
//! budget is the run's, and nothing of the inbound request is forwarded.
//!
//! Every host in this file is under `.invalid` (RFC 6761), so no case can turn into a request to
//! a host that exists: a rule that stopped refusing would fail on a 502, never on live traffic.
//!
//! Struck as untestable here: the readable-content-type rule, the response-size ceiling and the
//! redirect chain all need a live **https** upstream, and the test stub speaks plain HTTP, which
//! `checked` refuses two lines into the handler. They are proven by the unit tests of
//! `routes::fetch` — `only_readable_content_types_pass`,
//! `a_redirect_is_followed_only_to_a_host_on_the_list`,
//! `the_outbound_request_carries_no_header_of_the_inbound_one` — against the same functions this
//! handler calls.

mod common;

use axum::body::Body;
use axum::http::StatusCode;
use common::{app, authed, body_of, sample_run, Bases, RUN_ID, TICKET};
use tower::ServiceExt;

/// A proxy for a run that may read `hosts` and spend `budget` bytes doing it.
fn proxy(hosts: &[&str], budget: u64) -> axum::Router {
    let mut run = sample_run(false);
    run.allowed_hosts = hosts.iter().map(|host| (*host).to_owned()).collect();
    run.max_egress_bytes_per_run = budget;
    app(run, Bases::default())
}

async fn fetch(hosts: &[&str], budget: u64, query: &str) -> axum::http::Response<Body> {
    proxy(hosts, budget)
        .oneshot(
            authed("GET", &format!("/v1/fetch{query}"))
                .body(Body::empty())
                .expect("a request"),
        )
        .await
        .expect("an answer")
}

/// The percent-encoding of one `url` value, so a case says what it means to send.
fn encoded(url: &str) -> String {
    url::form_urlencoded::byte_serialize(url.as_bytes()).collect()
}

/// AG-22: the way out is closed to a caller with no run credentials.
#[tokio::test]
async fn a_fetch_without_credentials_is_refused() {
    let response = proxy(&["docs.invalid"], 1024)
        .oneshot(
            axum::http::Request::builder()
                .uri("/v1/fetch?url=https%3A%2F%2Fdocs.invalid%2F")
                .body(Body::empty())
                .expect("a request"),
        )
        .await
        .expect("an answer");
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

/// AG-22: a ticket that is not this run's opens nothing, and neither does a run id nobody holds.
#[tokio::test]
async fn a_ticket_that_is_not_this_runs_opens_nothing() {
    for (run, ticket) in [(RUN_ID, "replayed-from-another-run"), ("nobody", TICKET)] {
        let response = proxy(&["docs.invalid"], 1024)
            .oneshot(
                axum::http::Request::builder()
                    .uri("/v1/fetch?url=https%3A%2F%2Fdocs.invalid%2F")
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

/// AG-65: no `url`, an empty `url` and a `url` that is not a URL are each a 400 that says what
/// was wrong, not a request that goes somewhere by default.
#[tokio::test]
async fn a_missing_empty_or_unparseable_url_is_a_bad_request() {
    for query in ["", "?", "?page=2", "?url=", "?url=%20", "?url=not-a-url"] {
        let response = fetch(&["docs.invalid"], 1024, query).await;
        assert_eq!(
            response.status(),
            StatusCode::BAD_REQUEST,
            "query '{query}'"
        );
    }
}

/// AG-65: https or nothing. Plain HTTP would put the request on the wire in the clear and
/// `file:` would read the proxy's own disk.
#[tokio::test]
async fn every_scheme_but_https_is_refused() {
    for url in [
        "http://docs.invalid/",
        "file:///etc/passwd",
        "ftp://docs.invalid/",
        "gopher://docs.invalid/",
        "HTTP://docs.invalid/",
    ] {
        let response = fetch(&["docs.invalid"], 1024, &format!("?url={}", encoded(url))).await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN, "{url} passed");
    }
}

/// AG-50: the allow-list is matched on the parsed host, whole and case-insensitively. A
/// neighbour, a suffix, a trailing dot, a look-alike spelled in another script and an address
/// that names the node itself are all different hosts than the one the operator reviewed.
#[tokio::test]
async fn only_the_exact_host_of_the_allow_list_passes() {
    // The host the profile named, however it is cased: past the check and into a request that
    // fails because nothing answers at `.invalid` — never because the host was refused.
    for url in ["https://DOCS.INVALID/serde/", "https://docs.invalid/serde/"] {
        let response = fetch(&["Docs.INVALID"], 1024, &format!("?url={}", encoded(url))).await;
        assert_eq!(
            response.status(),
            StatusCode::BAD_GATEWAY,
            "{url} was refused as a host"
        );
    }
    for url in [
        "https://docs.invalid.evil.test/serde/",
        "https://evil-docs.invalid/serde/",
        "https://invalid/serde/",
        "https://user.docs.invalid/serde/",
        "https://docs.invalid./serde/",
        "https://127.0.0.1/serde/",
        "https://localhost/serde/",
        "https://169.254.169.254/latest/meta-data/",
        "https://[::1]/serde/",
        "https://d\u{043e}cs.invalid/serde/",
    ] {
        let response = fetch(&["docs.invalid"], 1024, &format!("?url={}", encoded(url))).await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN, "{url} passed");
        let detail = body_of(response).await;
        assert!(
            detail.contains("egress allow-list"),
            "the refusal says why: {detail}"
        );
    }
}

/// AG-65: a URL that carries a credential is refused rather than handed to the host — whatever
/// case the parameter is spelled in, and whether it sits in the userinfo or the query.
#[tokio::test]
async fn a_url_carrying_a_credential_is_refused_rather_than_sent() {
    for url in [
        "https://user:pw@docs.invalid/",
        "https://user@docs.invalid/",
        "https://docs.invalid/?token=abc",
        "https://docs.invalid/?API_KEY=abc",
        "https://docs.invalid/?page=2&Access_Token=abc",
        "https://docs.invalid/?signature=abc",
        "https://docs.invalid/?password=abc",
    ] {
        let response = fetch(&["docs.invalid"], 1024, &format!("?url={}", encoded(url))).await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{url} passed");
    }
}

/// AG-50, AG-65: a run whose profile names no budget reaches nothing, and the refusal says so
/// rather than pretending the host was the problem.
#[tokio::test]
async fn a_run_without_an_egress_budget_reaches_nothing() {
    let response = fetch(&["docs.invalid"], 0, "?url=https%3A%2F%2Fdocs.invalid%2F").await;
    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(
        response
            .headers()
            .get("X-JC-Egress-Remaining")
            .and_then(|value| value.to_str().ok()),
        Some("0"),
        "the run is told what is left"
    );
    let detail = body_of(response).await;
    assert!(
        detail.contains("no egress budget") && detail.contains("AG-50"),
        "{detail}"
    );
}

/// AG-50: the host is checked before the budget, so a run with nothing left still learns nothing
/// about its profile's allow-list that a run with a budget would not learn.
#[tokio::test]
async fn the_host_is_checked_before_the_budget() {
    let response = fetch(&["docs.invalid"], 0, "?url=https%3A%2F%2Fevil.test%2F").await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
}

/// AG-65: the first `url` is the one read, and a second copy cannot smuggle a different target
/// past the checks — both are the workspace's own, and only one request is ever built.
#[tokio::test]
async fn a_second_url_parameter_does_not_change_the_target() {
    let response = fetch(
        &["docs.invalid"],
        1024,
        "?url=https%3A%2F%2Fevil.test%2F&url=https%3A%2F%2Fdocs.invalid%2F",
    )
    .await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
}

/// AG-65: a host on the list that cannot be reached is a 502, and what it says back carries no
/// credential of the platform.
#[tokio::test]
async fn an_unreachable_allowed_host_leaks_no_credential() {
    let response = fetch(
        &["docs.invalid"],
        4096,
        "?url=https%3A%2F%2Fdocs.invalid%2Fserde%2F",
    )
    .await;
    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    let detail = body_of(response).await;
    for secret in [
        "mock-model-key",
        "mock-forge-token",
        "mock-token-for",
        TICKET,
    ] {
        assert!(!detail.contains(secret), "'{secret}' leaked: {detail}");
    }
}

/// AG-65: a `url` whose value carries CR, LF, a NUL or a space cannot become a second request
/// line — either the URL parser refuses it or the bytes stay inside one path.
#[tokio::test]
async fn a_url_with_a_control_character_in_it_never_becomes_a_second_request() {
    for raw in [
        "https://docs.invalid/%0D%0AHost:%20evil.test",
        "https://docs.invalid%0D%0A/",
        "https://docs.invalid/\u{0}",
        "https://docs.invalid/a b",
        "https://docs.invalid/\u{0d}\u{0a}",
    ] {
        let response = fetch(&["docs.invalid"], 1024, &format!("?url={}", encoded(raw))).await;
        assert!(
            matches!(
                response.status(),
                StatusCode::BAD_REQUEST | StatusCode::FORBIDDEN | StatusCode::BAD_GATEWAY
            ),
            "{raw} answered {}",
            response.status()
        );
    }
}
