//! The `SyncSource` loop: when a run is due, whether the source moved, and what proposal a
//! run becomes (T-0142, MF-27…MF-31).
//!
//! A sync run is an import with a schedule in front of it. That is not a simplification of
//! the requirement, it is the requirement: MF-28 says every run goes through the standard
//! import gates, so this module runs [`crate::commands::import::collect`] rather than a
//! second, laxer path, and turns its report into the `kind: Change` envelope
//! [`crate::lanes`] already builds for every other proposal. A sync that could merge what an
//! import would refuse would be the way around MF-24.
//!
//! Nothing here opens a socket, clones a repository or talks to a forge. Like the CKAN
//! publisher, the registration reconciler and the foreign-model mirror next door, the
//! transport is a trait the caller implements: it is the caller that resolved
//! `source.*.secretRef` and holds the credential, and no type in this module has a field a
//! token could live in (MF-31). Pushing the branch and opening the merge request belongs to
//! the same caller — this module decides *what* the proposal is, and hands back the manifests
//! to write and the envelope to open it with.
//!
//! Nothing is written either: [`poll`] is the whole judgement, and the caller writes the
//! files of an accepted [`Proposal`] onto a branch. A run whose plan is empty produces no
//! proposal at all, so a source that has not moved does not put a merge request on the forge
//! every half hour (CC-18).

use crate::commands::import::{self, Conflict, Options, Report};
use crate::commands::plan::{Action, ChangeSet, ResourceChange};
use crate::lanes::{change_envelope, lane_of_changeset, Lane};
use crate::loader::{RawManifest, Repository, ResourceId};
use crate::waves::wave_of;
use jc_core::envelope::annotations::SYNC_SOURCE;
use jc_core::kinds::sync::{ConflictPolicy, Schedule, SyncMode, SyncOrigin, SyncSourceSpec};
use serde_json::{json, Map, Value};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// What a `SyncSource` reads its remote through.
///
/// Two calls, because the cheap one is the one that runs on every tick: a source that has not
/// moved must not cost a clone. The implementation carries whatever credential
/// `source.*.secretRef` resolved to, and nothing built here ever sees it (MF-31).
pub trait SyncRemote {
    /// The revision the origin is at now: a commit for `git`, a digest or `ETag` for a
    /// `bundle`, the source revision for a `platformApi`. Opaque to this module, which only
    /// ever compares it with the one the last run recorded.
    fn revision(&self, origin: &SyncOrigin) -> Result<String, RemoteError>;

    /// Materialises the origin at `revision` into `into`, as the manifests an import reads.
    fn checkout(&self, origin: &SyncOrigin, revision: &str, into: &Path)
        -> Result<(), RemoteError>;
}

/// Why the remote did not answer. Carries no credential.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RemoteError {
    /// The origin could not be reached.
    #[error("sync origin unavailable: {0}")]
    Unavailable(String),
    /// The origin answered and refused. A refusal is not a reason to fall back to a cached
    /// copy: an expired credential looks exactly like a source that removed everything.
    #[error("sync origin refused: {0}")]
    Refused(String),
}

/// Why a run could not be judged.
#[derive(Debug, thiserror::Error)]
pub enum SyncError {
    /// The manifest is not a `SyncSource`.
    #[error("not a SyncSource manifest")]
    NotASyncSource,
    /// The spec does not parse or does not validate.
    #[error("sync source spec: {0}")]
    Spec(String),
    /// A `SyncSource` syncs into its own project, and this manifest names none.
    #[error("the sync source has no namespace, so there is no project to sync into")]
    NoProject,
    /// The remote did not answer.
    #[error(transparent)]
    Remote(#[from] RemoteError),
    /// The checkout or the repository could not be read.
    #[error("{path}: {source}")]
    Io {
        /// What could not be read or written.
        path: PathBuf,
        /// The underlying failure.
        #[source]
        source: std::io::Error,
    },
    /// The import gates could not run at all.
    #[error("import: {0}")]
    Import(String),
}

/// What a `SyncSource` reports about itself (MF-30, Architecture/06 section 6).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    /// The repository matches the source at the observed revision.
    Synced,
    /// The source moved and a proposal is being prepared; nothing is waiting on a person.
    OutOfSync,
    /// A proposal is open and needs its lane's approvals (MF-29).
    PendingApproval,
    /// The last run failed. The observed revision is left where it was.
    Error,
    /// Syncing is switched off, by an operator or by `mode: oneshot` having finished.
    Paused,
}

impl Phase {
    /// The name the status block and the Portal carry.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Synced => "Synced",
            Self::OutOfSync => "OutOfSync",
            Self::PendingApproval => "PendingApproval",
            Self::Error => "Error",
            Self::Paused => "Paused",
        }
    }
}

/// What the previous run left behind, and the only memory a run has (MF-30).
///
/// Held by the caller between ticks, because the loop is a decision and not a process: a
/// daemon that restarted would otherwise re-propose everything it had already proposed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct State {
    /// The revision the last successful run imported.
    pub observed_revision: Option<String>,
    /// When that run happened, in seconds since the epoch.
    pub last_run_at: Option<u64>,
    /// The merge request a previous run opened and nobody has merged yet.
    pub open_proposal: Option<String>,
    /// Switched off by an operator, or by a finished `oneshot` (Architecture/06 section 6).
    pub paused: bool,
}

/// What one run decided.
#[derive(Debug, Clone, PartialEq)]
pub struct Run {
    /// What the source reports about itself now (MF-30).
    pub phase: Phase,
    /// The state to keep for the next tick.
    pub state: State,
    /// The proposal to open, absent when the run changes nothing.
    pub proposal: Option<Proposal>,
    /// What a run that did nothing did not do, in one clause, for the status conditions.
    pub reason: &'static str,
}

/// One reviewable change proposal: what to write on the branch, and the envelope to open the
/// merge request with (MF-28, MF-29).
#[derive(Debug, Clone, PartialEq)]
pub struct Proposal {
    /// Deterministic name, so a re-run finds its own proposal instead of opening a second
    /// one for the same revision (CC-18).
    pub name: String,
    /// The project the proposal belongs to.
    pub namespace: String,
    /// The revision of the source it carries.
    pub revision: String,
    /// The `kind: Change` envelope, with the lane and the plan counts (API/01 section 3).
    pub envelope: Value,
    /// The manifests to write, by repository-relative path.
    pub files: BTreeMap<PathBuf, RawManifest>,
    /// Repository paths the source no longer carries, when `prune` is on (CC-19).
    pub removed: Vec<PathBuf>,
    /// Whether this proposal may merge itself: `autoMerge` on a green lane and nothing else
    /// (MF-29, CC-70).
    pub auto_merge: bool,
    /// Everything the import gates refused. A non-empty list is why there is no proposal to
    /// write; it is carried so the status says which resource, not just that a run failed.
    pub rejected: Vec<String>,
}

/// Whether a scheduled run is due (MF-28).
///
/// A webhook schedule is never due on a timer, so this is `false` for one however long it has
/// waited: such a source runs when its origin says it moved, and [`poll_now`] is how a caller
/// says so.
pub fn due(schedule: &Schedule, state: &State, now: u64) -> bool {
    if state.paused {
        return false;
    }
    let Some(interval) = schedule.interval_seconds() else {
        return false;
    };
    match state.last_run_at {
        None => true,
        Some(last) => now.saturating_sub(last) >= interval,
    }
}

/// Runs one tick of the loop (MF-27, MF-28).
///
/// `workspace` is a directory this run may fill with the checkout; the caller owns it and
/// removes it. Nothing under `repo_dir` is written.
pub fn poll(
    source: &RawManifest,
    state: &State,
    now: u64,
    repo_dir: &Path,
    workspace: &Path,
    remote: &impl SyncRemote,
) -> Result<Run, SyncError> {
    judge(source, state, now, repo_dir, workspace, remote, false)
}

/// Runs the loop because somebody asked for it, whatever the schedule says (MF-28).
///
/// The two callers are the webhook route of a `schedule: { webhook: true }` source and an
/// operator's **Sync now**, which is why this exists at all: [`due`] is false for every webhook
/// schedule, so a source that runs on its origin's word can never reach a run through [`poll`].
///
/// Only the wait is skipped. A paused source is still paused and an open proposal still blocks
/// the next one — the switch and the reviewer are not overridden by the button — and a source
/// that has not moved still costs one `revision` call and no checkout.
pub fn poll_now(
    source: &RawManifest,
    state: &State,
    now: u64,
    repo_dir: &Path,
    workspace: &Path,
    remote: &impl SyncRemote,
) -> Result<Run, SyncError> {
    judge(source, state, now, repo_dir, workspace, remote, true)
}

/// The judgement both entry points share; `asked` is whether the wait has already been served.
#[allow(clippy::too_many_arguments)]
fn judge(
    source: &RawManifest,
    state: &State,
    now: u64,
    repo_dir: &Path,
    workspace: &Path,
    remote: &impl SyncRemote,
    asked: bool,
) -> Result<Run, SyncError> {
    let (spec, namespace) = spec_of(source)?;

    if state.paused {
        return Ok(idle(state, Phase::Paused, "syncing is paused"));
    }
    if state.open_proposal.is_some() {
        // A second proposal on top of an unreviewed one is how a queue of merge requests for
        // one source is built. The reviewer's answer is what unblocks the next run.
        return Ok(idle(
            state,
            Phase::PendingApproval,
            "a proposal from an earlier run is still open",
        ));
    }
    if !asked && !due(&spec.schedule, state, now) {
        return Ok(idle(state, Phase::Synced, "the next run is not due yet"));
    }

    let revision = remote.revision(&spec.source)?;
    if state.observed_revision.as_deref() == Some(revision.as_str()) {
        let mut next = state.clone();
        next.last_run_at = Some(now);
        return Ok(Run {
            phase: Phase::Synced,
            state: next,
            proposal: None,
            reason: "the source is at the revision this repository already carries",
        });
    }

    let checkout = workspace.join(&revision);
    std::fs::create_dir_all(&checkout).map_err(|source| SyncError::Io {
        path: checkout.clone(),
        source,
    })?;
    remote.checkout(&spec.source, &revision, &checkout)?;
    if !spec.selector.is_empty() {
        deselect(&checkout, &spec.selector)?;
    }

    let options = Options {
        namespace: Some(namespace.clone()),
        org_domain: None,
        conflict: conflict_of(spec.conflict_policy),
    };
    let report = import::collect(&checkout, repo_dir, &options)
        .map_err(|error| SyncError::Import(error.to_string()))?;

    Ok(proposal_of(
        source, &spec, &namespace, &revision, report, state, now, repo_dir,
    ))
}

/// The name a proposal for one revision of one source always has (CC-18).
pub fn proposal_name(source: &str, revision: &str) -> String {
    let short: String = revision.chars().take(7).collect();
    format!("chg-sync-{source}-{short}")
}

/// The annotation value that marks a resource as belonging to one sync source (MF-08, MF-27).
pub fn owner_annotation(namespace: &str, name: &str) -> String {
    format!("{namespace}/{name}")
}

/// A tick that decided to do nothing, with the state it leaves untouched.
fn idle(state: &State, phase: Phase, reason: &'static str) -> Run {
    Run {
        phase,
        state: state.clone(),
        proposal: None,
        reason,
    }
}

/// The `SyncSource` spec and the project it syncs into.
fn spec_of(source: &RawManifest) -> Result<(SyncSourceSpec, String), SyncError> {
    if source.kind != "SyncSource" {
        return Err(SyncError::NotASyncSource);
    }
    let namespace = source
        .metadata
        .namespace
        .clone()
        .ok_or(SyncError::NoProject)?;
    let spec: SyncSourceSpec = serde_json::from_value(source.spec.clone())
        .map_err(|error| SyncError::Spec(error.to_string()))?;
    spec.validate()
        .map_err(|error| SyncError::Spec(error.to_string()))?;
    Ok((spec, namespace))
}

const fn conflict_of(policy: ConflictPolicy) -> Conflict {
    match policy {
        ConflictPolicy::Fail => Conflict::Fail,
        ConflictPolicy::Skip => Conflict::Skip,
        ConflictPolicy::Replace => Conflict::Replace,
        ConflictPolicy::Rename => Conflict::Rename,
    }
}

/// Removes the checked-out documents the selector does not name (MF-10).
///
/// Before the import rather than after it: `collect` refuses a reference to something that is
/// in neither the bundle nor the repository, and a set filtered afterwards would have passed
/// that gate on resources it then dropped.
fn deselect(checkout: &Path, selector: &BTreeMap<String, String>) -> Result<(), SyncError> {
    for entry in walkdir::WalkDir::new(checkout).follow_links(false) {
        let entry = entry.map_err(|error| SyncError::Io {
            path: checkout.to_path_buf(),
            source: error.into(),
        })?;
        if !entry.file_type().is_file() {
            continue;
        }
        let path = entry.path();
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if !name.ends_with(".yaml") && !name.ends_with(".yml") {
            continue;
        }
        let text = std::fs::read_to_string(path).map_err(|source| SyncError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        let kept: Vec<String> = crate::loader::parse_yaml_documents(&text)
            .into_iter()
            .map(|chunk| chunk.content)
            .filter(|document| {
                if crate::loader::is_empty_doc(document) {
                    return false;
                }
                match serde_norway::from_str::<RawManifest>(document) {
                    Ok(manifest) => selects(&manifest, selector),
                    // A document the import gates would refuse anyway is left for them to
                    // refuse by name, rather than disappearing here as "not selected".
                    Err(_) => true,
                }
            })
            .collect();

        if kept.is_empty() {
            std::fs::remove_file(path).map_err(|source| SyncError::Io {
                path: path.to_path_buf(),
                source,
            })?;
        } else {
            std::fs::write(path, kept.join("---\n")).map_err(|source| SyncError::Io {
                path: path.to_path_buf(),
                source,
            })?;
        }
    }
    Ok(())
}

/// Whether one manifest carries every label the selector names (MF-10).
fn selects(manifest: &RawManifest, selector: &BTreeMap<String, String>) -> bool {
    let labels = manifest
        .metadata
        .rest
        .get("labels")
        .and_then(Value::as_object);
    selector.iter().all(|(key, value)| {
        labels
            .and_then(|labels| labels.get(key))
            .and_then(Value::as_str)
            == Some(value.as_str())
    })
}

/// Turns an import report into the proposal a reviewer sees (MF-28, MF-29).
#[allow(clippy::too_many_arguments)]
fn proposal_of(
    source: &RawManifest,
    spec: &SyncSourceSpec,
    namespace: &str,
    revision: &str,
    report: Report,
    state: &State,
    now: u64,
    repo_dir: &Path,
) -> Run {
    let rejected: Vec<String> = report.rejections.iter().map(ToString::to_string).collect();
    let owner = owner_annotation(namespace, &source.metadata.name);

    let mut files = BTreeMap::new();
    let mut changes: Vec<ResourceChange> = Vec::new();
    for imported in &report.imported {
        if imported.outcome == import::Outcome::Skipped {
            continue;
        }
        let mut manifest = imported.manifest.clone();
        stamp(&mut manifest, &owner);
        changes.push(ResourceChange {
            id: ResourceId::from_manifest(&manifest),
            action: match imported.outcome {
                import::Outcome::Replaced => Action::Update,
                _ => Action::Create,
            },
            diff: Vec::new(),
            declared: Some(manifest.clone()),
            live: None,
        });
        files.insert(imported.path.clone(), manifest);
    }

    let mut removed = Vec::new();
    if spec.prune {
        for (id, path) in dropped(repo_dir, &owner, &report) {
            changes.push(ResourceChange {
                id,
                action: Action::Delete,
                diff: Vec::new(),
                declared: None,
                live: None,
            });
            removed.push(path);
        }
    }

    let mut next = state.clone();
    next.last_run_at = Some(now);

    if !rejected.is_empty() {
        // An error is not a finished oneshot: the source is left attached so the next run can
        // try the revision the gates refused, once somebody has fixed it.
        // MF-24 is a gate for a sync run too: a run with a rejection proposes nothing, and
        // the observed revision stays where it was so the next run tries the same source
        // again rather than treating a refused revision as imported.
        return Run {
            phase: Phase::Error,
            state: next,
            proposal: Some(Proposal {
                name: proposal_name(&source.metadata.name, revision),
                namespace: namespace.to_owned(),
                revision: revision.to_owned(),
                envelope: Value::Null,
                files: BTreeMap::new(),
                removed: Vec::new(),
                auto_merge: false,
                rejected,
            }),
            reason: "the import gates refused a resource of the source",
        };
    }

    // A oneshot is copied once and then detached (Architecture/06 section 6): every run that
    // reached the source is that once.
    next.paused = state.paused || spec.mode == SyncMode::Oneshot;

    if changes.is_empty() {
        // The source moved, but nothing it carries changes this repository. Recording the
        // revision is what stops the next tick from fetching it all over again.
        next.observed_revision = Some(revision.to_owned());
        return Run {
            phase: Phase::Synced,
            state: next,
            proposal: None,
            reason: "the source moved but changes nothing in this repository",
        };
    }

    let changeset = changeset_of(changes);
    let verdict = lane_of_changeset(&changeset);
    let name = proposal_name(&source.metadata.name, revision);
    // `autoMerge` on anything but green is a merge without the approvals its own lane
    // demands. The flag is not ignored, it is refused by the lane (MF-29, CC-70).
    let auto_merge = spec.auto_merge && verdict.lane == Lane::Green;

    // The revision is not observed yet: the source is imported when the proposal merges, not
    // when it is written. A run that recorded it here would leave the repository one revision
    // behind for good if the proposal were closed unmerged.
    next.open_proposal = Some(name.clone());

    Run {
        phase: Phase::PendingApproval,
        state: next,
        proposal: Some(Proposal {
            envelope: change_envelope(&changeset, &name, namespace, None),
            name,
            namespace: namespace.to_owned(),
            revision: revision.to_owned(),
            files,
            removed,
            auto_merge,
            rejected,
        }),
        reason: "the source moved and the plan is not empty",
    }
}

/// Marks a manifest as belonging to this sync source (MF-08, MF-27).
fn stamp(manifest: &mut RawManifest, owner: &str) {
    let annotations = manifest
        .metadata
        .rest
        .entry("annotations")
        .or_insert_with(|| Value::Object(Map::new()));
    if let Value::Object(annotations) = annotations {
        annotations.insert(SYNC_SOURCE.to_owned(), json!(owner));
    }
}

/// The resources this sync source put in the repository that the current revision no longer
/// carries (CC-19).
///
/// Scoped by the annotation rather than by the namespace: a project holds hand-authored
/// resources beside synced ones, and `prune` must never reach them.
fn dropped(repo_dir: &Path, owner: &str, report: &Report) -> Vec<(ResourceId, PathBuf)> {
    let Ok(repo) = Repository::load(repo_dir) else {
        return Vec::new();
    };
    let carried: Vec<ResourceId> = report
        .imported
        .iter()
        .map(|imported| ResourceId::from_manifest(&imported.manifest))
        .collect();

    repo.iter()
        .filter(|(_, resource)| {
            resource
                .manifest
                .metadata
                .rest
                .get("annotations")
                .and_then(Value::as_object)
                .and_then(|annotations| annotations.get(SYNC_SOURCE))
                .and_then(Value::as_str)
                == Some(owner)
        })
        .filter(|(id, _)| !carried.contains(id))
        .map(|(id, resource)| (id.clone(), resource.path.clone()))
        .collect()
}

/// The changes grouped into the waves `apply` would run them in, which is the shape the lane
/// rules and the `kind: Change` envelope already read (CC-18).
fn changeset_of(changes: Vec<ResourceChange>) -> ChangeSet {
    let mut waves: BTreeMap<u8, Vec<ResourceChange>> = BTreeMap::new();
    for change in changes {
        let wave = wave_of(&change.id.kind).unwrap_or(u8::MAX);
        waves.entry(wave).or_default().push(change);
    }
    ChangeSet {
        waves: waves.into_iter().collect(),
        ..ChangeSet::default()
    }
}
