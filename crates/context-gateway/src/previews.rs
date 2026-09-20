//! Workspace previews served beside `main` (CC-78, PF-83, Architecture/06 §7.2).
//!
//! The Portal holds the render of every running preview and lists it on its internal
//! listener: the prefix and the files of the workspace's branch. The poller writes each entry
//! into a directory of its own under the previews directory, named by its prefix, and the
//! reaper loads that directory through `Repository::load_preview` beside the repository. The
//! prefix is the isolation: every space, id and slug of a preview is its own, so a preview
//! cannot answer for, or write into, anything of `main`.

use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet};
use std::hash::{Hash, Hasher};
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

/// How often the Portal is asked. A preview is a person trying something, not a revocation.
pub const INTERVAL: Duration = Duration::from_secs(10);

/// One running preview as the Portal lists it.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Preview {
    /// `ws-{name}-`, what the loader puts in front of every organization-unique name.
    pub prefix: String,
    /// The branch's files by their path in the repository.
    pub files: BTreeMap<String, String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct List {
    items: Vec<Preview>,
}

/// A prefix the loader renders: `ws-{name}-`, lowercase letters, digits and hyphens.
pub fn valid_prefix(prefix: &str) -> bool {
    (5..=64).contains(&prefix.len())
        && prefix.starts_with("ws-")
        && prefix.ends_with('-')
        && prefix
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

/// A path inside the preview's directory, or `None` for one that would leave it.
fn inside(root: &Path, relative: &str) -> Option<PathBuf> {
    let path = Path::new(relative);
    let plain = path.components().count() > 0
        && path
            .components()
            .all(|part| matches!(part, Component::Normal(name) if !name.to_string_lossy().starts_with('.')));
    plain.then(|| root.join(path))
}

/// The preview directories under `dir` with their prefixes; a name that is no prefix is left.
pub fn listed(dir: &Path) -> Vec<(String, PathBuf)> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut found: Vec<(String, PathBuf)> = entries
        .flatten()
        .filter(|entry| entry.path().is_dir())
        .filter_map(|entry| {
            let name = entry.file_name().to_str()?.to_owned();
            valid_prefix(&name).then(|| (name, entry.path()))
        })
        .collect();
    found.sort();
    found
}

/// Brings `dir` to what the Portal listed: a changed preview is rewritten beside and swapped
/// in by a rename, so the reaper never reads half of one; a preview no longer listed goes.
pub struct Mirror {
    dir: PathBuf,
    seen: BTreeMap<String, u64>,
}

impl Mirror {
    /// A mirror that writes under `dir`.
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self {
            dir: dir.into(),
            seen: BTreeMap::new(),
        }
    }

    /// Writes what the Portal listed and removes what it no longer lists.
    pub fn apply(&mut self, previews: Vec<Preview>) -> std::io::Result<()> {
        std::fs::create_dir_all(&self.dir)?;
        let mut wanted = BTreeSet::new();
        for preview in previews {
            if !valid_prefix(&preview.prefix) {
                tracing::warn!(prefix = %preview.prefix, "a preview with no valid prefix is left out");
                continue;
            }
            wanted.insert(preview.prefix.clone());
            let digest = digest(&preview.files);
            let target = self.dir.join(&preview.prefix);
            if self.seen.get(&preview.prefix) == Some(&digest) && target.is_dir() {
                continue;
            }
            let staging = self.dir.join(format!(".{}", preview.prefix));
            let _ = std::fs::remove_dir_all(&staging);
            for (relative, text) in &preview.files {
                let Some(path) = inside(&staging, relative) else {
                    tracing::warn!(prefix = %preview.prefix, path = %relative, "a file outside the preview is left out");
                    continue;
                };
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                std::fs::write(path, text)?;
            }
            std::fs::create_dir_all(&staging)?;
            let _ = std::fs::remove_dir_all(&target);
            std::fs::rename(&staging, &target)?;
            self.seen.insert(preview.prefix, digest);
        }
        for (prefix, path) in listed(&self.dir) {
            if !wanted.contains(&prefix) {
                std::fs::remove_dir_all(path)?;
                self.seen.remove(&prefix);
            }
        }
        Ok(())
    }

    /// Asks the Portal every [`INTERVAL`]. A failed call changes nothing: the previews last
    /// listed keep answering until the Portal says otherwise.
    ///
    /// `token` is the gateway's own workload identity (PF-46, AG-52). Without one the Portal's
    /// internal listener refuses the call, which is what an instance that configured no client
    /// looks like: previews stop being served and a line says why, rather than the listener
    /// answering anybody who reaches the port.
    pub async fn follow(mut self, url: String, token: Option<std::sync::Arc<WorkloadToken>>) {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(20))
            .build()
            .expect("a client with a timeout builds");
        let mut ticker = tokio::time::interval(INTERVAL);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            ticker.tick().await;
            let mut request = client.get(&url);
            if let Some(token) = token.as_ref() {
                match token.get().await {
                    Ok(bearer) => request = request.bearer_auth(bearer),
                    Err(error) => {
                        tracing::warn!(%error, "no token for the Portal's preview list");
                        continue;
                    }
                }
            }
            let listed = match request.send().await {
                Ok(response) if response.status().is_success() => response.json::<List>().await,
                Ok(response) => {
                    tracing::warn!(status = %response.status(), "the Portal did not list the previews");
                    continue;
                }
                Err(error) => {
                    tracing::warn!(%error, "the Portal did not list the previews");
                    continue;
                }
            };
            match listed {
                Ok(list) => {
                    if let Err(error) = self.apply(list.items) {
                        tracing::warn!(%error, dir = %self.dir.display(), "the previews were not written");
                    }
                }
                Err(error) => tracing::warn!(%error, "the preview list is not readable"),
            }
        }
    }
}

/// The gateway's own identity when it asks the Portal for the previews (PF-46, AG-52).
///
/// `client_credentials` on the gateway's confidential client, cached until shortly before it
/// expires. The token is audience-bound to the Portal's internal listener by an audience mapper on
/// that client, so it opens this one route and nothing else — and the Portal refuses the call
/// without it, which is the whole point: the NetworkPolicy on port 9090 is the second control.
pub struct WorkloadToken {
    token_url: String,
    client_id: String,
    client_secret: String,
    http: reqwest::Client,
    held: tokio::sync::Mutex<Option<(std::time::Instant, String)>>,
}

/// How long before expiry a held token is replaced, so a call is never made with one that dies
/// on the way.
const EARLY: Duration = Duration::from_secs(30);

impl WorkloadToken {
    /// `token_url` is the realm's token endpoint, which the deployment names
    /// (`Config::token_url`): the issuer is the address a browser uses, and on a single-node
    /// cluster a pod dialling that public hostname reaches nothing.
    pub fn new(token_url: String, client_id: String, client_secret: String) -> Self {
        Self {
            token_url,
            client_id,
            client_secret,
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(5))
                .build()
                .expect("a client with a timeout builds"),
            held: tokio::sync::Mutex::new(None),
        }
    }

    /// The held token while it lasts, else one fetched now.
    pub async fn get(&self) -> Result<String, String> {
        let mut held = self.held.lock().await;
        if let Some((expires, token)) = held.as_ref() {
            if *expires > std::time::Instant::now() + EARLY {
                return Ok(token.clone());
            }
        }
        let (token, lifetime) = self.fetch().await?;
        *held = Some((std::time::Instant::now() + lifetime, token.clone()));
        Ok(token)
    }

    async fn fetch(&self) -> Result<(String, Duration), String> {
        #[derive(Deserialize)]
        struct Granted {
            access_token: String,
            #[serde(default)]
            expires_in: u64,
        }
        let answer = self
            .http
            .post(&self.token_url)
            .form(&[
                ("grant_type", "client_credentials"),
                ("client_id", self.client_id.as_str()),
                ("client_secret", self.client_secret.as_str()),
            ])
            .send()
            .await
            .map_err(|error| error.to_string())?;
        if !answer.status().is_success() {
            // The status and never the body: a token endpoint's error body can carry the grant
            // back, and this line is read by whoever holds the logs.
            return Err(format!("the realm refused the grant: {}", answer.status()));
        }
        let granted: Granted = answer.json().await.map_err(|error| error.to_string())?;
        // Exactly the lifetime the realm named, and nothing invented on top of it: a token held
        // past its expiry is a poll the Portal refuses. A realm that names none leaves the held
        // token stale at once, so the next poll asks again.
        Ok((
            granted.access_token,
            Duration::from_secs(granted.expires_in),
        ))
    }
}

fn digest(files: &BTreeMap<String, String>) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    files.hash(&mut hasher);
    hasher.finish()
}
