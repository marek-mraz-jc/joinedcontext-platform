//! Workspace event forwarding to Portal backend (/v1/runs/events).

use crate::audit::{log_request, AuditEntry};
use crate::auth::authenticate;
use crate::routes::portal_bearer;
use crate::ProxyState;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use std::time::Instant;

/// One event is one line of a conversation (Architecture/19 §4).
///
/// The cap is this route's, not axum's default: without it a workspace decides how much of the
/// Portal's run store and of every connected browser's event stream one event may take (AG-45,
/// AG-46). Counted on the bytes that arrived, before anything is parsed from them.
const MAX_EVENT_BYTES: usize = 64 * 1024;

/// The kinds this proxy reports on the Portal's event channel itself, which a workspace
/// therefore may not post (AG-25, AG-41, T-2363).
///
/// `routes::llm` posts `{"kind":"usage","payload":{"tokensThisStep": n}}` after every model call
/// and the Portal applies it to the run's token and step record. A workspace that could post the
/// same kind would be writing the record its own budget is read from and its steps are counted
/// with, with any number in it, a negative one included. `preview` and `navigate` stay the run's
/// own to send: the first is its output and the second is validated. Matched without case,
/// because the Portal matches the kind exactly and `USAGE` would sail past a `==` here and land
/// as `USAGE` there the day the Portal stops caring about case.
const PROXY_OWN_KINDS: [&str; 1] = ["usage"];

/// One event payload with every secret-shaped run replaced.
///
/// The redaction is textual, so it runs over the serialized payload and the result is parsed
/// back. A scrubbed payload that no longer parses is sent as the one string it is, never as
/// the original: the shape of an event is worth less than the credential in it.
fn redacted(payload: &serde_json::Value) -> serde_json::Value {
    let text = payload.to_string();
    let scrubbed = crate::routes::diagnostics::redact(&text);
    if scrubbed == text {
        return payload.clone();
    }
    serde_json::from_str(&scrubbed).unwrap_or(serde_json::Value::String(scrubbed))
}

#[derive(serde::Deserialize)]
pub struct EventPayload {
    pub kind: String,
    pub payload: serde_json::Value,
}

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

    if raw.len() > MAX_EVENT_BYTES {
        return (
            StatusCode::PAYLOAD_TOO_LARGE,
            jc_core::ProblemDetails::new(
                413,
                "payload-too-large",
                format!(
                    "an event is at most {MAX_EVENT_BYTES} bytes; this one is {}",
                    raw.len()
                ),
            ),
        )
            .into_response();
    }

    let body: EventPayload = match serde_json::from_slice(&raw) {
        Ok(body) => body,
        Err(err) => {
            return (
                StatusCode::BAD_REQUEST,
                jc_core::ProblemDetails::new(400, "invalid-body", err.to_string()),
            )
                .into_response()
        }
    };

    if PROXY_OWN_KINDS
        .iter()
        .any(|own| own.eq_ignore_ascii_case(&body.kind))
    {
        return jc_core::ProblemDetails::forbidden()
            .with_detail(format!(
                "`{}` is the proxy's own accounting channel and cannot be posted by a workspace",
                body.kind
            ))
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

    let mut url = state.config.portal_base.clone();
    url.set_path("internal/agent-runs/events");

    // AG-40, AG-56: the payload is the model's own words, and a credential that slipped into
    // them would be stored, streamed to every reader of the run and read back by the model on
    // the next turn. It is redacted here, before the Portal sees it, by the same rules the
    // diagnostics door uses (T-0957). The document's shape is kept: only the values change,
    // and text that redacts to itself is forwarded as it arrived.
    let payload = redacted(&body.payload);

    let post_body = serde_json::json!({
        "runId": run.id,
        "kind": body.kind,
        "payload": payload,
    });

    let bearer = match portal_bearer(&state.credentials).await {
        Ok(token) => token,
        Err(response) => return *response,
    };
    let resp = state
        .http
        .post(url)
        .bearer_auth(&bearer)
        .json(&post_body)
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
                path: "internal/agent-runs/events",
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
        path: "internal/agent-runs/events",
        status: status.as_u16(),
        bytes: bytes.len(),
        duration_ms: start.elapsed().as_millis(),
    });

    (status, bytes).into_response()
}
