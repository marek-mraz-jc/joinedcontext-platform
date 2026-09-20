//! Edge cases of `routes::packages::handler` (T-1871).
//!
//! The package door is the second way out: a package manager inside the workspace asks for a
//! crate and the proxy fetches it from a host the profile named. The host comes from the path,
//! so the allow-list is the whole of the control, and the path must stay a path on that host.
//!
//! Every host here is under `.invalid`, so a case that passes the allow-list fails on a name
//! that resolves nowhere rather than reaching a registry that exists.
//!
//! A registry that does not answer is `502 Bad Gateway`, not `500`: since T-1695 this route
//! reaches the registry through `fetch::checked`/`fetch::follow`, the same two functions the
//! fetch route uses, so both answer an unreachable outside host in the same words. The four
//! cases below reach the network stage on a name nothing resolves, which is what they are for —
//! the refusal they each prove happens before it.

mod common;

use axum::body::Body;
use axum::http::StatusCode;
use common::{app, authed, body_of, sample_run, Bases, RUN_ID, TICKET};
use tower::ServiceExt;

fn proxy(hosts: &[&str]) -> axum::Router {
    let mut run = sample_run(false);
    run.allowed_hosts = hosts.iter().map(|host| (*host).to_owned()).collect();
    app(run, Bases::default())
}

/// The same proxy, asking a Portal that holds no run and answers `404` to every lookup.
///
/// `Bases::default()` points the Portal at a name that resolves nowhere, which since T-2418 is a
/// failure of the platform's (`503`) rather than a bad credential (`401`). A case about a run id
/// nobody holds has to ask a Portal that is there.
fn proxy_asking(hosts: &[&str], portal: &wiremock::MockServer) -> axum::Router {
    let mut run = sample_run(false);
    run.allowed_hosts = hosts.iter().map(|host| (*host).to_owned()).collect();
    app(
        run,
        Bases {
            portal: portal.uri(),
            ..Bases::default()
        },
    )
}

async fn get(hosts: &[&str], uri: &str) -> axum::http::Response<Body> {
    proxy(hosts)
        .oneshot(authed("GET", uri).body(Body::empty()).expect("a request"))
        .await
        .expect("an answer")
}

/// AG-22: a package manager with no run credentials downloads nothing.
#[tokio::test]
async fn a_download_without_credentials_is_refused() {
    let response = proxy(&["registry.invalid"])
        .oneshot(
            axum::http::Request::builder()
                .uri("/v1/packages/registry.invalid/api/v1/crates/serde/1.0.0/download")
                .body(Body::empty())
                .expect("a request"),
        )
        .await
        .expect("an answer");
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

/// AG-22: a ticket of another run, and a run id nobody holds, are the same refusal.
#[tokio::test]
async fn a_ticket_of_another_run_downloads_nothing() {
    let portal = wiremock::MockServer::start().await;
    for (run, ticket) in [(RUN_ID, "another-runs-ticket"), ("nobody", TICKET)] {
        let response = proxy_asking(&["registry.invalid"], &portal)
            .oneshot(
                axum::http::Request::builder()
                    .uri("/v1/packages/registry.invalid/serde")
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

/// AG-50: a host the profile did not name is refused, and so is every spelling that only looks
/// like one it did — a suffix, a neighbour, a trailing dot, the node itself, a look-alike.
#[tokio::test]
async fn only_the_exact_host_of_the_allow_list_passes() {
    for host in [
        "registry.invalid.evil.test",
        "evil-registry.invalid",
        "invalid",
        "registry.invalid.",
        "127.0.0.1",
        "localhost",
        "169.254.169.254",
        "r\u{0435}gistry.invalid",
    ] {
        let response = get(&["registry.invalid"], &format!("/v1/packages/{host}/serde")).await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN, "{host} passed");
        let detail = body_of(response).await;
        assert!(
            detail.contains("allow-list"),
            "the refusal says why: {detail}"
        );
    }
}

/// AG-50: the host the profile named passes whatever case either is written in, and the request
/// that follows is the one that fails on a name nothing answers.
#[tokio::test]
async fn the_allowed_host_passes_in_any_case() {
    for host in ["registry.invalid", "REGISTRY.INVALID"] {
        let response = get(&["Registry.Invalid"], &format!("/v1/packages/{host}/serde")).await;
        assert_eq!(
            response.status(),
            StatusCode::BAD_GATEWAY,
            "{host} was refused"
        );
    }
}

/// T-1301: the host is allow-listed, so the path must stay a path on it — nothing encoded, no
/// backslash, no dot segment, no empty segment, no absolute path.
#[tokio::test]
async fn a_path_that_would_leave_the_host_is_refused() {
    for rest in [
        "../admin",
        "%2e%2e/admin",
        "%252e%252e/admin",
        "a/../../b",
        "a//b",
        "./a",
    ] {
        let response = get(
            &["registry.invalid"],
            &format!("/v1/packages/registry.invalid/{rest}"),
        )
        .await;
        assert!(
            matches!(
                response.status(),
                StatusCode::FORBIDDEN | StatusCode::NOT_FOUND
            ),
            "'{rest}' answered {}",
            response.status()
        );
    }
}

/// T-1301: a NUL or a space inside a segment is not a way out of the host: the URL parser
/// encodes it back into the one segment it came from, so the request is the ordinary one that
/// fails on a name nothing answers.
#[tokio::test]
async fn a_control_character_in_the_path_stays_inside_one_segment() {
    for rest in ["a%00b", "a%20b", "a%0d%0ab"] {
        let response = get(
            &["registry.invalid"],
            &format!("/v1/packages/registry.invalid/{rest}"),
        )
        .await;
        assert_eq!(
            response.status(),
            StatusCode::BAD_GATEWAY,
            "'{rest}' answered {}",
            response.status()
        );
    }
}

/// AG-50: the door is a download and nothing else. Every other method is refused by the router
/// before the handler is entered, so a package manager cannot publish through it.
#[tokio::test]
async fn nothing_but_a_get_is_a_download() {
    for verb in ["POST", "PUT", "PATCH", "DELETE"] {
        let response = proxy(&["registry.invalid"])
            .oneshot(
                authed(verb, "/v1/packages/registry.invalid/serde")
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

/// AG-50: a host segment that is empty leaves the route unmatched rather than becoming a request
/// to the proxy's own base.
#[tokio::test]
async fn an_empty_host_or_an_empty_path_matches_no_route() {
    for uri in [
        "/v1/packages//serde",
        "/v1/packages/registry.invalid",
        "/v1/packages/",
        "/v1/packages",
    ] {
        let response = get(&["registry.invalid"], uri).await;
        assert!(
            !response.status().is_success(),
            "{uri} answered {}",
            response.status()
        );
    }
}

/// AG-35: what the workspace is told when the registry cannot be reached carries none of the
/// credentials this proxy holds.
#[tokio::test]
async fn an_unreachable_registry_leaks_no_credential() {
    let response = get(&["registry.invalid"], "/v1/packages/registry.invalid/serde").await;
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

/// AG-50: a run whose profile names no host at all reaches no registry, without a second rule
/// saying so.
#[tokio::test]
async fn a_run_whose_profile_names_no_host_downloads_nothing() {
    let response = get(&[], "/v1/packages/registry.invalid/serde").await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
}

/// AG-50: the query string is not carried to the registry, so a parameter a workspace appends
/// cannot change what is downloaded behind the path that was checked.
#[tokio::test]
async fn a_query_string_does_not_travel_with_the_download() {
    let response = get(
        &["registry.invalid"],
        "/v1/packages/registry.invalid/serde?redirect=https://evil.test/",
    )
    .await;
    assert_eq!(
        response.status(),
        StatusCode::BAD_GATEWAY,
        "the path was accepted and the unreachable host refused it"
    );
}
