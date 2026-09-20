//! What the gateway concluded internally never leaves in a header the caller did not ask
//! for (SP-05, R22).
//!
//! The counterpart of [`super::tenancy::strip_client_headers`]: that one removes what a
//! client said on the way in, this one removes what the gateway said on the way out. One
//! layer rather than a line at every place that builds an answer, so a handler added later
//! cannot forget it.

use axum::extract::Request;
use axum::http::{HeaderName, HeaderValue};
use axum::middleware::Next;
use axum::response::Response;

/// The narrowing signal. Opt-in: a caller who sends it on the request is told when an answer
/// was narrowed, and a caller who does not ask is answered as if the result were simply what
/// it is (R22, GW12). It tells a prober that something was there to hide.
pub const RESULTS_RESTRICTED: HeaderName = HeaderName::from_static("ngsild-results-restricted");

/// What a caller has to know to read the answer they got (CIM 009 clause 6.3.11).
///
/// Sent to everybody, unlike [`RESULTS_RESTRICTED`]: it names types the caller may read and says
/// why they were not queried, which is the difference between an empty list a developer can fix
/// and one they bisect with more filters until it answers (T-1862).
pub const WARNING: HeaderName = HeaderName::from_static("ngsild-warning");

/// The total a broker reports for a query that asked to be counted (CIM 009 clause 6.3.13).
///
/// The gateway never writes it: it is the broker's number, over the entities the broker saw. It
/// is removed from an answer the gateway narrowed, because a total over what was withheld is the
/// same disclosure as the withheld entities (R22, T-2131).
pub const RESULTS_COUNT: HeaderName = HeaderName::from_static("ngsild-results-count");

/// Whether the request asked to be told about narrowing (R22).
pub fn asked_about_narrowing(request: &Request) -> bool {
    request
        .headers()
        .get_all(&RESULTS_RESTRICTED)
        .iter()
        .any(|value| value.as_bytes().eq_ignore_ascii_case(b"true"))
}

/// Removes the tenant from every answer, the narrowing signal from every answer nobody asked for
/// it in, and says that no answer of this gateway belongs in a shared cache.
pub async fn scrub(request: Request, next: Next) -> Response {
    let asked = asked_about_narrowing(&request);
    let mut response = next.run(request).await;
    let headers = response.headers_mut();
    // The tenant is an internal name and a probe for which spaces exist. It is pinned on the
    // hop to the broker and it never comes back out, on either surface (SP-05).
    while headers.remove(&super::tenancy::TENANT).is_some() {}
    if !asked {
        while headers.remove(&RESULTS_RESTRICTED).is_some() {}
    }
    keep_out_of_shared_caches(headers);
    response
}

/// Says that every answer here is one caller's (R9, EP-26, T-2261).
///
/// Two callers share a URL and are answered differently, because the answer is the intersection of
/// that URL with their own grants. A shared cache that stored one and replayed it to the other
/// would serve an answer nobody decided — so every answer says whose it is, and on what the
/// difference depends. The edge sets `no-store` on top of this today; the gateway is the
/// enforcement point and does not depend on the edge for it, exactly as it does not depend on the
/// broker for the projection.
///
/// A document the gateway wants revalidated keeps its own `Cache-Control` (the schema artifacts
/// carry `no-cache` with a strong `ETag`, EP-51) and only gains `private` and the `Vary`.
fn keep_out_of_shared_caches(headers: &mut axum::http::HeaderMap) {
    let revalidated = headers
        .get(axum::http::header::CACHE_CONTROL)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.contains("no-cache"));
    headers.insert(
        axum::http::header::CACHE_CONTROL,
        match revalidated {
            true => HeaderValue::from_static("private, no-cache"),
            false => HeaderValue::from_static("private, no-store"),
        },
    );
    // `Authorization` is what the answer differs by; the two request headers a caller may narrow
    // themselves with are named so a cache keyed on them cannot mix them either.
    headers.insert(
        axum::http::header::VARY,
        HeaderValue::from_static("Authorization, Accept, NGSILD-Results-Restricted"),
    );
}
