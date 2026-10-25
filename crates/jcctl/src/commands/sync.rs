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
        let files = collect(self.dir)?;
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
        let files = collect(self.dir)?;
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

/// Every file under `root`, by its path relative to it, through the loader's contained walk: a
/// link that resolves outside the checkout is refused and a link loop is an error, so a remote
/// checkout can neither pull a host file into the workspace nor recurse without end (CC-08,
/// T-5701).
fn collect(root: &Path) -> Result<Vec<(String, Vec<u8>)>, RemoteError> {
    crate::loader::walk_files(root)
        .map_err(|error| RemoteError::Unavailable(error.to_string()))?
        .into_iter()
        .map(|entry| {
            let relative = entry
                .path()
                .strip_prefix(root)
                .map_err(|_| RemoteError::Unavailable("path escaped the checkout".to_owned()))?
                .to_string_lossy()
                .into_owned();
            let body = std::fs::read(entry.path())
                .map_err(|error| RemoteError::Unavailable(error.to_string()))?;
            Ok((relative, body))
        })
        .collect()
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

    /// T-5701: a checkout is read through the contained walk. A link to a host file outside it
    /// is refused, not copied into the workspace, and a link back to a parent is an error
    /// instead of a walk without end.
    #[cfg(unix)]
    #[test]
    fn a_checkout_link_out_of_the_tree_or_into_a_loop_is_refused() {
        use super::{Checkout, SyncOrigin, SyncRemote};
        let base = std::env::temp_dir().join(format!(
            "jcctl-sync-links-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|since| since.as_nanos())
                .unwrap_or_default()
        ));
        let outside = base.join("outside");
        let escape = base.join("escape");
        let looped = base.join("looped");
        for dir in [&outside, &escape, &looped] {
            std::fs::create_dir_all(dir).expect("a scratch dir");
        }
        std::fs::write(outside.join("host-secret"), "not yours").expect("a host file");
        std::fs::write(escape.join("a.yaml"), "kind: Space\n").expect("a manifest");
        std::os::unix::fs::symlink(outside.join("host-secret"), escape.join("x.yaml"))
            .expect("a link out");
        std::fs::write(looped.join("a.yaml"), "kind: Space\n").expect("a manifest");
        std::os::unix::fs::symlink("..", looped.join("loop")).expect("a loop");

        let origin = SyncOrigin::default();
        let into = base.join("into");
        let escaped = Checkout { dir: &escape }.checkout(&origin, "r", &into);
        assert!(escaped.is_err(), "a link out of the checkout is refused");
        assert!(
            !into.join("x.yaml").exists(),
            "nothing of the host is written"
        );
        assert!(Checkout { dir: &escape }.revision(&origin).is_err());
        assert!(
            Checkout { dir: &looped }.revision(&origin).is_err(),
            "a link loop is an error, not a walk without end"
        );
        std::fs::remove_dir_all(&base).expect("the scratch dir goes");
    }
}
