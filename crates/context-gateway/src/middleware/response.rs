//! What the gateway concluded internally never leaves in a header the caller did not ask
//! for (SP-05, R22).
//!
//! The counterpart of [`super::tenancy::strip_client_headers`]: that one removes what a
//! client said on the way in, this one removes what the gateway said on the way out. One
//! layer rather than a line at every place that builds an answer, so a handler added later
//! cannot forget it.

use crate::app::Gateway;
use crate::handlers::schema::Artifact;
use axum::extract::{Request, State};
use axum::http::{HeaderName, HeaderValue};
use axum::middleware::Next;
use axum::response::Response;
use std::sync::Arc;

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
/// it in, says that no answer of this gateway belongs in a shared cache, and points every answer
/// that carries data at the schema it is described by.
pub async fn scrub(State(gateway): State<Arc<Gateway>>, request: Request, next: Next) -> Response {
    let asked = asked_about_narrowing(&request);
    // Read before the handler runs, because the path is what names the endpoint and the
    // request is moved into it.
    let described = describedby(&gateway, request.uri().path());
    let mut response = next.run(request).await;
    let carries_data = response.status().is_success();
    let headers = response.headers_mut();
    // EP-50: only on an answer that carries data. A refusal that named a schema would tell a
    // caller who was refused that the endpoint exists (EP-03, R20).
    if carries_data {
        for link in described {
            headers.append(axum::http::header::LINK, link);
        }
    }
    // The tenant is an internal name and a probe for which spaces exist. It is pinned on the
    // hop to the broker and it never comes back out, on either surface (SP-05).
    while headers.remove(&super::tenancy::TENANT).is_some() {}
    if !asked {
        while headers.remove(&RESULTS_RESTRICTED).is_some() {}
    }
    keep_out_of_shared_caches(headers);
    response
}

/// The schema documents an answer is described by, as `Link` headers (EP-50, EP-46, EP-49).
///
/// One link per formalism a consumer validates with — the JSON Schema and the SHACL shapes —
/// and one set per model major the endpoint publishes, so an endpoint carrying two majors names
/// four. The target is the schema surface that already serves them, written as a path so it
/// resolves against whatever host the request arrived on rather than against a `Host` header the
/// gateway would have to trust.
///
/// The link says nothing about the caller: it is the same for everyone on one endpoint, which is
/// why `Vary` does not change. What the document behind it holds is projected to the grant when
/// it is fetched (EP-47).
fn describedby(gateway: &Gateway, path: &str) -> Vec<HeaderValue> {
    let Some(endpoint) = data_endpoint(gateway, path) else {
        return Vec::new();
    };
    let base = format!("{}{}", gateway.base_url(), endpoint.base_path);
    let mut majors: Vec<u32> = endpoint.models.iter().map(|model| model.major).collect();
    majors.sort_unstable();
    majors.dedup();

    let mut links = Vec::new();
    for major in majors {
        for artifact in [Artifact::JsonSchema, Artifact::Shacl] {
            let target = format!(
                "<{base}/schema/v{major}/{}>; rel=\"describedby\"; type=\"{}\"",
                artifact.file_name(),
                artifact.media_type()
            );
            if let Ok(value) = HeaderValue::from_str(&target) {
                links.push(value);
            }
        }
    }
    links
}

/// The endpoint whose data this path serves, or `None` where the path serves something else.
///
/// The data representations of EP-50 and nothing else: the NGSI-LD tree, the file downloads, the
/// OGC resource tree and the SensorThings one, on the endpoint surface and on the space surface.
/// The schema documents themselves, the endpoint record, the access surface and the probes are
/// not data and describe themselves.
fn data_endpoint(gateway: &Gateway, path: &str) -> Option<Arc<crate::resolver::Endpoint>> {
    let carries_data = |rest: &str| {
        rest.starts_with("ngsi-ld/v1")
            || rest.starts_with("file.")
            || rest.starts_with("ogc/features")
            || rest.starts_with("sta/v1.1")
    };
    if let Some(rest) = path.strip_prefix("/api/endpoint/") {
        let (slug, rest) = rest.split_once('/')?;
        return carries_data(rest)
            .then(|| gateway.resolver.resolve(slug))
            .flatten();
    }
    if let Some(rest) = path.strip_prefix("/cs/") {
        let (space, rest) = rest.split_once('/')?;
        return carries_data(rest)
            .then(|| gateway.resolver.resolve_space(space))
            .flatten()
            .map(|space| Arc::clone(&space.endpoint));
    }
    None
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
    // `Authorization` is what the answer differs by; the request headers a caller may narrow
    // or shape the answer with are named so a cache keyed on them cannot mix them either.
    // `Accept-Language` is one of them since EP-37: it picks the text of a LanguageProperty in
    // a GeoJSON feature and the title of the OGC landing page.
    headers.insert(
        axum::http::header::VARY,
        HeaderValue::from_static(
            "Authorization, Accept, Accept-Language, NGSILD-Results-Restricted",
        ),
    );
}
