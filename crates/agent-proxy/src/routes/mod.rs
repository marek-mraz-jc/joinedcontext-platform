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
