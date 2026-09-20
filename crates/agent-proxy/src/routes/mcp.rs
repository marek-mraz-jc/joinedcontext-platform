//! The operations registry, reached by a run (/v1/mcp).
//!
//! A workspace that has something to change in the platform calls the same MCP server a person's
//! client calls, through this route: the proxy authenticates the run, and the Portal runs the
//! call as the person who started it, narrowed by the run's `AgentProfile` (AG-64, AG-70). The
//! run comes from the ticket, never from the body, so a workspace cannot call as another run.

use crate::audit::{log_request, AuditEntry};
use crate::auth::authenticate;
use crate::routes::portal_bearer;
use crate::ProxyState;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use std::time::Instant;

/// The most a workspace may put in one JSON-RPC message.
///
/// Tighter than the Portal's own ceiling on that door on purpose: a person pastes a manifest into
/// their client, a workspace composes its calls, and the run is the caller with no hands on it.
const MAX_MESSAGE_BYTES: usize = 256 * 1024;

pub async fn handler(
    State(state): State<ProxyState>,
    headers: HeaderMap,
    raw: Bytes,
) -> impl IntoResponse {
    let start = Instant::now();
    let run = match authenticate(&headers, &state.runs, &state.config).await {
        Ok(r) => r,
        Err(p) => return (*p).into_response(),
    };

    if raw.len() > MAX_MESSAGE_BYTES {
        return (
            StatusCode::PAYLOAD_TOO_LARGE,
            jc_core::ProblemDetails::new(
                413,
                "payload-too-large",
                format!(
                    "a message is at most {MAX_MESSAGE_BYTES} bytes; this one is {}",
                    raw.len()
                ),
            ),
        )
            .into_response();
    }

    if let Err(msg) = state
        .limits
        .check_rpm(&run.id, run.requests_per_minute)
        .await
    {
        return (
            StatusCode::TOO_MANY_REQUESTS,
            jc_core::ProblemDetails::new(429, "too-many-requests", msg),
        )
            .into_response();
    }

    let path = format!("internal/agent-runs/{}/mcp", run.id);
    let mut url = state.config.portal_base.clone();
    url.set_path(&path);

    let bearer = match portal_bearer(&state.credentials).await {
        Ok(token) => token,
        Err(response) => return *response,
    };
    let resp = state
        .http
        .post(url)
        .bearer_auth(&bearer)
        .header("content-type", "application/json")
        .body(raw.clone())
        .send()
        .await;

    let (status, bytes) = match resp {
        Ok(r) => {
            let s = StatusCode::from_u16(r.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
            let b = r.bytes().await.unwrap_or_default();
            (s, b)
        }
        Err(e) => {
            log_request(&AuditEntry {
                run_id: &run.id,
                user: &run.created_by,
                upstream: "portal",
                method: "POST",
                path: &path,
                status: 502,
                bytes: 0,
                duration_ms: start.elapsed().as_millis(),
            });
            return super::upstream_unavailable(super::PORTAL, &e);
        }
    };

    log_request(&AuditEntry {
        run_id: &run.id,
        user: &run.created_by,
        upstream: "portal",
        method: "POST",
        path: &path,
        status: status.as_u16(),
        bytes: bytes.len(),
        duration_ms: start.elapsed().as_millis(),
    });

    (
        status,
        [(axum::http::header::CONTENT_TYPE, "application/json")],
        bytes,
    )
        .into_response()
}
