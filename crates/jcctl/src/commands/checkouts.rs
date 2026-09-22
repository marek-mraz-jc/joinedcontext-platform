//! `jcctl checkouts`: every registered project checked out at its ref, for the daemons that
//! read checkouts and never the forge (CC-86, CC-89, T-2646).
//!
//! The sidecar beside the gateway runs it. It reads the registry of the organization checkout,
//! fetches each entry with [`GitCheckouts`] into `{projects}/.cache/`, and points the link
//! `{projects}/{slug}` at the tree of the entry's commit, swapped in one rename, so a reader sees
//! the old tree or the new one and never half of either. A project taken out of the registry
//! loses its link; one whose fetch fails keeps the link it had and says why.

use crate::assemble::{read_registry, Basic, GitCheckouts, Resolver};
use jc_core::{Secret, SecretRef};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// What one pass did with one registry entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Linked {
    /// The registry slug.
    pub slug: String,
    /// The tree the link points at now, or why the fetch failed and the link stayed.
    pub result: Result<PathBuf, String>,
    /// Whether this pass moved the link.
    pub moved: bool,
}

/// Where the forge and the external repositories' credentials come from.
pub struct Options {
    /// The organization in the local forge, `{base}/{org}`.
    pub forge: String,
    /// A file holding the forge's read-only token, a mounted Secret.
    pub token_file: Option<PathBuf>,
    /// The mounted Secrets an external entry's `secretRef` names: `{dir}/{name}/username` and
    /// `{dir}/{name}/password`.
    pub secrets_dir: Option<PathBuf>,
}

fn read_trimmed(path: &Path) -> Result<String, String> {
    std::fs::read_to_string(path)
        .map(|text| text.trim().to_owned())
        .map_err(|err| format!("{}: {err}", path.display()))
}

/// The credential a mounted Secret holds. The error names the Secret and never its content.
pub fn mounted_secret(dir: Option<&Path>, secret: &SecretRef) -> Result<Basic, String> {
    let dir = dir.ok_or_else(|| {
        format!(
            "the repository reads with the Secret `{}` and no --secrets-dir is mounted",
            secret.name
        )
    })?;
    let base = dir.join(&secret.name);
    if secret.name.contains(['/', '\\']) || secret.name.starts_with('.') {
        return Err(format!("`{}` is not a Secret name", secret.name));
    }
    let user = read_trimmed(&base.join("username"))
        .map_err(|_| format!("the Secret `{}` has no username", secret.name))?;
    let password = read_trimmed(&base.join("password"))
        .map_err(|_| format!("the Secret `{}` has no password", secret.name))?;
    Ok((user, Secret::new(password)))
}

/// One pass: every registry entry of `org_dir` linked under `projects` at its ref.
pub fn sync(org_dir: &Path, projects: &Path, options: &Options) -> Result<Vec<Linked>, String> {
    let registry = read_registry(org_dir).map_err(|err| err.to_string())?;
    let forge_credential = match &options.token_file {
        Some(file) => Some(("jcctl".to_owned(), Secret::new(read_trimmed(file)?))),
        None => None,
    };
    let secrets_dir = options.secrets_dir.clone();
    let secrets = move |secret: &SecretRef| mounted_secret(secrets_dir.as_deref(), secret);
    let resolver = GitCheckouts {
        cache: projects.join(".cache"),
        forge: options.forge.clone(),
        forge_credential,
        secrets: &secrets,
    };
    std::fs::create_dir_all(projects).map_err(|err| format!("{}: {err}", projects.display()))?;

    let mut linked = Vec::new();
    for (slug, entry) in &registry {
        let (result, moved) = match resolver
            .checkout(slug, &entry.spec)
            .and_then(|tree| link(projects, slug, &tree).map(|moved| (tree, moved)))
        {
            Ok((tree, moved)) => (Ok(tree), moved),
            Err(err) => (Err(err), false),
        };
        linked.push(Linked {
            slug: slug.clone(),
            result,
            moved,
        });
    }

    let registered: BTreeSet<&String> = registry.keys().collect();
    let listing =
        std::fs::read_dir(projects).map_err(|err| format!("{}: {err}", projects.display()))?;
    for item in listing.flatten() {
        let name = item.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') || registered.contains(&name) {
            continue;
        }
        let path = item.path();
        let removed = if path.is_symlink() {
            std::fs::remove_file(&path)
        } else {
            std::fs::remove_dir_all(&path)
        };
        removed.map_err(|err| format!("{}: {err}", path.display()))?;
        let cache = projects.join(".cache");
        if let Ok(trees) = std::fs::read_dir(&cache) {
            for tree in trees.flatten() {
                let tree_name = tree.file_name().to_string_lossy().into_owned();
                if tree_name == format!("{name}.git")
                    || tree_name.starts_with(&format!("{name}.tree-"))
                {
                    let _ = std::fs::remove_dir_all(tree.path());
                }
            }
        }
    }
    Ok(linked)
}

/// Points `{projects}/{slug}` at `tree` with one rename over the old link; whether it moved.
fn link(projects: &Path, slug: &str, tree: &Path) -> Result<bool, String> {
    let target = projects.join(slug);
    let relative = tree.strip_prefix(projects).unwrap_or(tree);
    if std::fs::read_link(&target).is_ok_and(|current| current == relative) {
        return Ok(false);
    }
    let next = projects.join(format!(".{slug}.link"));
    let _ = std::fs::remove_file(&next);
    #[cfg(unix)]
    std::os::unix::fs::symlink(relative, &next).map_err(|err| err.to_string())?;
    #[cfg(not(unix))]
    return Err("checkouts are linked on unix only".to_owned());
    if target.is_dir() && !target.is_symlink() {
        std::fs::remove_dir_all(&target).map_err(|err| err.to_string())?;
    }
    std::fs::rename(&next, &target).map_err(|err| err.to_string())?;
    Ok(true)
}
