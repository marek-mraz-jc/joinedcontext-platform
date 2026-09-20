//! What the person said, relayed to the workspace (/v1/runs/inbox).
//!
//! The run comes from the ticket the request carries, never from the query: a workspace asks
//! "what was said to me" and cannot ask it about another run (AG-46, AG-52).

use crate::audit::{log_request, AuditEntry};
use crate::auth::authenticate;
use crate::routes::portal_bearer;
use crate::ProxyState;
use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use std::time::Instant;

/// Longest a workspace may hold the call open. The Portal clamps it again on its side; this is
/// the proxy refusing to hold a socket for an agent that asked for an hour.
const MAX_WAIT_SECS: u64 = 25;

#[derive(serde::Deserialize)]
pub struct InboxQuery {
    #[serde(default)]
    pub after: i64,
    pub wait: Option<u64>,
}

pub async fn handler(
    State(state): State<ProxyState>,
    headers: HeaderMap,
    Query(query): Query<InboxQuery>,
) -> impl IntoResponse {
    let start = Instant::now();
    let run = match authenticate(&headers, &state.runs, &state.config).await {
        Ok(r) => r,
        Err(p) => return (*p).into_response(),
    };

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

    let wait = query.wait.unwrap_or(MAX_WAIT_SECS).min(MAX_WAIT_SECS);
    let path = format!("internal/agent-runs/{}/inbox", run.id);
    let mut url = state.config.portal_base.clone();
    url.set_path(&path);
    url.set_query(Some(&format!("after={}&wait={wait}", query.after)));

    let bearer = match portal_bearer(&state.credentials).await {
        Ok(token) => token,
        Err(response) => return *response,
    };
    let resp = state.http.get(url).bearer_auth(&bearer).send().await;

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
                method: "GET",
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
        method: "GET",
        path: &path,
        status: status.as_u16(),
        bytes: bytes.len(),
        duration_ms: start.elapsed().as_millis(),
    });

    (status, bytes).into_response()
}
