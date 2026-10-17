//! The gateway's MCP: the hub and the named servers, and the two describe tools beside the
//! broker's MCP surface of each view (plan 5.3, T-3648).

use crate::app::Gateway;
use crate::auth::subject::HUB_PATH;
use axum::routing::{get, post};
use axum::Router;
use std::sync::Arc;
pub mod describe;
pub mod endpoint;
pub mod hub;
pub mod jsonrpc;
pub mod server;
pub mod tools;
pub mod units;

/// What the hub and every named server tell a client in `initialize`: that the connector speaks
/// NGSI-LD, its query language and the order of work (ADR-N-025 §4c, EP-116). It names no slug,
/// token or host.
pub(crate) const PRIMER: &str = include_str!("primer.md");

/// This surface's routes; `app::router` merges them under its guards.
pub(crate) fn routes() -> Router<Arc<Gateway>> {
    Router::new()
        .route("/api/endpoint/{slug}/mcp", post(endpoint::mcp_message))
        .route("/cs/{space}/mcp", post(endpoint::space_mcp_message))
        .route(
            "/api/endpoint/{slug}/.well-known/oauth-protected-resource",
            get(endpoint::protected_resource),
        )
        // One connector over every Endpoint the token reaches (EP-87, ADR-N-025).
        .route(HUB_PATH, post(hub::message))
        .route("/api/endpoints", get(hub::endpoints))
        .route(
            "/api/mcp/.well-known/oauth-protected-resource",
            get(hub::protected_resource),
        )
        // A named server over chosen Endpoints (EP-92, ADR-N-043).
        .route("/api/mcp/{project}/{name}", post(server::message))
        .route(
            "/api/mcp/{project}/{name}/.well-known/oauth-protected-resource",
            get(server::protected_resource),
        )
}
