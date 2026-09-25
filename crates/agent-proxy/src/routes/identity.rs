//! `POST /internal/runs/{run}/identity`: the Portal hands a run its person's token (ADR-N-038 §3.1).
//!
//! Only the Portal's own service-account token opens this route (AG-52); a workspace holds none,
//! and a person's token of the Portal's client is refused like any other. The caller is judged
//! before anything about the run is looked up, so the route tells a stranger nothing about which
//! runs exist.

use crate::auth::run_id_is_well_formed;
use crate::delegation::BindError;
use crate::runs::RunError;
use crate::ProxyState;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct HandOver {
    /// The person's current access token. Never logged, never kept past the exchange.
    subject_token: String,
}

fn unavailable(what: &str) -> Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        jc_core::ProblemDetails::new(503, "upstream-unavailable", "Upstream Unavailable")
            .with_detail(what.to_owned()),
    )
        .into_response()
}

pub async fn handler(
    State(state): State<ProxyState>,
    Path(run_id): Path<String>,
    headers: HeaderMap,
    body: Result<Json<HandOver>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let bearer = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(str::trim)
        .filter(|v| !v.is_empty());
    let refused = || {
        jc_core::ProblemDetails::unauthorized()
            .with_detail("only the Portal hands a run the identity of its person")
            .into_response()
    };
    let Some(bearer) = bearer else {
        return refused();
    };
    match state.credentials.grants().is_portal(bearer).await {
        Ok(true) => {}
        Ok(false) => {
            tracing::warn!("an identity hand-over not from the Portal's service account");
            return refused();
        }
        Err(error) => {
            tracing::error!(%error, "the realm could not judge an identity hand-over");
            return unavailable("the realm could not be reached");
        }
    }

    let Json(HandOver { subject_token }) = match body {
        Ok(body) => body,
        Err(rejection) => {
            return jc_core::ProblemDetails::new(400, "bad-request", "Bad Request")
                .with_detail(format!(
                    "the body is {{\"subjectToken\": \"…\"}}: {}",
                    rejection.body_text()
                ))
                .into_response()
        }
    };

    if !run_id_is_well_formed(&run_id) {
        return jc_core::ProblemDetails::not_found()
            .with_detail("no run holds this id")
            .into_response();
    }
    // Past the cache: a run the Portal has just created is not in it yet.
    let run = match state.runs.resolve_fresh(&run_id).await {
        Ok(run) => run,
        Err(RunError::NotFound(_)) => {
            return jc_core::ProblemDetails::not_found()
                .with_detail("no run holds this id")
                .into_response()
        }
        Err(RunError::NotActive(status)) => {
            return jc_core::ProblemDetails::new(409, "conflict", "Conflict")
                .with_detail(format!("the run is {status} and takes no identity"))
                .into_response()
        }
        Err(RunError::Transport(reason)) => {
            tracing::error!(run = %run_id, %reason, "the run could not be read from the Portal");
            return unavailable("the run could not be read from the Portal");
        }
    };

    match state.credentials.grants().bind(&run, &subject_token).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(error @ (BindError::NotTheRunsPerson | BindError::Refused(_))) => {
            tracing::warn!(run = %run.id, %error, "an identity hand-over was refused");
            jc_core::ProblemDetails::forbidden()
                .with_detail(error.to_string())
                .into_response()
        }
        Err(BindError::Transport(error)) => {
            tracing::error!(run = %run.id, %error, "the realm could not exchange a run's identity");
            unavailable("the realm could not be reached")
        }
    }
}
