//! `jcctl apply --repo-dir <path>` (T-0129, T-0130, CC-04, CC-16, CC-18, CC-19, CC-20).
//!
//! Converges the platform wave by wave. Idempotent by construction: `apply` writes only
//! what [`plan`](super::plan) reports, and a second run finds nothing to report (CC-18).
//!
//! Deletion is a separate lane. A resource that left the repository is never removed by a
//! plain `apply`, because a partial checkout would otherwise wipe live spaces; it takes
//! both `--prune` and `--confirm-deletions` (CC-19).

use super::plan::{compute, Action, ChangeSet, ResourceChange};
use crate::loader::{Repository, ResourceId};
use crate::platform::{Platform, PlatformError};
use std::path::Path;
use std::process::Command;

/// How much `apply` is allowed to do (CC-19).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Options {
    /// Remove resources that left the repository. Needs [`Options::confirm_deletions`].
    pub prune: bool,
    /// The second, deliberate confirmation that deletions are meant.
    pub confirm_deletions: bool,
}

impl Options {
    /// Whether a reported deletion may actually be carried out (CC-19).
    pub const fn deletes(self) -> bool {
        self.prune && self.confirm_deletions
    }
}

/// What happened to one resource.
#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    /// The platform now matches the manifest.
    Applied,
    /// Nothing to do: the live resource already matched.
    Unchanged,
    /// Deliberately not attempted, with the reason a reader needs.
    Skipped(String),
    /// The platform refused it. The run continues; the resource stays for the next run.
    Failed(PlatformError),
}

/// One resource and what happened to it.
#[derive(Debug, Clone, PartialEq)]
pub struct ResourceResult {
    /// Which resource.
    pub id: ResourceId,
    /// What `apply` set out to do.
    pub action: Action,
    /// What came of it.
    pub outcome: Outcome,
}

/// The per-resource record of one run (CC-18, CC-20).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Report {
    /// Every resource the plan named, in execution order.
    pub results: Vec<ResourceResult>,
    /// The commit the repository was on, so live state is traceable to a revision (CC-20).
    pub revision: Option<String>,
}

impl Report {
    /// Whether every resource converged.
    pub fn is_successful(&self) -> bool {
        !self
            .results
            .iter()
            .any(|r| matches!(r.outcome, Outcome::Failed(_)))
    }

    /// How many resources the run actually wrote or removed.
    pub fn applied(&self) -> usize {
        self.results
            .iter()
            .filter(|r| r.outcome == Outcome::Applied)
            .count()
    }

    /// The resources the platform refused.
    pub fn failures(&self) -> impl Iterator<Item = &ResourceResult> {
        self.results
            .iter()
            .filter(|r| matches!(r.outcome, Outcome::Failed(_)))
    }
}

/// Converges the platform to the repository (CC-16, CC-18).
///
/// Creates and updates run in ascending wave order and stop at the first wave that had a
/// failure: everything after it depends on that wave, and guessing past a broken
/// dependency is how a reconciler corrupts a platform. Failures inside one wave do not
/// stop their own wave, so independent resources still converge (CC-18). Deletions run
/// last, in descending wave order, so a space is emptied before it is removed.
pub fn run(
    repo: &Repository,
    platform: &mut dyn Platform,
    options: Options,
) -> Result<Report, PlatformError> {
    let changes = compute(repo, platform)?;
    let mut report = Report {
        results: Vec::new(),
        revision: revision_of(repo.root()),
    };

    let mut blocked_from: Option<u8> = None;
    for (wave, wave_changes) in changes.waves.iter() {
        if let Some(failed) = blocked_from {
            record_blocked(&mut report, wave_changes, failed);
            continue;
        }
        let mut wave_failed = false;
        for change in wave_changes.iter().filter(|c| c.action != Action::Delete) {
            let outcome = converge(platform, change);
            wave_failed |= matches!(outcome, Outcome::Failed(_));
            report.results.push(ResourceResult {
                id: change.id.clone(),
                action: change.action,
                outcome,
            });
        }
        if wave_failed {
            blocked_from = Some(*wave);
        }
    }

    prune(&mut report, platform, &changes, options);

    Ok(report)
}

/// Writes one resource. An unchanged resource is not written at all, which is what makes
/// a second run cost nothing (CC-18).
fn converge(platform: &mut dyn Platform, change: &ResourceChange) -> Outcome {
    match change.action {
        Action::Unchanged => Outcome::Unchanged,
        Action::Delete => Outcome::Skipped("deletion runs in the prune phase".to_owned()),
        Action::Create | Action::Update => {
            let Some(manifest) = change.declared.as_ref() else {
                return Outcome::Skipped("no manifest to write".to_owned());
            };
            match platform.put(manifest) {
                Ok(()) => Outcome::Applied,
                Err(err) => Outcome::Failed(err),
            }
        }
    }
}

/// Removes what left the repository, in descending wave order, and only when both flags
/// say so (CC-19).
fn prune(report: &mut Report, platform: &mut dyn Platform, changes: &ChangeSet, options: Options) {
    for (_, wave_changes) in changes.waves.iter().rev() {
        for change in wave_changes.iter().filter(|c| c.action == Action::Delete) {
            let outcome = if options.deletes() {
                match platform.delete(&change.id) {
                    Ok(()) => Outcome::Applied,
                    Err(err) => Outcome::Failed(err),
                }
            } else {
                Outcome::Skipped(
                    "left in place: deletion needs --prune and --confirm-deletions".to_owned(),
                )
            };
            report.results.push(ResourceResult {
                id: change.id.clone(),
                action: Action::Delete,
                outcome,
            });
        }
    }
}

fn record_blocked(report: &mut Report, changes: &[ResourceChange], failed_wave: u8) {
    for change in changes.iter().filter(|c| c.action != Action::Delete) {
        report.results.push(ResourceResult {
            id: change.id.clone(),
            action: change.action,
            outcome: Outcome::Skipped(format!("wave {failed_wave} did not converge")),
        });
    }
}

/// The commit the repository directory is on, if it is a checkout at all (CC-20).
fn revision_of(root: &Path) -> Option<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["rev-parse", "HEAD"])
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
        .filter(|sha| !sha.is_empty())
}
