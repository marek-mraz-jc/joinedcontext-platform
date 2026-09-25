//! Edge cases of `routes::llm::handler` (T-1869).
//!
//! The model door holds the one key the workspace must never see, counts the steps and the
//! tokens a run is allowed, and forwards nothing of the inbound request but the body.

mod common;

use axum::body::Body;
use axum::http::StatusCode;
use common::{app, authed, body_of, sample_run, Bases, RUN_ID, TICKET};
use tower::ServiceExt;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// A proxy whose model provider is `model`, for a run bounded by `steps` and `tokens`.
fn proxy(model: &str, steps: u32, tokens: u64) -> axum::Router {
    let mut run = sample_run(false);
    run.steps_per_run = steps;
    run.max_tokens = tokens;
    app(
        run,
        Bases {
            model: model.to_owned(),
            ..Bases::default()
        },
    )
}

/// The same proxy, asking a Portal that holds no run and answers `404` to every lookup.
///
/// `Bases::default()` points the Portal at a name that resolves nowhere, which since T-2418 is a
/// failure of the platform's (`503`) rather than a bad credential (`401`). A case about a run id
/// nobody holds has to ask a Portal that is there.
fn proxy_asking(model: &str, portal: &wiremock::MockServer) -> axum::Router {
    app(
        sample_run(false),
        Bases {
            model: model.to_owned(),
            portal: portal.uri(),
            ..Bases::default()
        },
    )
}

fn body(text: &str) -> Body {
    Body::from(text.to_owned())
}

/// AG-22: no run credentials, no model call — and the provider is never asked.
#[tokio::test]
async fn a_model_call_without_credentials_is_refused_before_the_provider_is_asked() {
    let provider = MockServer::start().await;
    let response = proxy(&provider.uri(), 0, 1000)
        .oneshot(
            axum::http::Request::builder()
                .method("POST")
                .uri("/v1/llm/v1/messages")
                .body(body("{}"))
                .expect("a request"),
        )
        .await
        .expect("an answer");

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert!(provider
        .received_requests()
        .await
        .unwrap_or_default()
        .is_empty());
}

/// AG-22: a ticket of another run buys no model time.
#[tokio::test]
async fn a_ticket_of_another_run_buys_no_model_time() {
    let portal = wiremock::MockServer::start().await;
    for (run, ticket) in [(RUN_ID, "another-runs-ticket"), ("nobody", TICKET)] {
        let response = proxy_asking("http://model.invalid", &portal)
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/v1/llm/v1/messages")
                    .header("x-jc-run", run)
                    .header("x-jc-ticket", ticket)
                    .body(body("{}"))
                    .expect("a request"),
            )
            .await
            .expect("an answer");
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED, "run '{run}'");
    }
}

/// AG-51: the door is the completion endpoints and nothing else. A path that only looks like one
/// is refused, and so is one that would leave the provider's base.
#[tokio::test]
async fn only_the_completion_endpoints_are_open() {
    let provider = MockServer::start().await;
    for rest in [
        "v1/models",
        "v1/messages/batches",
        "messages/count_tokens",
        "v1/chat/completions/x",
        "../admin",
        "v1/organizations/me",
        "",
    ] {
        let response = proxy(&provider.uri(), 0, 1000)
            .oneshot(
                authed("POST", &format!("/v1/llm/{rest}"))
                    .body(body("{}"))
                    .expect("a request"),
            )
            .await
            .expect("an answer");
        assert!(
            !response.status().is_success(),
            "'{rest}' answered {}",
            response.status()
        );
    }
    assert!(
        provider
            .received_requests()
            .await
            .unwrap_or_default()
            .is_empty(),
        "nothing reached the provider"
    );
}

/// AG-51: the door is a POST. A GET on it is refused by the router, before a step is counted.
#[tokio::test]
async fn the_model_door_takes_no_get() {
    let response = proxy("http://model.invalid", 0, 1000)
        .oneshot(
            authed("GET", "/v1/llm/v1/messages")
                .body(Body::empty())
                .expect("a request"),
        )
        .await
        .expect("an answer");
    assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
}

/// AG-25, AG-51: one model call is one step, and the call past the profile's `stepsPerRun` is
/// refused however the run's driver loops.
#[tokio::test]
async fn the_call_past_the_step_limit_is_refused() {
    let provider = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_string("{}"))
        .mount(&provider)
        .await;
    let app = proxy(&provider.uri(), 2, 100_000);

    for expected in [
        StatusCode::OK,
        StatusCode::OK,
        StatusCode::TOO_MANY_REQUESTS,
    ] {
        let response = app
            .clone()
            .oneshot(
                authed("POST", "/v1/llm/v1/messages")
                    .body(body(r#"{"model":"m"}"#))
                    .expect("a request"),
            )
            .await
            .expect("an answer");
        assert_eq!(response.status(), expected);
    }
    assert_eq!(
        provider.received_requests().await.unwrap_or_default().len(),
        2,
        "the refused call never reached the provider"
    );
}

/// AG-41: the run's token budget is counted from what the provider reports, in either of the two
/// shapes, and the call after the budget is spent is refused.
#[tokio::test]
async fn the_budget_is_counted_from_the_providers_usage_and_then_refuses() {
    for usage in [
        serde_json::json!({ "total_tokens": 120 }),
        serde_json::json!({ "input_tokens": 100, "output_tokens": 20 }),
    ] {
        let provider = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({ "usage": usage })),
            )
            .mount(&provider)
            .await;
        let app = proxy(&provider.uri(), 0, 100);

        let first = app
            .clone()
            .oneshot(
                authed("POST", "/v1/llm/v1/messages")
                    .body(body(r#"{"model":"m"}"#))
                    .expect("a request"),
            )
            .await
            .expect("an answer");
        assert_eq!(first.status(), StatusCode::OK, "{usage}");

        let second = app
            .oneshot(
                authed("POST", "/v1/llm/v1/messages")
                    .body(body(r#"{"model":"m"}"#))
                    .expect("a request"),
            )
            .await
            .expect("an answer");
        assert_eq!(second.status(), StatusCode::TOO_MANY_REQUESTS, "{usage}");
    }
}

/// AG-41: an answer that reports no usage, or is not JSON at all, costs the run nothing and
/// crashes nothing — the door stays open for the next call.
#[tokio::test]
async fn an_answer_without_usage_costs_nothing_and_breaks_nothing() {
    for answer in ["{}", "not json at all", "[]", r#"{"usage":"lots"}"#, ""] {
        let provider = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_string(answer))
            .mount(&provider)
            .await;
        let app = proxy(&provider.uri(), 0, 100);

        for _ in 0..2 {
            let response = app
                .clone()
                .oneshot(
                    authed("POST", "/v1/llm/v1/messages")
                        .body(body(r#"{"model":"m"}"#))
                        .expect("a request"),
                )
                .await
                .expect("an answer");
            assert_eq!(response.status(), StatusCode::OK, "answer '{answer}'");
        }
    }
}

/// AG-35: the provider sees the platform's key and nothing the workspace sent — not its ticket,
/// not an `Authorization` header of its own, not a cookie.
#[tokio::test]
async fn the_provider_sees_the_platforms_key_and_nothing_of_the_workspace() {
    let provider = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_string("{}"))
        .mount(&provider)
        .await;

    let response = proxy(&provider.uri(), 0, 1000)
        .oneshot(
            authed("POST", "/v1/llm/v1/messages")
                .header("authorization", "Bearer the-workspaces-own-key")
                .header("cookie", "session=stolen")
                .header("x-api-key", "the-workspaces-own-key")
                .body(body(r#"{"model":"m"}"#))
                .expect("a request"),
        )
        .await
        .expect("an answer");
    assert_eq!(response.status(), StatusCode::OK);

    let seen = provider
        .received_requests()
        .await
        .expect("the mock recorded");
    let headers = &seen
        .first()
        .expect("one request reached the provider")
        .headers;
    // The default provider is Anthropic, which is told the key in `x-api-key`; the workspace's
    // own value in that slot is replaced rather than appended to.
    assert_eq!(
        headers
            .get("x-api-key")
            .and_then(|value| value.to_str().ok()),
        Some("mock-model-key"),
        "the provider is told the platform's key"
    );
    for forbidden in ["cookie", "x-jc-run", "x-jc-ticket", "authorization"] {
        assert!(
            headers.get(forbidden).is_none(),
            "'{forbidden}' was relayed to the provider"
        );
    }
}

/// AG-35: the key is not in what the workspace gets back, whatever the provider answers and
/// whether the provider answers at all.
#[tokio::test]
async fn the_model_key_is_in_no_answer_the_workspace_reads() {
    let provider = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(401).set_body_string(r#"{"error":"bad key"}"#))
        .mount(&provider)
        .await;

    for model in [provider.uri(), "http://model.invalid".to_owned()] {
        let response = proxy(&model, 0, 1000)
            .oneshot(
                authed("POST", "/v1/llm/v1/messages")
                    .body(body(r#"{"model":"m"}"#))
                    .expect("a request"),
            )
            .await
            .expect("an answer");
        let detail = body_of(response).await;
        for secret in ["mock-model-key", "mock-forge-token", TICKET] {
            assert!(!detail.contains(secret), "'{secret}' leaked: {detail}");
        }
    }
}

/// AG-41, T-0811: a prompt past the proxy's ceiling is refused, and the step is spent before the
/// body is read — the ceiling is the proxy's memory, not the provider's.
#[tokio::test]
async fn a_body_past_the_ceiling_is_refused() {
    let provider = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_string("{}"))
        .mount(&provider)
        .await;

    let response = proxy(&provider.uri(), 0, 1000)
        .oneshot(
            authed("POST", "/v1/llm/v1/messages")
                .body(Body::from(vec![b'x'; 4 * 1024 * 1024 + 1]))
                .expect("a request"),
        )
        .await
        .expect("an answer");

    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    assert!(provider
        .received_requests()
        .await
        .unwrap_or_default()
        .is_empty());
}

/// AG-72: the run's reasoning effort reaches the provider in the shape that endpoint reads, and
/// a body that already carries one of its own is sent as it is.
#[tokio::test]
async fn the_runs_reasoning_effort_travels_and_a_body_that_has_one_is_left_alone() {
    let provider = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_string("{}"))
        .mount(&provider)
        .await;

    let mut run = sample_run(false);
    run.reasoning_effort = Some("low".to_owned());
    let app = app(
        run,
        Bases {
            model: provider.uri(),
            ..Bases::default()
        },
    );

    for (rest, sent, expected) in [
        (
            "v1/messages",
            r#"{"model":"m","max_tokens":100}"#,
            serde_json::json!({"type":"enabled","budget_tokens":2048}),
        ),
        (
            "v1/chat/completions",
            r#"{"model":"m"}"#,
            serde_json::json!({"effort":"low"}),
        ),
    ] {
        let response = app
            .clone()
            .oneshot(
                authed("POST", &format!("/v1/llm/{rest}"))
                    .body(body(sent))
                    .expect("a request"),
            )
            .await
            .expect("an answer");
        assert_eq!(response.status(), StatusCode::OK, "{rest}");

        let seen = provider
            .received_requests()
            .await
            .expect("the mock recorded");
        let last: serde_json::Value =
            serde_json::from_slice(&seen.last().expect("a request").body).expect("JSON reached");
        let key = match rest {
            "v1/messages" => "thinking",
            _ => "reasoning",
        };
        assert_eq!(last[key], expected, "{rest}: {last}");
    }
}

/// AG-72: a body that is not JSON is forwarded as the bytes it is, so a provider that reads
/// another format is not broken by a setting this proxy could not place.
#[tokio::test]
async fn a_body_that_is_not_json_is_forwarded_unchanged() {
    let provider = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_string("{}"))
        .mount(&provider)
        .await;

    let mut run = sample_run(false);
    run.reasoning_effort = Some("high".to_owned());
    let app = app(
        run,
        Bases {
            model: provider.uri(),
            ..Bases::default()
        },
    );

    let response = app
        .oneshot(
            authed("POST", "/v1/llm/v1/messages")
                .body(body("not json at all"))
                .expect("a request"),
        )
        .await
        .expect("an answer");
    assert_eq!(response.status(), StatusCode::OK);

    let seen = provider
        .received_requests()
        .await
        .expect("the mock recorded");
    assert_eq!(
        seen.first().expect("a request").body,
        b"not json at all",
        "the bytes travelled as they arrived"
    );
}

/// T-2821, ADR-N-032: a streamed call passes through as the provider's events, and the usage
/// its last chunk reports is counted before the stream ends, so the next call past the budget
/// is refused as for a whole answer.
#[tokio::test]
async fn a_stream_passes_through_and_its_usage_is_counted() {
    let provider = MockServer::start().await;
    let stream = "data: {\"choices\":[{\"delta\":{\"content\":\"Two stations\"}}]}\n\n\
                  data: {\"choices\":[{\"delta\":{\"content\":\" are empty.\"}}]}\n\n\
                  data: {\"choices\":[],\"usage\":{\"prompt_tokens\":100,\"completion_tokens\":20,\"total_tokens\":120}}\n\n\
                  data: [DONE]\n\n";
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .and(wiremock::matchers::body_partial_json(serde_json::json!({
            "stream": true,
            "stream_options": { "include_usage": true }
        })))
        .respond_with(ResponseTemplate::new(200).set_body_raw(stream, "text/event-stream"))
        .mount(&provider)
        .await;
    let app = proxy(&provider.uri(), 0, 100);
    let ask = || {
        authed("POST", "/v1/llm/v1/chat/completions")
            .body(body(r#"{"model":"m","stream":true,"max_tokens":50}"#))
            .expect("a request")
    };
    let first = app.clone().oneshot(ask()).await.expect("an answer");
    assert_eq!(first.status(), StatusCode::OK);
    assert_eq!(
        first.headers()["content-type"],
        "text/event-stream",
        "the provider's events, not a JSON body"
    );
    let text = body_of(first).await;
    assert!(
        text.contains("Two stations") && text.contains("[DONE]"),
        "{text}"
    );
    let second = app.oneshot(ask()).await.expect("an answer");
    assert_eq!(second.status(), StatusCode::TOO_MANY_REQUESTS);
}

/// AG-41: a stream that reports no usage is never free: it counts as the output it was allowed
/// plus a quarter of its body, so a run cannot stream past its budget.
#[tokio::test]
async fn a_stream_without_usage_counts_as_what_it_was_allowed() {
    let provider = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\ndata: [DONE]\n\n",
            "text/event-stream",
        ))
        .mount(&provider)
        .await;
    let app = proxy(&provider.uri(), 0, 100);
    let ask = |max: u64| {
        authed("POST", "/v1/llm/v1/chat/completions")
            .body(body(&format!(
                r#"{{"model":"m","stream":true,"max_tokens":{max}}}"#
            )))
            .expect("a request")
    };
    // 60 allowed plus the body's quarter stays under 100; the next such call goes over.
    let first = app.clone().oneshot(ask(60)).await.expect("an answer");
    assert_eq!(first.status(), StatusCode::OK);
    body_of(first).await;
    let second = app.clone().oneshot(ask(60)).await.expect("an answer");
    assert_eq!(second.status(), StatusCode::OK);
    body_of(second).await;
    let third = app.oneshot(ask(60)).await.expect("an answer");
    assert_eq!(third.status(), StatusCode::TOO_MANY_REQUESTS);
}

/// T-2771, AG-72: every model call reaches the Portal as a `usage` frame with its halves, the
/// input the provider read from its cache, how long the provider took, and the model named in the
/// call, so a slow answer is traced to its call.
#[tokio::test]
async fn a_model_call_is_reported_with_its_latency_tokens_and_model() {
    let provider = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({
                    "choices": [{ "message": { "content": "hi" } }],
                    "usage": { "prompt_tokens": 3980, "completion_tokens": 140, "total_tokens": 4120,
                               "prompt_tokens_details": { "cached_tokens": 3200 } }
                }))
                .set_delay(std::time::Duration::from_millis(120)),
        )
        .mount(&provider)
        .await;
    let portal = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/internal/agent-runs/events"))
        .respond_with(ResponseTemplate::new(202).set_body_json(serde_json::json!({ "seq": 1 })))
        .mount(&portal)
        .await;

    let response = proxy_asking(&provider.uri(), &portal)
        .oneshot(
            authed("POST", "/v1/llm/v1/chat/completions")
                .body(body(r#"{"model":"google/gemini-3.8-flash","messages":[]}"#))
                .expect("a request"),
        )
        .await
        .expect("an answer");
    assert_eq!(response.status(), StatusCode::OK);

    // The report is sent beside the answer, never before it: wait for it.
    let mut reported = None;
    for _ in 0..100 {
        let seen = portal.received_requests().await.unwrap_or_default();
        if let Some(request) = seen.first() {
            reported = serde_json::from_slice::<serde_json::Value>(&request.body).ok();
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    let reported = reported.expect("the usage reached the Portal");
    assert_eq!(reported["kind"], "usage");
    let usage = &reported["payload"];
    assert_eq!(usage["tokensThisStep"], 4120);
    assert_eq!(usage["inputTokens"], 3980);
    assert_eq!(usage["outputTokens"], 140);
    assert_eq!(usage["cachedTokens"], 3200);
    assert_eq!(usage["model"], "google/gemini-3.8-flash");
    let latency = usage["latencyMs"].as_u64().expect("a latency");
    assert!((120..10_000).contains(&latency), "{latency}");
}
