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

pub mod app;
pub mod auth;
pub mod config;
pub mod domain_gate;
pub mod egress;
pub mod federation;
pub mod handlers;
pub mod mcp;
pub mod middleware;
pub mod operations;
pub mod pdp;
pub mod previews;
pub mod proxy;
pub mod query;
pub mod resolver;
pub mod store;
pub mod telemetry;
pub mod translators;
pub mod units;
