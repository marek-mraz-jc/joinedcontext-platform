//! `jc-agent-proxy`: Credential-free proxy mediating all workspace communication for autonomous agents.

pub mod audit;
pub mod auth;
pub mod config;
pub mod inject;
pub mod limits;
pub mod routes;
pub mod runs;

use axum::routing::{get, post};
use axum::Router;
use std::sync::Arc;

#[derive(Clone)]
pub struct ProxyState {
    pub config: Arc<config::Config>,
    pub runs: runs::RunResolver,
    pub credentials: inject::CredentialManager,
    pub limits: limits::LimitManager,
    pub http: reqwest::Client,
    /// The client the fetch route uses. It follows no redirect of its own: every hop is checked
    /// against the run's allow-list first, which `reqwest`'s policy cannot do (AG-65).
    pub egress: reqwest::Client,
}

pub fn router(state: Arc<ProxyState>) -> Router {
    Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .route(
            "/v1/data/endpoints/{slug}/{*rest}",
            axum::routing::any(routes::data::endpoint_handler),
        )
        .route(
            "/v1/data/{*rest}",
            axum::routing::any(routes::data::handler),
        )
        .route(
            "/v1/forge/{*rest}",
            axum::routing::any(routes::forge::handler),
        )
        .route("/v1/fetch", get(routes::fetch::handler))
        .route("/v1/llm/{*rest}", post(routes::llm::handler))
        .route(
            "/v1/packages/{host}/{*rest}",
            get(routes::packages::handler),
        )
        .route(
            "/v1/diagnostics/{component}/{id}",
            get(routes::diagnostics::handler),
        )
        .route("/v1/mcp", post(routes::mcp::handler))
        .route("/v1/runs/events", post(routes::events::handler))
        .route("/v1/runs/inbox", get(routes::inbox::handler))
        .fallback(|| async {
            jc_core::ProblemDetails::forbidden().with_detail("endpoint not recognized by proxy")
        })
        .with_state((*state).clone())
}
