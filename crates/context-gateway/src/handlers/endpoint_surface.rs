//! The DCAT-AP record an Endpoint answers with at its own root (T-0337, T-0338, EP-27,
//! EP-68, EP-69): the gateway hands `jc-dcat` what it resolved, and serves what comes back
//! (ADR-N-051, T-4499).

use crate::resolver::{Endpoint, Space};
use jc_core::kinds::Catalog;
use serde_json::Value;

pub(crate) use jc_dcat::{duties, path_of};
pub use jc_dcat::{feed, feed_turtle};

/// Calls `write` with the [`jc_dcat::Subject`] of `endpoint` and its space.
fn with_subject<T>(
    endpoint: &Endpoint,
    space: Option<&Space>,
    write: impl FnOnce(&jc_dcat::Subject<'_>) -> T,
) -> T {
    let classes: Vec<&str> = match endpoint.declared_types.as_deref() {
        Some(declared) => declared.classes.iter().map(String::as_str).collect(),
        None => endpoint
            .models
            .iter()
            .flat_map(|model| model.classes.iter().map(String::as_str))
            .collect(),
    };
    let assigners: Vec<&str> = endpoint
        .policies
        .iter()
        .map(|policy| policy.assigner.as_str())
        .collect();
    write(&jc_dcat::Subject {
        slug: &endpoint.slug,
        space: &endpoint.space,
        base_path: &endpoint.base_path,
        title: &endpoint.title,
        description: &endpoint.description,
        space_title: space.map(|s| &s.title),
        space_description: space.map(|s| &s.description),
        audience: endpoint.audience,
        representations: &endpoint.representations,
        catalog: endpoint.catalog.as_deref(),
        classes: &classes,
        assigners: &assigners,
    })
}

/// The DCAT-AP dataset record of one endpoint (EP-27, EP-68, EP-69, EP-78, EP-79).
pub fn dataset(endpoint: &Endpoint, space: Option<&Space>, index: &Value, base: &str) -> Value {
    with_subject(endpoint, space, |s| jc_dcat::dataset(s, index, base))
}

/// The record as a page (EP-27).
pub fn dataset_html(
    endpoint: &Endpoint,
    space: Option<&Space>,
    index: &Value,
    base: &str,
) -> String {
    with_subject(endpoint, space, |s| jc_dcat::dataset_html(s, index, base))
}

/// The Endpoint's catalogue block with its derived themes and keywords (T-4498).
pub(crate) fn classified(endpoint: &Endpoint) -> Catalog {
    with_subject(endpoint, None, jc_dcat::classified)
}

/// The Smart Data Model classification of the types an Endpoint holds (T-4498).
pub(crate) fn classification(endpoint: &Endpoint) -> jc_core::classification::Classification {
    with_subject(endpoint, None, jc_dcat::classification)
}
