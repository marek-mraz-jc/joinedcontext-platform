//! Declarative reconciler, migration engine, and configuration loader for joinedcontext (API/03, CC-08).

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod apisix;
pub mod assemble;
pub mod bento;
pub mod blueprints;
pub mod commands;
pub mod csr;
pub mod diff;
pub mod entities;
pub mod foreign_models;
pub mod gateway;
pub mod lanes;
pub mod loader;
pub mod model;
pub mod pipeline_test;
pub mod pipelines;
pub mod pipelines_derived;
pub mod platform;
pub mod publish;
pub mod roles;
pub mod secrets;
pub mod service_accounts;
pub mod sync;
pub mod taxonomy;
pub mod waves;

pub use blueprints::{expand, ExpandError, Expanded};
pub use diff::{diff, FieldDiff};
pub use lanes::{change_envelope, lane_of, lane_of_changeset, Lane, LaneVerdict};
pub use loader::{LoadError, LoadedResource, RawManifest, RawMetadata, Repository, ResourceId};
pub use secrets::{SecretStore, SecretValue, SopsError};
pub use waves::{
    plan, wave_of, Plan, WAVE_ACCESS, WAVE_EXPOSURE, WAVE_FEDERATION, WAVE_ROOTS, WAVE_RUNTIME,
    WAVE_SPACES,
};
