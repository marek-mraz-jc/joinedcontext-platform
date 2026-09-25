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

/// Whether the call asks for a stream (`"stream": true`, ADR-N-032).
fn wants_stream(body: &Bytes) -> bool {
    serde_json::from_slice::<Value>(body)
        .ok()
        .and_then(|value| value.get("stream").and_then(Value::as_bool))
        .unwrap_or(false)
}

/// A streamed chat completion that asks for the usage chunk, so the proxy can count the call:
/// OpenAI-compatible providers send it only when `stream_options.include_usage` is set. A body
/// that already says, or Anthropic's messages (whose stream always carries usage), is left alone.
fn with_stream_usage(body: &Bytes, rest: &str) -> Bytes {
    if rest.ends_with("messages") {
        return body.clone();
    }
    let Ok(Value::Object(mut map)) = serde_json::from_slice::<Value>(body) else {
        return body.clone();
    };
    let options = map.entry("stream_options").or_insert_with(|| json!({}));
    let Value::Object(options) = options else {
        return body.clone();
    };
    options.entry("include_usage").or_insert(json!(true));
    serde_json::to_vec(&map)
        .map(Bytes::from)
        .unwrap_or_else(|_| body.clone())
}

/// What a stream that reports no usage is counted as: the output it was allowed and a quarter of
/// its body's bytes for the input. Never nothing: the budget is a cost control (AG-41).
fn uncounted_stream(body: &Bytes) -> u64 {
    let allowed = serde_json::from_slice::<Value>(body)
        .ok()
        .and_then(|value| value.get("max_tokens").and_then(Value::as_u64))
        .unwrap_or(STREAM_OUTPUT_UNSAID);
    allowed + body.len() as u64 / 4
}

/// The output a stream is counted for when its request names no `max_tokens`.
const STREAM_OUTPUT_UNSAID: u64 = 8192;

/// The longest line of a stream the usage reader keeps; a longer one is dropped unread.
const STREAM_LINE_MAX: usize = 1 << 20;

/// The usage a provider reports inside its stream, read line by line as the chunks pass.
#[derive(Default)]
struct StreamUsage {
    line: Vec<u8>,
    total: Option<u64>,
    input: u64,
    output: u64,
}

impl StreamUsage {
    fn feed(&mut self, chunk: &[u8]) {
        for &byte in chunk {
            if byte == b'\n' {
                let line = std::mem::take(&mut self.line);
                self.read_line(&line);
            } else if self.line.len() < STREAM_LINE_MAX {
                self.line.push(byte);
            }
        }
    }

    fn read_line(&mut self, line: &[u8]) {
        let Some(data) = std::str::from_utf8(line)
            .ok()
            .and_then(|line| line.trim().strip_prefix("data:"))
        else {
            return;
        };
        let Ok(value) = serde_json::from_str::<Value>(data.trim()) else {
            return;
        };
        // OpenAI-compatible: `usage` on the last chunk. Anthropic: `message.usage` on
        // `message_start` (the input), `usage` on `message_delta` (the output so far).
        let Some(usage) = value
            .get("usage")
            .or_else(|| value.pointer("/message/usage"))
        else {
            return;
        };
        let count = |keys: &[&str]| keys.iter().find_map(|key| usage.get(*key)?.as_u64());
        if let Some(total) = count(&["total_tokens"]) {
            self.total = Some(total);
        }
        self.input = self
            .input
            .max(count(&["prompt_tokens", "input_tokens"]).unwrap_or(0));
        self.output = self
            .output
            .max(count(&["completion_tokens", "output_tokens"]).unwrap_or(0));
    }

    fn tokens(&self) -> Option<u64> {
        self.total
            .or_else(|| (self.input + self.output > 0).then_some(self.input + self.output))
    }
}

/// The tokens of one call, counted against the run and reported to the Portal (AG-41).
fn record_usage(
    state: &ProxyState,
    run_id: &str,
    tokens: u64,
) -> impl std::future::Future<Output = ()> {
    let state = state.clone();
    let run_id = run_id.to_owned();
    async move {
        if tokens == 0 {
            return;
        }
        state.limits.record_tokens(&run_id, tokens).await;
        let portal_base = state.config.portal_base.clone();
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
    let streamed = wants_stream(&body_bytes);
    let body_bytes = if streamed {
        with_stream_usage(&body_bytes, &rest)
    } else {
        body_bytes
    };
    let uncounted = uncounted_stream(&body_bytes);

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
    if let Some(refusal) =
        super::refused_redirect(status, super::location_of(&upstream_resp), "model provider")
    {
        return refusal;
    }
    if streamed && status.is_success() {
        let call = Streamed {
            run,
            method,
            rest,
            uncounted,
            start,
        };
        return stream_through(state, call, status, upstream_resp);
    }
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
        record_usage(&state, &run.id, tokens).await;
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

/// One streamed call: whose it is, what it asked and what it counts as without a usage.
struct Streamed {
    run: std::sync::Arc<crate::runs::RunContext>,
    method: Method,
    rest: String,
    uncounted: u64,
    start: Instant,
}

/// A stream passed through chunk by chunk (ADR-N-032). Its usage is read as it passes and
/// counted before the stream closes, so a caller that read to the end finds the call charged; a
/// stream without usage, or one the caller leaves, is counted as `uncounted` and the provider's
/// request is dropped with it.
fn stream_through(
    state: ProxyState,
    call: Streamed,
    status: StatusCode,
    upstream: reqwest::Response,
) -> Response {
    let Streamed {
        run,
        method,
        rest,
        uncounted,
        start,
    } = call;
    use futures::StreamExt;
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Bytes, std::io::Error>>(32);
    tokio::spawn(async move {
        let mut chunks = upstream.bytes_stream();
        let mut usage = StreamUsage::default();
        let mut bytes = 0usize;
        let mut whole = true;
        while let Some(chunk) = chunks.next().await {
            match chunk {
                Ok(chunk) => {
                    bytes += chunk.len();
                    usage.feed(&chunk);
                    if tx.send(Ok(chunk)).await.is_err() {
                        whole = false;
                        break;
                    }
                }
                Err(err) => {
                    whole = false;
                    let _ = tx.send(Err(std::io::Error::other(err))).await;
                    break;
                }
            }
        }
        let tokens = match usage.tokens() {
            Some(tokens) if whole => tokens,
            seen => seen.unwrap_or(0).max(uncounted),
        };
        record_usage(&state, &run.id, tokens).await;
        log_request(&AuditEntry {
            run_id: &run.id,
            user: &run.created_by,
            upstream: "model-provider",
            method: method.as_str(),
            path: &rest,
            status: status.as_u16(),
            bytes,
            duration_ms: start.elapsed().as_millis(),
        });
        drop(tx);
    });
    let body = futures::stream::unfold(rx, |mut rx| async move {
        rx.recv().await.map(|chunk| (chunk, rx))
    });
    Response::builder()
        .status(status)
        .header("Content-Type", "text/event-stream")
        .header("Cache-Control", "no-cache")
        .body(Body::from_stream(body))
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

    #[test]
    fn a_stream_is_counted_from_its_own_usage_frames_split_anywhere() {
        let anthropic = "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":90,\"output_tokens\":1}}}\n\nevent: message_delta\ndata: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":30}}\n\n";
        let mut usage = StreamUsage::default();
        for piece in anthropic.as_bytes().chunks(7) {
            usage.feed(piece);
        }
        assert_eq!(usage.tokens(), Some(120));

        let mut openai = StreamUsage::default();
        openai.feed(b"data: {\"usage\":{\"total_tokens\":42}}\n\ndata: [DONE]\n\n");
        assert_eq!(openai.tokens(), Some(42));
        let mut none = StreamUsage::default();
        none.feed(b"data: {\"choices\":[]}\n\n: keep-alive\n");
        assert_eq!(none.tokens(), None);
    }

    #[test]
    fn a_streamed_chat_asks_for_its_usage_and_messages_are_left_alone() {
        let asked = |body: &str, rest: &str| -> Value {
            serde_json::from_slice(&with_stream_usage(&Bytes::from(body.to_owned()), rest)).unwrap()
        };
        let chat = asked(r#"{"stream":true}"#, "v1/chat/completions");
        assert_eq!(chat["stream_options"], json!({ "include_usage": true }));
        let own = asked(
            r#"{"stream":true,"stream_options":{"include_usage":false}}"#,
            "chat/completions",
        );
        assert_eq!(own["stream_options"], json!({ "include_usage": false }));
        assert!(asked(r#"{"stream":true}"#, "v1/messages")
            .get("stream_options")
            .is_none());
        assert!(wants_stream(&Bytes::from_static(br#"{"stream":true}"#)));
        assert!(!wants_stream(&Bytes::from_static(b"not json")));
        assert_eq!(
            uncounted_stream(&Bytes::from_static(br#"{"max_tokens":60}"#)),
            60 + 4
        );
    }
}
