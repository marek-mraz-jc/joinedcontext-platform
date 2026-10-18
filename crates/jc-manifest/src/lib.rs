//! The manifest repository as a library (T-4604): what jcctl, the context gateway and the
//! assistant read from a configuration repository, without linking the CLI.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod assemble;
pub mod bento;
pub mod foreign_models;
pub mod loader;
pub mod model_schema;
pub mod pipeline_identity;
pub mod roles;
pub mod validate;
