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

    let host_lower = host.to_lowercase();
    if !run
        .allowed_hosts
        .iter()
        .any(|h| h.eq_ignore_ascii_case(&host_lower))
    {
        return jc_core::ProblemDetails::forbidden()
            .with_detail(format!(
                "host '{host}' not present in profile allow-list (AG-50)"
            ))
            .into_response();
    }

    // The host is allow-listed; the path must stay a path on it (T-1301).
    if super::escapes(&rest) {
        return jc_core::ProblemDetails::forbidden()
            .with_detail("path traversal not permitted")
            .into_response();
    }
    let target_url = format!("https://{host}/{rest}");
    let upstream_resp = match state.http.get(&target_url).send().await {
        Ok(r) => r,
        Err(e) => return super::upstream_unavailable(super::REGISTRY, &e),
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
        upstream: &host,
        method: "GET",
        path: &rest,
        status: status.as_u16(),
        bytes: resp_bytes.len(),
        duration_ms: start.elapsed().as_millis(),
    });

    Response::builder()
        .status(status)
        .body(Body::from(resp_bytes))
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
}
