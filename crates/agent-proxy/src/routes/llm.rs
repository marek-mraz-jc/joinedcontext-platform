//! LLM model provider mediation and token budget tracking (/v1/llm/*).

use crate::audit::{log_request, AuditEntry};
use crate::auth::authenticate;
use crate::ProxyState;
use axum::body::{Body, Bytes};
use axum::extract::{Path, State};
use axum::http::{HeaderMap, Method, Request, StatusCode};
use axum::response::{IntoResponse, Response};
use serde_json::{json, Value};
use std::time::Instant;

/// The body with the run's reasoning setting in the shape the endpoint reads (AG-72): OpenRouter's
/// `reasoning.effort` on chat completions, a thinking budget on Anthropic messages, whose
/// `max_tokens` grows by the budget so the answer keeps the room it asked for. A body that
/// already carries a setting, is not a JSON object, or an effort outside the three is sent as is.
fn with_reasoning(body: &Bytes, rest: &str, effort: &str) -> Bytes {
    let budget = match effort {
        "low" => 2048,
        "medium" => 8192,
        "high" => 24576,
        _ => return body.clone(),
    };
    let Ok(Value::Object(mut map)) = serde_json::from_slice::<Value>(body) else {
        return body.clone();
    };
    if rest.ends_with("messages") {
        if map.contains_key("thinking") {
            return body.clone();
        }
        let max_tokens = map.get("max_tokens").and_then(Value::as_u64).unwrap_or(0);
        map.insert("max_tokens".into(), json!(max_tokens + budget));
        map.insert(
            "thinking".into(),
            json!({ "type": "enabled", "budget_tokens": budget }),
        );
    } else {
        if map.contains_key("reasoning") || map.contains_key("reasoning_effort") {
            return body.clone();
        }
        map.insert("reasoning".into(), json!({ "effort": effort }));
    }
    serde_json::to_vec(&map)
        .map(Bytes::from)
        .unwrap_or_else(|_| body.clone())
}

pub async fn handler(
    State(state): State<ProxyState>,
    method: Method,
    headers: HeaderMap,
    Path(rest): Path<String>,
    req: Request<Body>,
) -> Response {
    let start = Instant::now();
    let run = match authenticate(&headers, &state.runs, &state.config).await {
        Ok(r) => r,
        Err(p) => return (*p).into_response(),
    };

    if !matches!(
        rest.as_str(),
        "v1/chat/completions" | "v1/messages" | "chat/completions" | "messages"
    ) {
        return jc_core::ProblemDetails::forbidden()
            .with_detail("only chat/messages completion endpoints permitted")
            .into_response();
    }

    // One model call is one step (AG-51): the run stops here however its driver loops.
    if let Err(msg) = state.limits.check_steps(&run.id, run.steps_per_run).await {
        return (
            StatusCode::TOO_MANY_REQUESTS,
            jc_core::ProblemDetails::new(429, "too-many-requests", msg),
        )
            .into_response();
    }

    if let Err(msg) = state.limits.check_tokens(&run.id, run.max_tokens).await {
        return (
            StatusCode::TOO_MANY_REQUESTS,
            jc_core::ProblemDetails::new(429, "too-many-requests", msg),
        )
            .into_response();
    }

    let body_bytes = match super::body::bounded(req.into_body()).await {
        Ok(bytes) => bytes,
        Err(refusal) => return *refusal,
    };

    let body_bytes = match run.reasoning_effort.as_deref() {
        Some(effort) => with_reasoning(&body_bytes, &rest, effort),
        None => body_bytes,
    };

    let target_url = format!(
        "{}/{}",
        state.config.model_base.as_str().trim_end_matches('/'),
        rest.trim_start_matches('/')
    );

    let key = state.credentials.get_model_key();
    let mut client_req = state.http.request(method.clone(), &target_url);

    if state.config.model_provider == "anthropic" {
        client_req = client_req
            .header("x-api-key", key)
            .header("anthropic-version", "2023-06-01")
            .header("content-type", "application/json");
    } else {
        client_req = client_req
            .bearer_auth(key)
            .header("content-type", "application/json");
    }

    client_req = client_req.body(body_bytes);

    let upstream_resp = match client_req.send().await {
        Ok(r) => r,
        Err(e) => return super::upstream_unavailable(super::MODEL, &e),
    };

    let status =
        StatusCode::from_u16(upstream_resp.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    let resp_bytes = upstream_resp.bytes().await.unwrap_or_default();

    // Extract token usage
    if let Ok(v) = serde_json::from_slice::<serde_json::Value>(&resp_bytes) {
        let tokens = if let Some(usage) = v.get("usage") {
            let total = usage.get("total_tokens").and_then(|n| n.as_u64());
            let input = usage
                .get("input_tokens")
                .and_then(|n| n.as_u64())
                .unwrap_or(0);
            let output = usage
                .get("output_tokens")
                .and_then(|n| n.as_u64())
                .unwrap_or(0);
            total.unwrap_or(input + output)
        } else {
            0
        };

        if tokens > 0 {
            state.limits.record_tokens(&run.id, tokens).await;
            // Report usage asynchronously to Portal
            let portal_base = state.config.portal_base.clone();
            let run_id = run.id.clone();
            let http = state.http.clone();
            let credentials = state.credentials.clone();
            tokio::spawn(async move {
                // The report travels on the same identity as every other callback, minted in the
                // spawned task so the answer to the run is never held up for the realm. A realm
                // that cannot be reached costs this report and nothing else.
                let Ok(bearer) = credentials.get_portal_token().await else {
                    tracing::warn!(run = %run_id, "no token to report usage with");
                    return;
                };
                let mut url = portal_base;
                url.set_path("internal/agent-runs/events");
                let _ = http
                    .post(url)
                    .bearer_auth(bearer)
                    .json(&serde_json::json!({
                        "runId": run_id,
                        "kind": "usage",
                        "payload": { "tokensThisStep": tokens }
                    }))
                    .send()
                    .await;
            });
        }
    }

    log_request(&AuditEntry {
        run_id: &run.id,
        user: &run.created_by,
        upstream: "model-provider",
        method: method.as_str(),
        path: &rest,
        status: status.as_u16(),
        bytes: resp_bytes.len(),
        duration_ms: start.elapsed().as_millis(),
    });

    Response::builder()
        .status(status)
        .header("Content-Type", "application/json")
        .body(Body::from(resp_bytes))
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sent(body: &str, rest: &str, effort: &str) -> Value {
        serde_json::from_slice(&with_reasoning(&Bytes::from(body.to_owned()), rest, effort))
            .unwrap()
    }

    #[test]
    fn chat_completions_carry_the_effort_and_messages_a_thinking_budget() {
        let chat = sent(
            r#"{"model":"m","max_tokens":100}"#,
            "v1/chat/completions",
            "medium",
        );
        assert_eq!(chat["reasoning"], json!({ "effort": "medium" }));
        assert_eq!(chat["max_tokens"], 100);

        let messages = sent(r#"{"model":"m","max_tokens":100}"#, "v1/messages", "low");
        assert_eq!(
            messages["thinking"],
            json!({ "type": "enabled", "budget_tokens": 2048 })
        );
        assert_eq!(messages["max_tokens"], 2148);
    }

    #[test]
    fn a_setting_already_in_the_body_or_an_unknown_effort_is_left_alone() {
        let own = r#"{"reasoning":{"effort":"high"}}"#;
        assert_eq!(
            sent(own, "chat/completions", "low"),
            serde_json::from_str::<Value>(own).unwrap()
        );
        assert!(sent("{}", "chat/completions", "extreme")
            .get("reasoning")
            .is_none());
        let not_json = Bytes::from_static(b"not json");
        assert_eq!(with_reasoning(&not_json, "messages", "high"), not_json);
    }
}
