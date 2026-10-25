//! The Context Gateway: the only way into context data (R1, ADR-N-003).
//!
//! Every request passes the same three stages. The PEP strips whatever the client claimed
//! about its own identity or tenancy and establishes both from the endpoint and the
//! verified token. The PDP turns the caller's grants into one verdict and, for the normal
//! case, the constraint set that narrows the request. The response is projected back down
//! to the attributes the grants actually cover, so nothing the caller may not see ever
//! reaches the wire.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod activity;
pub mod app;
pub mod auth;
pub mod config;
/// The JSON-LD context guard and the unit rules, in the crate jc-ngsi-input since T-5323.
pub use jc_ngsi_input::context_guard;
pub mod creates;
pub mod domain_gate;
pub mod fanout;
pub mod federation;
pub mod handlers;
pub mod ingest;
pub mod mcp;
pub mod middleware;
pub mod mqtt;
pub mod mqtt_refusals;
pub(crate) mod notification_target;
pub mod oauth;
pub mod operations;
pub mod pdp;
pub mod previews;
pub mod proxy;
pub mod quality;
pub mod query;
/// The relationship rules of a space's model, in their own crate since T-4771.
pub use jc_relationships as relationships;
pub mod resolver;
pub mod store;
pub mod telemetry;
pub use jc_ngsi_input::units;
pub mod usage;
pub mod view_mapping;
