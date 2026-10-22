//! `jcctl changelog --repo-dir <checkout> [--days 30] [--format md|json]`: the public changelog
//! of a project's Endpoints (EP-148, EP-149, API/10 §4).
//!
//! The history is the checkout's first-parent log: on `main` that is one commit per merged
//! change. For each commit the manifests it touched are read before and after, and an Endpoint
//! public on either side gets an entry. What changed is computed from the two manifests, in a
//! consumer's terms; a commit message, an author, a policy or a role never reaches the page.

use chrono::{DateTime, NaiveDate, Utc};
use jc_core::kinds::endpoint_deprecation::Deprecation;
use jc_core::kinds::{Audience, EndpointSpec, Representation};
use serde::Serialize;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::process::Command;

/// The widest window a changelog reads.
pub const MAX_DAYS: u32 = 366;

/// One change of one public Endpoint (EP-149: nothing else).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Entry {
    /// The day the change merged, UTC.
    pub day: NaiveDate,
    /// The Endpoint's `metadata.name`.
    pub endpoint: String,
    /// The Endpoint's title, English first.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// The Endpoint's public address, a path under the API host.
    pub address: String,
    /// The surfaces the change reaches.
    pub surfaces: Vec<Representation>,
    /// What changed, one sentence each.
    pub changes: Vec<String>,
}

/// What `jcctl changelog` was asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Options {
    /// The project checkout.
    pub repo_dir: std::path::PathBuf,
    /// The window, 1..=[`MAX_DAYS`] days back from now.
    pub days: u32,
    /// JSON instead of the Markdown page.
    pub json: bool,
}

impl Options {
    /// `--repo-dir <path> [--days <n>] [--format md|json]`; `None` for anything else.
    pub fn parse(mut args: &[&str]) -> Option<Self> {
        let (mut repo_dir, mut days, mut json) = (None, 30, false);
        while let [flag, value, rest @ ..] = args {
            match *flag {
                "--repo-dir" => repo_dir = Some(std::path::PathBuf::from(value)),
                "--days" => days = value.parse().ok().filter(|d| (1..=MAX_DAYS).contains(d))?,
                "--format" => {
                    json = match *value {
                        "md" => false,
                        "json" => true,
                        _ => return None,
                    }
                }
                _ => return None,
            }
            args = rest;
        }
        args.is_empty().then_some(())?;
        Some(Self {
            repo_dir: repo_dir?,
            days,
            json,
        })
    }

    /// The page, or its JSON, for the window ending at `now`.
    pub fn render(&self, now: DateTime<Utc>) -> Result<String, ChangelogError> {
        let since = now - chrono::Duration::days(i64::from(self.days));
        let list = entries(&self.repo_dir, since)?;
        Ok(match self.json {
            true => serde_json::to_string_pretty(&list).unwrap_or_default(),
            false => markdown(&list, self.days),
        })
    }
}

/// Why no changelog was written.
#[derive(Debug, thiserror::Error)]
pub enum ChangelogError {
    /// A git command failed.
    #[error("git {args}: {message}")]
    Git {
        /// The arguments: refs and paths of the checkout, no secret.
        args: String,
        /// What git said.
        message: String,
    },
}

fn git(dir: &Path, args: &[&str]) -> Result<String, ChangelogError> {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .map_err(|err| ChangelogError::Git {
            args: args.join(" "),
            message: err.to_string(),
        })?;
    if !output.status.success() {
        return Err(ChangelogError::Git {
            args: args.join(" "),
            message: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        });
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// The file at `rev`, or `None` where it does not exist there.
fn file_at(dir: &Path, rev: &str, path: &str) -> Option<String> {
    git(dir, &["show", &format!("{rev}:{path}")]).ok()
}

/// Every public Endpoint change merged on the checkout's first-parent history since `since`,
/// newest first.
pub fn entries(repo: &Path, since: DateTime<Utc>) -> Result<Vec<Entry>, ChangelogError> {
    let log = git(
        repo,
        &[
            "log",
            "--first-parent",
            "--format=%H %P%x09%cI",
            &format!("--since={}", since.to_rfc3339()),
            "HEAD",
        ],
    )?;
    let mut out = Vec::new();
    for line in log.lines() {
        let Some((shas, date)) = line.split_once('\t') else {
            continue;
        };
        let mut shas = shas.split(' ');
        let commit = shas.next().unwrap_or_default();
        let parent = shas.next().filter(|p| !p.is_empty());
        let Ok(day) = DateTime::parse_from_rfc3339(date.trim()) else {
            continue;
        };
        let day = day.with_timezone(&Utc).date_naive();
        let changed = match parent {
            Some(parent) => git(
                repo,
                &["diff", "--name-only", "--no-renames", parent, commit],
            )?,
            None => git(repo, &["show", "--name-only", "--format=", commit])?,
        };
        for path in changed.lines().filter(|p| is_manifest(p)) {
            let before = parent.and_then(|p| file_at(repo, p, path));
            let after = file_at(repo, commit, path);
            out.extend(compare(
                day,
                &endpoints_in(before.as_deref()),
                &endpoints_in(after.as_deref()),
            ));
        }
    }
    Ok(out)
}

fn is_manifest(path: &str) -> bool {
    (path.ends_with(".yaml") || path.ends_with(".yml"))
        && !jc_manifest::loader::is_encrypted_file(path)
}

/// One Endpoint as the changelog reads it.
struct Seen {
    title: Option<String>,
    spec: EndpointSpec,
}

/// The Endpoints of one YAML file, by `metadata.name`. A document that does not parse as an
/// Endpoint is left out: it was refused when it was proposed, so it never answered.
fn endpoints_in(text: Option<&str>) -> BTreeMap<String, Seen> {
    use serde::Deserialize;
    let Some(text) = text else {
        return BTreeMap::new();
    };
    serde_norway::Deserializer::from_str(text)
        .filter_map(|doc| Value::deserialize(doc).ok())
        .filter(|doc| doc["kind"] == "Endpoint")
        .filter_map(|doc| {
            let name = doc["metadata"]["name"].as_str()?.to_owned();
            let spec = serde_json::from_value(doc["spec"].clone()).ok()?;
            let title = match &doc["metadata"]["title"] {
                Value::String(text) => Some(text.clone()),
                Value::Object(map) => map
                    .get("en")
                    .or_else(|| map.values().next())
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                _ => None,
            };
            Some((name, Seen { title, spec }))
        })
        .collect()
}

fn public(seen: Option<&Seen>) -> Option<&Seen> {
    seen.filter(|s| s.spec.audience == Audience::Public)
}

fn compare(
    day: NaiveDate,
    before: &BTreeMap<String, Seen>,
    after: &BTreeMap<String, Seen>,
) -> Vec<Entry> {
    let names: BTreeSet<&String> = before.keys().chain(after.keys()).collect();
    names
        .into_iter()
        .filter_map(|name| {
            let (old, new) = (before.get(name), after.get(name));
            let (mut surfaces, changes) = match (public(old), public(new)) {
                (None, None) => return None,
                (None, Some(new)) => (new.spec.served_representations(), vec!["published".into()]),
                (Some(old), None) => (
                    old.spec.served_representations(),
                    vec![match new {
                        Some(_) => "no longer public".into(),
                        None => "withdrawn".into(),
                    }],
                ),
                (Some(old), Some(new)) => changes(&old.spec, &new.spec)?,
            };
            surfaces.sort_unstable();
            let shown = public(new).or(public(old))?;
            Some(Entry {
                day,
                endpoint: name.clone(),
                title: shown.title.clone(),
                address: format!("/api/endpoint/{}/", shown.spec.slug),
                surfaces,
                changes,
            })
        })
        .collect()
}

/// What a consumer of an Endpoint public before and after notices, with the surfaces it
/// reaches; `None` when the edit was internal only (a policy, a role, a title).
fn changes(old: &EndpointSpec, new: &EndpointSpec) -> Option<(Vec<Representation>, Vec<String>)> {
    let served = |spec: &EndpointSpec| -> BTreeSet<Representation> {
        spec.served_representations().into_iter().collect()
    };
    let (was, is) = (served(old), served(new));
    let mut reached: BTreeSet<Representation> = BTreeSet::new();
    let mut said = Vec::new();
    let added: Vec<_> = is.difference(&was).copied().collect();
    let removed: Vec<_> = was.difference(&is).copied().collect();
    if !added.is_empty() {
        said.push(format!("surface added: {}", listed(&added)));
        reached.extend(&added);
    }
    if !removed.is_empty() {
        said.push(format!("surface removed: {}", listed(&removed)));
        reached.extend(&removed);
    }
    let hidden = |spec: &EndpointSpec| -> BTreeSet<String> {
        spec.projection
            .as_ref()
            .map(|p| p.hidden_attributes.iter().cloned().collect())
            .unwrap_or_default()
    };
    let (was_hidden, is_hidden) = (hidden(old), hidden(new));
    let mut whole = false;
    if is_hidden != was_hidden {
        let newly: Vec<_> = is_hidden.difference(&was_hidden).cloned().collect();
        let again: Vec<_> = was_hidden.difference(&is_hidden).cloned().collect();
        if !newly.is_empty() {
            said.push(format!("attributes no longer served: {}", newly.join(", ")));
        }
        if !again.is_empty() {
            said.push(format!("attributes served again: {}", again.join(", ")));
        }
        whole = true;
    }
    if old.view_mapping_ref != new.view_mapping_ref || old.projection_ref != new.projection_ref {
        said.push("data model changed".into());
        whole = true;
    }
    let per_minute = |spec: &EndpointSpec| spec.rate_limits.as_ref().map(|r| r.requests_per_minute);
    if per_minute(old) != per_minute(new) {
        let shown = |n: Option<u32>| n.map_or("the default".to_owned(), |n| format!("{n}/min"));
        said.push(format!(
            "rate limit {} to {}",
            shown(per_minute(old)),
            shown(per_minute(new))
        ));
        whole = true;
    }
    match (&old.deprecation, &new.deprecation) {
        (_, Some(d)) if old.deprecation.as_ref() != Some(d) => {
            said.push(announced(d));
            match d.representations.is_empty() {
                true => whole = true,
                false => reached.extend(&d.representations),
            }
        }
        (Some(_), None) => {
            said.push("deprecation withdrawn".into());
            whole = true;
        }
        _ => {}
    }
    if said.is_empty() {
        return None;
    }
    if whole {
        reached.extend(&is);
    }
    Some((reached.into_iter().collect(), said))
}

fn listed(surfaces: &[Representation]) -> String {
    surfaces
        .iter()
        .map(|s| s.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}

fn announced(d: &Deprecation) -> String {
    let mut text = format!("deprecated, sunset {}", d.sunset);
    if !d.attributes.is_empty() {
        text.push_str(&format!(" (attributes {})", d.attributes.join(", ")));
    }
    if let Some(link) = &d.link {
        text.push_str(&format!("; see {link}"));
    }
    text
}

/// The page: one section per surface, newest first, each entry on one line.
pub fn markdown(entries: &[Entry], days: u32) -> String {
    let mut page = format!(
        "# Public API changelog\n\nChanges of public Endpoints merged in the last {days} days, \
         newest first, per surface.\n"
    );
    let mut by_surface: BTreeMap<&str, Vec<&Entry>> = BTreeMap::new();
    for entry in entries {
        for surface in &entry.surfaces {
            by_surface.entry(surface.as_str()).or_default().push(entry);
        }
    }
    if by_surface.is_empty() {
        page.push_str("\nNo public Endpoint changed in this window.\n");
    }
    for (surface, list) in by_surface {
        page.push_str(&format!("\n## {surface}\n\n"));
        for entry in list {
            let name = entry.title.as_deref().unwrap_or(&entry.endpoint);
            page.push_str(&format!(
                "- {} **{}** (`{}`): {}\n",
                entry.day,
                escaped(name),
                entry.address,
                escaped(&entry.changes.join("; "))
            ));
        }
    }
    page
}

/// A title is the author's text: on a rendered page it stays text, never markup.
fn escaped(text: &str) -> String {
    text.chars()
        .flat_map(|c| match c {
            '\\' | '`' | '*' | '_' | '[' | ']' | '<' | '>' | '#' | '|' => vec!['\\', c],
            '\n' | '\r' => vec![' '],
            c => vec![c],
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn markup_in_a_title_stays_text() {
        assert_eq!(
            escaped("<script>*x*</script>\n[a](b)"),
            "\\<script\\>\\*x\\*\\</script\\> \\[a\\](b)"
        );
    }
}
