//! The gateway's MCP: the hub and the named servers, and the two describe tools beside the
//! broker's MCP surface of each view (plan 5.3, T-3648).

use crate::app::Gateway;
use crate::auth::subject::HUB_PATH;
use axum::routing::{get, post};
use axum::Router;
use std::sync::{Arc, LazyLock};
pub mod catalog;
pub mod describe;
pub mod endpoint;
pub mod hub;
pub mod jsonrpc;
pub mod personal;
pub mod search;
pub mod server;
pub mod tools;
pub mod units;
pub mod writes;

/// What the hub and every named server tell a client in `initialize`: catalogue first, then the
/// dataset (`hub_lead.md`, T-4501), then that the connector speaks NGSI-LD, its query language
/// and every tool argument: the primer each Antares view sends too, from its one source
/// (`antares_ql::PRIMER`, ADR-N-025 §4c, EP-116, T-4502, T-4532). It names no slug, token or host.
pub(crate) static PRIMER: LazyLock<String> =
    LazyLock::new(|| [include_str!("hub_lead.md"), antares_ql::PRIMER].concat());

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

#[cfg(test)]
mod tests {
    use super::{hub, tools, PRIMER};

    /// The hub's own lines of [`PRIMER`], before the NGSI-LD primer every broker view shares.
    const HUB_LEAD: &str = include_str!("hub_lead.md");
    use serde_json::Value;

    /// Every argument name of a schema, its nested objects' included.
    fn names(schema: &Value, out: &mut Vec<String>) {
        if let Some(properties) = schema.get("properties").and_then(Value::as_object) {
            for (name, property) in properties {
                out.push(name.clone());
                names(property, out);
                if let Some(items) = property.get("items") {
                    names(items, out);
                }
            }
        }
    }

    /// T-4502: an argument a tool takes is explained in the primer, so a new one without its
    /// line fails here.
    #[test]
    fn every_tool_argument_is_explained_in_the_primer() {
        let mut arguments = vec!["endpoint".to_owned()];
        for tool in tools::TOOLS {
            names(&tool.input_schema(), &mut arguments);
        }
        names(&hub::list_endpoints_tool()["inputSchema"], &mut arguments);
        names(&hub::get_endpoint_tool()["inputSchema"], &mut arguments);
        names(&super::search::tool()["inputSchema"], &mut arguments);
        arguments.sort();
        arguments.dedup();
        let unexplained: Vec<&String> = arguments
            .iter()
            .filter(|name| {
                let word = regex::Regex::new(&format!(r"\b{}\b", regex::escape(name)))
                    .expect("a word pattern");
                !word.is_match(&PRIMER)
            })
            .collect();
        assert!(unexplained.is_empty(), "not in the primer: {unexplained:?}");
    }

    /// T-4501, T-4502: the hub's own lines stay short, and the whole primer under 5,000 bytes.
    #[test]
    fn the_hub_lead_and_the_primer_stay_short() {
        assert!(HUB_LEAD.len() < 600, "{}", HUB_LEAD.len());
        assert!(PRIMER.len() < 5_000, "{}", PRIMER.len());
        assert!(PRIMER.starts_with(HUB_LEAD));
    }
}
