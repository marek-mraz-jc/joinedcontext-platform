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
    checkouts: Option<store::Checkouts>,
    previews: Option<PathBuf>,
    seen: Option<Fingerprint>,
    walked: usize,
}

/// What "the repository changed" means without reading every file: the set of manifest
/// paths with their sizes and modification times.
///
/// Content the loader ignores never triggers a reload, and a change the file system does
/// not record (same size, same mtime, different bytes) is not distinguished — a rewrite
/// that fine-grained is a `touch` away from being seen, and the alternative is hashing
/// every file every second for a case the reconciler does not produce.
///
/// A tree linked in by commit is its link instead (T-4444): git-sync's `current` and each
/// `{projects}/{slug}` of `jcctl checkouts` point at a directory named by its commit, written
/// once and swapped in by one rename, so the link's target says all its files would. At
/// 10,000 Endpoints the walk statted some 31,000 files a second; the links are a hundred reads.
type Fingerprint = Vec<(PathBuf, u64, Option<std::time::SystemTime>)>;

impl Reaper {
    /// A reaper for the repository the gateway was loaded from.
    pub fn new(gateway: Arc<Gateway>, dir: impl Into<PathBuf>) -> Self {
        let dir = dir.into();
        let mut reaper = Self {
            seen: None,
            gateway,
            dir,
            checkouts: None,
            previews: None,
            walked: 0,
        };
        reaper.seen = reaper.fingerprint();
        reaper
    }

    /// Follows the project checkouts of a layout 2 organization too: a project added to or
    /// removed from the registry, and a project checkout moving to another commit, reload the
    /// tables like a change of the organization does (CC-86).
    pub fn with_checkouts(mut self, checkouts: store::Checkouts) -> Self {
        self.checkouts = Some(checkouts);
        self.seen = self.fingerprint();
        self
    }

    /// Follows the workspace previews under `dir` too (Architecture/06 §7.2).
    pub fn with_previews(mut self, dir: impl Into<PathBuf>) -> Self {
        self.previews = Some(dir.into());
        self.seen = self.fingerprint();
        self
    }

    /// Re-reads the repository if it changed, and swaps both tables if it loaded.
    ///
    /// Returns whether anything was swapped, which is what the tests assert on and what
    /// the log line reports.
    pub fn tick(&mut self) -> bool {
        let current = self.fingerprint();
        if current.is_none() {
            // The directory went away: keep serving, say so once per tick.
            tracing::warn!(dir = %self.dir.display(), "the manifest repository is unreadable");
            return false;
        }
        if current == self.seen {
            return false;
        }
        match store::load_with_previews(
            &self.dir,
            self.checkouts.as_ref(),
            self.previews.as_deref(),
        ) {
            Ok((
                endpoints,
                spaces,
                accounts,
                federations,
                agreements,
                limits,
                servers,
                homes,
                brokers,
                mappings,
            )) => {
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
                // ADR-N-035: a changed body limit holds from the next request on.
                self.gateway.replace_limits(limits);
                // ADR-N-043: a named server's members follow the manifest at once.
                self.gateway.replace_servers(servers);
                // EP-53: a Mapping withdrawn from the repository stops being published.
                self.gateway.replace_mappings(mappings);
                // ADR-N-047: a space follows its broker from the next request on.
                self.gateway.broker.route(&homes);
                // ADR-N-048: a broker's roles and its pipelines' topics hold from the next
                // CONNECT on; open connections keep the ACL they got until their token expires.
                self.gateway.replace_brokers(brokers);
                self.seen = current;
                tracing::info!(
                    endpoints = counts.0,
                    spaces = counts.1,
                    accounts = counts.2,
                    // What each idle tick stats: a link per linked checkout, else every file.
                    files_walked = self.files_walked(),
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

    /// The files the last fingerprint statted one by one: what an idle tick costs (T-4444).
    pub fn files_walked(&self) -> usize {
        self.walked
    }

    /// The organization's fingerprint, then the project checkouts', then the previews'.
    fn fingerprint(&mut self) -> Option<Fingerprint> {
        self.walked = 0;
        let mut all = linked(&self.dir)
            .map(|link| vec![link])
            .or_else(|| fingerprint(&self.dir, &mut self.walked))?;
        if let Some(projects) = self.checkouts.as_ref().map(|c| c.projects.clone()) {
            if let Some(more) = checkouts(&projects, &mut self.walked) {
                all.extend(more);
            }
        }
        if let Some(more) = self
            .previews
            .clone()
            .and_then(|dir| fingerprint(&dir, &mut self.walked))
        {
            all.extend(more);
        }
        Some(all)
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

/// `dir` as the commit it links to, when it is a link: a tree swapped in whole (git-sync's
/// `current`), never written in place.
fn linked(dir: &Path) -> Option<(PathBuf, u64, Option<std::time::SystemTime>)> {
    let target = std::fs::read_link(dir).ok()?;
    Some((dir.join(target), 0, None))
}

/// The project checkouts under `projects`: each `{slug}` link by its target, and a project
/// directory that is not a link (a test, a hand-made tree) walked file by file.
fn checkouts(projects: &Path, walked: &mut usize) -> Option<Fingerprint> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(projects).ok()?.flatten() {
        if entry
            .file_name()
            .to_str()
            .is_some_and(|name| name.starts_with('.'))
        {
            continue;
        }
        let path = entry.path();
        match linked(&path) {
            Some(link) => out.push(link),
            None => out.extend(fingerprint(&path, walked).unwrap_or_default()),
        }
    }
    out.sort();
    Some(out)
}

/// The manifest files under `dir` with their sizes and modification times, or `None` when
/// the directory cannot be read at all. Checkouts or previews that cannot be read count as
/// none, so the organization alone still reloads.
fn fingerprint(dir: &Path, walked: &mut usize) -> Option<Fingerprint> {
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
            *walked += 1;
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
