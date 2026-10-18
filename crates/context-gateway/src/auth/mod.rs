//! Establishing who is calling (PF-34, PF-37).

pub mod accounts;
pub mod dataspace_token;
pub mod subject;

// The verifier lives in jc-core so the functions runtime need not link the gateway (T-4605).
pub use jc_core::auth::{jwks, token};
