//! `jcctl sync`: one run of a `SyncSource`'s loop, outside the Portal (MF-27…MF-34).
//!
//! The loop itself lives in [`crate::sync`] and the Portal drives it; this is the other caller
//! Architecture/06 section 6 promises, for a CI job or an operator with a checkout in hand. The
//! crate stays transport-free: the origin is a directory the caller filled (`--checkout`), the
//! way the Portal hands [`crate::sync::poll_now`] a staged tree.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use crate::loader::{RawManifest, Repository};
use crate::sync::{self, RemoteError, Run, State, SyncRemote};
use jc_core::kinds::SyncOrigin;

/// What one CLI run was told.
#[derive(Debug, Clone)]
pub struct Options {
    /// `<project>/<name>` of the `SyncSource` to run.
    pub source: String,
    /// The origin, already materialised by the caller.
    pub checkout: PathBuf,
    /// Where the run's state is kept between calls; an absent file is a first run.
    pub state: Option<PathBuf>,
    /// Run whatever the schedule says (`--once`), rather than only when it is due.
    pub once: bool,
}

/// Why a run could not be made.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The repository did not load.
    #[error("{0}")]
    Repository(String),
    /// No `SyncSource` of that name in that project.
    #[error("no SyncSource '{0}' in this repository")]
    NoSuchSource(String),
    /// `--source` is not `<project>/<name>`.
    #[error("--source must be <project>/<name>, not '{0}'")]
    BadSource(String),
    /// The state file could not be read or written.
    #[error("sync state at {path}: {reason}")]
    State {
        /// The file that could not be read or written.
        path: String,
        /// What the filesystem said.
        reason: String,
    },
    /// The loop refused the run.
    #[error("{0}")]
    Sync(String),
}

/// The origin as a directory the caller filled: its revision is the digest of what it holds, so
/// a second run over an unchanged checkout recognises it and writes nothing (MF-28).
struct Checkout<'a> {
    dir: &'a Path,
}

impl SyncRemote for Checkout<'_> {
    fn revision(&self, _origin: &SyncOrigin) -> Result<String, RemoteError> {
        let mut files: Vec<(String, Vec<u8>)> = Vec::new();
        collect(self.dir, self.dir, &mut files)
            .map_err(|error| RemoteError::Unavailable(error.to_string()))?;
        files.sort_by(|a, b| a.0.cmp(&b.0));
        use sha2::{Digest, Sha256};
        let mut hasher = Sha256::new();
        for (path, body) in files {
            hasher.update(path.as_bytes());
            hasher.update([0]);
            hasher.update(&body);
        }
        Ok(format!("{:x}", hasher.finalize())[..40].to_owned())
    }

    fn checkout(
        &self,
        _origin: &SyncOrigin,
        _revision: &str,
        into: &Path,
    ) -> Result<(), RemoteError> {
        let mut files: Vec<(String, Vec<u8>)> = Vec::new();
        collect(self.dir, self.dir, &mut files)
            .map_err(|error| RemoteError::Unavailable(error.to_string()))?;
        for (path, body) in files {
            let target = into.join(&path);
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|error| RemoteError::Unavailable(error.to_string()))?;
            }
            std::fs::write(&target, body)
                .map_err(|error| RemoteError::Unavailable(error.to_string()))?;
        }
        Ok(())
    }
}

/// Every file under `dir`, by its path relative to `root`. A symlink is read as what it points
/// at, and a directory that climbs out of the checkout cannot be reached at all: the walk only
/// ever descends.
fn collect(root: &Path, dir: &Path, into: &mut Vec<(String, Vec<u8>)>) -> std::io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let kind = std::fs::metadata(&path)?;
        if kind.is_dir() {
            if entry.file_name().to_string_lossy().starts_with('.') {
                continue;
            }
            collect(root, &path, into)?;
            continue;
        }
        let relative = path
            .strip_prefix(root)
            .map_err(|_| std::io::Error::other("path escaped the checkout"))?
            .to_string_lossy()
            .into_owned();
        into.push((relative, std::fs::read(&path)?));
    }
    Ok(())
}

/// Runs one tick and answers what it decided (MF-28, MF-34).
pub fn run(repo_dir: &Path, options: &Options) -> Result<Run, Error> {
    let (project, name) = options
        .source
        .split_once('/')
        .ok_or_else(|| Error::BadSource(options.source.clone()))?;
    let repository =
        Repository::load(repo_dir).map_err(|error| Error::Repository(error.to_string()))?;
    let source = find(&repository, project, name)
        .ok_or_else(|| Error::NoSuchSource(options.source.clone()))?;

    let state = read_state(options.state.as_deref())?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|since| since.as_secs())
        .unwrap_or_default();
    let workspace = workspace_for(&options.source, now);
    std::fs::create_dir_all(&workspace).map_err(|error| Error::State {
        path: workspace.display().to_string(),
        reason: error.to_string(),
    })?;

    let remote = Checkout {
        dir: &options.checkout,
    };
    let run = if options.once {
        sync::poll_now(&source, &state, now, repo_dir, &workspace, &remote)
    } else {
        sync::poll(&source, &state, now, repo_dir, &workspace, &remote)
    }
    .map_err(|error| Error::Sync(error.to_string()));
    let _ = std::fs::remove_dir_all(&workspace);
    let run = run?;

    write_state(options.state.as_deref(), &run.state)?;
    Ok(run)
}

/// What the command prints: the `kind: Change` envelope of the proposal, or what the run did
/// instead of proposing anything.
pub fn report(run: &Run) -> Value {
    match &run.proposal {
        Some(proposal) => proposal.envelope.clone(),
        None => json!({
            "phase": format!("{:?}", run.phase),
            "reason": run.reason,
            "observedRevision": run.state.observed_revision,
        }),
    }
}

fn find(repository: &Repository, project: &str, name: &str) -> Option<RawManifest> {
    repository
        .iter()
        .map(|(_, loaded)| &loaded.manifest)
        .find(|manifest| {
            manifest.kind == "SyncSource"
                && manifest.metadata.name == name
                && manifest.metadata.namespace.as_deref() == Some(project)
        })
        .cloned()
}

/// A scratch directory of this run alone. The process, a nanosecond clock and a counter name it:
/// two runs of one source in the same second shared `{source}-{now}`, and whichever finished
/// first removed it under the other (T-2240).
fn workspace_for(source: &str, now: u64) -> PathBuf {
    static RUNS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let stem: String = source
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|since| since.subsec_nanos())
        .unwrap_or_default();
    let run = RUNS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let pid = std::process::id();
    std::env::temp_dir().join(format!("jcctl-sync-{stem}-{now}-{pid}-{nanos}-{run}"))
}

fn read_state(path: Option<&Path>) -> Result<State, Error> {
    let Some(path) = path else {
        return Ok(State::default());
    };
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(State::default()),
        Err(error) => {
            return Err(Error::State {
                path: path.display().to_string(),
                reason: error.to_string(),
            })
        }
    };
    let stored: BTreeMap<String, Value> =
        serde_json::from_str(&text).map_err(|error| Error::State {
            path: path.display().to_string(),
            reason: error.to_string(),
        })?;
    Ok(State {
        observed_revision: stored
            .get("observedRevision")
            .and_then(Value::as_str)
            .map(str::to_owned),
        last_run_at: stored.get("lastRunAt").and_then(Value::as_u64),
        open_proposal: stored
            .get("openProposal")
            .and_then(Value::as_str)
            .map(str::to_owned),
        paused: stored
            .get("paused")
            .and_then(Value::as_bool)
            .unwrap_or(false),
    })
}

fn write_state(path: Option<&Path>, state: &State) -> Result<(), Error> {
    let Some(path) = path else {
        return Ok(());
    };
    let body = json!({
        "observedRevision": state.observed_revision,
        "lastRunAt": state.last_run_at,
        "openProposal": state.open_proposal,
        "paused": state.paused,
    });
    std::fs::write(path, format!("{body:#}\n")).map_err(|error| Error::State {
        path: path.display().to_string(),
        reason: error.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::workspace_for;

    /// TS-12, T-2240: two runs of one source in the same second (the CLI beside a test, two
    /// terminals, a retry) each get their own scratch directory. A shared one was removed under
    /// the run still using it, which exited 1 before the run reached its answer.
    #[test]
    fn two_runs_of_one_source_in_the_same_second_get_their_own_workspace() {
        let first = workspace_for("ovzdusie/regional", 1_758_450_000);
        let second = workspace_for("ovzdusie/regional", 1_758_450_000);
        assert_ne!(first, second);
        assert!(first
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with("jcctl-sync-ovzdusie-regional-")));
    }
}
