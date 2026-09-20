//! `jcctl workspace render|diff` (CC-76, CC-78, T-1238): the preview render and the comparison
//! the Portal uses, from a checkout.
//!
//! `render` is [`Repository::load_preview`] written out, one file per manifest. `diff`
//! compares a checkout of the workspace's branch with a checkout of its base, as the Portal's
//! compare does: what the workspace creates, changes and removes, field by field.

use std::path::Path;

use crate::diff::{diff, FieldDiff};
use crate::loader::{LoadError, Repository, ResourceId};

/// The overlay `JC_ENVIRONMENT` names, as `validate` and `plan` read it (CC-73).
fn environment() -> Option<String> {
    std::env::var("JC_ENVIRONMENT")
        .ok()
        .filter(|value| !value.trim().is_empty())
}

/// The file a rendered manifest is written to: `{namespace}/{kind}-{name}.yaml`.
pub fn file_name(id: &ResourceId) -> String {
    format!(
        "{}/{}-{}.yaml",
        id.namespace.as_deref().unwrap_or("org"),
        id.kind.to_ascii_lowercase(),
        id.name
    )
}

/// Every manifest of `dir` rendered with `prefix`, as `(file name, YAML)`, in id order.
/// Refused when the render would keep an unprefixed organization-unique name (PF-83).
pub fn render(dir: &Path, prefix: &str) -> Result<Vec<(String, String)>, LoadError> {
    let env = environment();
    let repo = Repository::load_preview(dir, env.as_deref(), prefix)?;
    Ok(repo
        .iter()
        .map(|(id, loaded)| {
            let yaml = serde_norway::to_string(&loaded.manifest)
                .unwrap_or_else(|err| format!("# {id}: {err}\n"));
            (file_name(id), yaml)
        })
        .collect())
}

/// What a workspace does to one resource.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Operation {
    /// Only the workspace has it.
    Create,
    /// Both have it, and some field differs.
    Update,
    /// Only the base has it.
    Delete,
}

impl Operation {
    /// The name the Portal's compare gives the operation.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Create => "Create",
            Self::Update => "Update",
            Self::Delete => "Delete",
        }
    }
}

/// One resource the workspace changes, with its fields (`declared` is the workspace's value,
/// `live` the base's).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// The resource.
    pub id: ResourceId,
    /// What the workspace does to it.
    pub operation: Operation,
    /// The fields an update changes; empty for a create or a delete.
    pub fields: Vec<FieldDiff>,
}

/// The resources `ours` creates, changes and removes against `base`, in id order.
pub fn compare(base: &Repository, ours: &Repository) -> Vec<Entry> {
    let mut entries = Vec::new();
    for (id, loaded) in ours.iter() {
        match base.get(id) {
            None => entries.push(Entry {
                id: id.clone(),
                operation: Operation::Create,
                fields: Vec::new(),
            }),
            Some(before) => {
                let mut fields = diff(&loaded.manifest, &before.manifest);
                // A member the workspace removed is one the base declares and it does not.
                fields.extend(
                    diff(&before.manifest, &loaded.manifest)
                        .into_iter()
                        .filter(|field| field.live.is_none())
                        .map(|field| FieldDiff {
                            path: field.path,
                            declared: None,
                            live: field.declared,
                        }),
                );
                fields.sort_by(|a, b| a.path.cmp(&b.path));
                if !fields.is_empty() {
                    entries.push(Entry {
                        id: id.clone(),
                        operation: Operation::Update,
                        fields,
                    });
                }
            }
        }
    }
    for (id, _) in base.iter() {
        if ours.get(id).is_none() {
            entries.push(Entry {
                id: id.clone(),
                operation: Operation::Delete,
                fields: Vec::new(),
            });
        }
    }
    entries.sort_by(|a, b| a.id.cmp(&b.id));
    entries
}

/// Loads both checkouts and compares them.
pub fn diff_dirs(base_dir: &Path, workspace_dir: &Path) -> Result<Vec<Entry>, LoadError> {
    let env = environment();
    let base = Repository::load_for(base_dir, env.as_deref())?;
    let ours = Repository::load_for(workspace_dir, env.as_deref())?;
    Ok(compare(&base, &ours))
}

/// The comparison as JSON: `[{ "resource", "operation", "fields": [{ "path", "from", "to" }] }]`.
pub fn to_json(entries: &[Entry]) -> serde_json::Value {
    serde_json::Value::Array(
        entries
            .iter()
            .map(|entry| {
                serde_json::json!({
                    "resource": entry.id.to_string(),
                    "operation": entry.operation.as_str(),
                    "fields": entry.fields.iter().map(|field| serde_json::json!({
                        "path": field.path,
                        "from": field.live,
                        "to": field.declared,
                    })).collect::<Vec<_>>(),
                })
            })
            .collect(),
    )
}
