//! Publication of platform resources to systems outside the platform (EP-62…EP-67).
//!
//! A publisher never reaches into a broker or a database: it reads what an Endpoint
//! already serves and writes it where a catalogue expects it, so the published copy can
//! only ever carry what the Endpoint's policy set allows (EP-66).

pub mod ckan;
pub mod ckan_datastore;
pub mod ckan_http;
