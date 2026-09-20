//! Secret resolution for the reconciler (CC-06, OPS-37, ADR-N-012).
//!
//! Manifests carry named references only. The values live outside them: in SOPS-encrypted
//! files in the repository for a single-city installation, and in OpenBao for a regional one
//! (ADR-N-012). Both backends resolve a [`jc_core::SecretRef`] into a [`SecretValue`], which
//! is the only type in this crate that holds a plaintext credential.

pub mod bao_http;
pub mod openbao;
pub mod sops;

pub use bao_http::HttpBao;
pub use openbao::{BaoApi, BaoError, BaoStore, Session};
pub use sops::{identities_from_file, SecretStore, SecretValue, SopsError};
