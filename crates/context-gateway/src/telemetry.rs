//! The gateway's own Prometheus surface (OPS-16, TS-22).
//!
//! `components/monitoring` scrapes `/metrics` on the service's `http` port every fifteen
//! seconds. The endpoint stays inside the cluster: no APISIX route, no ingress, and nothing
//! here reads a token or a body.
//!
//! Every series the gateway publishes is named in this module and nowhere else, so the
//! surface documents itself and a dashboard can be written against a list rather than a
//! grep. Labels are bounded by configuration — a route pattern, an endpoint slug, an
//! operation of the closed CIM 009 vocabulary — and never by anything a caller supplies: an
//! entity id or a token in a label is an unbounded series and a disclosure at once.

use std::sync::OnceLock;
use std::time::Instant;

use axum::body::Body;
use axum::extract::{MatchedPath, Request};
use axum::http::header::CONTENT_TYPE;
use axum::http::Response;
use axum::middleware::Next;
use axum::response::IntoResponse;
use jc_core::kinds::Operation;
use metrics_exporter_prometheus::{PrometheusBuilder, PrometheusHandle};

/// The Prometheus text format, the version every scraper since 2014 reads.
pub const TEXT_FORMAT: &str = "text/plain; version=0.0.4";

/// Requests that reached the surface, by route, endpoint and status.
const REQUESTS: &str = "jc_gateway_requests_total";
/// How long the gateway took to answer one, the enforcement point included.
const REQUEST_SECONDS: &str = "jc_gateway_request_duration_seconds";
/// What the policy decision point decided, by operation and verdict (GW1).
const DECISIONS: &str = "jc_gateway_pdp_decisions_total";
/// The broker round trip inside one answer, which is most of a slow request.
const BROKER_SECONDS: &str = "jc_gateway_broker_request_duration_seconds";

/// The paths that describe the process rather than the traffic. Counting a scrape as a
/// request would make the rate a function of the scrape interval.
const UNCOUNTED: &[&str] = &["/metrics", "/healthz", "/livez"];

/// The bucket edges every duration here is counted into, in seconds.
///
/// Without them the exporter renders a duration as a *summary*: a quantile computed inside one
/// replica, which cannot be combined with another replica's. The gateway runs behind an HPA, so
/// the p95 a dashboard draws would be the p95 of whichever pod Prometheus happened to label —
/// buckets can be summed, and the answer stays right however many pods are running.
///
/// The edges are placed for the budget the platform is held to: p95 ≤ 3ms and p99 ≤ 8ms of
/// gateway overhead (docs Deployment/05 §2), so the resolution is where the answer is decided.
const SECONDS: &[f64] = &[
    0.001, 0.0025, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0,
];

fn handle() -> &'static PrometheusHandle {
    static HANDLE: OnceLock<PrometheusHandle> = OnceLock::new();
    HANDLE.get_or_init(|| {
        let handle = PrometheusBuilder::new()
            .set_buckets(SECONDS)
            .expect("the bucket list is not empty")
            .install_recorder()
            .expect("this process installs the one recorder");
        describe();
        handle
    })
}

/// Installs the recorder and describes the series.
///
/// Called when the router is built rather than on the first scrape: the `metrics::` macros
/// no-op until a recorder exists, so a surface that starts serving before this runs answers
/// requests nobody counted.
pub fn install() {
    let _ = handle();
}

fn describe() {
    metrics::describe_counter!(REQUESTS, "requests answered by the gateway surface");
    metrics::describe_histogram!(REQUEST_SECONDS, "seconds to answer one request");
    metrics::describe_counter!(DECISIONS, "policy decisions, by operation and verdict");
    metrics::describe_histogram!(BROKER_SECONDS, "seconds of one broker round trip");
}

/// The scrape.
pub async fn metrics() -> Response<Body> {
    ([(CONTENT_TYPE, TEXT_FORMAT)], handle().render()).into_response()
}

/// One decision, as the decision point made it (GW1).
pub fn decided(operation: Operation, denied: bool) {
    metrics::counter!(
        DECISIONS,
        "operation" => operation.as_str(),
        "verdict" => if denied { "deny" } else { "rewrite" },
    )
    .increment(1);
}

/// One broker round trip, however it ended.
pub fn broker_round_trip(seconds: f64, reached: bool) {
    metrics::histogram!(BROKER_SECONDS, "reached" => if reached { "yes" } else { "no" })
        .record(seconds);
}

/// Counts and times every request the surface answers.
pub async fn record(request: Request, next: Next) -> axum::response::Response {
    let route = request
        .extensions()
        .get::<MatchedPath>()
        .map(|matched| matched.as_str().to_owned())
        // Nothing matched, so the fallback answered. One series rather than one per path a
        // scanner invents.
        .unwrap_or_else(|| "unmatched".to_owned());
    if UNCOUNTED.contains(&route.as_str()) {
        return next.run(request).await;
    }

    let method = request.method().as_str().to_owned();
    let endpoint = named(request.uri().path()).unwrap_or("-").to_owned();
    let started = Instant::now();
    let response = next.run(request).await;
    let seconds = started.elapsed().as_secs_f64();

    metrics::counter!(
        REQUESTS,
        "route" => route.clone(),
        "method" => method.clone(),
        "endpoint" => endpoint.clone(),
        "status" => response.status().as_u16().to_string(),
    )
    .increment(1);
    metrics::histogram!(
        REQUEST_SECONDS,
        "route" => route,
        "method" => method,
        "endpoint" => endpoint,
    )
    .record(seconds);
    response
}

/// The endpoint slug or space name a path belongs to.
///
/// Both surfaces name a configured thing in the same position, and there are as many of them
/// as the repository declares, so the label stays bounded (EP-05, SP-03).
fn named(path: &str) -> Option<&str> {
    let mut segments = path.split('/').skip(1);
    match segments.next()? {
        "api" => match segments.next()? {
            "endpoint" => segments.next(),
            _ => None,
        },
        "cs" => segments.next(),
        _ => None,
    }
    .filter(|name| !name.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_series_is_named_after_the_endpoint_or_the_space_it_belongs_to() {
        assert_eq!(
            named("/api/endpoint/public-air/ngsi-ld/v1/entities"),
            Some("public-air")
        );
        assert_eq!(named("/api/endpoint/public-air"), Some("public-air"));
        assert_eq!(named("/cs/ovzdusie/ngsi-ld/v1/entities"), Some("ovzdusie"));
    }

    /// A path with no endpoint in it must not turn its next segment into a label, or a
    /// scanner walking `/foo/bar` writes one series per guess.
    #[test]
    fn a_path_that_names_neither_carries_no_name() {
        assert_eq!(named("/healthz"), None);
        assert_eq!(named("/api/other/thing"), None);
        assert_eq!(named("/api/endpoint/"), None);
        assert_eq!(named("/"), None);
    }
}
