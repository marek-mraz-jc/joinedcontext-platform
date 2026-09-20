//! `jcctl drift --repo-dir <path>` (T-0132, CC-21, CC-22, CC-38, CC-67, UI-26).
//!
//! Drift is a `plan` read the other way round. `plan` answers "what would `apply` do", and
//! before a merge that is a proposal; run against a platform nobody was supposed to change,
//! the same non-empty answer means somebody changed it out of band (CC-21).
//!
//! Every drifted resource is offered exactly the two resolutions UI-26 puts on the screen:
//! **revert**, which re-applies what Git declares, and **adopt**, which takes the live state
//! as a manifest and turns it into an ordinary change proposal (CC-38, CC-68). Both are
//! computed here rather than described, so the caller has the YAML in hand either way — and
//! where one of them would be a deletion, it is absent with a reason instead of offered,
//! because nothing removes a live resource outside the explicit-deletion path (CC-19).
//!
//! Unmanaged sandbox spaces are not drift and never can be: they are created live through
//! the green lane, carry a TTL, and are never backed by Git (CC-67). A run that reported
//! them would report every sandbox on the platform every time and teach operators to ignore
//! the report.

use crate::commands::export;
use crate::commands::plan::{compute, Action, ResourceChange};
use crate::diff::FieldDiff;
use crate::loader::{extract_space, RawManifest, Repository, ResourceId};
use crate::platform::{Platform, PlatformError};
use jc_core::registry;
use serde_json::{json, Value};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// One resource whose live state and Git truth disagree.
#[derive(Debug, Clone, PartialEq)]
pub struct Drifted {
    /// Which resource.
    pub id: ResourceId,
    /// What happened to it, in the words an operator reads.
    pub kind: Kind,
    /// The members that differ; empty when the resource exists on only one side.
    pub diff: Vec<FieldDiff>,
    /// The manifest *revert* would re-apply, or `None` when reverting is a deletion.
    pub revert: Option<RawManifest>,
    /// The manifest *adopt* would propose, or `None` when there is no live state to adopt.
    pub adopt: Option<RawManifest>,
    /// Credentials dropped while cleaning the live state for adoption (MF-17).
    pub redactions: Vec<String>,
}

/// How a resource drifted, which is what decides the two buttons (UI-26).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// The resource exists on both sides and a member Git declares differs.
    Modified,
    /// Git declares it and the platform does not have it: deleted out of band.
    Missing,
    /// The platform has it and Git does not declare it: created out of band.
    Unexpected,
}

impl Kind {
    /// The wire name in the JSON contract (API/03 section 3).
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Modified => "MODIFIED",
            Self::Missing => "MISSING",
            Self::Unexpected => "UNEXPECTED",
        }
    }

    /// Why the resolution this drift does not offer is absent, for the operator who
    /// looks for the second button and does not find it (UI-26).
    pub const fn unavailable(self) -> Option<&'static str> {
        match self {
            Self::Modified => None,
            // Adopting "it is gone" means removing the manifest from Git, which is a
            // deletion and goes through the explicit-deletion path, not through a button.
            Self::Missing => Some("adopt: removing a manifest is an explicit deletion (CC-19)"),
            // Reverting an unexpected resource means deleting it off the platform, same rule.
            Self::Unexpected => {
                Some("revert: removing a live resource is an explicit deletion (CC-19)")
            }
        }
    }
}

/// What one drift run found.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Report {
    /// Resources compared, sandboxes excluded.
    pub checked: usize,
    /// The drifted ones, in the order `apply` would converge them.
    pub drifted: Vec<Drifted>,
    /// Live resources skipped because they belong to an unmanaged sandbox (CC-67).
    pub sandboxed: Vec<ResourceId>,
}

impl Report {
    /// Whether the platform still matches Git (API/03 exit code 0).
    pub fn is_clean(&self) -> bool {
        self.drifted.is_empty()
    }

    /// The JSON one line of stdout carries.
    pub fn to_json(&self) -> Value {
        json!({
            "summary": {
                "checked": self.checked,
                "drifted": self.drifted.len(),
                "sandboxed": self.sandboxed.len(),
            },
            "drifted": self.drifted.iter().map(|d| json!({
                "kind": d.id.kind,
                "id": d.id.to_string(),
                "drift": d.kind.as_str(),
                "diff": d.diff.iter().map(|field| json!({
                    "path": field.path,
                    "declared": field.declared,
                    "live": field.live,
                })).collect::<Vec<_>>(),
                "resolutions": resolutions(d),
                "unavailable": d.kind.unavailable(),
                "redactions": d.redactions,
            })).collect::<Vec<_>>(),
        })
    }

    /// The human rendering: one line per resource, then what to do about it.
    pub fn render(&self) -> String {
        if self.is_clean() {
            return format!("{} resources checked, no drift\n", self.checked);
        }
        let mut out = String::new();
        for drifted in &self.drifted {
            out.push_str(&format!(
                "{:<10} {} ({})\n",
                drifted.kind.as_str().to_lowercase(),
                drifted.id,
                match drifted.kind.unavailable() {
                    Some(why) => format!("{}; {why}", resolutions(drifted).join(", ")),
                    None => resolutions(drifted).join(", "),
                }
            ));
            for field in &drifted.diff {
                out.push_str(&format!("           {}\n", field.path));
            }
        }
        out.push_str(&format!(
            "{} of {} resources drifted\n",
            self.drifted.len(),
            self.checked
        ));
        out
    }
}

/// The resolutions this drift actually offers.
fn resolutions(drifted: &Drifted) -> Vec<&'static str> {
    let mut available = Vec::new();
    if drifted.revert.is_some() {
        available.push("revert");
    }
    if drifted.adopt.is_some() {
        available.push("adopt");
    }
    available
}

/// Compares the platform with the repository and reports what does not match (CC-21).
pub fn detect(repo: &Repository, platform: &impl Platform) -> Result<Report, PlatformError> {
    let changes = compute(repo, platform)?;
    let sandboxes = unmanaged_sandboxes(&changes);

    let mut report = Report::default();
    for change in changes.iter() {
        if let Some(space) = sandbox_of(change, &sandboxes) {
            debug_assert!(!space.is_empty());
            report.sandboxed.push(change.id.clone());
            continue;
        }
        report.checked += 1;

        let kind = match change.action {
            Action::Unchanged => continue,
            Action::Update => Kind::Modified,
            Action::Create => Kind::Missing,
            Action::Delete => Kind::Unexpected,
        };
        let (adopted, redactions) = match &change.live {
            Some(live) => {
                let (manifest, dropped) = export::adopt(live);
                (Some(manifest), dropped)
            }
            None => (None, Vec::new()),
        };
        report.drifted.push(Drifted {
            id: change.id.clone(),
            kind,
            diff: change.diff.clone(),
            revert: change.declared.clone(),
            adopt: adopted,
            redactions,
        });
    }
    Ok(report)
}

/// Writes every adoptable manifest under `out_dir`, at the path its kind prescribes (MF-06).
///
/// This is the material half of *adopt*: the files an operator commits to turn live state
/// into the ordinary change proposal CC-38 asks for. Nothing else is written, so a drift run
/// that only reports stays read-only.
pub fn write_adoptions(out_dir: &Path, report: &Report) -> std::io::Result<usize> {
    let resources = report
        .drifted
        .iter()
        .filter_map(|drifted| {
            let manifest = drifted.adopt.as_ref()?;
            let info = registry::by_kind(&manifest.kind)?;
            let project = drifted.id.namespace.as_deref().unwrap_or("org");
            let path = info.repo_path(
                project,
                extract_space(&manifest.spec),
                &manifest.metadata.name,
            );
            Some(export::Exported {
                path: PathBuf::from(path),
                manifest: manifest.clone(),
            })
        })
        .collect();

    export::write(
        out_dir,
        &export::Report {
            resources,
            redactions: Vec::new(),
        },
    )
}

/// The names of the live-only ContextSpaces that are unmanaged sandboxes (CC-67).
///
/// Live-only is part of the definition: a sandbox is never backed by Git, so a space the
/// repository *does* declare is managed configuration whatever its `isSandbox` says, and
/// drift on it is real drift.
fn unmanaged_sandboxes(changes: &crate::commands::plan::ChangeSet) -> BTreeSet<String> {
    changes
        .iter()
        .filter(|change| change.action == Action::Delete && change.id.kind == "ContextSpace")
        .filter(|change| {
            change
                .live
                .as_ref()
                .and_then(|live| live.spec.get("isSandbox"))
                .and_then(Value::as_bool)
                == Some(true)
        })
        .map(|change| change.id.name.clone())
        .collect()
}

/// The sandbox this change belongs to, if any: the space itself, or a resource inside it.
fn sandbox_of<'a>(change: &ResourceChange, sandboxes: &'a BTreeSet<String>) -> Option<&'a String> {
    if change.id.kind == "ContextSpace" {
        return sandboxes.get(&change.id.name);
    }
    let spec = change
        .live
        .as_ref()
        .or(change.declared.as_ref())
        .map(|manifest| &manifest.spec)?;
    let space = extract_space(spec);
    (!space.is_empty()).then(|| sandboxes.get(space))?
}
