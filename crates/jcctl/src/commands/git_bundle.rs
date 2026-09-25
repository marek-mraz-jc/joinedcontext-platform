//! A whole project as Git (MF-45, MF-46, MF-47, ADR-N-029).
//!
//! `jcctl export --format git` writes one `git bundle` per repository of a project, the
//! project's and one per application, with every branch and tag; the project's registry entry
//! with no parameter values, so the target sets its own (CC-88); and a `kind: Bundle` index
//! listing each bundle with its role, its SHA-256 and the head commit it ends at.
//!
//! `jcctl import --format git` clones each bundle and refuses the import unless every file is
//! the one the index names and every head is the one it lists. A project repository of a
//! layout this release does not read is refused; an organization repository of layout 1 is
//! migrated on the way in, and the migrated repositories are what the import lands (MF-47).

use crate::assemble::layout_at;
use crate::commands::migrate;
use jc_core::kinds::{
    Bundle, BundleFile, BundleModel, BundleModelOrigin, BundleRepository, BundleRole as Role,
    BundleSpec, Project,
};
use jc_core::project::{self, RepositoryRole, PROJECT_FILE};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

/// The index a git-native export writes beside its bundles.
pub const INDEX: &str = "bundle.yaml";

/// Why an export or an import did not run.
#[derive(Debug, thiserror::Error)]
pub enum BundleError {
    /// What the command was given is not what it takes.
    #[error("{0}")]
    Refused(String),
    /// A git command failed.
    #[error("git {args}: {message}")]
    Git {
        /// The arguments; every one is a path or a ref of the command.
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
    /// A migration on import failed.
    #[error(transparent)]
    Migrate(#[from] migrate::MigrateError),
}

fn git(dir: &Path, args: &[&str]) -> Result<String, BundleError> {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .map_err(|err| BundleError::Git {
            args: args.join(" "),
            message: err.to_string(),
        })?;
    if !output.status.success() {
        return Err(BundleError::Git {
            args: args.join(" "),
            message: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        });
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

fn io(path: &Path) -> impl FnOnce(std::io::Error) -> BundleError + '_ {
    move |source| BundleError::Io {
        path: path.to_path_buf(),
        source,
    }
}

fn refused(message: impl Into<String>) -> BundleError {
    BundleError::Refused(message.into())
}

fn empty(dir: &Path) -> Result<(), BundleError> {
    if dir
        .read_dir()
        .is_ok_and(|mut listing| listing.next().is_some())
    {
        return Err(refused(format!(
            "{} is not empty; the command writes into an empty directory",
            dir.display()
        )));
    }
    std::fs::create_dir_all(dir).map_err(io(dir))
}

fn sha256_of(path: &Path) -> Result<String, BundleError> {
    let bytes = std::fs::read(path).map_err(io(path))?;
    Ok(format!("{:x}", Sha256::digest(&bytes)))
}

/// Exports the project repository checked out at `repo_dir`, and the application repositories
/// of `apps` (name → checkout), into `out_dir` (MF-45), with a copy of every organization model
/// the project imports, read from the organization checkout `org_dir` (MF-49). Returns the index
/// it wrote.
pub fn export(
    repo_dir: &Path,
    slug: &str,
    apps: &BTreeMap<String, PathBuf>,
    org_dir: Option<&Path>,
    out_dir: &Path,
    exported_by: &str,
) -> Result<Bundle, BundleError> {
    project::registry_path(slug).map_err(|err| refused(err.to_string()))?;
    layout_at(repo_dir, RepositoryRole::Project).map_err(|err| refused(err.to_string()))?;
    let file = repo_dir.join(PROJECT_FILE);
    let text = std::fs::read_to_string(&file).map_err(io(&file))?;
    let own = Project::from_yaml(&text).map_err(|err| refused(format!("project.yaml: {err}")))?;
    if own.spec.is_registry_entry() {
        return Err(refused(
            "project.yaml is a registry entry; export takes a project repository",
        ));
    }
    // Before anything is written: a project that imports an organization model is exported
    // with it or not at all (MF-49).
    let carried = crate::model::organization_imports(repo_dir, org_dir).map_err(refused)?;
    empty(out_dir)?;

    let mut repositories = Vec::new();
    let mut files = Vec::new();
    let sources = std::iter::once((slug.to_owned(), repo_dir.to_path_buf(), Role::Project)).chain(
        apps.iter()
            .map(|(name, dir)| (name.clone(), dir.clone(), Role::Application)),
    );
    for (name, dir, role) in sources {
        let bundle_file = format!("{name}.bundle");
        let target = out_dir.join(&bundle_file);
        let target_arg = target.to_string_lossy().into_owned();
        git(
            &dir,
            &[
                "bundle",
                "create",
                "-q",
                &target_arg,
                "HEAD",
                "--branches",
                "--tags",
            ],
        )?;
        files.push(BundleFile {
            path: bundle_file.clone(),
            sha256: sha256_of(&target)?,
        });
        repositories.push(BundleRepository {
            name,
            role,
            file: bundle_file,
            head: git(&dir, &["rev-parse", "HEAD"])?,
        });
    }

    // Schema files only: the manifest and the LinkML source, as the organization holds them.
    let mut models = Vec::new();
    let org_root = org_dir.unwrap_or(repo_dir);
    for model in carried {
        let major = model.version.major();
        let manifest = format!("models/{}.v{major}.yaml", model.name);
        let file = format!("models/{}.v{major}.linkml.yaml", model.name);
        for (to, from) in [(&manifest, &model.manifest), (&file, &model.source)] {
            let target = out_dir.join(to);
            std::fs::create_dir_all(target.parent().unwrap_or(out_dir)).map_err(io(out_dir))?;
            std::fs::copy(org_root.join(from), &target).map_err(io(&target))?;
            files.push(BundleFile {
                path: to.clone(),
                sha256: sha256_of(&target)?,
            });
        }
        models.push(BundleModel {
            sha256: sha256_of(&out_dir.join(&file))?,
            origin: BundleModelOrigin {
                organization: own.spec.organization_ref.name().to_owned(),
                name: model.name.clone(),
            },
            name: model.name,
            version: model.version,
            manifest,
            file,
        });
    }

    let entry = serde_json::json!({
        "apiVersion": jc_core::API_VERSION,
        "kind": "Project",
        "metadata": { "name": slug, "namespace": "org" },
        "spec": {
            "organizationRef": serde_json::to_value(&own.spec.organization_ref)
                .map_err(|err| refused(err.to_string()))?,
            "repository": { "name": slug },
            "ref": "main",
        },
    });
    let entry_path = format!("projects/{slug}.yaml");
    let entry_text = serde_norway::to_string(&entry).map_err(|err| refused(err.to_string()))?;
    let written = out_dir.join(&entry_path);
    std::fs::create_dir_all(written.parent().unwrap_or(out_dir)).map_err(io(out_dir))?;
    std::fs::write(&written, &entry_text).map_err(io(&written))?;
    files.push(BundleFile {
        path: entry_path,
        sha256: sha256_of(&written)?,
    });

    let index = Bundle {
        api_version: jc_core::API_VERSION.to_owned(),
        kind: "Bundle".to_owned(),
        metadata: jc_core::ObjectMeta {
            name: slug.to_owned(),
            namespace: Some("org".to_owned()),
            ..Default::default()
        },
        spec: BundleSpec {
            exported_at: chrono::Utc::now(),
            exported_by: exported_by.to_owned(),
            source_instance: None,
            source_revision: repositories[0].head.clone(),
            items: Vec::new(),
            native_files: Vec::new(),
            files,
            omitted: 0,
            readme: None,
            schemas: None,
            repositories,
            models,
        },
        status: None,
    };
    index.validate().map_err(|err| refused(err.to_string()))?;
    let index_path = out_dir.join(INDEX);
    let yaml = serde_norway::to_string(&index).map_err(|err| refused(err.to_string()))?;
    std::fs::write(&index_path, yaml).map_err(io(&index_path))?;
    Ok(index)
}

/// What an import landed: each repository where it is, at the head the index listed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Imported {
    /// `(name, checkout, head)` in the index's order; a migrated organization is listed as the
    /// repositories the migration wrote.
    pub repositories: Vec<(String, PathBuf, String)>,
}

/// Imports the git-native export at `dir` into `out_dir` (MF-46, MF-47).
pub fn import(dir: &Path, out_dir: &Path) -> Result<Imported, BundleError> {
    let index_path = dir.join(INDEX);
    let text = std::fs::read_to_string(&index_path).map_err(io(&index_path))?;
    let index = Bundle::from_yaml(&text).map_err(|err| refused(format!("{INDEX}: {err}")))?;
    index
        .validate()
        .map_err(|err| refused(format!("{INDEX}: {err}")))?;
    if index.spec.repositories.is_empty() {
        return Err(refused(format!(
            "{INDEX} lists no repository; a YAML bundle is imported without --format git"
        )));
    }
    let checksums: BTreeMap<&str, &str> = index
        .spec
        .files
        .iter()
        .map(|file| (file.path.as_str(), file.sha256.as_str()))
        .collect();
    for (path, sha256) in &checksums {
        let actual = sha256_of(&dir.join(path))?;
        if actual != *sha256 {
            return Err(refused(format!(
                "{path} is not the file the index lists: its SHA-256 differs (MF-42)"
            )));
        }
    }
    empty(out_dir)?;

    let mut landed = Vec::new();
    for repository in &index.spec.repositories {
        if !checksums.contains_key(repository.file.as_str()) {
            return Err(refused(format!(
                "{} carries no SHA-256 in the index, so it cannot be verified (MF-42)",
                repository.file
            )));
        }
        let target = out_dir.join(&repository.name);
        let source = dir.join(&repository.file).to_string_lossy().into_owned();
        let target_arg = target.to_string_lossy().into_owned();
        git(out_dir, &["clone", "-q", &source, &target_arg])?;
        let head = git(&target, &["rev-parse", "HEAD"])?;
        if head != repository.head {
            return Err(refused(format!(
                "{} ends at {head}, and the index lists {}; the transfer is not verified \
                 (MF-46)",
                repository.name, repository.head
            )));
        }
        git(&target, &["remote", "remove", "origin"])?;
        match repository.role {
            Role::Project => {
                layout_at(&target, RepositoryRole::Project)
                    .map_err(|err| refused(format!("{}: {err}", repository.name)))?;
                landed.push((repository.name.clone(), target, head));
            }
            Role::Application => landed.push((repository.name.clone(), target, head)),
            Role::Organization => {
                let layout = layout_at(&target, RepositoryRole::Organization)
                    .map_err(|err| refused(format!("{}: {err}", repository.name)))?;
                if layout == 1 {
                    let migrated = out_dir.join(format!("{}-layout-2", repository.name));
                    let migration = migrate::run(&target, &migrated)?;
                    landed.push((
                        repository.name.clone(),
                        migration.organization.clone(),
                        git(&migration.organization, &["rev-parse", "HEAD"])?,
                    ));
                    landed.extend(migration.projects);
                } else {
                    landed.push((repository.name.clone(), target, head));
                }
            }
        }
    }
    for file in &index.spec.files {
        if file.path.starts_with("projects/") || file.path.starts_with("models/") {
            let target = out_dir.join(&file.path);
            std::fs::create_dir_all(target.parent().unwrap_or(out_dir)).map_err(io(out_dir))?;
            std::fs::copy(dir.join(&file.path), &target).map_err(io(&target))?;
        }
    }
    Ok(Imported {
        repositories: landed,
    })
}
