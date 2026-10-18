//! One render from many repositories (layout 2, CC-86, ADR-N-029).
//!
//! In layout 2 the organization repository holds the organization-level kinds and the project
//! registry, `projects/{slug}.yaml`, and each project lives in a repository of its own. The
//! assembly writes the tree of layout 1 into a directory: the organization checkout, and each
//! registered project checkout at `projects/{slug}/`, its manifests mounted under the slug and
//! its parameters rendered (CC-88), so every consumer that reads a path keeps working. The
//! [`Repository`] is built from the manifests as they were rendered, and is the one
//! [`Repository::load_for`] would read back from that tree (T-4457). A layout 1 organization
//! repository is loaded where it is.

use crate::loader::{
    document, is_native_yaml, parse_yaml_documents, read_documents, DocChunk, LoadError,
    LoadedResource, Repository,
};
use jc_core::kinds::{Project, ProjectSpec};
use jc_core::project::{self, LiteralParameter, Parameters, RepositoryRole};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
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
    /// Whether its render was taken from the last assembly, its linked tree and its registry
    /// entry unchanged (T-4460).
    pub reused: bool,
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

/// The render of each linked project checkout, kept from one [`assemble_reusing`] to the next
/// so a project whose checkout did not move is not rendered again (T-4460).
///
/// Only a checkout that is a link is kept: `jcctl checkouts` and git-sync point it at a tree
/// named by its commit, written once and swapped in by one rename (T-4444), so the tree the
/// link resolves to names its content. A plain directory can change in place and is rendered
/// on every run.
#[derive(Debug, Default)]
pub struct Renders(BTreeMap<String, Render>);

#[derive(Debug, Clone)]
struct Render {
    tree: PathBuf,
    entry: ProjectSpec,
    documents: Arc<[LoadedResource]>,
    findings: Arc<[Finding]>,
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
    assemble_reusing(
        org_root,
        resolver,
        into,
        environment,
        &mut Renders::default(),
    )
}

/// [`assemble`], taking each linked project whose tree and registry entry are the ones
/// `renders` holds from the last assembly instead of rendering it again (T-4460). The
/// repository is still built and checked whole, so nothing it grants outlives a change.
pub fn assemble_reusing(
    org_root: &Path,
    resolver: &dyn Resolver,
    into: &Path,
    environment: Option<&str>,
    renders: &mut Renders,
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
    copy_tree(
        org_root,
        &next,
        &|rel| rel.starts_with("projects") && rel.components().count() == 2,
        &mut copy_file,
    )?;
    // The organization's own manifests, read before any project is mounted beside them; each
    // project's are kept as the mount renders them, never read back (T-4457).
    let mut documents = read_documents(&next, &next)?;

    let mut entries = Vec::new();
    let mut findings = Vec::new();
    let mut kept = BTreeMap::new();
    // Renders moved whole out of `into`, each put back when this assembly is refused.
    let mut moved: Vec<(PathBuf, PathBuf)> = Vec::new();
    let loaded = (|| {
        for (slug, entry) in &registry {
            let git_ref = entry.spec.git_ref.clone().unwrap_or_default();
            let target = next.join("projects").join(slug);
            let last = into.join("projects").join(slug);
            match resolver.checkout(slug, &entry.spec) {
                Ok(checkout) => {
                    let tree = linked_tree(&checkout);
                    let reused = renders.0.get(slug).filter(|render| {
                        Some(&render.tree) == tree.as_ref()
                            && render.entry == entry.spec
                            && last.is_dir()
                    });
                    let render = match reused {
                        Some(render) => {
                            std::fs::create_dir_all(next.join("projects")).map_err(io(&next))?;
                            std::fs::rename(&last, &target).map_err(io(&last))?;
                            moved.push((target, last));
                            render.clone()
                        }
                        None => {
                            let (mut own, mut found) = (Vec::new(), Vec::new());
                            mount_project(
                                slug,
                                &entry.spec,
                                &checkout,
                                &next,
                                &mut own,
                                &mut found,
                            )?;
                            Render {
                                tree: PathBuf::new(),
                                entry: entry.spec.clone(),
                                documents: own.into(),
                                findings: found.into(),
                            }
                        }
                    };
                    documents.extend(render.documents.iter().cloned());
                    findings.extend(render.findings.iter().cloned());
                    if let Some(tree) = tree {
                        kept.insert(slug.clone(), Render { tree, ..render });
                    }
                    entries.push(EntryStatus {
                        slug: slug.clone(),
                        git_ref,
                        error: None,
                        rendered: true,
                        reused: reused.is_some(),
                    });
                }
                Err(error) => {
                    let rendered = last.is_dir();
                    if rendered {
                        copy_tree(&last, &target, &|_| false, &mut copy_file)?;
                        documents.extend(read_documents(&next, &target)?);
                    }
                    entries.push(EntryStatus {
                        slug: slug.clone(),
                        git_ref,
                        error: Some(error),
                        rendered,
                        reused: false,
                    });
                }
            }
        }
        let loaded =
            Repository::from_documents(&next, std::mem::take(&mut documents), environment)?;
        unique_across_projects(&loaded)?;
        Ok(loaded)
    })();
    let loaded = match loaded {
        Ok(repository) => repository,
        Err(err) => {
            for (from, back) in moved.iter().rev() {
                // Put back, or the next assembly renders that project again: `into` misses it.
                let _ = std::fs::rename(from, back);
            }
            let _ = std::fs::remove_dir_all(&next);
            return Err(err);
        }
    };

    let previous = sibling(into, "previous");
    remove(&previous)?;
    if into.exists() {
        std::fs::rename(into, &previous).map_err(io(into))?;
    }
    std::fs::rename(&next, into).map_err(io(into))?;
    remove(&previous)?;
    renders.0 = kept;

    Ok(Assembly {
        layout,
        repository: loaded.moved_to(into),
        entries,
        findings,
    })
}

/// Every registry entry of the organization checked out at `org_root`, by slug (PF-86). The
/// layout 2 organization repository holds files under `projects/` and no project directories.
pub fn read_registry(org_root: &Path) -> Result<BTreeMap<String, Project>, AssembleError> {
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

/// Writes one project checkout into the tree at `root/projects/{slug}`: every manifest
/// mounted under `slug` with the parameters rendered, and kept in `documents` as it was
/// written; every other file as it is.
fn mount_project(
    slug: &str,
    entry: &ProjectSpec,
    checkout: &Path,
    root: &Path,
    documents: &mut Vec<LoadedResource>,
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

    let mounted = Path::new("projects").join(slug);
    copy_tree(
        checkout,
        &root.join(&mounted),
        &|_| false,
        &mut |rel, from, to| {
            // A dot-directory holds the forge's files (`.gitea/workflows/`, CC-90) and `.jc/`,
            // never a manifest: copied as they are, as the loader skips them (CC-08).
            if rel.parent().is_some_and(|dir| {
                dir.components()
                    .any(|part| part.as_os_str().to_string_lossy().starts_with('.'))
            }) {
                return copy_file(rel, from, to);
            }
            let name = rel
                .file_name()
                .map(|name| name.to_string_lossy())
                .unwrap_or_default();
            let mapping = name == "bento.yaml" || name.ends_with(".blobl");
            let manifest =
                (name.ends_with(".yaml") || name.ends_with(".yml")) && !is_native_yaml(&name);
            if !mapping && !manifest {
                return copy_file(rel, from, to);
            }
            let text = std::fs::read_to_string(from).map_err(io(from))?;
            if mapping {
                for unknown in parameters.unknown_in_mapping(&text) {
                    findings.push(Finding {
                        slug: slug.to_owned(),
                        path: rel.to_path_buf(),
                        message: format!(
                            "reads env(\"{unknown}\"), which the project does not provide: \
                             declare the parameter in project.yaml, or read a non-secret one \
                             (CC-88)"
                        ),
                    });
                }
                return std::fs::write(to, text).map_err(io(to));
            }
            let (rendered, manifests) = render_manifests(slug, &parameters, rel, &text, findings)?;
            let path = mounted.join(rel);
            for (chunk, manifest) in manifests {
                let manifest = serde_json::from_value(manifest).map_err(|err| err.to_string());
                documents.push(document(&path, &chunk, manifest)?);
            }
            std::fs::write(to, rendered).map_err(io(to))
        },
    )
}

/// The file at `rel`, rendered, and each of its documents with where it starts in the render.
fn render_manifests(
    slug: &str,
    parameters: &Parameters,
    rel: &Path,
    text: &str,
    findings: &mut Vec<Finding>,
) -> Result<(String, Vec<(DocChunk, serde_json::Value)>), AssembleError> {
    let at = |document: usize| format!("{slug}: {} (document {document})", rel.display());
    let mut out = String::new();
    let mut manifests = Vec::new();
    let mut written = 0;
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
        let text = serde_norway::to_string(&manifest)
            .map_err(|err| refused(at(chunk.document_index), err))?;
        // Where `parse_yaml_documents` places it in the render: the first at line 1, each later
        // one at its `---`.
        let start_line = written + 1;
        if !out.is_empty() {
            out.push_str("---\n");
            written += 1;
        }
        written += text.lines().count();
        manifests.push((
            DocChunk {
                document_index: manifests.len() + 1,
                start_line,
                content: String::new(),
            },
            manifest,
        ));
        out.push_str(&text);
    }
    Ok((out, manifests))
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

/// The tree a linked checkout resolves to, which names its content ([`Renders`]); `None` for a
/// checkout that is not a link.
fn linked_tree(checkout: &Path) -> Option<PathBuf> {
    let link = std::fs::symlink_metadata(checkout).ok()?;
    if !link.file_type().is_symlink() {
        return None;
    }
    checkout.canonicalize().ok()
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

/// Copies `from` into `to`, leaving out `.git` and every relative path `skip` names; each file
/// is written by `file(rel, from, to)`. A link is copied as the file it points to, and one out
/// of `from` is refused.
fn copy_tree(
    from: &Path,
    to: &Path,
    skip: &dyn Fn(&Path) -> bool,
    file: &mut dyn FnMut(&Path, &Path, &Path) -> Result<(), AssembleError>,
) -> Result<(), AssembleError> {
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
        // Only a link can lead out: a file reached through a directory link inside the tree is
        // inside it, and the link itself was checked when the walk reached it. Canonicalising
        // every file cost a path walk each, seconds over a 10,000-Endpoint tree (T-4443).
        if item.path_is_symlink()
            && !item
                .path()
                .canonicalize()
                .map_err(io(item.path()))?
                .starts_with(&root)
        {
            return Err(refused(
                rel.display().to_string(),
                "links out of the repository, and is not followed",
            ));
        }
        let dest = to.join(rel);
        if item.file_type().is_dir() {
            std::fs::create_dir_all(&dest).map_err(io(&dest))?;
        } else {
            file(rel, item.path(), &dest)?;
        }
    }
    Ok(())
}

fn copy_file(_rel: &Path, from: &Path, to: &Path) -> Result<(), AssembleError> {
    std::fs::copy(from, to).map(drop).map_err(io(to))
}

/// The HTTP basic credential a repository is read with: a user name and a password or token.
pub type Basic = (String, jc_core::Secret);

/// How [`GitCheckouts`] reads one registry entry's repository (CC-89).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Credential {
    /// The local forge's read-only token: a repository of the forge, by name.
    Forge,
    /// The entry's own `secretRef`: a repository outside the forge.
    SecretRef(jc_core::SecretRef),
    /// Nothing: a public repository outside the forge.
    Anonymous,
}

/// Checkouts fetched with git at each entry's ref, for the daemons that assemble (CC-86).
///
/// Each repository is mirrored under `cache` and each ref is checked out into a directory of
/// its own commit, so a render never reads a half-fetched tree. Credentials reach git through
/// its environment, never through its arguments or a URL, and git never prompts.
pub struct GitCheckouts<'a> {
    /// Where mirrors and checkouts are kept between runs.
    pub cache: PathBuf,
    /// The organization in the local forge, `https://forge.example/git/{org}`; a repository
    /// `name` is `{forge}/{name}.git`.
    pub forge: String,
    /// The local forge's read-only credential, for the forge's own repositories only.
    pub forge_credential: Option<Basic>,
    /// Resolves an external repository's `secretRef` to the credential it names.
    pub secrets: &'a dyn Fn(&jc_core::SecretRef) -> Result<Basic, String>,
}

impl GitCheckouts<'_> {
    /// Which credential an entry is read with: the forge's for a repository of the forge, and
    /// for one outside it the entry's own `secretRef` or none, never the forge's (CC-89).
    pub fn credential(entry: &ProjectSpec) -> Result<Credential, String> {
        let repository = entry
            .repository
            .as_ref()
            .ok_or("a registry entry names a repository")?;
        Ok(match (&repository.name, &repository.url) {
            (Some(_), None) => Credential::Forge,
            (None, Some(_)) => match &repository.secret_ref {
                Some(secret) => Credential::SecretRef(secret.clone()),
                None => Credential::Anonymous,
            },
            _ => return Err("a repository names either a forge repository or a url".into()),
        })
    }

    fn url(&self, entry: &ProjectSpec) -> Result<String, String> {
        let repository = entry
            .repository
            .as_ref()
            .ok_or("a registry entry names a repository")?;
        match (&repository.name, &repository.url) {
            (Some(name), None) => Ok(format!("{}/{name}.git", self.forge.trim_end_matches('/'))),
            (None, Some(url)) => Ok(url.clone()),
            _ => Err("a repository names either a forge repository or a url".into()),
        }
    }

    fn git(&self, credential: Option<&Basic>, args: &[&str]) -> Result<String, String> {
        use base64::Engine as _;
        let mut command = std::process::Command::new("git");
        command
            .args(args)
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_CONFIG_NOSYSTEM", "1");
        if let Some((user, secret)) = credential {
            let pair = format!("{user}:{}", secret.expose());
            let header = format!(
                "Authorization: Basic {}",
                base64::engine::general_purpose::STANDARD.encode(pair)
            );
            command
                .env("GIT_CONFIG_COUNT", "1")
                .env("GIT_CONFIG_KEY_0", "http.extraHeader")
                .env("GIT_CONFIG_VALUE_0", header);
        }
        let output = command
            .output()
            .map_err(|err| format!("git does not run: {err}"))?;
        if !output.status.success() {
            return Err(format!(
                "git {}: {}",
                args.first().copied().unwrap_or_default(),
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
    }
}

impl Resolver for GitCheckouts<'_> {
    fn checkout(&self, slug: &str, entry: &ProjectSpec) -> Result<PathBuf, String> {
        let url = self.url(entry)?;
        let resolved;
        let credential = match Self::credential(entry)? {
            Credential::Forge => self.forge_credential.as_ref(),
            Credential::SecretRef(secret) => {
                resolved = (self.secrets)(&secret)?;
                Some(&resolved)
            }
            Credential::Anonymous => None,
        };
        let git_ref = entry
            .git_ref
            .as_deref()
            .ok_or("a registry entry pins a ref")?;
        std::fs::create_dir_all(&self.cache).map_err(|err| err.to_string())?;
        let mirror = self.cache.join(format!("{slug}.git"));
        let mirror_arg = mirror.to_string_lossy().into_owned();
        if mirror.is_dir() {
            self.git(
                credential,
                &[
                    "--git-dir",
                    &mirror_arg,
                    "remote",
                    "set-url",
                    "origin",
                    &url,
                ],
            )?;
            self.git(
                credential,
                &["--git-dir", &mirror_arg, "remote", "update", "--prune"],
            )?;
        } else {
            self.git(
                credential,
                &["clone", "--mirror", "--quiet", &url, &mirror_arg],
            )?;
        }
        let commit = self
            .git(
                None,
                &[
                    "--git-dir",
                    &mirror_arg,
                    "rev-parse",
                    "--verify",
                    "--quiet",
                    &format!("{git_ref}^{{commit}}"),
                ],
            )
            .map_err(|_| format!("the ref `{git_ref}` names no commit of {url}"))?;
        let tree = self.cache.join(format!("{slug}.tree-{commit}"));
        if !tree.is_dir() {
            let partial = self.cache.join(format!(".{slug}.tree-{commit}.partial"));
            let _ = std::fs::remove_dir_all(&partial);
            std::fs::create_dir_all(&partial).map_err(|err| err.to_string())?;
            let partial_arg = partial.to_string_lossy().into_owned();
            self.git(
                None,
                &[
                    "--git-dir",
                    &mirror_arg,
                    "--work-tree",
                    &partial_arg,
                    "checkout",
                    "--force",
                    &commit,
                    "--",
                    ".",
                ],
            )?;
            std::fs::rename(&partial, &tree).map_err(|err| err.to_string())?;
        }
        // The checkouts of other commits of this project are no longer read.
        let prefix = format!("{slug}.tree-");
        if let Ok(listing) = std::fs::read_dir(&self.cache) {
            for item in listing.flatten() {
                let name = item.file_name().to_string_lossy().into_owned();
                if name.starts_with(&prefix) && item.path() != tree {
                    let _ = std::fs::remove_dir_all(item.path());
                }
            }
        }
        Ok(tree)
    }
}
