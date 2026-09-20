//! `jcctl plan --repo-dir <path>` (T-0128, CC-15, CC-19, CC-20, MF-13, TS-20).
//!
//! Read-only: the repository is the desired state, the platform answers with the live
//! state, and the difference is reported per resource in wave order (Architecture/06
//! section 3). Nothing is written, so `plan` is what CI runs on a merge request.

use crate::diff::{diff, FieldDiff};
use crate::loader::{RawManifest, Repository, ResourceId};
use crate::platform::{Platform, PlatformError};
use crate::service_accounts::owner_policies;
use crate::waves::wave_of;
use jc_core::registry;
use serde_json::{json, Map, Value};
use std::collections::{BTreeMap, BTreeSet};

/// What `apply` would do to one resource (CC-15).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// The resource is declared and the platform does not have it.
    Create,
    /// The resource exists and some member it declares differs.
    Update,
    /// The platform has it and the repository does not (CC-19).
    Delete,
    /// The live resource already satisfies the manifest.
    Unchanged,
}

impl Action {
    /// The wire name of the action in the `--json` contract (API/03 section 2).
    pub const fn as_str(self) -> &'static str {
        match self {
            Action::Create => "CREATE",
            Action::Update => "UPDATE",
            Action::Delete => "DELETE",
            Action::Unchanged => "UNCHANGED",
        }
    }
}

/// One resource and what would happen to it.
#[derive(Debug, Clone, PartialEq)]
pub struct ResourceChange {
    /// Which resource.
    pub id: ResourceId,
    /// What `apply` would do.
    pub action: Action,
    /// The members that differ, empty for a create or a delete.
    pub diff: Vec<FieldDiff>,
    /// The manifest to write, absent for a delete.
    pub declared: Option<RawManifest>,
    /// The live resource as the platform answered, absent for a create.
    ///
    /// `plan` prints the diff and never needs it; `drift` does, because adopting live
    /// state into Git means writing exactly what the platform holds (CC-22, CC-38).
    pub live: Option<RawManifest>,
}

/// Everything `apply` would do, grouped into the waves it would do it in (CC-18).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ChangeSet {
    /// Non-empty waves in ascending order, each with its changes in convergence order.
    pub waves: Vec<(u8, Vec<ResourceChange>)>,
    /// What the plan noticed and will not act on, one sentence each (API/03 section 3).
    ///
    /// Advisory only: a flag never changes the exit code, because a peer that publishes no
    /// schema is a fact about the peer and not a fault in the repository (DM-48).
    pub flags: Vec<String>,
}

impl ChangeSet {
    /// Every change in the order `apply` would execute it.
    pub fn iter(&self) -> impl Iterator<Item = &ResourceChange> {
        self.waves.iter().flat_map(|(_, changes)| changes)
    }

    /// How many resources carry `action`.
    pub fn count(&self, action: Action) -> usize {
        self.iter().filter(|c| c.action == action).count()
    }

    /// Whether the live platform already matches the repository (API/03 exit code 0).
    pub fn is_clean(&self) -> bool {
        self.iter().all(|c| c.action == Action::Unchanged)
    }

    /// The `--json` contract of API/03 section 2.
    pub fn to_json(&self) -> Value {
        let changes: Vec<Value> = self
            .iter()
            .filter(|c| c.action != Action::Unchanged)
            .map(|change| {
                json!({
                    "kind": change.id.kind,
                    "id": change.id.name,
                    "action": change.action.as_str(),
                    "diff": nest(&change.diff),
                })
            })
            .collect();

        json!({
            "summary": {
                "to_add": self.count(Action::Create),
                "to_change": self.count(Action::Update),
                "to_delete": self.count(Action::Delete),
            },
            "changes": changes,
            "flags": self.flags,
        })
    }

    /// The per-resource diff a reviewer reads (CC-15, CC-20).
    pub fn render(&self) -> String {
        let mut out = String::new();
        for (wave, changes) in &self.waves {
            out.push_str(&format!("wave {wave}\n"));
            for change in changes {
                out.push_str(&format!(
                    "  {:<9} {}\n",
                    change.action.as_str().to_lowercase(),
                    change.id
                ));
                for field in &change.diff {
                    out.push_str(&format!(
                        "      {}: {} -> {}\n",
                        field.path,
                        render_value(field.live.as_ref()),
                        render_value(field.declared.as_ref()),
                    ));
                }
            }
        }
        out.push_str(&format!(
            "{} to add, {} to change, {} to delete\n",
            self.count(Action::Create),
            self.count(Action::Update),
            self.count(Action::Delete),
        ));
        for flag in &self.flags {
            out.push_str(&format!("note: {flag}\n"));
        }
        out
    }
}

/// Computes what `apply` would do (CC-15).
///
/// Deletions are only ever *reported* here; removing them needs the explicit flags of
/// `apply` (CC-19).
pub fn compute(repo: &Repository, platform: &dyn Platform) -> Result<ChangeSet, PlatformError> {
    let mut live = BTreeMap::new();
    for (project, plural) in collections(repo, platform)? {
        for manifest in platform.list(&project, &plural)? {
            live.insert(ResourceId::from_manifest(&manifest), manifest);
        }
    }

    let mut by_wave: BTreeMap<u8, Vec<ResourceChange>> = BTreeMap::new();

    for (id, declared) in desired(repo) {
        let Some(wave) = wave_of(&id.kind) else {
            continue;
        };
        let declared = &declared;
        let change = match live.remove(&id) {
            None => ResourceChange {
                id: id.clone(),
                action: Action::Create,
                diff: Vec::new(),
                declared: Some(declared.clone()),
                live: None,
            },
            Some(existing) => {
                let fields = diff(declared, &existing);
                ResourceChange {
                    id: id.clone(),
                    action: if fields.is_empty() {
                        Action::Unchanged
                    } else {
                        Action::Update
                    },
                    diff: fields,
                    declared: Some(declared.clone()),
                    live: Some(existing),
                }
            }
        };
        by_wave.entry(wave).or_default().push(change);
    }

    // Whatever is left lives on the platform and no longer in Git (CC-19). Deletion runs
    // in reverse wave order, so a wave is emptied before the wave it depends on.
    for (id, existing) in live {
        if let Some(wave) = wave_of(&id.kind) {
            by_wave.entry(wave).or_default().push(ResourceChange {
                id,
                action: Action::Delete,
                diff: Vec::new(),
                declared: None,
                live: Some(existing),
            });
        }
    }

    Ok(ChangeSet {
        waves: by_wave.into_iter().collect(),
        flags: crate::foreign_models::plan_flags(repo),
    })
}

/// Every resource the repository asks for: what it declares, plus what the reconciler
/// generates from it. A ServiceAccount brings its own owner policies, so no writer can
/// reach live state without a policy governing it (CC-61).
fn desired(repo: &Repository) -> BTreeMap<ResourceId, RawManifest> {
    let org_domain = organization_domain(repo);
    let mut desired = BTreeMap::new();

    for (id, resource) in repo.iter() {
        desired.insert(id.clone(), resource.manifest.clone());
    }
    // Second pass, so a hand-written manifest always wins over a generated one of the
    // same identity: what somebody committed is never silently replaced.
    for (_, resource) in repo.iter() {
        for policy in owner_policies(&resource.manifest, &org_domain) {
            desired
                .entry(ResourceId::from_manifest(&policy))
                .or_insert(policy);
        }
    }
    desired
}

/// The organization's domain, which every generated policy names as its assigner.
fn organization_domain(repo: &Repository) -> String {
    repo.iter()
        .find(|(id, _)| id.kind == "Organization")
        .and_then(|(_, resource)| {
            resource
                .manifest
                .spec
                .get("domain")
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .unwrap_or_default()
}

/// The `(project, plural)` collections worth listing: every project the repository or the
/// platform knows, times every reconciled kind.
fn collections(
    repo: &Repository,
    platform: &dyn Platform,
) -> Result<Vec<(String, String)>, PlatformError> {
    let mut projects: BTreeSet<String> = repo
        .iter()
        .filter_map(|(id, _)| id.namespace.clone())
        .collect();
    for manifest in platform.list("org", "projects")? {
        projects.insert(manifest.metadata.name.clone());
    }
    projects.insert("org".to_owned());

    Ok(projects
        .iter()
        .flat_map(|project| {
            registry::KINDS
                .iter()
                .filter(|kind| wave_of(kind.kind).is_some())
                .map(move |kind| (project.clone(), kind.plural.to_owned()))
        })
        .collect())
}

/// Folds dotted field paths back into the nested object the JSON contract shows.
fn nest(fields: &[FieldDiff]) -> Value {
    let mut root = Map::new();
    for field in fields {
        let mut cursor = &mut root;
        let mut segments = field.path.split('.').peekable();
        while let Some(segment) = segments.next() {
            if segments.peek().is_none() {
                cursor.insert(
                    segment.to_owned(),
                    field.declared.clone().unwrap_or(Value::Null),
                );
                break;
            }
            cursor = cursor
                .entry(segment)
                .or_insert_with(|| Value::Object(Map::new()))
                .as_object_mut()
                .expect("a path segment cannot be both a leaf and a branch");
        }
    }
    Value::Object(root)
}

fn render_value(value: Option<&Value>) -> String {
    value.map_or_else(|| "(absent)".to_owned(), Value::to_string)
}
