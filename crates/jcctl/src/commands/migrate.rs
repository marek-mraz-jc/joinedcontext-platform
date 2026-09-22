//! `jcctl migrate --repo-dir <layout 1 clone> --out-dir <dir>`: layout 1 to layout 2 (CC-85,
//! MF-47, ADR-N-029).
//!
//! The only writer of a layout change. From a git clone of a layout 1 organization repository it
//! writes, under `--out-dir`:
//!
//! - `org/`, the organization repository with its whole history, each `projects/{slug}/`
//!   replaced by the registry entry `projects/{slug}.yaml` tracking the project's `main`, and
//!   `.jc/layout` set to `2`, in one commit on top;
//! - `projects/{slug}/`, one repository per project: the `git subtree split` of
//!   `projects/{slug}/`, so every commit that touched the project is in its own history, and one
//!   commit on top writing `.jc/layout`.
//!
//! The source clone is read and never written: no branch is made there. A source already in
//! layout 2 is refused, and so is an output directory that holds anything.

use crate::assemble::layout_at;
use jc_core::project::{RepositoryRole, LAYOUT_FILE, PROJECT_FILE};
use std::path::{Path, PathBuf};
use std::process::Command;

/// What `migrate` wrote.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Migration {
    /// The organization repository.
    pub organization: PathBuf,
    /// One repository per project, by slug, each with the commit its history ends at.
    pub projects: Vec<(String, PathBuf, String)>,
}

/// Why a migration did not run.
#[derive(Debug, thiserror::Error)]
pub enum MigrateError {
    /// The source or the output is not what a migration starts from.
    #[error("{0}")]
    Refused(String),
    /// A git command failed.
    #[error("git {args}: {message}")]
    Git {
        /// The arguments, without secrets: every one is a path or a ref of the migration.
        args: String,
        /// What git said.
        message: String,
    },
    /// Reading or writing a file failed.
    #[error("{path}: {source}")]
    Io {
        /// The path.
        path: PathBuf,
        /// The failure.
        source: std::io::Error,
    },
}

const AUTHOR: [&str; 4] = [
    "-c",
    "user.name=jcctl migrate",
    "-c",
    "user.email=jcctl-migrate@joinedcontext.invalid",
];

fn git(dir: &Path, args: &[&str]) -> Result<String, MigrateError> {
    let output = Command::new("git")
        .args(AUTHOR)
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .map_err(|err| MigrateError::Git {
            args: args.join(" "),
            message: err.to_string(),
        })?;
    if !output.status.success() {
        return Err(MigrateError::Git {
            args: args.join(" "),
            message: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        });
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

fn write(path: &Path, body: &str) -> Result<(), MigrateError> {
    let io = |source| MigrateError::Io {
        path: path.to_path_buf(),
        source,
    };
    std::fs::create_dir_all(path.parent().unwrap_or(path)).map_err(io)?;
    std::fs::write(path, body).map_err(io)
}

/// Migrates the layout 1 clone at `repo_dir` into `out_dir` (CC-85).
pub fn run(repo_dir: &Path, out_dir: &Path) -> Result<Migration, MigrateError> {
    let layout = layout_at(repo_dir, RepositoryRole::Organization)
        .map_err(|err| MigrateError::Refused(err.to_string()))?;
    if layout != 1 {
        return Err(MigrateError::Refused(format!(
            "{} is already layout {layout}; a migration runs once, from layout 1",
            repo_dir.display()
        )));
    }
    if out_dir
        .read_dir()
        .is_ok_and(|mut listing| listing.next().is_some())
    {
        return Err(MigrateError::Refused(format!(
            "{} is not empty; the migration writes into an empty directory",
            out_dir.display()
        )));
    }
    if !git(repo_dir, &["status", "--porcelain"])?.is_empty() {
        return Err(MigrateError::Refused(format!(
            "{} has uncommitted changes; the migration moves what is committed, so commit or \
             discard them first",
            repo_dir.display()
        )));
    }

    let slugs = project_slugs(repo_dir)?;
    let mut projects = Vec::new();
    for slug in &slugs {
        let prefix = format!("projects/{slug}");
        let split = git(
            repo_dir,
            &["subtree", "split", "-q", &format!("--prefix={prefix}")],
        )?;
        let target = out_dir.join("projects").join(slug);
        std::fs::create_dir_all(&target).map_err(|source| MigrateError::Io {
            path: target.clone(),
            source,
        })?;
        git(&target, &["init", "-q", "-b", "main"])?;
        let source = repo_dir.to_string_lossy().into_owned();
        git(&target, &["fetch", "-q", &source, &split])?;
        git(&target, &["reset", "-q", "--hard", "FETCH_HEAD"])?;
        write(&target.join(LAYOUT_FILE), "2\n")?;
        git(&target, &["add", LAYOUT_FILE])?;
        git(
            &target,
            &[
                "commit",
                "-q",
                "-m",
                "jcctl migrate: the project repository of layout 2 (CC-85)",
            ],
        )?;
        let head = git(&target, &["rev-parse", "HEAD"])?;
        projects.push((slug.clone(), target, head));
    }

    let organization = out_dir.join("org");
    let source = repo_dir.to_string_lossy().into_owned();
    let into = organization.to_string_lossy().into_owned();
    git(out_dir, &["clone", "-q", "--no-local", &source, &into])?;
    git(&organization, &["remote", "remove", "origin"])?;
    for slug in &slugs {
        let entry = registry_entry(repo_dir, slug)?;
        git(
            &organization,
            &["rm", "-r", "-q", &format!("projects/{slug}")],
        )?;
        write(&organization.join(format!("projects/{slug}.yaml")), &entry)?;
        git(&organization, &["add", &format!("projects/{slug}.yaml")])?;
    }
    write(&organization.join(LAYOUT_FILE), "2\n")?;
    git(&organization, &["add", LAYOUT_FILE])?;
    git(
        &organization,
        &[
            "commit",
            "-q",
            "-m",
            "jcctl migrate: layout 2, one repository per project, the registry in their place (CC-85, PF-86)",
        ],
    )?;
    Ok(Migration {
        organization,
        projects,
    })
}

/// Every directory under `projects/` that holds a `project.yaml`, in name order.
fn project_slugs(repo_dir: &Path) -> Result<Vec<String>, MigrateError> {
    let dir = repo_dir.join("projects");
    let mut slugs = Vec::new();
    let listing = match std::fs::read_dir(&dir) {
        Ok(listing) => listing,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(slugs),
        Err(source) => return Err(MigrateError::Io { path: dir, source }),
    };
    for item in listing {
        let item = item.map_err(|source| MigrateError::Io {
            path: dir.clone(),
            source,
        })?;
        let name = item.file_name().to_string_lossy().into_owned();
        if !item.path().is_dir() {
            continue;
        }
        if !item.path().join(PROJECT_FILE).is_file() {
            return Err(MigrateError::Refused(format!(
                "projects/{name}/ holds no project.yaml, so it is no project to move; declare the project or remove the directory first"
            )));
        }
        slugs.push(name);
    }
    slugs.sort();
    Ok(slugs)
}

/// The registry entry of a migrated project: its repository in the forge, tracking `main`, no
/// parameter values, the organization its `project.yaml` names (PF-86).
fn registry_entry(repo_dir: &Path, slug: &str) -> Result<String, MigrateError> {
    let path = repo_dir.join("projects").join(slug).join(PROJECT_FILE);
    let text = std::fs::read_to_string(&path).map_err(|source| MigrateError::Io {
        path: path.clone(),
        source,
    })?;
    let own: serde_json::Value = serde_norway::from_str(&text)
        .map_err(|err| MigrateError::Refused(format!("{}: {err}", path.display())))?;
    let entry = serde_json::json!({
        "apiVersion": jc_core::API_VERSION,
        "kind": "Project",
        "metadata": { "name": slug, "namespace": "org" },
        "spec": {
            "organizationRef": own["spec"]["organizationRef"],
            "repository": { "name": slug },
            "ref": "main",
        },
    });
    serde_norway::to_string(&entry)
        .map_err(|err| MigrateError::Refused(format!("{}: {err}", path.display())))
}
