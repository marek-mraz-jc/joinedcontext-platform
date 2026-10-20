//! The gateway's MCP: the hub and the named servers, and the two describe tools beside the
//! broker's MCP surface of each view (plan 5.3, T-3648).

use crate::app::Gateway;
use crate::auth::subject::HUB_PATH;
use axum::routing::{get, post};
use axum::Router;
use std::sync::{Arc, LazyLock};
pub mod annotations;
pub mod catalog;
pub mod describe;
pub mod documents;
pub mod endpoint;
pub mod hub;
pub mod jsonrpc;
pub mod limits;
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
        // RFC 9728 §3.1 places the metadata before the resource's path; a client that builds
        // it so (Gemini, ADR-N-052) finds the same documents (T-4631).
        .route(
            "/.well-known/oauth-protected-resource/api/endpoint/{slug}/mcp",
            get(endpoint::protected_resource),
        )
        .route(
            "/.well-known/oauth-protected-resource/api/mcp",
            get(hub::protected_resource),
        )
        .route(
            "/.well-known/oauth-protected-resource/api/mcp/{project}/{name}",
            get(server::protected_resource),
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

    /// AG-124: list_endpoints names every EU theme code it filters by, with its label.
    #[test]
    fn list_endpoints_names_every_theme_code() {
        let tool = hub::list_endpoints_tool();
        let described = tool["inputSchema"]["properties"]["theme"]["description"]
            .as_str()
            .unwrap_or_default();
        for theme in jc_core::kinds::DataTheme::ALL {
            let named = format!("{} {}", theme.code(), theme.label_in("en"));
            assert!(described.contains(&named), "{named} in {described}");
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
        names(&super::documents::tool("")["inputSchema"], &mut arguments);
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

    /// T-4732: every tool the hub or an Endpoint lists carries a title of at most 64 characters,
    /// the same at the top and in `annotations`, no two alike, and its three hints as booleans; the
    /// names stay as they were.
    #[test]
    fn every_listed_tool_has_a_unique_short_title_and_its_hints() {
        let mut listed: Vec<Value> = tools::TOOLS
            .iter()
            .map(|tool| {
                serde_json::json!({ "name": tool.name, "annotations": {
                    "readOnlyHint": tool.read_only(), "destructiveHint": tool.destructive() } })
            })
            .collect();
        listed.extend([
            super::search::tool(),
            hub::list_endpoints_tool(),
            hub::get_endpoint_tool(),
            super::documents::tool(""),
        ]);
        let names: Vec<Value> = listed.iter().map(|tool| tool["name"].clone()).collect();
        super::annotations::annotate(&mut listed);
        let mut titles = std::collections::HashSet::new();
        for (tool, name) in listed.iter().zip(&names) {
            assert_eq!(&tool["name"], name, "the name never changes");
            let title = tool["title"].as_str().unwrap_or_default();
            assert!(
                !title.trim().is_empty() && title.chars().count() <= 64,
                "{tool}"
            );
            assert_eq!(tool["annotations"]["title"], tool["title"], "{tool}");
            assert!(titles.insert(title.to_owned()), "two tools titled {title}");
            for hint in ["readOnlyHint", "destructiveHint", "openWorldHint"] {
                assert!(tool["annotations"][hint].is_boolean(), "{hint}: {tool}");
            }
        }
        assert_eq!(listed[0]["title"], "Append attributes");

        // A broker's tool as its view lists it: none, its own or a blank title.
        let mut broker = vec![
            serde_json::json!({ "name": "query_temporal" }),
            serde_json::json!({ "name": "get_entity", "title": "Read one entity" }),
            serde_json::json!({ "name": "list_types", "title": " ", "annotations": null }),
        ];
        super::annotations::annotate(&mut broker);
        assert_eq!(broker[0]["title"], "Query temporal");
        // No readOnlyHint is never guessed, so neither is destructiveHint; the world is closed.
        assert_eq!(
            broker[0]["annotations"],
            serde_json::json!({ "title": "Query temporal", "openWorldHint": false })
        );
        assert_eq!(broker[1]["title"], "Read one entity");
        assert_eq!(broker[1]["annotations"]["title"], "Read one entity");
        assert_eq!(broker[2]["title"], "List types");
        assert_eq!(broker[2]["annotations"]["title"], "List types");

        // The hints a tool states stay; a read is not destructive, a write is unless it says not.
        let mut hinted = vec![
            serde_json::json!({ "name": "get_type", "annotations": {
                "readOnlyHint": true, "openWorldHint": false } }),
            serde_json::json!({ "name": "delete_entity", "annotations": { "readOnlyHint": false } }),
            serde_json::json!({ "name": "create_entity", "annotations": {
                "readOnlyHint": false, "destructiveHint": false, "openWorldHint": true } }),
        ];
        super::annotations::annotate(&mut hinted);
        let hints = |tool: &Value| {
            let a = &tool["annotations"];
            (
                a["readOnlyHint"].clone(),
                a["destructiveHint"].clone(),
                a["openWorldHint"].clone(),
            )
        };
        let (t, f) = (Value::Bool(true), Value::Bool(false));
        assert_eq!(hints(&hinted[0]), (t.clone(), f.clone(), f.clone()));
        assert_eq!(hints(&hinted[1]), (f.clone(), t.clone(), f.clone()));
        assert_eq!(hints(&hinted[2]), (f.clone(), f, t));
    }

    /// T-4501, T-4502: the hub's own lines stay short, and the whole primer under 5,000 bytes.
    #[test]
    fn the_hub_lead_and_the_primer_stay_short() {
        assert!(HUB_LEAD.len() < 600, "{}", HUB_LEAD.len());
        assert!(PRIMER.len() < 5_000, "{}", PRIMER.len());
        assert!(PRIMER.starts_with(HUB_LEAD));
    }
}
