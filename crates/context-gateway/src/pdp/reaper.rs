//! Revocation reaper: the running gateway follows the repository (T-0177, R48, EP-19, OPS-45).
//!
//! Everything the enforcement point decides with — the endpoint table, the policies of each
//! endpoint, the service accounts a token's `azp` may name — is a projection of the manifest
//! repository, read once at start-up. That is a cache, and R48 says a `Policy` change has to
//! reach it within a bounded time: until this module existed, revoking a grant meant
//! restarting the pod, and an emergency runbook that restarts pods is not a 5-second bound.
//!
//! The reaper is a poll, not a push, and deliberately so: the repository arrives as a
//! ConfigMap volume, a git-sync sidecar checkout or a plain directory, and none of those
//! sends an event. Every replica polls its own copy, so all of them converge inside one
//! interval without talking to each other and without an admin endpoint an attacker could
//! reach. The interval is one second, under the two the endpoint table is allowed (EP-19)
//! and well under the five a revocation has (OPS-45).
//!
//! A reload that fails changes nothing: the old table keeps serving. A repository that is
//! being written to (half-synced checkout, ConfigMap mid-swap) is briefly unreadable or
//! incomplete, and answering 404 for every endpoint because of it would be a worse outage
//! than serving a table one second old.

use crate::app::Gateway;
use crate::store;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

/// How often the repository is re-read. Under EP-19's two seconds and OPS-45's five.
pub const INTERVAL: Duration = Duration::from_secs(1);

/// Watches one repository directory and swaps the gateway's tables when it changes.
pub struct Reaper {
    gateway: Arc<Gateway>,
    dir: PathBuf,
    previews: Option<PathBuf>,
    seen: Option<Fingerprint>,
}

/// What "the repository changed" means without reading every file: the set of manifest
/// paths with their sizes and modification times.
///
/// Content the loader ignores never triggers a reload, and a change the file system does
/// not record (same size, same mtime, different bytes) is not distinguished — a rewrite
/// that fine-grained is a `touch` away from being seen, and the alternative is hashing
/// every file every second for a case the reconciler does not produce.
type Fingerprint = Vec<(PathBuf, u64, Option<std::time::SystemTime>)>;

impl Reaper {
    /// A reaper for the repository the gateway was loaded from.
    pub fn new(gateway: Arc<Gateway>, dir: impl Into<PathBuf>) -> Self {
        let dir = dir.into();
        Self {
            seen: fingerprint(&dir),
            gateway,
            dir,
            previews: None,
        }
    }

    /// Follows the workspace previews under `dir` too (Architecture/06 §7.2).
    pub fn with_previews(mut self, dir: impl Into<PathBuf>) -> Self {
        let dir = dir.into();
        self.seen = combined(&self.dir, Some(&dir));
        self.previews = Some(dir);
        self
    }

    /// Re-reads the repository if it changed, and swaps both tables if it loaded.
    ///
    /// Returns whether anything was swapped, which is what the tests assert on and what
    /// the log line reports.
    pub fn tick(&mut self) -> bool {
        let current = combined(&self.dir, self.previews.as_deref());
        if current.is_none() {
            // The directory went away: keep serving, say so once per tick.
            tracing::warn!(dir = %self.dir.display(), "the manifest repository is unreadable");
            return false;
        }
        if current == self.seen {
            return false;
        }
        match store::load_with_previews(&self.dir, self.previews.as_deref()) {
            Ok((endpoints, spaces, accounts, federations, agreements)) => {
                let counts = (endpoints.len(), spaces.len(), accounts.len());
                // The endpoint table carries the policies, so replacing it purges every
                // grant the PDP would have honoured; the space table carries the same
                // policies for the `/cs` surface and has to be swapped with it, or a
                // withdrawn grant would still be honoured there; the accounts table is
                // what a token's `azp` resolves through, so a withdrawn credential stops
                // resolving here; the federation table decides whether a read over a space
                // can be served at all, so a registration that changed identity mode takes
                // effect with the rest and not one reload later (PF-48); the agreement table
                // is what a transfer token is checked against (DS-12).
                self.gateway.resolver.replace(endpoints);
                self.gateway.resolver.replace_spaces(spaces);
                self.gateway.replace_accounts(accounts);
                self.gateway.replace_federation(federations);
                // DS-12: an agreement that ended stops authorising reads here in the same
                // reconcile that withdraws the `Policy` entities compiled from it, so an
                // outstanding transfer token names an agreement nobody serves any more.
                self.gateway.replace_agreements(agreements);
                self.seen = current;
                tracing::info!(
                    endpoints = counts.0,
                    spaces = counts.1,
                    accounts = counts.2,
                    dir = %self.dir.display(),
                    "repository reloaded, policy caches purged"
                );
                true
            }
            Err(error) => {
                // A repository mid-write is unreadable for a moment. The fingerprint is
                // not stored, so the next tick tries the same change again.
                tracing::warn!(%error, dir = %self.dir.display(), "the repository did not reload");
                false
            }
        }
    }

    /// Polls until the process ends. Started once, next to the server.
    pub async fn run(mut self) {
        let mut ticker = tokio::time::interval(INTERVAL);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            ticker.tick().await;
            self.tick();
        }
    }
}

/// The manifest files under `dir` with their sizes and modification times, or `None` when
/// the directory cannot be read at all.
/// The repository's fingerprint with the previews' after it; previews that cannot be read
/// count as none, so the repository alone still reloads.
fn combined(dir: &Path, previews: Option<&Path>) -> Option<Fingerprint> {
    let mut all = fingerprint(dir)?;
    if let Some(more) = previews.and_then(fingerprint) {
        all.extend(more);
    }
    Some(all)
}

fn fingerprint(dir: &Path) -> Option<Fingerprint> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(current) = stack.pop() {
        let entries = std::fs::read_dir(&current).ok()?;
        for entry in entries.flatten() {
            let path = entry.path();
            // `..data` and friends: a ConfigMap volume keeps its versions in dot
            // directories and the loader skips them, so the fingerprint does too.
            if entry
                .file_name()
                .to_str()
                .is_some_and(|name| name.starts_with('.'))
            {
                continue;
            }
            let Ok(meta) = std::fs::metadata(&path) else {
                continue;
            };
            if meta.is_dir() {
                stack.push(path);
            } else {
                out.push((path, meta.len(), meta.modified().ok()));
            }
        }
    }
    out.sort();
    Some(out)
}
