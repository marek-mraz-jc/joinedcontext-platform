//! Outbound package download mediation for package managers (/v1/packages/*).

use crate::audit::{log_request, AuditEntry};
use crate::auth::authenticate;
use crate::ProxyState;
use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use std::time::Instant;

pub async fn handler(
    State(state): State<ProxyState>,
    method: Method,
    headers: HeaderMap,
    Path((host, rest)): Path<(String, String)>,
) -> Response {
    let start = Instant::now();
    let run = match authenticate(&headers, &state.runs, &state.config).await {
        Ok(r) => r,
        Err(p) => return (*p).into_response(),
    };

    if method != Method::GET {
        return StatusCode::METHOD_NOT_ALLOWED.into_response();
    }

    // The path must stay a path on the host it is appended to (T-1301).
    if super::escapes(&rest) {
        return jc_core::ProblemDetails::forbidden()
            .with_detail("path traversal not permitted")
            .into_response();
    }

    // https, a host the profile names, and no credential in the URL — the fetch route's rules,
    // because they are the same rules: a registry is an outside host a run was allowed to read.
    let target = match super::fetch::checked(&format!("https://{host}/{rest}"), &run.allowed_hosts)
    {
        Ok(url) => url,
        Err(problem) => return *problem,
    };

    // A registry answers a download with a redirect to its CDN, so the hops are followed — each
    // one checked against the same allow-list as the first URL. `state.http` used to carry this
    // request and followed whatever the registry named, to any host on the internet, which is a
    // redirect away from an allow-list that had already been passed (AG-50, T-1695).
    let (upstream_resp, reached) =
        match super::fetch::follow(&state, &target, &run.allowed_hosts).await {
            Ok(reached) => reached,
            Err(problem) => return *problem,
        };

    let status =
        StatusCode::from_u16(upstream_resp.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    let resp_bytes = upstream_resp.bytes().await.unwrap_or_default();

    if (resp_bytes.len() as u64) > run.max_response_bytes {
        return (
            StatusCode::PAYLOAD_TOO_LARGE,
            jc_core::ProblemDetails::new(
                413,
                "payload-too-large",
                "package download exceeds byte limit",
            ),
        )
            .into_response();
    }

    log_request(&AuditEntry {
        run_id: &run.id,
        user: &run.created_by,
        upstream: &reached,
        method: "GET",
        path: target.path(),
        status: status.as_u16(),
        bytes: resp_bytes.len(),
        duration_ms: start.elapsed().as_millis(),
    });

    Response::builder()
        .status(status)
        .body(Body::from(resp_bytes))
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
}
