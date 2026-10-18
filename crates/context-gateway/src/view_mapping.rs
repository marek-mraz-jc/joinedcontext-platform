//! The compiled IR of a Mapping an Endpoint's `spec.viewMappingRef` names (T-0175, EP-54,
//! DM-51, DM-52), checked when the Endpoint is loaded.
//!
//! The broker's view runs the IR: it inverts the caller's query and rebuilds every entity in the
//! target model (T-3799, Antares `antares-ops` `mapping`), and it validates every slot when the
//! view is loaded. The gateway keeps only the version check (T-4547, ADR-N-051), so an IR from a
//! newer Model Tools is left out here with a warning (`store.rs`) rather than handed on, and an
//! Endpoint with a mapping takes no write (`handlers/ngsi_ld.rs`).

use serde_json::Value;

/// The IR schema version this build hands to the broker (DM-52).
///
/// Version 2 added the expression tree of a computed slot. A repository holding version-1
/// artifacts recompiles them.
pub const IR_VERSION: u64 = 2;

/// Why an IR document is not one this gateway serves.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum InvalidIr {
    /// The document is not a JSON object with a numeric `version`.
    #[error("the mapping IR is not an object with a version")]
    Malformed,
    /// A version this build does not know, which is a Model Tools newer than this build.
    #[error("the mapping IR is version {0}, and this gateway serves version {IR_VERSION}")]
    Version(u64),
}

/// A Mapping whose IR is the version this build serves; the broker's view runs it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ViewMapping {
    version: u64,
}

impl ViewMapping {
    /// Checks one compiled mapping IR document's version (DM-52).
    pub fn parse(document: &Value) -> Result<Self, InvalidIr> {
        let version = document
            .get("version")
            .and_then(Value::as_u64)
            .ok_or(InvalidIr::Malformed)?;
        if version != IR_VERSION {
            return Err(InvalidIr::Version(version));
        }
        Ok(Self { version })
    }
}
