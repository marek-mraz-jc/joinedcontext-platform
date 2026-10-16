//! The gateway's MCP: the hub and the named servers, and the two describe tools beside the
//! broker's MCP surface of each view (plan 5.3, T-3648).

pub mod describe;
pub mod hub;
pub mod jsonrpc;
pub mod server;
pub mod tools;
pub mod units;

/// What the hub and every named server tell a client in `initialize`: that the connector speaks
/// NGSI-LD, its query language and the order of work (ADR-N-025 §4c, EP-116). It names no slug,
/// token or host.
pub(crate) const PRIMER: &str = include_str!("primer.md");
