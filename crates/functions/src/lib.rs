//! `jc-functions`: the runtime of application functions (SDK-22, SDK-23, Architecture/20 §3).
//!
//! One route, `POST /invoke`, for the Portal only: its Keycloak token must name the audience
//! `jc-functions` and be issued to the Portal's client. The runtime holds no credential and no
//! code of its own; every call brings the files, the request and the caller's token, and runs in
//! a fresh QuickJS runtime on a thread of its own, at most [`SLOTS`] at a time.

pub mod endpoint;
pub mod sandbox;

use std::collections::BTreeMap;
use std::sync::Arc;

use axum::extract::{DefaultBodyLimit, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use context_gateway::auth::token::{bearer, Verifier};
use jc_core::ProblemDetails;
use serde::Deserialize;
use serde_json::Value;
use tokio::sync::Semaphore;

use endpoint::Endpoint;
use sandbox::Invocation;

/// Invocations running at once per replica; the next one answers 429.
pub const SLOTS: usize = 16;
/// The whole invocation: the files, request and token.
pub const INVOCATION_LIMIT: usize = 8 * 1024 * 1024;
/// The function's request body, as JSON.
pub const REQUEST_BODY_LIMIT: usize = 256 * 1024;

pub struct AppState {
    pub verifier: Arc<Verifier>,
    /// The audience a caller's token must name (`JC_FUNCTIONS_AUDIENCE`).
    pub audience: String,
    /// The Keycloak client a caller's token must be issued to (`JC_FUNCTIONS_CALLER`).
    pub caller: String,
    /// Scheme and authority of the Context Gateway (`JC_GATEWAY_URL`).
    pub gateway: String,
    pub http: reqwest::Client,
    pub slots: Arc<Semaphore>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct InvokeRequest {
    files: BTreeMap<String, String>,
    entry: String,
    request: FnRequest,
    config: Value,
    #[serde(default)]
    token: Option<String>,
}

#[derive(Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
struct FnRequest {
    method: String,
    #[serde(default)]
    query: BTreeMap<String, String>,
    #[serde(default)]
    body: Value,
    #[serde(default)]
    user: Value,
}

fn problem(status: StatusCode, detail: impl Into<String>) -> Response {
    let slug = status
        .canonical_reason()
        .unwrap_or("error")
        .to_lowercase()
        .replace(' ', "-");
    ProblemDetails::new(
        status.as_u16(),
        &slug,
        status.canonical_reason().unwrap_or_default(),
    )
    .with_detail(detail)
    .into_response()
}

async fn invoke(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    let authorized = bearer(
        headers
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok()),
    )
    .and_then(|token| {
        state
            .verifier
            .verify(token, std::slice::from_ref(&state.audience))
    })
    .is_ok_and(|claims| claims.azp.as_deref() == Some(state.caller.as_str()));
    if !authorized {
        return problem(
            StatusCode::UNAUTHORIZED,
            "a token for jc-functions issued to the Portal",
        );
    }
    let Ok(permit) = state.slots.clone().try_acquire_owned() else {
        return problem(
            StatusCode::TOO_MANY_REQUESTS,
            format!("{SLOTS} invocations are running"),
        );
    };
    let call: InvokeRequest = match serde_json::from_slice(&body) {
        Ok(call) => call,
        Err(error) => return problem(StatusCode::BAD_REQUEST, error.to_string()),
    };
    if serde_json::to_vec(&call.request.body).map_or(true, |b| b.len() > REQUEST_BODY_LIMIT) {
        return problem(
            StatusCode::PAYLOAD_TOO_LARGE,
            "the request body is larger than 256 KiB",
        );
    }
    if !matches!(call.request.method.as_str(), "GET" | "POST") {
        return problem(
            StatusCode::BAD_REQUEST,
            "a function is called with GET or POST",
        );
    }
    let Some(slug) = call
        .config
        .get("slug")
        .and_then(Value::as_str)
        .filter(|s| endpoint::is_slug(s))
    else {
        return problem(
            StatusCode::BAD_REQUEST,
            "config.slug must be the endpoint's slug",
        );
    };
    let invocation = Invocation {
        endpoint: Endpoint {
            http: state.http.clone(),
            gateway: state.gateway.clone(),
            slug: slug.to_owned(),
            token: call.token.filter(|t| !t.is_empty()),
        },
        files: call.files,
        entry: call.entry,
        request: serde_json::to_value(&call.request).unwrap_or_default(),
        config: call.config,
    };
    // A QuickJS runtime is not `Send`: each call gets a thread and a single-threaded executor.
    let outcome = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map(|executor| executor.block_on(sandbox::run(invocation)))
    })
    .await;
    match outcome {
        Ok(Ok(outcome)) => Json(outcome).into_response(),
        _ => problem(
            StatusCode::INTERNAL_SERVER_ERROR,
            "the invocation could not be run",
        ),
    }
}

pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/invoke", post(invoke))
        .route("/healthz", get(|| async { "ok" }))
        .layer(DefaultBodyLimit::max(INVOCATION_LIMIT))
        .with_state(state)
}
