//! Structural diff between a declared manifest and the live resource (T-0127, CC-17,
//! CC-69, MF-04).
//!
//! Member order never matters: `serde_json` keeps objects in a sorted map, and an array
//! of scalars is compared as a multiset. Server-computed members are stripped from both
//! sides before anything is compared, so live telemetry and timestamps are not drift.
//!
//! Ownership follows CC-69: the manifest owns exactly the members it declares. A member
//! only the live resource carries is someone else's, and is reported only when the
//! `joinedcontext.com/managed-attributes` annotation claims it.

use crate::loader::RawManifest;
use jc_core::annotations::MANAGED_ATTRIBUTES;
use serde_json::{Map, Value};

/// Members the platform computes and never reads back from Git (CC-17).
const SERVER_MANAGED: &[&str] = &["status", "createdAt", "modifiedAt"];

/// One member whose declared and live values disagree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FieldDiff {
    /// Dotted path inside the manifest, for example `spec.rateLimits.burst`.
    pub path: String,
    /// What the manifest declares, `None` when only the live resource has the member.
    pub declared: Option<Value>,
    /// What the live resource carries, `None` when the member does not exist yet.
    pub live: Option<Value>,
}

/// The members of `declared` the live resource does not match (CC-17, CC-69).
///
/// An empty result means the live resource already satisfies the manifest, which is what
/// makes a second `apply` a no-op (CC-18).
pub fn diff(declared: &RawManifest, live: &RawManifest) -> Vec<FieldDiff> {
    let managed = managed_attributes(declared);
    let mut out = Vec::new();
    diff_object(
        "metadata",
        &declared.metadata.rest,
        &live.metadata.rest,
        managed.as_deref(),
        &mut out,
    );
    diff_value(
        "spec",
        &declared.spec,
        &live.spec,
        managed.as_deref(),
        &mut out,
    );
    out
}

/// The attribute names the `joinedcontext.com/managed-attributes` annotation claims, or
/// `None` when the manifest owns exactly what it declares (CC-69, the default).
fn managed_attributes(manifest: &RawManifest) -> Option<Vec<String>> {
    let value = manifest
        .metadata
        .rest
        .get("annotations")?
        .get(MANAGED_ATTRIBUTES)?
        .as_str()?;
    let names: Vec<String> = value
        .split(',')
        .map(str::trim)
        .filter(|n| !n.is_empty())
        .map(str::to_owned)
        .collect();
    (!names.is_empty()).then_some(names)
}

/// Compares two values. `managed` restricts the members compared, and applies to this
/// level only: ownership is declared per attribute, not per leaf.
fn diff_value(
    path: &str,
    declared: &Value,
    live: &Value,
    managed: Option<&[String]>,
    out: &mut Vec<FieldDiff>,
) {
    match (declared, live) {
        (Value::Object(d), Value::Object(l)) => diff_object(path, d, l, managed, out),
        (Value::Array(d), Value::Array(l)) => diff_array(path, d, l, out),
        _ if declared != live => out.push(FieldDiff {
            path: path.to_owned(),
            declared: Some(declared.clone()),
            live: Some(live.clone()),
        }),
        _ => {}
    }
}

fn diff_object(
    path: &str,
    declared: &Map<String, Value>,
    live: &Map<String, Value>,
    managed: Option<&[String]>,
    out: &mut Vec<FieldDiff>,
) {
    let owns = |name: &str| managed.is_none_or(|names| names.iter().any(|n| n == name));

    for (name, value) in declared {
        if SERVER_MANAGED.contains(&name.as_str()) || !owns(name) {
            continue;
        }
        match live.get(name) {
            Some(live_value) => diff_value(&member(path, name), value, live_value, None, out),
            None => out.push(FieldDiff {
                path: member(path, name),
                declared: Some(value.clone()),
                live: None,
            }),
        }
    }

    // A claimed attribute the manifest has since dropped: the live value is drift, because
    // the annotation says the manifest owns it (CC-69).
    for name in managed.unwrap_or_default() {
        if !declared.contains_key(name) {
            if let Some(live_value) = live.get(name.as_str()) {
                out.push(FieldDiff {
                    path: member(path, name),
                    declared: None,
                    live: Some(live_value.clone()),
                });
            }
        }
    }
}

/// Arrays of objects are keyed by `datasetId` when every element carries one (CC-17);
/// arrays of scalars are sets, so a reordered list is not a change. Anything else is
/// compared as a whole, the smallest honest answer for an ordered list.
fn diff_array(path: &str, declared: &[Value], live: &[Value], out: &mut Vec<FieldDiff>) {
    if let (Some(d), Some(l)) = (keyed_by_dataset_id(declared), keyed_by_dataset_id(live)) {
        for (key, value) in &d {
            match l.iter().find(|(k, _)| k == key) {
                Some((_, live_value)) => {
                    diff_value(&format!("{path}[{key}]"), value, live_value, None, out)
                }
                None => out.push(FieldDiff {
                    path: format!("{path}[{key}]"),
                    declared: Some((*value).clone()),
                    live: None,
                }),
            }
        }
        return;
    }

    let equal = match (scalars(declared), scalars(live)) {
        (true, true) => sorted(declared) == sorted(live),
        _ => declared == live,
    };
    if !equal {
        out.push(FieldDiff {
            path: path.to_owned(),
            declared: Some(Value::Array(declared.to_vec())),
            live: Some(Value::Array(live.to_vec())),
        });
    }
}

/// The elements paired with their `datasetId`, or `None` if any element lacks one.
fn keyed_by_dataset_id(values: &[Value]) -> Option<Vec<(&str, &Value)>> {
    if values.is_empty() {
        return None;
    }
    values
        .iter()
        .map(|v| v.get("datasetId")?.as_str().map(|k| (k, v)))
        .collect()
}

fn scalars(values: &[Value]) -> bool {
    values
        .iter()
        .all(|v| !matches!(v, Value::Object(_) | Value::Array(_)))
}

fn sorted(values: &[Value]) -> Vec<String> {
    let mut keys: Vec<String> = values.iter().map(Value::to_string).collect();
    keys.sort();
    keys
}

fn member(path: &str, name: &str) -> String {
    format!("{path}.{name}")
}
