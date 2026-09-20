//! `jcctl import <source> --repo-dir <path>` (T-0134, MF-20, MF-22, MF-23, MF-24, PF-22).
//!
//! The inverse of `export`, and the engine under project duplication (PF-22, MF-26): a bundle
//! somebody downloaded from another instance becomes manifests in this repository, under this
//! project, with the ids rewritten to this organisation.
//!
//! Three things make that safe rather than a paste.
//!
//! It is *checked before it is written* (MF-24). A manifest carrying a literal credential, an
//! `apiVersion` this platform does not serve, a kind it does not know, or a reference to
//! something that exists neither in the bundle nor in the destination is rejected by name, and
//! a run with any rejection writes nothing at all: half an import is a repository that does
//! not validate, and the half that landed is the half nobody reviewed.
//!
//! It is *rewritten explicitly* (MF-22). The target namespace and the organisation domain are
//! arguments, never guessed from the bundle: an import that silently kept the source's
//! namespace would write another project's configuration into this one.
//!
//! It *says what it will do about a collision* (MF-23). `fail`, `skip`, `replace` and `rename`
//! are the four answers, and `fail` is the default because the other three all lose something
//! an operator may not have meant to lose.

use crate::commands::export;
use crate::loader::{is_empty_doc, parse_yaml_documents, RawManifest, Repository, ResourceId};
use jc_core::envelope::annotations::IMPORTED_FROM;
use jc_core::{registry, API_VERSION};
use serde_json::{json, Map, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::{Path, PathBuf};

/// What to do when the destination repository already declares the resource (MF-23).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Conflict {
    /// Refuse the whole import. The default: the other three all discard something.
    #[default]
    Fail,
    /// Keep what the repository has and leave the imported copy out.
    Skip,
    /// Overwrite the repository's copy with the imported one.
    Replace,
    /// Import it beside the existing one under the next free name.
    Rename,
}

impl Conflict {
    /// Parses the policy name as the flag spells it.
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "fail" => Some(Self::Fail),
            "skip" => Some(Self::Skip),
            "replace" => Some(Self::Replace),
            "rename" => Some(Self::Rename),
            _ => None,
        }
    }
}

/// How one import is rewritten and what it does about collisions.
#[derive(Debug, Clone, Default)]
pub struct Options {
    /// The project every project-scoped resource lands in (MF-22).
    pub namespace: Option<String>,
    /// The `{orgDomain}` segment every entity URN is rewritten to (MF-22, PF-22).
    pub org_domain: Option<String>,
    /// What to do when the repository already has the resource (MF-23).
    pub conflict: Conflict,
}

/// What happened to one manifest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// The repository did not have it.
    Created,
    /// It replaced what the repository had.
    Replaced,
    /// It landed beside the existing one, under this name.
    Renamed(String),
    /// The repository keeps its own copy and this one was left out.
    Skipped,
}

impl Outcome {
    /// The wire name in the JSON contract.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Created => "CREATED",
            Self::Replaced => "REPLACED",
            Self::Renamed(_) => "RENAMED",
            Self::Skipped => "SKIPPED",
        }
    }
}

/// One manifest and where it lands.
#[derive(Debug, Clone, PartialEq)]
pub struct Imported {
    /// Repository-relative path, from the kind's own template (MF-06).
    pub path: PathBuf,
    /// The manifest as it will be written: rewritten, and tagged with its provenance.
    pub manifest: RawManifest,
    /// What the conflict policy decided.
    pub outcome: Outcome,
}

/// One thing the import refused, and why (MF-24).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rejection {
    /// Where it came from: the file, and the document inside it.
    pub source: String,
    /// What is wrong, in the words the operator has to act on.
    pub reason: String,
}

impl fmt::Display for Rejection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.source, self.reason)
    }
}

/// What one import run produced.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Report {
    /// The manifests, in repository path order.
    pub imported: Vec<Imported>,
    /// Everything MF-24 refused. A non-empty list means nothing is written.
    pub rejections: Vec<Rejection>,
}

impl Report {
    /// Whether the import may be written (MF-24).
    pub fn is_acceptable(&self) -> bool {
        self.rejections.is_empty()
    }

    /// The JSON one line of stdout carries.
    pub fn to_json(&self) -> Value {
        json!({
            "summary": {
                "imported": self.imported.iter().filter(|i| i.outcome != Outcome::Skipped).count(),
                "skipped": self.imported.iter().filter(|i| i.outcome == Outcome::Skipped).count(),
                "rejected": self.rejections.len(),
            },
            "resources": self.imported.iter().map(|i| json!({
                "kind": i.manifest.kind,
                "name": i.manifest.metadata.name,
                "namespace": i.manifest.metadata.namespace,
                "path": i.path.to_string_lossy(),
                "outcome": i.outcome.as_str(),
            })).collect::<Vec<_>>(),
            "rejections": self.rejections.iter().map(|r| json!({
                "source": r.source,
                "reason": r.reason,
            })).collect::<Vec<_>>(),
        })
    }
}

/// Why an import could not even be attempted.
#[derive(Debug, thiserror::Error)]
pub enum ImportError {
    /// The source or the destination could not be read.
    #[error("{path}: {source}")]
    Io {
        /// What could not be read.
        path: PathBuf,
        /// The underlying failure.
        #[source]
        source: std::io::Error,
    },
    /// The destination repository does not load, so a collision cannot be judged.
    #[error("the destination repository does not load: {0}")]
    Destination(String),
    /// The source is a compressed archive, which this command does not open.
    #[error(
        "{0} is a compressed archive; extract it and import the directory. Unpacking is what \
         `tar` and `unzip` are for, and an importer that opened archives itself would be a \
         second place for a path-traversal bug to live (MF-25)"
    )]
    Archive(String),
    /// Two documents in the bundle are the same resource.
    #[error("{0} appears twice in the bundle, so it has no single desired state")]
    Duplicate(Box<ResourceId>),
}

/// Reads a bundle, rewrites it for this repository and decides every collision (MF-22, MF-23).
///
/// Nothing is written: `collect` is the whole judgement, and [`write()`] is the consequence, so
/// a caller can print the plan and stop.
pub fn collect(source: &Path, repo_dir: &Path, options: &Options) -> Result<Report, ImportError> {
    let destination =
        Repository::load(repo_dir).map_err(|e| ImportError::Destination(e.to_string()))?;
    let mut report = Report::default();

    let mut manifests: BTreeMap<ResourceId, (String, RawManifest)> = BTreeMap::new();
    for (origin, document) in documents(source)? {
        let mut manifest: RawManifest = match serde_norway::from_str(&document) {
            Ok(manifest) => manifest,
            Err(err) => {
                report.rejections.push(Rejection {
                    source: origin,
                    reason: format!("does not parse as a manifest: {err}"),
                });
                continue;
            }
        };
        // The bundle index describes the bundle; it is not one of its resources (MF-17).
        if manifest.kind == "Bundle" {
            continue;
        }
        if let Some(reason) = unacceptable(&manifest) {
            report.rejections.push(Rejection {
                source: origin,
                reason,
            });
            continue;
        }

        rewrite(&mut manifest, options, &origin);
        let id = ResourceId::from_manifest(&manifest);
        if manifests.contains_key(&id) {
            return Err(ImportError::Duplicate(Box::new(id)));
        }
        manifests.insert(id, (origin, manifest));
    }

    let arriving: BTreeSet<(String, String)> = manifests
        .keys()
        .map(|id| (id.kind.clone(), id.name.clone()))
        .collect();
    let existing: BTreeSet<(String, String)> = destination
        .iter()
        .map(|(id, _)| (id.kind.clone(), id.name.clone()))
        .collect();

    let mut taken = existing.clone();
    for (id, (origin, mut manifest)) in manifests {
        for reason in unresolvable_references(&manifest, &arriving, &existing) {
            report.rejections.push(Rejection {
                source: origin.clone(),
                reason,
            });
        }

        let outcome = match destination.get(&id) {
            None => Outcome::Created,
            Some(_) => match options.conflict {
                Conflict::Fail => {
                    report.rejections.push(Rejection {
                        source: origin.clone(),
                        reason: format!(
                            "{id} is already in the repository; choose --conflict \
                             skip|replace|rename to say what should happen to it (MF-23)"
                        ),
                    });
                    continue;
                }
                Conflict::Skip => Outcome::Skipped,
                Conflict::Replace => Outcome::Replaced,
                Conflict::Rename => {
                    let name = free_name(&id.kind, &manifest.metadata.name, &taken);
                    taken.insert((id.kind.clone(), name.clone()));
                    manifest.metadata.name.clone_from(&name);
                    Outcome::Renamed(name)
                }
            },
        };

        report.imported.push(Imported {
            path: PathBuf::from(path_of(&manifest)),
            manifest,
            outcome,
        });
    }

    report.imported.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(report)
}

/// Writes an accepted import into the repository, returning how many files it wrote.
///
/// A report carrying rejections writes nothing: MF-24 is a gate, not a warning.
pub fn write(repo_dir: &Path, report: &Report) -> Result<usize, ImportError> {
    if !report.is_acceptable() {
        return Ok(0);
    }
    let mut written = 0;
    for resource in &report.imported {
        if resource.outcome == Outcome::Skipped {
            continue;
        }
        let path = repo_dir.join(&resource.path);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|source| ImportError::Io {
                path: parent.to_path_buf(),
                source,
            })?;
        }
        let yaml = serde_norway::to_string(&resource.manifest).map_err(|err| ImportError::Io {
            path: resource.path.clone(),
            source: std::io::Error::other(err.to_string()),
        })?;
        std::fs::write(&path, yaml).map_err(|source| ImportError::Io {
            path: resource.path.clone(),
            source,
        })?;
        written += 1;
    }
    Ok(written)
}

/// Every YAML document of the source, each with the place it came from.
///
/// The source is one manifest file or the directory an exported bundle unpacks to (MF-17,
/// MF-25). A `.tar`, `.tgz` or `.zip` is refused rather than opened.
fn documents(source: &Path) -> Result<Vec<(String, String)>, ImportError> {
    let compressed = ["tar", "tgz", "gz", "zip", "bz2", "xz", "zst"];
    if source
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| compressed.contains(&e))
    {
        return Err(ImportError::Archive(source.display().to_string()));
    }

    let mut files = Vec::new();
    if source.is_dir() {
        // The loader's walk, because the containment rule belongs in one place: this one read a
        // link whose target left the bundle, so a plan could carry a manifest from anywhere on the
        // machine the bundle was unpacked on (CC-08, T-1478).
        let walked = crate::loader::walk_files(source).map_err(|error| ImportError::Io {
            path: source.to_path_buf(),
            source: std::io::Error::other(error),
        })?;
        for entry in walked {
            let path = entry.path();
            if entry.file_type().is_file()
                && path
                    .extension()
                    .and_then(|e| e.to_str())
                    .is_some_and(|e| e == "yaml" || e == "yml")
            {
                files.push(path.to_path_buf());
            }
        }
    } else {
        files.push(source.to_path_buf());
    }

    let mut documents = Vec::new();
    for file in files {
        let text = std::fs::read_to_string(&file).map_err(|source| ImportError::Io {
            path: file.clone(),
            source,
        })?;
        for chunk in parse_yaml_documents(&text) {
            if is_empty_doc(&chunk.content) {
                continue;
            }
            documents.push((
                format!("{}:{}", file.display(), chunk.start_line),
                chunk.content,
            ));
        }
    }
    Ok(documents)
}

/// Why a manifest may not be imported at all (MF-24), or `None` when it may.
fn unacceptable(manifest: &RawManifest) -> Option<String> {
    if !jc_core::serves(&manifest.kind, &manifest.api_version) {
        return Some(format!(
            "apiVersion `{}` is not served by this platform (expected `{API_VERSION}`)",
            manifest.api_version
        ));
    }
    if manifest.kind != "Bundle" && registry::by_kind(&manifest.kind).is_none() {
        return Some(format!(
            "kind `{}` is not a kind this platform knows, so nothing would reconcile it",
            manifest.kind
        ));
    }
    let credentials = export::literal_credentials(&manifest.spec);
    if !credentials.is_empty() {
        return Some(format!(
            "carries the literal credential{} `{}`; a manifest holds a secretRef and never a \
             secret (MF-24)",
            if credentials.len() > 1 { "s" } else { "" },
            credentials.join("`, `")
        ));
    }
    None
}

/// Rewrites one manifest for this repository (MF-22, PF-22, MF-20).
fn rewrite(manifest: &mut RawManifest, options: &Options, origin: &str) {
    let source_namespace = manifest.metadata.namespace.clone();
    manifest.metadata.collapse_language_maps();

    if let Some(target) = &options.namespace {
        let organization_scoped = crate::commands::export::belongs_to_the_organization(
            &manifest.kind,
            manifest.metadata.namespace.as_deref(),
        );
        if !organization_scoped {
            manifest.metadata.namespace = Some(target.clone());
        }
        if let Some(from) = &source_namespace {
            retarget_namespaces(&mut manifest.spec, from, target);
        }
    }

    if let Some(domain) = &options.org_domain {
        rewrite_urns(&mut manifest.spec, domain);
    }

    // MF-20: an imported object says where it came from, so a reviewer reading the merge
    // request knows which bundle to look at and a later re-import can be recognised.
    let annotations = manifest
        .metadata
        .rest
        .entry("annotations")
        .or_insert_with(|| Value::Object(Map::new()));
    if let Value::Object(annotations) = annotations {
        annotations.insert(IMPORTED_FROM.to_owned(), json!(origin));
    }
}

/// Points every typed reference that named the source project at the target one (MF-22).
fn retarget_namespaces(value: &mut Value, from: &str, to: &str) {
    match value {
        Value::Object(members) => {
            let typed = members.contains_key("kind") && members.contains_key("name");
            if typed && members.get("namespace").and_then(Value::as_str) == Some(from) {
                members.insert("namespace".to_owned(), json!(to));
            }
            for member in members.values_mut() {
                retarget_namespaces(member, from, to);
            }
        }
        Value::Array(values) => values
            .iter_mut()
            .for_each(|value| retarget_namespaces(value, from, to)),
        _ => {}
    }
}

/// Rewrites the `{orgDomain}` segment of every entity URN (PF-22).
///
/// `urn:ngsi-ld:{Type}:{orgDomain}:{space}:{localId}`: the fourth segment and nothing else, so
/// a local id that happens to contain the old domain is left alone.
fn rewrite_urns(value: &mut Value, domain: &str) {
    match value {
        Value::String(text) => {
            if let Some(rewritten) = with_domain(text, domain) {
                *text = rewritten;
            }
        }
        Value::Object(members) => members
            .values_mut()
            .for_each(|member| rewrite_urns(member, domain)),
        Value::Array(values) => values
            .iter_mut()
            .for_each(|value| rewrite_urns(value, domain)),
        _ => {}
    }
}

/// One URN, or one anchored `idPattern`, with its organisation domain replaced; `None` when
/// the string is neither.
///
/// A Policy and a registration anchor their coverage with a pattern rather than an id
/// (`^urn:ngsi-ld:AirQualityObserved:hel\.fi:air-quality:.*$`, R33), which is the same scheme
/// behind a `^` with the domain's dots escaped. Left alone, an imported registration keeps
/// routing at the organisation the bundle came from (MF-22, T-0826).
fn with_domain(urn: &str, domain: &str) -> Option<String> {
    let pattern = urn.starts_with('^');
    let rest = urn
        .strip_prefix('^')
        .unwrap_or(urn)
        .strip_prefix("urn:ngsi-ld:")?;
    let mut segments: Vec<String> = rest.split(':').map(str::to_owned).collect();
    // Type, orgDomain, space, localId: a URN with fewer segments is not this scheme, and
    // rewriting one that is not would corrupt an identifier this command does not own.
    if segments.len() < 4 {
        return None;
    }
    segments[1] = if pattern {
        regex::escape(domain)
    } else {
        domain.to_owned()
    };
    let joined = segments.join(":");
    Some(if pattern {
        format!("^urn:ngsi-ld:{joined}")
    } else {
        format!("urn:ngsi-ld:{joined}")
    })
}

/// References the import would leave dangling (MF-24).
///
/// A typed reference names its kind, so it is checked exactly. A bare `…Ref` names only a
/// resource, so it is satisfied by any resource of that name: the kind it means is the
/// referring field's business and the loader checks that when the repository is validated.
fn unresolvable_references(
    manifest: &RawManifest,
    arriving: &BTreeSet<(String, String)>,
    existing: &BTreeSet<(String, String)>,
) -> Vec<String> {
    let mut missing = Vec::new();
    collect_references(&manifest.spec, "spec", &mut |path, reference| {
        let resolved = match reference {
            Reference::Typed { kind, name } => {
                let key = (kind.to_owned(), name.to_owned());
                arriving.contains(&key) || existing.contains(&key)
            }
            Reference::Name(name) => {
                arriving.iter().any(|(_, n)| n == name) || existing.iter().any(|(_, n)| n == name)
            }
        };
        if !resolved {
            missing.push(format!(
                "{path} points at {reference}, which is neither in the bundle nor in the \
                 destination repository (MF-24)"
            ));
        }
    });
    missing
}

/// A reference as a manifest writes it (MF-07).
enum Reference<'a> {
    Typed { kind: &'a str, name: &'a str },
    Name(&'a str),
}

impl fmt::Display for Reference<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Typed { kind, name } => write!(f, "{kind}/{name}"),
            Self::Name(name) => write!(f, "`{name}`"),
        }
    }
}

fn collect_references(value: &Value, path: &str, found: &mut impl FnMut(&str, Reference)) {
    match value {
        Value::Object(members) => {
            if let (Some(Value::String(kind)), Some(Value::String(name))) =
                (members.get("kind"), members.get("name"))
            {
                found(path, Reference::Typed { kind, name });
                return;
            }
            for (member, child) in members {
                let child_path = format!("{path}.{member}");
                if member.ends_with("Ref") {
                    if let Value::String(name) = child {
                        found(&child_path, Reference::Name(name));
                        continue;
                    }
                }
                collect_references(child, &child_path, found);
            }
        }
        Value::Array(values) => values.iter().enumerate().for_each(|(index, child)| {
            collect_references(child, &format!("{path}[{index}]"), found)
        }),
        _ => {}
    }
}

/// The next free name for a renamed import: `name-2`, `name-3`, … (MF-23).
fn free_name(kind: &str, name: &str, taken: &BTreeSet<(String, String)>) -> String {
    (2..)
        .map(|n| format!("{name}-{n}"))
        .find(|candidate| !taken.contains(&(kind.to_owned(), candidate.clone())))
        .expect("an unbounded range always yields a free name")
}

/// The repository path a manifest belongs at, from its own kind (MF-06).
fn path_of(manifest: &RawManifest) -> String {
    let Some(info) = registry::by_kind(&manifest.kind) else {
        return format!("{}.yaml", manifest.metadata.name);
    };
    let project = manifest.metadata.namespace.as_deref().unwrap_or("org");
    info.repo_path(
        project,
        crate::loader::extract_space(&manifest.spec),
        &manifest.metadata.name,
    )
}
