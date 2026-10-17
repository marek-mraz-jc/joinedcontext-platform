//! The gateway's MCP: the hub and the named servers, and the two describe tools beside the
//! broker's MCP surface of each view (plan 5.3, T-3648).

pub mod catalog;
pub mod describe;
pub mod hub;
pub mod jsonrpc;
pub mod server;
pub mod tools;
pub mod units;

/// What the hub and every named server tell a client in `initialize`: catalogue first, then the
/// dataset (`hub_lead.md`, T-4501), then that the connector speaks NGSI-LD, its query language
/// and every tool argument (`primer.md`, ADR-N-025 §4c, EP-116, T-4502). It names no slug,
/// token or host.
pub(crate) const PRIMER: &str = concat!(include_str!("hub_lead.md"), include_str!("primer.md"));

/// The hub's own lines of [`PRIMER`], before the NGSI-LD primer every broker view shares.
#[cfg(test)]
pub(crate) const HUB_LEAD: &str = include_str!("hub_lead.md");

#[cfg(test)]
mod tests {
    use super::{hub, tools, HUB_LEAD, PRIMER};
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
        arguments.sort();
        arguments.dedup();
        let unexplained: Vec<&String> = arguments
            .iter()
            .filter(|name| {
                let word = regex::Regex::new(&format!(r"\b{}\b", regex::escape(name)))
                    .expect("a word pattern");
                !word.is_match(PRIMER)
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
