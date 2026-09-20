//! `jcctl export --space <id> --out-dir <path>` (T-0133, CC-22, MF-16, MF-17, PF-20).
//!
//! The inverse of `apply`: the platform is read and the answer is written as the
//! repository that would produce it, so a space someone built by hand can be adopted into
//! Git without retyping it. Two properties make the output usable rather than merely
//! informative.
//!
//! It is *clean*. `status` never survives the [`RawManifest`] boundary at all — the type
//! refuses unknown members (MF-04) — and the system metadata the platform keeps beside
//! the declaration (`uid`, `resourceVersion`, the reconciler's own annotations) is
//! stripped here, because a manifest carrying them is a snapshot, not a declaration
//! (MF-16).
//!
//! It is *safe*. A live platform may hand back a credential where a manifest is only ever
//! allowed a `secretRef` (MF-17, MF-24). Exporting that would write a secret into Git,
//! which is the one thing the whole configuration-as-code story forbids, so the member is
//! dropped and named in the report: a lossy export the operator can see beats a faithful
//! one nobody may commit.

use crate::loader::{extract_space, RawManifest};
use crate::platform::{Platform, PlatformError};
use jc_core::registry;
use jc_core::Scope;
use serde_json::{Map, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

/// Metadata the platform owns and a declaration never carries (MF-16).
const SYSTEM_METADATA: &[&str] = &[
    "uid",
    "resourceVersion",
    "generation",
    "creationTimestamp",
    "deletionTimestamp",
    "managedFields",
    "selfLink",
    "revision",
    "status",
];

/// Annotation prefixes the reconciler writes and owns; a `joinedcontext.com/` annotation
/// an operator wrote by hand is theirs and survives (MF-08).
const SYSTEM_ANNOTATIONS: &[&str] = &[
    "joinedcontext.com/last-applied",
    "joinedcontext.com/revision",
    "joinedcontext.com/managed-attributes",
    "joinedcontext.com/reconciled-at",
];

/// Members that hold a credential itself rather than a reference to one (MF-17, MF-24).
///
/// Matched whole and case-insensitively, never as a substring: `tokenEndpoint` is a URL
/// and `secretRef` is a reference, and dropping either would corrupt the export to guard
/// something that was never a secret.
const CREDENTIAL_MEMBERS: &[&str] = &[
    "password",
    "passwd",
    "passphrase",
    "token",
    "secret",
    "clientsecret",
    "apikey",
    "privatekey",
    "credential",
    "credentials",
    "dsn",
    "connectionstring",
    "accesskey",
    "secretkey",
];

/// One resource, cleaned, and the repository path it belongs at (MF-06).
#[derive(Debug, Clone, PartialEq)]
pub struct Exported {
    /// Repository-relative path, from the kind's own template.
    pub path: PathBuf,
    /// The manifest as it will be written.
    pub manifest: RawManifest,
}

/// What `export` produced (API/03 section 1).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Report {
    /// The manifests, in repository path order.
    pub resources: Vec<Exported>,
    /// `kind/name: member` for every credential that was dropped (MF-17).
    pub redactions: Vec<String>,
}

/// Reads one space off the platform and cleans it, writing nothing (CC-22).
///
/// `project` is the namespace the space lives in; the platform has no project directory,
/// so the caller names it. A resource that names no context space is not part of a space
/// and is left where it is: `export` copies a space out, not a whole installation.
pub fn collect(
    platform: &impl Platform,
    project: &str,
    space: &str,
) -> Result<Report, PlatformError> {
    let mut report = Report::default();

    // Two kinds may share a plural — `Policy` and `ScopeDefinition` are both `policies` —
    // so the answer to a query is not one kind's resources and the path template comes
    // from each manifest's own kind, not from the query.
    // A kind that lives in either place is asked for in both, so a project's own roles are
    // exported beside the organization's (PF-68).
    let queries: BTreeSet<(&str, &str)> = registry::KINDS
        .iter()
        .flat_map(|info| {
            let organization = info
                .scope
                .allows_organization()
                .then_some((jc_core::envelope::ORG_NAMESPACE, info.plural));
            let in_project = info
                .scope
                .allows_project()
                .then_some((project, info.plural));
            [organization, in_project]
        })
        .flatten()
        .collect();

    let mut found: BTreeMap<String, RawManifest> = BTreeMap::new();
    for (namespace, plural) in queries {
        for manifest in platform.list(namespace, plural)? {
            let Some(info) = registry::by_kind(&manifest.kind) else {
                continue;
            };
            if !belongs_to(&manifest, space) {
                continue;
            }
            // The manifest's own namespace decides where it lands, which is what tells a
            // project's role from the organization's (PF-68).
            let namespace = manifest.metadata.namespace.as_deref().unwrap_or(project);
            found.insert(
                info.repo_path(namespace, space, &manifest.metadata.name),
                manifest,
            );
        }
    }

    for (path, mut manifest) in found {
        strip_system_metadata(&mut manifest.metadata.rest);
        manifest.metadata.collapse_language_maps();
        let mut dropped = Vec::new();
        redact(&mut manifest.spec, &mut dropped);
        report.redactions.extend(
            dropped
                .into_iter()
                .map(|member| format!("{}/{}: {member}", manifest.kind, manifest.metadata.name)),
        );

        report.resources.push(Exported {
            path: PathBuf::from(path),
            manifest,
        });
    }

    Ok(report)
}

/// One live resource cleaned into the manifest that would declare it (CC-22, CC-38).
///
/// The same cleaning `export` does, for one resource instead of a space: this is what
/// *adopt* commits when an operator decides the live state is the truth (CC-68). The
/// second half of the answer is what had to be dropped to make it committable.
pub fn adopt(live: &RawManifest) -> (RawManifest, Vec<String>) {
    let mut manifest = live.clone();
    strip_system_metadata(&mut manifest.metadata.rest);
    manifest.metadata.collapse_language_maps();
    let mut dropped = Vec::new();
    redact(&mut manifest.spec, &mut dropped);
    (manifest, dropped)
}

/// The literal credentials a spec carries, named, without changing it (MF-24).
///
/// `export` drops these because a manifest may not carry one; `import` refuses a manifest
/// that does, for the same reason read the other way round. One list, one definition of
/// what counts as a credential.
pub fn literal_credentials(spec: &Value) -> Vec<String> {
    let mut copy = spec.clone();
    let mut found = Vec::new();
    redact(&mut copy, &mut found);
    found
}

/// Writes a collected report as a repository under `out_dir`.
pub fn write(out_dir: &Path, report: &Report) -> std::io::Result<usize> {
    for resource in &report.resources {
        let path = out_dir.join(&resource.path);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let yaml = serde_norway::to_string(&resource.manifest)
            .map_err(|err| std::io::Error::other(err.to_string()))?;
        std::fs::write(path, yaml)?;
    }
    Ok(report.resources.len())
}

/// Whether a live resource belongs to the space being exported.
///
/// The space itself is in its own export; everything else is in it if it says so. A
/// resource that names no space (a Project, a ServiceAccount) belongs to the installation
/// rather than to this space and is not copied out with it.
/// Whether this manifest belongs to the organization rather than to one project (PF-68).
///
/// A kind that lives in one place is answered by its scope alone; `Role`, which lives in
/// either, is answered by the namespace it carries.
pub fn belongs_to_the_organization(kind: &str, namespace: Option<&str>) -> bool {
    registry::by_kind(kind).is_some_and(|info| match info.scope {
        Scope::Organization => true,
        Scope::Project => false,
        Scope::OrganizationOrProject => namespace == Some(jc_core::envelope::ORG_NAMESPACE),
    })
}

fn belongs_to(manifest: &RawManifest, space: &str) -> bool {
    if manifest.kind == "ContextSpace" {
        return manifest.metadata.name == space;
    }
    extract_space(&manifest.spec) == space
}

/// Removes the metadata the platform owns, so the export is a declaration (MF-16).
fn strip_system_metadata(metadata: &mut Map<String, Value>) {
    metadata.retain(|member, _| !SYSTEM_METADATA.contains(&member.as_str()));
    if let Some(Value::Object(annotations)) = metadata.get_mut("annotations") {
        annotations.retain(|name, _| !SYSTEM_ANNOTATIONS.contains(&name.as_str()));
        if annotations.is_empty() {
            metadata.remove("annotations");
        }
    }
}

/// Drops every literal credential from a spec, naming what went (MF-17).
///
/// Only a scalar is dropped: a member called `credentials` holding a list of `secretRef`
/// entries is a declaration, and removing it would be removing the configuration rather
/// than the secret.
fn redact(spec: &mut Value, dropped: &mut Vec<String>) {
    match spec {
        Value::Object(members) => {
            members.retain(|name, value| {
                let literal = value.is_string()
                    && CREDENTIAL_MEMBERS.contains(&name.to_ascii_lowercase().as_str());
                if literal {
                    dropped.push(name.clone());
                }
                !literal
            });
            for value in members.values_mut() {
                redact(value, dropped);
            }
        }
        Value::Array(values) => values.iter_mut().for_each(|value| redact(value, dropped)),
        _ => {}
    }
}

// --- the repository export (MF-16, MF-17, T-0824) -----------------------------------------

/// Everything a bundle of one project holds: the manifests, the files beside them, and the
/// index that says what it is.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Bundle {
    /// Manifests, cleaned, at the path they had in the repository.
    pub resources: Vec<Exported>,
    /// Native files (`bento.yaml`, LinkML sources, generated artifacts), byte for byte.
    pub natives: Vec<(PathBuf, Vec<u8>)>,
    /// `kind/name: member` for every credential that was dropped (MF-17).
    pub redactions: Vec<String>,
}

/// Reads `projects/{project}/` of a checkout as the bundle a download produces (MF-16, MF-17).
///
/// The configuration kinds live in Git (CC-72), so this is the whole export: a manifest is
/// parsed, cleaned of `status` and of any literal credential, and written back at the path it
/// had; anything else beside it is a native file and travels byte for byte, because it is the
/// pipeline's or the model's own format, not ours to rewrite.
pub fn collect_project(repo_dir: &Path, project: &str) -> std::io::Result<Bundle> {
    let root = repo_dir.join("projects").join(project);
    let mut bundle = Bundle::default();
    let mut files: Vec<PathBuf> = walk(&root)?;
    files.sort();

    for path in files {
        let relative = path
            .strip_prefix(repo_dir)
            .map_err(|_| std::io::Error::other("path escaped the repository"))?
            .to_path_buf();
        let body = std::fs::read(&path)?;
        let manifest = match relative.extension().and_then(|e| e.to_str()) {
            Some("yaml") | Some("yml") => std::str::from_utf8(&body)
                .ok()
                .and_then(|text| serde_norway::from_str::<RawManifest>(text).ok())
                .filter(|manifest| registry::by_kind(&manifest.kind).is_some()),
            _ => None,
        };
        match manifest {
            Some(mut manifest) => {
                strip_system_metadata(&mut manifest.metadata.rest);
                let mut dropped = Vec::new();
                redact(&mut manifest.spec, &mut dropped);
                bundle.redactions.extend(dropped.into_iter().map(|member| {
                    format!("{}/{}: {member}", manifest.kind, manifest.metadata.name)
                }));
                bundle.resources.push(Exported {
                    path: relative,
                    manifest,
                });
            }
            None => bundle.natives.push((relative, body)),
        }
    }
    Ok(bundle)
}

/// Writes a bundle under `out_dir`, index included, and answers how many files it wrote.
pub fn write_bundle(
    out_dir: &Path,
    project: &str,
    revision: &str,
    exported_by: &str,
    bundle: &Bundle,
) -> std::io::Result<usize> {
    let mut written = 0usize;
    for resource in &bundle.resources {
        let path = out_dir.join(&resource.path);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let yaml = serde_norway::to_string(&resource.manifest)
            .map_err(|err| std::io::Error::other(err.to_string()))?;
        std::fs::write(path, yaml)?;
        written += 1;
    }
    for (relative, body) in &bundle.natives {
        let path = out_dir.join(relative);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, body)?;
        written += 1;
    }
    // A Bundle lists at least one resource, so a project with no manifest gets no index
    // rather than one the platform refuses (MF-17, T-0823).
    if bundle.resources.is_empty() {
        return Ok(written);
    }
    let index = index_of(project, revision, exported_by, bundle);
    let yaml =
        serde_norway::to_string(&index).map_err(|err| std::io::Error::other(err.to_string()))?;
    std::fs::write(out_dir.join("bundle.yaml"), yaml)?;
    Ok(written + 1)
}

/// The lowercase hexadecimal SHA-256 of some bytes, as the bundle index carries it (MF-42).
pub fn sha256_of(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    format!("{:x}", Sha256::digest(bytes))
}

/// The `kind: Bundle` index of a bundle, as the Portal's download writes it (MF-17, T-0823).
///
/// The platform's own kind, built from its own types: an index the registry refuses is a bundle
/// the platform rejects as soon as anyone validates the tree it unpacks to.
fn index_of(project: &str, revision: &str, exported_by: &str, bundle: &Bundle) -> Value {
    let items: Vec<jc_core::kinds::BundleItem> = bundle
        .resources
        .iter()
        .map(|resource| {
            let organization_scoped = belongs_to_the_organization(
                &resource.manifest.kind,
                resource.manifest.metadata.namespace.as_deref(),
            );
            jc_core::kinds::BundleItem {
                kind: resource.manifest.kind.clone(),
                namespace: resource
                    .manifest
                    .metadata
                    .namespace
                    .clone()
                    .filter(|_| !organization_scoped),
                name: resource.manifest.metadata.name.clone(),
                path: resource.path.to_string_lossy().into_owned(),
            }
        })
        .collect();
    // The checksum of every file as this bundle writes it, so an import can verify the transfer
    // before anyone deletes the source (MF-42).
    let mut files: Vec<jc_core::kinds::BundleFile> = bundle
        .resources
        .iter()
        .filter_map(|resource| {
            let yaml = serde_norway::to_string(&resource.manifest).ok()?;
            Some(jc_core::kinds::BundleFile {
                path: resource.path.to_string_lossy().into_owned(),
                sha256: sha256_of(yaml.as_bytes()),
            })
        })
        .chain(
            bundle
                .natives
                .iter()
                .map(|(path, body)| jc_core::kinds::BundleFile {
                    path: path.to_string_lossy().into_owned(),
                    sha256: sha256_of(body),
                }),
        )
        .collect();
    files.sort_by(|a, b| a.path.cmp(&b.path));
    let spec = jc_core::kinds::BundleSpec {
        exported_at: chrono::Utc::now(),
        exported_by: exported_by.to_owned(),
        source_instance: None,
        source_revision: revision.to_owned(),
        items,
        native_files: bundle
            .natives
            .iter()
            .map(|(path, _)| path.to_string_lossy().into_owned())
            .collect(),
        omitted: 0,
        files,
        readme: None,
        schemas: None,
    };
    serde_json::json!({
        "apiVersion": jc_core::API_VERSION,
        "kind": "Bundle",
        "metadata": { "name": project, "namespace": "org" },
        "spec": spec,
    })
}

/// Every file under `dir`; a directory whose name starts with a dot is skipped, and a link whose
/// target leaves `dir` is refused rather than read (CC-08).
///
/// The walk is the loader's own, because the containment rule belongs in one place: this one used
/// `std::fs::metadata`, which follows a link, so a link to `/etc/passwd` inside a checkout put that
/// file's bytes in the archive (T-1478).
fn walk(dir: &Path) -> std::io::Result<Vec<PathBuf>> {
    if !dir.exists() {
        return Ok(Vec::new());
    }
    Ok(crate::loader::walk_files(dir)
        .map_err(std::io::Error::other)?
        .into_iter()
        .filter(|entry| entry.file_type().is_file())
        .map(|entry| entry.path().to_path_buf())
        .collect())
}
