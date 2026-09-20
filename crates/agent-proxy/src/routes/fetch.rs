//! The one way out: `GET /v1/fetch?url=` (AG-50, AG-65, T-0557).
//!
//! The workspace has no route of its own — its NetworkPolicy allows kube-dns and this proxy and
//! nothing else — so an agent that has to read a library's documentation reads it here or not at
//! all. What keeps that safe is not the absence of a network; it is that the profile names every
//! host, the run's bytes are counted, the platform's credentials never leave, and every fetch is
//! one audit line with the run id.
//!
//! Nothing of the inbound request is forwarded. The outbound request is built from the URL and
//! one `User-Agent`, so `Authorization`, the cookies and the run ticket cannot reach the upstream
//! even by mistake; a URL that carries a credential of its own is refused rather than sent.

use crate::audit::{log_request, AuditEntry};
use crate::auth::authenticate;
use crate::ProxyState;
use axum::body::Body;
use axum::extract::{RawQuery, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use std::time::Instant;
use url::Url;

/// How many redirects one fetch may follow. Documentation sites redirect a version to `latest`
/// and `http` to `https`; a chain longer than this is a loop or a crawl.
const MAX_REDIRECTS: usize = 5;

/// What the agent may read: prose and data, never something that runs. A `Content-Type` outside
/// this list is refused after the headers arrive and before the body is read.
const READABLE: [&str; 8] = [
    "text/plain",
    "text/markdown",
    "text/html",
    "text/csv",
    "application/json",
    "application/xml",
    "text/xml",
    "application/yaml",
];

/// Query parameter names that carry a credential often enough that a URL holding one is a
/// mistake worth refusing: the agent would be handing a secret to a host it only meant to read.
const CREDENTIAL_PARAMETERS: [&str; 7] = [
    "token",
    "access_token",
    "api_key",
    "apikey",
    "secret",
    "password",
    "signature",
];

/// The header every fetch answers with, so a run can see its budget shrink instead of finding
/// it empty (AG-65).
pub const REMAINING_HEADER: &str = "X-JC-Egress-Remaining";

pub async fn handler(
    State(state): State<ProxyState>,
    headers: HeaderMap,
    RawQuery(query): RawQuery,
) -> Response {
    let start = Instant::now();
    let run = match authenticate(&headers, &state.runs, &state.config).await {
        Ok(r) => r,
        Err(p) => return (*p).into_response(),
    };
    // The whole query string, parsed here rather than by an extractor: the URL is itself a URL
    // with its own query, and a rejection has to be a problem document like every other refusal.
    let Some(raw) = url::form_urlencoded::parse(query.unwrap_or_default().as_bytes())
        .find(|(name, _)| name == "url")
        .map(|(_, value)| value.into_owned())
        .filter(|value| !value.is_empty())
    else {
        return bad_request("the fetch route needs a `url` query parameter").into_response();
    };

    let target = match checked(&raw, &run.allowed_hosts) {
        Ok(url) => url,
        Err(problem) => return *problem,
    };

    // A profile that names no budget has none: the Portal resolves the profile's
    // `egress.maxBytesPerRun` and sends the number, and a run without one reaches nothing.
    let budget = run.max_egress_bytes_per_run;
    let remaining = state.limits.egress_remaining(&run.id, budget).await;
    if remaining == 0 {
        return (
            StatusCode::TOO_MANY_REQUESTS,
            [(REMAINING_HEADER, "0")],
            jc_core::ProblemDetails::new(
                429,
                "egress-budget-spent",
                match budget {
                    0 => {
                        "this run has no egress budget: the profile names no host to read \
                          (AG-50)"
                    }
                    _ => "the run's egress byte budget is spent (egress.maxBytesPerRun, AG-65)",
                },
            ),
        )
            .into_response();
    }

    let (response, final_host) = match follow(&state, &target, &run.allowed_hosts).await {
        Ok(reached) => reached,
        Err(problem) => return *problem,
    };

    let status =
        StatusCode::from_u16(response.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("")
        .to_owned();
    if status.is_success() && !readable(&content_type) {
        return forbidden(format!(
            "'{}' is not a readable content type; the fetch route serves text, JSON, XML and \
             YAML, never something that runs (AG-65)",
            match content_type.is_empty() {
                true => "(none)",
                false => &content_type,
            }
        ))
        .into_response();
    }

    let body = response.bytes().await.unwrap_or_default();
    if body.len() as u64 > run.max_response_bytes {
        return (
            StatusCode::PAYLOAD_TOO_LARGE,
            jc_core::ProblemDetails::new(
                413,
                "payload-too-large",
                format!(
                    "the answer is larger than this run's maxResponseBytes ({})",
                    run.max_response_bytes
                ),
            ),
        )
            .into_response();
    }

    // Counted after the fact and never truncated: a run may cross its budget by one answer,
    // which `maxResponseBytes` already bounds, and the next fetch is the one that is refused.
    let left = state
        .limits
        .record_egress(&run.id, body.len() as u64, budget)
        .await;

    log_request(&AuditEntry {
        run_id: &run.id,
        user: &run.created_by,
        upstream: &final_host,
        method: "GET",
        path: target.path(),
        status: status.as_u16(),
        bytes: body.len(),
        duration_ms: start.elapsed().as_millis(),
    });

    Response::builder()
        .status(status)
        .header(
            axum::http::header::CONTENT_TYPE,
            match content_type.is_empty() {
                true => "application/octet-stream".to_owned(),
                false => content_type,
            },
        )
        .header(REMAINING_HEADER, left.to_string())
        .body(Body::from(body))
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
}

/// One URL the run is allowed to ask for, or the problem document saying why not.
///
/// Every rule here is the difference between a documentation reader and an exfiltration path:
/// plain HTTP would put the request on the wire in the clear, a host off the list is a host the
/// operator never reviewed, and a URL carrying a credential is one the agent should not be
/// handing to anybody.
pub(crate) fn checked(raw: &str, allowed: &[String]) -> Result<Url, Box<Response>> {
    let url = Url::parse(raw)
        .map_err(|e| Box::new(bad_request(format!("'{raw}' is not a URL: {e}")).into_response()))?;
    if url.scheme() != "https" {
        return Err(Box::new(
            forbidden(format!(
                "scheme '{}' is refused; an outside host is reached over https only (AG-65)",
                url.scheme()
            ))
            .into_response(),
        ));
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(Box::new(
            bad_request("the URL carries userinfo; a fetch sends no credential anywhere (AG-65)")
                .into_response(),
        ));
    }
    for (name, _) in url.query_pairs() {
        let lowered = name.to_ascii_lowercase();
        if CREDENTIAL_PARAMETERS.contains(&lowered.as_str()) {
            return Err(Box::new(
                bad_request(format!(
                    "the URL carries a '{name}' parameter, which reads like a credential; a fetch \
                 sends no credential anywhere (AG-65)"
                ))
                .into_response(),
            ));
        }
    }
    let host = url.host_str().unwrap_or_default().to_ascii_lowercase();
    if !allowed
        .iter()
        .any(|entry| entry.eq_ignore_ascii_case(&host))
    {
        return Err(Box::new(
            forbidden(format!(
                "host '{host}' is not in the profile's egress allow-list (AG-50)"
            ))
            .into_response(),
        ));
    }
    Ok(url)
}

/// The answer, following redirects by hand so every hop is checked the way the first URL was.
///
/// `reqwest`'s own policy cannot see the allow-list, and a redirect is exactly how a host on the
/// list would hand the run to one that is not.
pub(crate) async fn follow(
    state: &ProxyState,
    target: &Url,
    allowed: &[String],
) -> Result<(reqwest::Response, String), Box<Response>> {
    let mut url = target.clone();
    for _ in 0..=MAX_REDIRECTS {
        let host = url.host_str().unwrap_or_default().to_owned();
        let response = outbound(&state.egress, &url).send().await.map_err(|e| {
            jc_core::ProblemDetails::new(502, "upstream-unavailable", e.to_string()).into_response()
        })?;
        if !response.status().is_redirection() {
            return Ok((response, host));
        }
        let location = response
            .headers()
            .get(reqwest::header::LOCATION)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_owned();
        url = next_hop(&url, &location, allowed)?;
    }
    Err(Box::new(
        forbidden(format!(
            "more than {MAX_REDIRECTS} redirects; the fetch route does not follow a chain that \
             long"
        ))
        .into_response(),
    ))
}

/// The request that goes out: the URL and one `User-Agent`.
///
/// Built from nothing but those two on purpose. The inbound request carries the run ticket, and
/// may carry an `Authorization` header and cookies; none of it is read here, so none of it can
/// reach an upstream (AG-65).
fn outbound(client: &reqwest::Client, url: &Url) -> reqwest::RequestBuilder {
    client
        .get(url.clone())
        .header(reqwest::header::USER_AGENT, "jc-agent-proxy")
}

/// Where a redirect points, checked the way the first URL was.
///
/// A redirect is exactly how a host on the allow-list would hand the run to one that is not, so
/// each hop goes through the same rules: https, on the list, no credential in the URL.
fn next_hop(current: &Url, location: &str, allowed: &[String]) -> Result<Url, Box<Response>> {
    let next = current.join(location).map_err(|e| {
        Box::new(
            jc_core::ProblemDetails::new(
                502,
                "upstream-unavailable",
                format!("the redirect to '{location}' is not a URL: {e}"),
            )
            .into_response(),
        )
    })?;
    checked(next.as_str(), allowed)
}

fn readable(content_type: &str) -> bool {
    let essence = content_type
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    READABLE.contains(&essence.as_str()) || essence.ends_with("+json") || essence.ends_with("+xml")
}

fn bad_request(detail: impl Into<String>) -> jc_core::ProblemDetails {
    jc_core::ProblemDetails::new(400, "bad-request", detail.into())
}

fn forbidden(detail: impl Into<String>) -> jc_core::ProblemDetails {
    jc_core::ProblemDetails::forbidden().with_detail(detail.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn allowed() -> Vec<String> {
        vec!["docs.maplibre.org".to_owned(), "Docs.RS".to_owned()]
    }

    #[test]
    fn a_host_on_the_list_is_accepted_whatever_its_case() {
        assert!(checked("https://docs.maplibre.org/api/map/", &allowed()).is_ok());
        assert!(checked("https://DOCS.rs/serde/latest/", &allowed()).is_ok());
    }

    #[test]
    fn a_neighbour_of_an_allowed_host_is_not_an_allowed_host() {
        // The mistake a substring check makes: `maplibre.org.evil.test` ends with nothing the
        // list names, and `evil-docs.maplibre.org` is a different host than the one reviewed.
        for url in [
            "https://docs.maplibre.org.evil.test/api/",
            "https://evil-docs.maplibre.org/api/",
            "https://maplibre.org/api/",
        ] {
            assert!(checked(url, &allowed()).is_err(), "{url} was allowed");
        }
    }

    #[test]
    fn plain_http_and_every_other_scheme_is_refused() {
        for url in [
            "http://docs.maplibre.org/api/",
            "file:///etc/passwd",
            "ftp://docs.maplibre.org/api/",
        ] {
            assert!(checked(url, &allowed()).is_err(), "{url} was allowed");
        }
    }

    #[test]
    fn a_url_carrying_a_credential_is_refused_rather_than_sent() {
        for url in [
            "https://user:password@docs.maplibre.org/api/",
            "https://docs.maplibre.org/api/?token=abcdef",
            "https://docs.maplibre.org/api/?page=2&API_KEY=abcdef",
        ] {
            assert!(checked(url, &allowed()).is_err(), "{url} was allowed");
        }
        assert!(checked("https://docs.maplibre.org/api/?page=2", &allowed()).is_ok());
    }

    #[test]
    fn a_redirect_is_followed_only_to_a_host_on_the_list() {
        let from = Url::parse("https://docs.maplibre.org/api/map/").expect("a URL");
        // A relative hop inside the same host is the usual case and keeps working.
        assert_eq!(
            next_hop(&from, "/api/map/latest/", &allowed())
                .map(|url| url.to_string())
                .unwrap_or_default(),
            "https://docs.maplibre.org/api/map/latest/"
        );
        // The dangerous ones: off the list, back to plain HTTP, and carrying a credential.
        for location in [
            "https://cdn.evil.test/payload",
            "http://docs.maplibre.org/api/",
            "https://docs.maplibre.org/api/?access_token=abcdef",
        ] {
            assert!(
                next_hop(&from, location, &allowed()).is_err(),
                "{location} was followed"
            );
        }
    }

    /// T-1695: the packages route follows a registry's redirect through the same check. A
    /// registry answers a download with a hop to its CDN, and that CDN is a host the profile
    /// named or it is not reached — `state.http` used to chase it wherever the registry pointed.
    #[test]
    fn a_registry_hands_a_download_on_only_to_a_host_the_profile_named() {
        let allowed = vec!["crates.io".to_owned(), "static.crates.io".to_owned()];
        let from =
            Url::parse("https://crates.io/api/v1/crates/serde/1.0.0/download").expect("a URL");
        assert_eq!(
            next_hop(
                &from,
                "https://static.crates.io/crates/serde/serde-1.0.0.crate",
                &allowed
            )
            .map(|url| url.to_string())
            .unwrap_or_default(),
            "https://static.crates.io/crates/serde/serde-1.0.0.crate",
        );
        for location in [
            "https://cdn.evil.test/serde-1.0.0.crate",
            "https://crates.io.evil.test/serde-1.0.0.crate",
            "http://static.crates.io/serde-1.0.0.crate",
            "https://static.crates.io/x?token=abcdef",
        ] {
            assert!(
                next_hop(&from, location, &allowed).is_err(),
                "{location} was followed",
            );
        }
    }

    #[test]
    fn the_outbound_request_carries_no_header_of_the_inbound_one() {
        // The inbound request carries the run ticket and may carry an Authorization header and
        // cookies. The outbound one is built from the URL and a user agent, so there is nothing
        // to strip and nothing to forget to strip.
        let client = reqwest::Client::new();
        let url = Url::parse("https://docs.maplibre.org/api/").expect("a URL");
        let request = outbound(&client, &url).build().expect("a request");
        let names: Vec<String> = request
            .headers()
            .keys()
            .map(|name| name.as_str().to_owned())
            .collect();
        assert_eq!(names, vec!["user-agent".to_owned()]);
        assert!(request.body().is_none());
    }

    #[test]
    fn only_readable_content_types_pass() {
        for ok in [
            "text/plain; charset=utf-8",
            "application/json",
            "text/markdown",
            "application/problem+json",
            "image/svg+xml",
        ] {
            assert!(readable(ok), "{ok} was refused");
        }
        for refused in [
            "application/octet-stream",
            "application/wasm",
            "application/x-executable",
            "image/png",
            "",
        ] {
            assert!(!readable(refused), "{refused} was allowed");
        }
    }
}
