pub mod body;
pub mod data;
pub mod diagnostics;
pub mod events;
pub mod fetch;
pub mod forge;
pub mod inbox;
pub mod llm;
pub mod mcp;
pub mod packages;

/// The bearer this proxy presents on the Portal's internal listener, or the answer a caller gets
/// when the realm cannot be reached (AG-52, T-2271).
///
/// Every callback goes through here, so there is one place where a realm outage is visible and one
/// place that decides what a run is told: 503, because the run may retry, and never the reason —
/// which names the realm and the client.
pub(crate) async fn portal_bearer(
    credentials: &crate::inject::CredentialManager,
) -> Result<String, Box<axum::response::Response>> {
    use axum::response::IntoResponse;
    credentials.get_portal_token().await.map_err(|error| {
        tracing::warn!(%error, "no token for the Portal's internal listener");
        (
            axum::http::StatusCode::SERVICE_UNAVAILABLE,
            "the proxy could not obtain a token of its own",
        )
            .into_response()
            .into()
    })
}

/// The refusal an upstream's redirect earns, or `None` when the answer is not a redirect.
///
/// The proxy's upstream client follows nothing by itself (`no_redirect_client`), so a `3xx` from
/// the gateway, the forge or the model provider arrives here instead of being chased with the
/// credential still attached. It is answered rather than relayed for two reasons: `Location` names
/// an address inside the cluster, which is not the workspace's business, and a run that received a
/// bare `302` would have nothing it could act on. An upstream that wants this proxy somewhere else
/// is a misconfiguration or an attack, and both belong in the operator's log (AG-52, T-1695).
pub(crate) fn refused_redirect(
    status: axum::http::StatusCode,
    location: Option<&str>,
    upstream: &str,
) -> Option<axum::response::Response> {
    use axum::response::IntoResponse;
    if !status.is_redirection() {
        return None;
    }
    tracing::warn!(
        %upstream,
        status = status.as_u16(),
        location = location.unwrap_or("(none)"),
        "an upstream answered a credentialed request with a redirect; it was not followed"
    );
    Some(
        jc_core::ProblemDetails::new(
            502,
            "upstream-redirect",
            format!(
                "the {upstream} answered with a redirect, which this proxy does not follow: a \
                 request that carries a credential is made to the address the platform \
                 configured and to no other. Nothing was sent on. Ask an operator to check that \
                 upstream's address (AG-52)."
            ),
        )
        .into_response(),
    )
}

/// The `Location` of an upstream answer, for the operator's log and nothing else.
pub(crate) fn location_of(response: &reqwest::Response) -> Option<&str> {
    response
        .headers()
        .get(reqwest::header::LOCATION)
        .and_then(|value| value.to_str().ok())
}

/// The names a run is allowed to learn for the things behind this proxy.
///
/// A role, never an address: the workspace is the untrusted side and its NetworkPolicy lets it
/// reach kube-dns and this proxy and nothing else, precisely so it cannot learn the cluster's
/// shape (AG-35, AG-40).
pub(crate) const PORTAL: &str = "the Portal";
pub(crate) const GATEWAY: &str = "the context gateway";
pub(crate) const FORGE: &str = "the git forge";
pub(crate) const MODEL: &str = "the model provider";

/// A correlation id: what the run is told, and what an operator greps the proxy's log for.
fn correlation_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| {
            u64::try_from(since.as_nanos()).unwrap_or(u64::MAX)
        });
    format!(
        "{nanos:x}{:04x}",
        NEXT.fetch_add(1, Ordering::Relaxed) & 0xffff
    )
}

/// The answer a workspace gets when an upstream cannot be reached (AG-64, T-2364).
///
/// A `reqwest` transport error carries the whole internal URL it failed on: scheme, cluster
/// hostname, port and internal path. Handing that to the workspace draws it the map its
/// NetworkPolicy exists to withhold, and it does so on every outage. So the error goes to the
/// log beside a correlation id, and the run is told which upstream is down by role, with the
/// same id to quote. `502`, not `500`: the proxy is well, the thing behind it is not, and the
/// run may retry.
pub(crate) fn upstream_unavailable(
    upstream: &str,
    error: &dyn std::fmt::Display,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    let request_id = correlation_id();
    tracing::warn!(%upstream, %request_id, error = %error, "an upstream could not be reached");
    jc_core::ProblemDetails::new(502, "upstream-unavailable", "Upstream Unavailable")
        .with_detail(format!(
            "{upstream} could not be reached; the proxy may be retried"
        ))
        .with_extension("requestId", serde_json::Value::String(request_id))
        .into_response()
}

/// Whether a path a workspace sends would leave the base it is appended to once the outbound URL
/// is parsed. axum has decoded the path once; the URL parser follows WHATWG, which reads `%2e%2e`
/// as a `..` segment and a backslash as a slash in an http(s) URL, so nothing still encoded, no
/// backslash, no dot segment and no empty segment gets through (T-0817, T-1300, T-1301). A
/// trailing slash, a directory listing, stays allowed.
pub(crate) fn escapes(path: &str) -> bool {
    path.contains('%')
        || path.contains('\\')
        || path.starts_with('/')
        || path.contains("//")
        || path.split('/').any(|segment| matches!(segment, "." | ".."))
}

#[cfg(test)]
mod tests {
    use super::escapes;

    #[test]
    fn a_path_that_would_leave_its_base_is_refused_and_an_ordinary_one_is_not() {
        for path in [
            "..",
            "../admin",
            "ngsi-ld/v1/../../x",
            "%2e%2e/x",
            "%252e%252e/x",
            "a/%2Fb",
            "a\\..\\b",
            "/etc/passwd",
            "a//b",
            "./x",
        ] {
            assert!(escapes(path), "{path}");
        }
        for path in [
            "ngsi-ld/v1/entities",
            "ngsi-ld/v1/entities/urn:ngsi-ld:Bike:hel.fi:helsinki:a..b",
            "mcp",
            "api/v1/crates/serde/1.0.0/download",
            "projects/helsinki/apps/bikes/",
            "",
        ] {
            assert!(!escapes(path), "{path}");
        }
    }
}
