//! One render from many repositories (layout 2, CC-86, ADR-N-029).
//!
//! In layout 2 the organization repository holds the organization-level kinds and the project
//! registry, `projects/{slug}.yaml`, and each project lives in a repository of its own. The
//! assembly writes the tree of layout 1 into a directory: the organization checkout, and each
//! registered project checkout at `projects/{slug}/`, its manifests mounted under the slug and
//! its parameters rendered (CC-88). [`Repository::load_for`] then loads that tree as it loads a
//! layout 1 repository, so every consumer that reads a path keeps working. A layout 1
//! organization repository is loaded where it is.

use crate::loader::{is_native_yaml, parse_yaml_documents, LoadError, Repository};
use jc_core::kinds::{Project, ProjectSpec};
use jc_core::project::{self, LiteralParameter, Parameters, RepositoryRole};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

/// Where a registry entry's repository is checked out at its ref.
pub trait Resolver {
    /// A directory holding the checkout of `entry`'s repository at `entry`'s ref, or why there
    /// is none. An external repository (CC-89) is read with its own `secretRef` only, never with
    /// a credential of the local forge.
    fn checkout(&self, slug: &str, entry: &ProjectSpec) -> Result<PathBuf, String>;
}

/// Checkouts that are already on disk, one directory per slug: `jcctl --project-dir
/// slug=path` and the tests.
#[derive(Debug, Clone, Default)]
pub struct Directories(pub BTreeMap<String, PathBuf>);

impl Resolver for Directories {
    fn checkout(&self, slug: &str, _entry: &ProjectSpec) -> Result<PathBuf, String> {
        self.0
            .get(slug)
            .cloned()
            .ok_or_else(|| format!("no checkout was given for the project `{slug}`"))
    }
}

/// What became of one registry entry (CC-86).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EntryStatus {
    /// The registry slug.
    pub slug: String,
    /// The ref the entry pins.
    pub git_ref: String,
    /// Why this render could not fetch the ref. The project then renders as the last
    /// assembly left it, or not at all when there was none.
    pub error: Option<String>,
    /// Whether the project is in this render.
    pub rendered: bool,
}

/// A finding in a project repository: where, and what.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    /// The registry slug.
    pub slug: String,
    /// The file, relative to the project repository.
    pub path: PathBuf,
    /// What is wrong, and what to write instead.
    pub message: String,
}

/// The render of an organization, and how each project reached it.
#[derive(Debug)]
pub struct Assembly {
    /// The organization's layout, from its `.jc/layout`.
    pub layout: u32,
    /// The assembled tree, loaded.
    pub repository: Repository,
    /// One per registry entry, in slug order; empty in layout 1.
    pub entries: Vec<EntryStatus>,
    /// Literals written where a parameter exists and mappings reading a parameter that is not
    /// there (CC-83, CC-88).
    pub findings: Vec<Finding>,
}

/// Why an organization did not assemble.
#[derive(Debug, thiserror::Error)]
pub enum AssembleError {
    /// A layout file, a registry entry or a project file that is refused.
    #[error("{path}: {message}")]
    Refused {
        /// The file, relative to its repository.
        path: String,
        /// Why.
        message: String,
    },
    /// Two projects claim one name that is unique in the organization (CC-86, PF-44).
    #[error("{what} `{value}` is claimed by the project `{first}` and by the project `{second}`")]
    Conflict {
        /// What kind of name.
        what: &'static str,
        /// The name.
        value: String,
        /// One claimant.
        first: String,
        /// The other.
        second: String,
    },
    /// The assembled tree does not load.
    #[error(transparent)]
    Load(#[from] LoadError),
    /// Reading or writing the tree failed.
    #[error("{path}: {source}")]
    Io {
        /// The path.
        path: PathBuf,
        /// The failure.
        source: std::io::Error,
    },
}

fn refused(path: impl Into<String>, message: impl ToString) -> AssembleError {
    AssembleError::Refused {
        path: path.into(),
        message: message.to_string(),
    }
}

fn io(path: &Path) -> impl FnOnce(std::io::Error) -> AssembleError + '_ {
    move |source| AssembleError::Io {
        path: path.to_path_buf(),
        source,
    }
}

/// The layout a checkout says it follows (CC-85).
pub fn layout_at(root: &Path, role: RepositoryRole) -> Result<u32, AssembleError> {
    let file = root.join(project::LAYOUT_FILE);
    let text = match std::fs::read_to_string(&file) {
        Ok(text) => Some(text),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => None,
        Err(source) => return Err(AssembleError::Io { path: file, source }),
    };
    project::layout_of(text.as_deref(), role).map_err(|err| refused(project::LAYOUT_FILE, err))
}

/// Assembles the organization checked out at `org_root` into `into` and loads it (CC-86).
///
/// `into` is kept between runs: a project whose ref cannot be fetched this time renders as the
/// last assembly left it, and its entry says why. The new tree is written beside `into` and
/// takes its place only once it loads, so a refused assembly leaves the last one serving.
pub fn assemble(
    org_root: &Path,
    resolver: &dyn Resolver,
    into: &Path,
    environment: Option<&str>,
) -> Result<Assembly, AssembleError> {
    let layout = layout_at(org_root, RepositoryRole::Organization)?;
    if layout == 1 {
        return Ok(Assembly {
            layout,
            repository: Repository::load_for(org_root, environment)?,
            entries: Vec::new(),
            findings: Vec::new(),
        });
    }

    let registry = read_registry(org_root)?;
    let next = sibling(into, "next");
    remove(&next)?;
    copy_tree(org_root, &next, &|rel| {
        rel.starts_with("projects") && rel.components().count() == 2
    })?;

    let mut entries = Vec::new();
    let mut findings = Vec::new();
    for (slug, entry) in &registry {
        let git_ref = entry.spec.git_ref.clone().unwrap_or_default();
        let target = next.join("projects").join(slug);
        match resolver.checkout(slug, &entry.spec) {
            Ok(checkout) => {
                mount_project(slug, &entry.spec, &checkout, &target, &mut findings)?;
                entries.push(EntryStatus {
                    slug: slug.clone(),
                    git_ref,
                    error: None,
                    rendered: true,
                });
            }
            Err(error) => {
                let last = into.join("projects").join(slug);
                let rendered = last.is_dir();
                if rendered {
                    copy_tree(&last, &target, &|_| false)?;
                }
                entries.push(EntryStatus {
                    slug: slug.clone(),
                    git_ref,
                    error: Some(error),
                    rendered,
                });
            }
        }
    }

    let loaded = Repository::load_for(&next, environment);
    let loaded = match loaded {
        Ok(repository) => repository,
        Err(err) => {
            let _ = std::fs::remove_dir_all(&next);
            return Err(err.into());
        }
    };
    if let Err(conflict) = unique_across_projects(&loaded) {
        let _ = std::fs::remove_dir_all(&next);
        return Err(conflict);
    }
    drop(loaded);

    let previous = sibling(into, "previous");
    remove(&previous)?;
    if into.exists() {
        std::fs::rename(into, &previous).map_err(io(into))?;
    }
    std::fs::rename(&next, into).map_err(io(into))?;
    remove(&previous)?;

    Ok(Assembly {
        layout,
        repository: Repository::load_for(into, environment)?,
        entries,
        findings,
    })
}

/// Every registry entry, by slug (PF-86). The layout 2 organization repository holds files
/// under `projects/` and no project directories.
fn read_registry(org_root: &Path) -> Result<BTreeMap<String, Project>, AssembleError> {
    let mut registry = BTreeMap::new();
    let dir = org_root.join("projects");
    let listing = match std::fs::read_dir(&dir) {
        Ok(listing) => listing,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(registry),
        Err(source) => return Err(AssembleError::Io { path: dir, source }),
    };
    for item in listing {
        let item = item.map_err(io(&dir))?;
        let name = item.file_name().to_string_lossy().into_owned();
        let path = format!("projects/{name}");
        if item.path().is_dir() {
            return Err(refused(
                path,
                "a layout 2 organization repository holds the registry entry \
                 projects/{slug}.yaml, and the project lives in its own repository; run `jcctl \
                 migrate` to move it there",
            ));
        }
        let Some(slug) = name
            .strip_suffix(".yaml")
            .or_else(|| name.strip_suffix(".yml"))
        else {
            continue;
        };
        let text = std::fs::read_to_string(item.path()).map_err(io(&item.path()))?;
        let entry = Project::from_yaml(&text).map_err(|err| refused(&path, err))?;
        entry.validate().map_err(|err| refused(&path, err))?;
        if !entry.spec.is_registry_entry() {
            return Err(refused(
                &path,
                "a registry entry names the project's spec.repository and spec.ref (PF-86)",
            ));
        }
        project::registry_path(slug).map_err(|err| refused(&path, err))?;
        if entry.metadata.name != slug {
            return Err(refused(
                &path,
                format!(
                    "the entry is named `{}` and its file `{slug}`; the file name is the slug",
                    entry.metadata.name
                ),
            ));
        }
        registry.insert(slug.to_owned(), entry);
    }
    Ok(registry)
}

/// Writes one project checkout into the tree at `target`: every manifest mounted under `slug`
/// with the parameters rendered, every other file as it is.
fn mount_project(
    slug: &str,
    entry: &ProjectSpec,
    checkout: &Path,
    target: &Path,
    findings: &mut Vec<Finding>,
) -> Result<(), AssembleError> {
    let in_project = |rel: &str| format!("{slug}: {rel}");
    layout_at(checkout, RepositoryRole::Project)
        .map_err(|err| refused(in_project(project::LAYOUT_FILE), err))?;
    let file = checkout.join(project::PROJECT_FILE);
    let text = std::fs::read_to_string(&file)
        .map_err(|err| refused(in_project(project::PROJECT_FILE), err))?;
    let own = Project::from_yaml(&text).map_err(|err| refused(in_project("project.yaml"), err))?;
    if own.spec.is_registry_entry() {
        return Err(refused(
            in_project(project::PROJECT_FILE),
            "is the project's own file; spec.repository and spec.ref belong to its registry \
             entry",
        ));
    }
    let parameters = project::resolve_parameters(&own.spec, entry)
        .map_err(|err| refused(format!("projects/{slug}.yaml"), err))?;

    copy_tree(checkout, target, &|_| false)?;
    for item in WalkDir::new(target).into_iter().filter_map(Result::ok) {
        if !item.file_type().is_file() {
            continue;
        }
        let path = item.path();
        let rel = path.strip_prefix(target).unwrap_or(path).to_path_buf();
        let name = item.file_name().to_string_lossy();
        let text = std::fs::read_to_string(path).map_err(io(path))?;
        if name == "bento.yaml" || name.ends_with(".blobl") {
            for unknown in parameters.unknown_in_mapping(&text) {
                findings.push(Finding {
                    slug: slug.to_owned(),
                    path: rel.clone(),
                    message: format!(
                        "reads env(\"{unknown}\"), which the project does not provide: declare \
                         the parameter in project.yaml, or read a non-secret one (CC-88)"
                    ),
                });
            }
            continue;
        }
        if !(name.ends_with(".yaml") || name.ends_with(".yml")) || is_native_yaml(&name) {
            continue;
        }
        let rendered = render_manifests(slug, &parameters, &rel, &text, findings)?;
        std::fs::write(path, rendered).map_err(io(path))?;
    }
    Ok(())
}

fn render_manifests(
    slug: &str,
    parameters: &Parameters,
    rel: &Path,
    text: &str,
    findings: &mut Vec<Finding>,
) -> Result<String, AssembleError> {
    let at = |document: usize| format!("{slug}: {} (document {document})", rel.display());
    let mut out = String::new();
    for chunk in parse_yaml_documents(text) {
        if crate::loader::is_empty_doc(&chunk.content) {
            continue;
        }
        let mut manifest: serde_json::Value = serde_norway::from_str(&chunk.content)
            .map_err(|err| refused(at(chunk.document_index), err))?;
        // The project's own file is where the defaults are written out, so it is not a literal.
        let literals =
            if manifest.get("kind").and_then(serde_json::Value::as_str) == Some("Project") {
                Vec::new()
            } else {
                parameters.literals(&manifest)
            };
        for LiteralParameter {
            path, placeholder, ..
        } in literals
        {
            findings.push(Finding {
                slug: slug.to_owned(),
                path: rel.to_path_buf(),
                message: format!(
                    "{path} writes the value of a parameter; write `{placeholder}` (CC-83, CC-88)"
                ),
            });
        }
        project::mount(&mut manifest, slug)
            .map_err(|err| refused(at(chunk.document_index), err))?;
        parameters
            .render(&mut manifest)
            .map_err(|err| refused(at(chunk.document_index), err))?;
        if !out.is_empty() {
            out.push_str("---\n");
        }
        out.push_str(
            &serde_norway::to_string(&manifest)
                .map_err(|err| refused(at(chunk.document_index), err))?,
        );
    }
    Ok(out)
}

/// A name the organization holds once, claimed by two projects, is refused (CC-86, PF-44).
fn unique_across_projects(repository: &Repository) -> Result<(), AssembleError> {
    let mut slugs: BTreeMap<String, String> = BTreeMap::new();
    let mut segments: BTreeMap<String, String> = BTreeMap::new();
    for (id, loaded) in repository.iter() {
        let project = id.namespace.clone().unwrap_or_default();
        let claim = match id.kind.as_str() {
            "Endpoint" => loaded
                .manifest
                .spec
                .get("slug")
                .and_then(serde_json::Value::as_str)
                .map(|slug| ("the Endpoint slug", &mut slugs, slug.to_owned())),
            "ContextSpace" => Some((
                "the space segment",
                &mut segments,
                repository.space_segment(&project, &id.name),
            )),
            _ => None,
        };
        let Some((what, seen, value)) = claim else {
            continue;
        };
        if let Some(first) = seen.get(&value) {
            if first != &project {
                return Err(AssembleError::Conflict {
                    what,
                    value,
                    first: first.clone(),
                    second: project,
                });
            }
        }
        seen.insert(value, project);
    }
    Ok(())
}

fn sibling(into: &Path, suffix: &str) -> PathBuf {
    let name = into
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "assembly".to_owned());
    into.with_file_name(format!(".{name}.{suffix}"))
}

fn remove(path: &Path) -> Result<(), AssembleError> {
    match std::fs::remove_dir_all(path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(AssembleError::Io {
            path: path.to_path_buf(),
            source,
        }),
    }
}

/// Copies `from` into `to`, leaving out `.git` and every relative path `skip` names. A link is
/// copied as the file it points to, and one out of `from` is refused.
fn copy_tree(from: &Path, to: &Path, skip: &dyn Fn(&Path) -> bool) -> Result<(), AssembleError> {
    let root = from.canonicalize().map_err(io(from))?;
    let walk = WalkDir::new(from)
        .follow_links(true)
        .into_iter()
        .filter_entry(|item| item.file_name() != ".git");
    for item in walk {
        let item = item.map_err(|err| AssembleError::Io {
            path: err.path().unwrap_or(from).to_path_buf(),
            source: err.into(),
        })?;
        let rel = item.path().strip_prefix(from).unwrap_or(item.path());
        if skip(rel) {
            continue;
        }
        let real = item.path().canonicalize().map_err(io(item.path()))?;
        if !real.starts_with(&root) {
            return Err(refused(
                rel.display().to_string(),
                "links out of the repository, and is not followed",
            ));
        }
        let dest = to.join(rel);
        if item.file_type().is_dir() {
            std::fs::create_dir_all(&dest).map_err(io(&dest))?;
        } else {
            std::fs::copy(item.path(), &dest).map_err(io(&dest))?;
        }
    }
    Ok(())
}
