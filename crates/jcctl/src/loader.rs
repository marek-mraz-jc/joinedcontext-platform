//! Repository walker and manifest loader (T-0124, CC-08, CC-09, CC-10, MF-05, MF-06).

use jc_core::registry;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

/// One manifest loaded from the repository, kept untyped (CC-09, MF-05).
#[derive(Debug, Clone, PartialEq)]
pub struct LoadedResource {
    /// Repository-relative path of the file it came from (MF-06).
    pub path: PathBuf,
    /// 1-based index of the YAML document inside that file (MF-05).
    pub document: usize,
    /// 1-based line in the file where this document starts, for diagnostics.
    pub line: usize,
    /// `apiVersion`, `kind`, `metadata` and `spec` exactly as written (MF-01).
    pub manifest: RawManifest,
}

/// The four envelope members every manifest carries (MF-01).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct RawManifest {
    /// Canonical API version string (MF-01).
    pub api_version: String,
    /// Manifest kind name (MF-01).
    pub kind: String,
    /// Manifest metadata block (MF-02).
    pub metadata: RawMetadata,
    /// Manifest specification payload, unparsed for untyped handling (MF-03).
    #[serde(default)]
    pub spec: serde_json::Value,
    /// The one part of `status` a repository holds: `status.build` of an `App`, written back by
    /// the build lane in the commit that publishes the artifact (AP-13a). Everything else about
    /// status is computed and never stored (MF-04), so this stays untyped like `spec` and the
    /// reader takes the member it knows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<serde_json::Value>,
}

/// The metadata members the loader needs to build an identity (MF-02, MF-06).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RawMetadata {
    /// Resource DNS-1123 name (MF-02).
    pub name: String,
    /// Resource namespace (project slug or `org`), if present (MF-02).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub namespace: Option<String>,
    /// Remaining metadata attributes preserved for fidelity (MF-02).
    #[serde(flatten)]
    pub rest: serde_json::Map<String, serde_json::Value>,
}

impl RawMetadata {
    /// Rewrites a legacy `{locale: text}` title or description as the one plain string a
    /// manifest now carries (UI-50): `en`, then the first non-empty value. Every manifest
    /// `jcctl` writes goes through it, so a repository converts itself over normal work.
    /// A map holding anything but strings is left for validation to refuse.
    pub fn collapse_language_maps(&mut self) {
        for key in ["title", "description"] {
            let Some(serde_json::Value::Object(map)) = self.rest.get(key) else {
                continue;
            };
            let Some(texts) = map
                .iter()
                .map(|(locale, text)| Some((locale.as_str(), text.as_str()?)))
                .collect::<Option<Vec<_>>>()
            else {
                continue;
            };
            let text = texts
                .iter()
                .find(|(locale, text)| *locale == "en" && !text.is_empty())
                .or_else(|| texts.iter().find(|(_, text)| !text.is_empty()))
                .map(|(_, text)| (*text).to_owned());
            match text {
                Some(text) => {
                    self.rest
                        .insert(key.to_owned(), serde_json::Value::String(text));
                }
                None => {
                    self.rest.remove(key);
                }
            }
        }
    }
}

/// `(group, kind, namespace, name)`, the identity a resource is indexed by (MF-06).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ResourceId {
    /// The API group segment before `/` in `apiVersion` (MF-06).
    pub group: String,
    /// Manifest kind name (MF-06).
    pub kind: String,
    /// Resource namespace, absent for organization-level resources (MF-06).
    pub namespace: Option<String>,
    /// Resource name (MF-06).
    pub name: String,
}

impl ResourceId {
    /// Creates a new resource identifier from individual components (MF-06).
    pub fn new(
        group: impl Into<String>,
        kind: impl Into<String>,
        namespace: Option<String>,
        name: impl Into<String>,
    ) -> Self {
        Self {
            group: group.into(),
            kind: kind.into(),
            namespace,
            name: name.into(),
        }
    }

    /// Derives a resource identifier from a loaded raw manifest envelope (MF-06).
    pub fn from_manifest(manifest: &RawManifest) -> Self {
        let group = match manifest.api_version.split_once('/') {
            Some((g, _)) => g.to_string(),
            None => manifest.api_version.clone(),
        };
        Self {
            group,
            kind: manifest.kind.clone(),
            namespace: manifest.metadata.namespace.clone(),
            name: manifest.metadata.name.clone(),
        }
    }
}

impl fmt::Display for ResourceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.namespace {
            Some(ns) => write!(f, "{}/{}/{}", self.kind, ns, self.name),
            None => write!(f, "{}/{}", self.kind, self.name),
        }
    }
}

/// Errors that can occur when loading an organization repository (CC-08, MF-06).
#[derive(Debug, thiserror::Error)]
pub enum LoadError {
    /// A workspace preview render still holds an organization-unique name without its render
    /// prefix, so it could reach the project it was branched from (CC-78, PF-83).
    #[error("the preview render leaves {what} without the prefix `{prefix}`: {value}")]
    UnprefixedName {
        /// The render prefix, `ws-{name}-`.
        prefix: String,
        /// What kind of name: a namespace, a space segment, an id.
        what: &'static str,
        /// Where it is and what it is.
        value: String,
    },

    /// File path or symlink target escapes repository root (CC-08).
    #[error("path escapes repository root: {path}")]
    PathEscapesRepository {
        /// Relative path that escapes repository root.
        path: PathBuf,
    },

    /// Manifest apiVersion does not match the canonical API version (MF-01).
    #[error("manifest in {path} (document {document}) has invalid apiVersion `{got}`, expected `{expected}`")]
    ApiVersion {
        /// Repository-relative file path where the error occurred.
        path: PathBuf,
        /// 1-based index of YAML document in file.
        document: usize,
        /// Expected API version string.
        expected: &'static str,
        /// Actual API version encountered.
        got: String,
    },

    /// Manifest kind is not recognized by the kind registry (MF-09).
    #[error("manifest in {path} (document {document}) has unknown kind `{kind}`")]
    UnknownKind {
        /// Repository-relative file path where the error occurred.
        path: PathBuf,
        /// 1-based index of YAML document in file.
        document: usize,
        /// Unknown kind name encountered.
        kind: String,
    },

    /// Two manifests share the same resource identity (MF-06).
    #[error("duplicate resource identity {id}: first declared in {first}, duplicate in {second}")]
    DuplicateIdentity {
        /// Conflicting resource identity, boxed to keep `LoadError` small.
        id: Box<ResourceId>,
        /// Repository-relative path of the first declaration.
        first: PathBuf,
        /// Repository-relative path of the duplicate declaration.
        second: PathBuf,
    },

    /// Underlying I/O error occurred while reading the repository (CC-08).
    #[error("I/O error reading {path}: {source}")]
    Io {
        /// Path where the I/O error occurred.
        path: PathBuf,
        /// Underlying I/O error source.
        #[source]
        source: std::io::Error,
    },

    /// Manifest document failed YAML parsing (MF-05).
    #[error("failed to parse YAML in {path} (document {document}, line {line}): {message}")]
    Parse {
        /// Repository-relative file path where the error occurred.
        path: PathBuf,
        /// 1-based index of YAML document in file.
        document: usize,
        /// 1-based line number where the document starts.
        line: usize,
        /// Parser error message.
        message: String,
    },

    /// The environment overlay `JC_ENVIRONMENT` names is not one (CC-73, CC-75).
    #[error("environment overlay {path} is not a valid Environment: {message}")]
    Overlay {
        /// Repository-relative path of the overlay.
        path: PathBuf,
        /// Why it was refused.
        message: String,
    },

    /// `JC_ENVIRONMENT` names an overlay the repository does not hold (CC-73).
    #[error("JC_ENVIRONMENT names `{name}`, and the repository holds no environments/{name}.yaml")]
    NoSuchEnvironment {
        /// The name that was asked for.
        name: String,
    },
}

/// Replaces every `{orgDomain}` of every string of `value`, however deep (CC-74).
///
/// The spec is where a domain appears — a URN, a `q` filter, a host in a source URL — and the
/// metadata is names and labels, which are DNS labels and locales and carry no domain.
pub(crate) fn render_in_place(value: &mut serde_json::Value, org_domain: &str) {
    match value {
        serde_json::Value::String(text) => {
            if text.contains(ORG_DOMAIN_PLACEHOLDER) {
                *text = text.replace(ORG_DOMAIN_PLACEHOLDER, org_domain);
            }
        }
        serde_json::Value::Array(items) => {
            for item in items {
                render_in_place(item, org_domain);
            }
        }
        serde_json::Value::Object(map) => {
            for (_, item) in map.iter_mut() {
                render_in_place(item, org_domain);
            }
        }
        _ => {}
    }
}

/// Every string of `value` that carries `org_domain` written out (CC-74).
fn literal_strings(value: &serde_json::Value, org_domain: &str, found: &mut Vec<String>) {
    match value {
        serde_json::Value::String(text) => {
            if text.contains(org_domain) {
                found.push(text.clone());
            }
        }
        serde_json::Value::Array(items) => {
            for item in items {
                literal_strings(item, org_domain, found);
            }
        }
        serde_json::Value::Object(map) => {
            for item in map.values() {
                literal_strings(item, org_domain, found);
            }
        }
        _ => {}
    }
}

/// What a manifest writes instead of a domain (CC-74).
pub const ORG_DOMAIN_PLACEHOLDER: &str = "{orgDomain}";

/// In-memory indexed repository of loaded manifests (CC-08, MF-06).
#[derive(Debug, Clone, PartialEq)]
pub struct Repository {
    root: PathBuf,
    resources: BTreeMap<ResourceId, LoadedResource>,
    /// The overlay this load rendered with, when `JC_ENVIRONMENT` named one (CC-73).
    environment: Option<String>,
    /// The organization's domain as this load rendered it: the overlay's `orgDomain`, else the
    /// `Organization` manifest's own `spec.domain` (CC-74).
    org_domain: Option<String>,
    /// The hosts of the environment, by component; empty without an overlay.
    hosts: BTreeMap<String, String>,
    /// Manifests that wrote the domain out instead of `{orgDomain}` (CC-74), as
    /// `(resource, path, the string)`. A finding while `dev`'s repository is being migrated,
    /// an error once it is.
    literal_domains: Vec<(ResourceId, PathBuf, String)>,
    /// SharedSpaceReferences whose `endpointRef` names no Endpoint, or one whose audience
    /// leaves the referring project out (EP-77, EP-15), as `(resource, path, why)`.
    unresolved_references: Vec<(ResourceId, PathBuf, String)>,
    /// DataAgreements sharing one `spec.agreementId` (DS-09), as `(resource, path, why)`, one
    /// entry per claiming manifest. A Dataspace Protocol identifier is unique by construction,
    /// and the gateway serves nothing for a contested one (T-2355), so both parties lose their
    /// transfer tokens until the repository says which manifest owns the id.
    contested_agreement_ids: Vec<(ResourceId, PathBuf, String)>,
}

/// The namespace an organization-scoped manifest lives in.
const ORG_NAMESPACE: &str = "org";

/// Whether `text` is an id, or an anchored id pattern, of the organization `domain`: its
/// domain segment is `domain`, with the dots escaped in a pattern.
fn of_organization(text: &str, domain: &str) -> bool {
    let body = text.trim_start_matches('^');
    body.starts_with("urn:ngsi-ld:")
        && body
            .split(':')
            .nth(3)
            .is_some_and(|segment| !domain.is_empty() && segment.replace("\\.", ".") == domain)
}

/// Every id of the organization `domain` written under `value`, whole or inside a longer
/// string such as a mapping (CC-78): the renderer moves whole ids, so an id inside text is
/// what a preview would leak.
fn ids_of(value: &serde_json::Value, domain: &str, found: &mut Vec<String>) {
    match value {
        serde_json::Value::String(text) => {
            for (start, _) in text.match_indices("urn:ngsi-ld:") {
                let id = &text[start..];
                if of_organization(id, domain) {
                    found.push(id.to_owned());
                }
            }
        }
        serde_json::Value::Array(items) => {
            items.iter().for_each(|item| ids_of(item, domain, found))
        }
        serde_json::Value::Object(map) => map.values().for_each(|item| ids_of(item, domain, found)),
        _ => {}
    }
}

/// Prefixes, under `value`, the `{space}` segment of every id of this organization, the
/// `namespace` of a typed reference into one of `projects`, and the `project` of an
/// `endpointRef` (CC-78, EP-77).
fn prefix_references(
    value: &mut serde_json::Value,
    prefix: &str,
    domain: &str,
    projects: &std::collections::BTreeSet<String>,
) {
    match value {
        serde_json::Value::String(text) if of_organization(text, domain) => {
            *text = jc_core::apply_render_prefix(text, prefix);
        }
        serde_json::Value::Array(items) => items
            .iter_mut()
            .for_each(|item| prefix_references(item, prefix, domain, projects)),
        serde_json::Value::Object(map) => {
            for (key, item) in map.iter_mut() {
                match item {
                    serde_json::Value::String(name)
                        if (key == "namespace" || key == "project") && projects.contains(name) =>
                    {
                        *name = format!("{prefix}{name}");
                    }
                    _ => prefix_references(item, prefix, domain, projects),
                }
            }
        }
        _ => {}
    }
}

/// The DataAgreements whose `spec.agreementId` another manifest also claims (DS-09, T-2355).
///
/// A Dataspace Protocol identifier is unique by construction, so a repository holding two is one
/// somebody wrote wrong — or wrote deliberately: the id decides which project's endpoints a
/// transfer token under it may be used in, and whoever may write a manifest in any project could
/// otherwise take another project's id. The gateway therefore serves **neither**, which makes the
/// victim's agreement stop working with nothing to read anywhere; that is why this is a finding
/// and not a warning. Every claiming manifest is named, so the answer is not "one of these is
/// wrong" but "these two claim the same id".
///
/// The id is read from the raw spec: a manifest that does not parse or does not validate is left
/// out of the gateway's table anyway, but it still claims the id in the repository, and a
/// collision that disappears when the other manifest is broken would be a finding that comes and
/// goes with an unrelated typo.
fn contested_agreement_ids(
    resources: &BTreeMap<ResourceId, LoadedResource>,
) -> Vec<(ResourceId, PathBuf, String)> {
    let mut claims: BTreeMap<&str, Vec<(&ResourceId, &LoadedResource)>> = BTreeMap::new();
    for (id, loaded) in resources
        .iter()
        .filter(|(id, _)| id.kind == "DataAgreement")
    {
        let Some(agreement_id) = loaded
            .manifest
            .spec
            .get("agreementId")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|text| !text.is_empty())
        else {
            continue;
        };
        claims.entry(agreement_id).or_default().push((id, loaded));
    }

    let mut found = Vec::new();
    for (agreement_id, claimants) in claims {
        if claimants.len() < 2 {
            continue;
        }
        let names: Vec<String> = claimants
            .iter()
            .map(|(id, _)| match &id.namespace {
                Some(namespace) => format!("{namespace}/{}", id.name),
                None => id.name.clone(),
            })
            .collect();
        for (id, loaded) in &claimants {
            found.push((
                (*id).clone(),
                loaded.path.clone(),
                format!(
                    "agreementId \"{agreement_id}\" is claimed by {} ({}). A Dataspace Protocol \
                     identifier belongs to one agreement, and the gateway serves none of them \
                     while it is contested (DS-09, DS-12).",
                    claimants.len(),
                    names.join(", ")
                ),
            ));
        }
    }
    found
}

/// Renders every SharedSpaceReference's `endpointRef` as the `endpointSlug` this environment
/// minted, and answers the ones it could not (EP-77).
///
/// A name never widens access: the reference resolves only when the source Endpoint's
/// audience admits the referring project (EP-15), so a project cannot learn a slug it may not
/// use by naming the Endpoint.
fn resolve_endpoint_refs(
    resources: &mut BTreeMap<ResourceId, LoadedResource>,
) -> Vec<(ResourceId, PathBuf, String)> {
    use serde_json::Value;
    let endpoints: BTreeMap<(String, String), Value> = resources
        .iter()
        .filter(|(id, _)| id.kind == "Endpoint")
        .filter_map(|(id, loaded)| {
            Some((
                (id.namespace.clone()?, id.name.clone()),
                loaded.manifest.spec.clone(),
            ))
        })
        .collect();
    let mut unresolved = Vec::new();
    for (id, loaded) in resources.iter_mut() {
        if id.kind != "SharedSpaceReference" {
            continue;
        }
        let Some(spec) = loaded.manifest.spec.as_object_mut() else {
            continue;
        };
        let Some(reference) = spec.get("endpointRef") else {
            continue;
        };
        let text = |key: &str| {
            reference
                .get(key)
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned()
        };
        let (project, name) = (text("project"), text("name"));
        let referrer = id.namespace.clone().unwrap_or_default();
        let Some(endpoint) = endpoints.get(&(project.clone(), name.clone())) else {
            unresolved.push((
                id.clone(),
                loaded.path.clone(),
                format!("endpointRef names Endpoint {project}/{name}, which the repository does not hold"),
            ));
            continue;
        };
        let admitted = match endpoint.get("audience").and_then(Value::as_str) {
            Some("public" | "organization") => true,
            _ => {
                referrer == project
                    || endpoint
                        .get("allowedProjects")
                        .and_then(Value::as_array)
                        .is_some_and(|allowed| {
                            allowed.iter().any(|p| p.as_str() == Some(&referrer))
                        })
            }
        };
        match endpoint.get("slug").and_then(Value::as_str) {
            Some(slug) if admitted => {
                spec.remove("endpointRef");
                spec.insert("endpointSlug".to_owned(), Value::String(slug.to_owned()));
            }
            _ => unresolved.push((
                id.clone(),
                loaded.path.clone(),
                format!("Endpoint {project}/{name} is not shared with project {referrer} (EP-15)"),
            )),
        }
    }
    unresolved
}

/// Every regular file under `root`, in deterministic path order (CC-08).
///
/// Links are followed, but anything resolving outside the root is refused rather than read:
/// both the manifest loader and the secret store walk the repository this way, so the
/// containment rule has one implementation, not one per caller.
/// YAML the repository holds that is not a manifest: the authoring source of a data model, a
/// Bento stream, the instance settings (Architecture/06 section 1), and an encrypted secrets
/// file, whose values the secret store decrypts and no walk reads (CC-06).
pub(crate) fn is_native_yaml(file_name: &str) -> bool {
    file_name.ends_with(".linkml.yaml")
        || file_name == "bento.yaml"
        || file_name == crate::model::SETTINGS_FILE
        || crate::secrets::sops::is_encrypted_file(file_name)
}

pub(crate) fn walk_files(root: &Path) -> Result<Vec<walkdir::DirEntry>, LoadError> {
    let canonical_root = root.canonicalize().map_err(|source| LoadError::Io {
        path: root.to_path_buf(),
        source,
    })?;

    let mut entries = Vec::new();
    // Links are followed: a Kubernetes ConfigMap or Secret volume is nothing but symlinks
    // into `..data/`, and a repository mounted that way has to load. Every link is still
    // checked against the root below, so a link out of the tree is refused, not followed
    // (CC-08); a loop is an error WalkDir reports.
    let mut it = WalkDir::new(root).follow_links(true).into_iter();

    loop {
        let entry = match it.next() {
            None => break,
            Some(Ok(entry)) => entry,
            Some(Err(err)) => {
                let path = err.path().unwrap_or(root).to_path_buf();
                // Following links, walkdir reports a dangling link as an error on the
                // link itself: nothing to read, so it is skipped with a warning.
                if path.is_symlink() {
                    let rel = path.strip_prefix(root).unwrap_or(&path).display();
                    tracing::warn!(path = %rel, %err, "unreadable link skipped");
                    continue;
                }
                return Err(LoadError::Io {
                    path,
                    source: err.into(),
                });
            }
        };

        if entry.depth() == 0 {
            continue;
        }

        let rel_path = entry
            .path()
            .strip_prefix(root)
            .unwrap_or(entry.path())
            .to_path_buf();
        // The containment check runs on the resolved target, link or not, before the walk
        // descends: a link whose target leaves the root is refused, never followed (CC-08).
        match entry.path().canonicalize() {
            Ok(canonical) if canonical.starts_with(&canonical_root) => {}
            Ok(_) => return Err(LoadError::PathEscapesRepository { path: rel_path }),
            Err(source) => {
                return Err(LoadError::Io {
                    path: rel_path,
                    source,
                })
            }
        }

        if entry.file_type().is_dir()
            && entry
                .file_name()
                .to_str()
                .is_some_and(|s| s.starts_with('.'))
        {
            it.skip_current_dir();
            continue;
        }

        entries.push(entry);
    }

    entries.sort_by(|a, b| a.path().cmp(b.path()));

    // What an entry is comes from the target, never from the link itself (walkdir reports
    // the target's type when links are followed): with the link's own type a
    // ConfigMap-mounted repository loaded as empty, silently, and an empty endpoint table
    // looks exactly like a routing bug (EP-03).
    entries.retain(|entry| {
        if entry.file_type().is_file() {
            return true;
        }
        if !entry.file_type().is_dir() {
            let rel = entry.path().strip_prefix(root).unwrap_or(entry.path());
            tracing::warn!(path = %rel.display(), "entry is neither a file nor a directory, skipped");
        }
        false
    });

    Ok(entries)
}

/// The slug a preview Endpoint answers on: the first 160 bits of SHA-256 over the prefix and
/// the origin's slug, in lowercase base32, so it is opaque like every slug (EP-02), the same on
/// every render and never the origin's (Architecture/06 §7.2).
pub fn preview_slug(prefix: &str, slug: &str) -> String {
    use sha2::{Digest, Sha256};
    const ALPHABET: &[u8; 32] = b"abcdefghijklmnopqrstuvwxyz234567";
    let digest = Sha256::digest(format!("{prefix}{slug}").as_bytes());
    let mut bits = 0u32;
    let mut held = 0u32;
    let mut out = String::with_capacity(32);
    for byte in &digest[..20] {
        bits = (bits << 8) | u32::from(*byte);
        held += 8;
        while held >= 5 {
            held -= 5;
            out.push(char::from(ALPHABET[((bits >> held) & 31) as usize]));
        }
    }
    out
}

impl Repository {
    /// Loads and indexes an organization repository from a directory path (CC-08, MF-06),
    /// rendered with the environment `JC_ENVIRONMENT` names (CC-73).
    pub fn load(root: &Path) -> Result<Self, LoadError> {
        let environment = std::env::var("JC_ENVIRONMENT")
            .ok()
            .filter(|value| !value.trim().is_empty());
        Self::load_for(root, environment.as_deref())
    }

    /// A workspace preview: the repository rendered with `prefix` (`ws-{name}-`) in front of
    /// every organization-unique identity (CC-78, ADR-N-024).
    ///
    /// Project namespaces are prefixed, and with them every `{project}-{name}` space segment
    /// (PF-84); a pinned segment is prefixed too, and so is the `{space}` segment of every id of
    /// this organization a spec writes, through [`jc_core::apply_render_prefix`]. Nothing is
    /// renamed at rest: the files keep their names and paths (CC-77). A render that still holds
    /// an unprefixed namespace, space segment or id of this organization is refused, so a
    /// preview cannot write into the project it came from by construction (PF-83). An empty
    /// prefix is the ordinary load.
    pub fn load_preview(
        root: &Path,
        environment: Option<&str>,
        prefix: &str,
    ) -> Result<Self, LoadError> {
        let mut repo = Self::load_for(root, environment)?;
        if prefix.is_empty() {
            return Ok(repo);
        }
        let domain = repo.org_domain.clone().unwrap_or_default();
        let projects: std::collections::BTreeSet<String> = repo
            .resources
            .keys()
            .filter_map(|id| id.namespace.clone())
            .filter(|namespace| namespace != ORG_NAMESPACE)
            .collect();
        let prefixed = |name: &str| format!("{prefix}{name}");
        let mut rendered = BTreeMap::new();
        for (mut id, mut loaded) in std::mem::take(&mut repo.resources) {
            if let Some(namespace) = id.namespace.as_mut().filter(|ns| projects.contains(*ns)) {
                *namespace = prefixed(namespace);
                loaded.manifest.metadata.namespace = Some(namespace.clone());
            }
            // A Project is named in `org`; its name is the namespace everything else lives in.
            if id.kind == "Project" {
                id.name = prefixed(&id.name);
                loaded.manifest.metadata.name = id.name.clone();
            }
            // A slug is unique in the organization too: the preview's Endpoint answers beside
            // the origin's, never in its place.
            if id.kind == "Endpoint" {
                if let Some(slug) = loaded.manifest.spec.get_mut("slug") {
                    if let Some(text) = slug.as_str() {
                        *slug = serde_json::Value::String(preview_slug(prefix, text));
                    }
                }
            }
            if id.kind == "ContextSpace" {
                if let Some(pin) = loaded.manifest.spec.get_mut("urnSegment") {
                    if let Some(text) = pin.as_str() {
                        *pin = serde_json::Value::String(prefixed(text));
                    }
                }
            }
            prefix_references(&mut loaded.manifest.spec, prefix, &domain, &projects);
            rendered.insert(id, loaded);
        }
        repo.resources = rendered;
        repo.check_prefixed(prefix, &domain)?;
        Ok(repo)
    }

    /// Refuses a preview render that holds an organization-unique name without `prefix`
    /// (CC-78).
    fn check_prefixed(&self, prefix: &str, domain: &str) -> Result<(), LoadError> {
        let unprefixed = |what, value: String| LoadError::UnprefixedName {
            prefix: prefix.to_owned(),
            what,
            value,
        };
        for (id, loaded) in &self.resources {
            if let Some(namespace) = id.namespace.as_deref() {
                if namespace != ORG_NAMESPACE && !namespace.starts_with(prefix) {
                    return Err(unprefixed("a project namespace", format!("{id}")));
                }
                if id.kind == "ContextSpace" {
                    let segment = self.space_segment(namespace, &id.name);
                    if !segment.starts_with(prefix) {
                        return Err(unprefixed("a space segment", format!("{id}: {segment}")));
                    }
                }
            }
            let mut ids = Vec::new();
            ids_of(&loaded.manifest.spec, domain, &mut ids);
            if let Some(id_text) = ids.into_iter().find(|text| {
                text.split(':')
                    .nth(4)
                    .is_some_and(|space| !space.starts_with(prefix))
            }) {
                return Err(unprefixed(
                    "an id of this organization",
                    format!("{id}: {id_text}"),
                ));
            }
        }
        Ok(())
    }

    /// The same load, with the environment named rather than read from the process (CC-73).
    pub fn load_for(root: &Path, environment: Option<&str>) -> Result<Self, LoadError> {
        let mut resources: BTreeMap<ResourceId, LoadedResource> = BTreeMap::new();

        for entry in walk_files(root)? {
            let rel_path = entry
                .path()
                .strip_prefix(root)
                .unwrap_or(entry.path())
                .to_path_buf();

            let file_name = match entry.file_name().to_str() {
                Some(s) => s,
                None => continue,
            };

            if !file_name.ends_with(".yaml") && !file_name.ends_with(".yml") {
                continue;
            }

            if is_native_yaml(file_name) {
                continue;
            }

            let content =
                std::fs::read_to_string(entry.path()).map_err(|source| LoadError::Io {
                    path: rel_path.clone(),
                    source,
                })?;

            let chunks = parse_yaml_documents(&content);
            for chunk in chunks {
                if is_empty_doc(&chunk.content) {
                    continue;
                }

                let manifest: RawManifest = match serde_norway::from_str(&chunk.content) {
                    Ok(m) => m,
                    Err(e) => {
                        return Err(LoadError::Parse {
                            path: rel_path,
                            document: chunk.document_index,
                            line: chunk.start_line,
                            message: e.to_string(),
                        });
                    }
                };

                if !jc_core::serves(&manifest.kind, &manifest.api_version) {
                    return Err(LoadError::ApiVersion {
                        path: rel_path,
                        document: chunk.document_index,
                        expected: jc_core::API_VERSION,
                        got: manifest.api_version,
                    });
                }

                if registry::by_kind(&manifest.kind).is_none() {
                    return Err(LoadError::UnknownKind {
                        path: rel_path,
                        document: chunk.document_index,
                        kind: manifest.kind,
                    });
                }

                let id = ResourceId::from_manifest(&manifest);
                if let Some(existing) = resources.get(&id) {
                    return Err(LoadError::DuplicateIdentity {
                        id: Box::new(id),
                        first: existing.path.clone(),
                        second: rel_path,
                    });
                }

                resources.insert(
                    id,
                    LoadedResource {
                        path: rel_path.clone(),
                        document: chunk.document_index,
                        line: chunk.start_line,
                        manifest,
                    },
                );
            }
        }

        // One repository, every environment (CC-73, CC-74): the overlay `JC_ENVIRONMENT` names
        // is merged over the manifests before anything validates them, so a manifest carries
        // `{orgDomain}` and never a host of its own.
        let wanted = environment
            .map(str::to_owned)
            .filter(|value| !value.trim().is_empty());
        let mut environment = None;
        let mut org_domain = None;
        let mut hosts = BTreeMap::new();
        if let Some(name) = wanted {
            let overlay = resources
                .iter()
                .find(|(id, _)| id.kind == "Environment" && id.name == name)
                .ok_or_else(|| LoadError::NoSuchEnvironment { name: name.clone() })?;
            let (_, loaded) = overlay;
            let path = loaded.path.clone();
            let spec: jc_core::kinds::EnvironmentSpec =
                serde_json::from_value(loaded.manifest.spec.clone()).map_err(|err| {
                    LoadError::Overlay {
                        path: path.clone(),
                        message: err.to_string(),
                    }
                })?;
            spec.validate().map_err(|err| LoadError::Overlay {
                path,
                message: err.to_string(),
            })?;
            org_domain = spec.org_domain.clone();
            hosts = spec.hosts.clone();
            environment = Some(name);
        }
        // Without an overlay, or one that sets no domain, the organization's own is what every
        // manifest rendered with until now.
        if org_domain.is_none() {
            org_domain = resources
                .iter()
                .find(|(id, _)| id.kind == "Organization")
                .and_then(|(_, loaded)| {
                    loaded
                        .manifest
                        .spec
                        .get("domain")
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_owned)
                });
        }
        let mut literal_domains = Vec::new();
        if let Some(domain) = org_domain.as_deref() {
            for (id, loaded) in resources.iter_mut() {
                // An `Environment` is where a domain is written out; every other manifest
                // writes `{orgDomain}` and is rendered here (CC-74).
                if id.kind != "Environment" {
                    let mut written = Vec::new();
                    literal_strings(&loaded.manifest.spec, domain, &mut written);
                    for text in written {
                        literal_domains.push((id.clone(), loaded.path.clone(), text));
                    }
                }
                render_in_place(&mut loaded.manifest.spec, domain);
            }
        }

        let unresolved_references = resolve_endpoint_refs(&mut resources);
        let contested_agreement_ids = contested_agreement_ids(&resources);

        Ok(Self {
            root: root.to_path_buf(),
            resources,
            environment,
            org_domain,
            hosts,
            literal_domains,
            unresolved_references,
            contested_agreement_ids,
        })
    }

    /// The strings that wrote the organization's domain out where `{orgDomain}` belongs
    /// (CC-74), as `(resource, path, the string)`.
    pub fn literal_domains(&self) -> &[(ResourceId, PathBuf, String)] {
        &self.literal_domains
    }

    /// The SharedSpaceReferences whose `endpointRef` did not resolve (EP-77), as
    /// `(resource, path, why)`.
    pub fn unresolved_references(&self) -> &[(ResourceId, PathBuf, String)] {
        &self.unresolved_references
    }

    /// The DataAgreements that claim an `agreementId` another manifest also claims (DS-09), as
    /// `(resource, path, why)`, one entry per claiming manifest.
    pub fn contested_agreement_ids(&self) -> &[(ResourceId, PathBuf, String)] {
        &self.contested_agreement_ids
    }

    /// The `bento.yaml` files beside Pipeline manifests that type a space segment or the
    /// project's name as a string literal where `env("JC_SPACE")` belongs (CC-83, PL-57), as
    /// `(pipeline, bento path, 1-based line, the literal)`.
    pub fn literal_spaces(&self) -> Vec<(ResourceId, PathBuf, usize, String)> {
        let mut found = Vec::new();
        for (id, resource) in self.iter().filter(|(id, _)| id.kind == "Pipeline") {
            let Some(project) = id.namespace.as_deref() else {
                continue;
            };
            let bento = resource
                .path
                .parent()
                .map(|dir| dir.join("bento.yaml"))
                .unwrap_or_else(|| PathBuf::from("bento.yaml"));
            let Ok(text) = std::fs::read_to_string(self.root.join(&bento)) else {
                continue;
            };
            let segments: Vec<String> = self
                .iter()
                .filter(|(space, _)| {
                    space.kind == "ContextSpace" && space.namespace.as_deref() == Some(project)
                })
                .map(|(space, _)| self.space_segment(project, &space.name))
                .collect();
            let mut names: Vec<&str> = segments.iter().map(String::as_str).collect();
            names.push(project);
            names.sort_unstable();
            names.dedup();
            for (line, literal) in crate::bento::literal_names(&text, &names) {
                found.push((id.clone(), bento.clone(), line, literal));
            }
        }
        found
    }

    /// The overlay this repository was loaded with, when `JC_ENVIRONMENT` named one (CC-73).
    pub fn environment(&self) -> Option<&str> {
        self.environment.as_deref()
    }

    /// The organization's domain as this load rendered it (CC-74).
    pub fn org_domain(&self) -> Option<&str> {
        self.org_domain.as_deref()
    }

    /// The hosts of the environment, by component; empty without an overlay.
    pub fn hosts(&self) -> &BTreeMap<String, String> {
        &self.hosts
    }

    /// Looks up a loaded resource by its identity (MF-06).
    pub fn get(&self, id: &ResourceId) -> Option<&LoadedResource> {
        self.resources.get(id)
    }

    /// The `{space}` segment of the Context Space `name` of `project` (PF-84).
    ///
    /// A space the repository does not hold, or cannot parse, renders without a pin: an id
    /// minted for it can then never land in a space that pins another segment.
    pub fn space_segment(&self, project: &str, name: &str) -> String {
        let pin = self
            .resources
            .iter()
            .find(|(id, _)| {
                id.kind == "ContextSpace"
                    && id.name == name
                    && id.namespace.as_deref() == Some(project)
            })
            .and_then(|(_, resource)| resource.manifest.spec.get("urnSegment"))
            .and_then(|pin| pin.as_str().map(str::to_owned));
        jc_core::kinds::urn_segment(project, name, pin.as_deref())
    }

    /// Iterates over all loaded resources in deterministic `ResourceId` sorted order (CC-18).
    pub fn iter(&self) -> impl Iterator<Item = (&ResourceId, &LoadedResource)> {
        self.resources.iter()
    }

    /// Returns the total count of loaded resources in the repository.
    pub fn len(&self) -> usize {
        self.resources.len()
    }

    /// Returns `true` if the repository contains no loaded resources.
    pub fn is_empty(&self) -> bool {
        self.resources.is_empty()
    }

    /// Returns the root path of the organization repository (CC-08).
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Computes the expected repository path for a resource based on its kind template (MF-06).
    pub fn expected_path(&self, id: &ResourceId) -> Option<String> {
        let loaded = self.resources.get(id)?;
        let kind_info = registry::by_kind(&id.kind)?;
        let project = id.namespace.as_deref().unwrap_or("");
        let space = extract_space(&loaded.manifest.spec);
        Some(kind_info.repo_path(project, space, &id.name))
    }

    /// Lists resources whose repository path differs from their canonical path template (MF-06).
    pub fn misplaced(&self) -> Vec<(ResourceId, PathBuf, String)> {
        let mut result = Vec::new();
        for (id, loaded) in &self.resources {
            if let Some(expected) = self.expected_path(id) {
                if loaded.path != Path::new(&expected) {
                    result.push((id.clone(), loaded.path.clone(), expected));
                }
            }
        }
        result
    }
}

/// The `{space}` placeholder of a path template: every space-scoped kind names it
/// `contextSpaceRef` (Architecture/06 section 2).
///
/// Two kinds write it two ways and both are the same space: most specs carry the bare
/// DNS-1123 label, while `Policy` and the federation kinds carry a typed `{kind, name}`
/// reference (MF-07). Reading only the first form put every `Policy` at
/// `projects/{project}/spaces//policies/…` and made `validate` report a correctly placed
/// manifest as misplaced.
pub(crate) fn extract_space(spec: &serde_json::Value) -> &str {
    match spec.get("contextSpaceRef") {
        Some(serde_json::Value::String(name)) => name,
        Some(serde_json::Value::Object(reference)) => reference
            .get("name")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default(),
        _ => "",
    }
}

/// Whether a YAML document carries nothing but blanks, comments and `...`.
pub fn is_empty_doc(s: &str) -> bool {
    s.lines().all(|line| {
        let t = line.trim();
        t.is_empty() || t.starts_with('#') || t == "..."
    })
}

/// One YAML document of a file, located well enough to report on.
pub struct DocChunk {
    /// 1-based index of the document inside the file.
    pub document_index: usize,
    /// 1-based line the document starts at.
    pub start_line: usize,
    /// The document text.
    pub content: String,
}

/// A line that starts a YAML document: `---` at column 0. An indented `---` belongs to
/// a block scalar (a `linkml:` payload, say) and must not split the file.
fn is_doc_separator(line: &str) -> bool {
    let line = line.trim_end();
    line == "---"
        || line
            .strip_prefix("---")
            .is_some_and(|r| r.starts_with([' ', '\t']))
}

/// Splits a file into its YAML documents, keeping the 1-based document index and start
/// line of each for diagnostics. Empty documents count towards the index, as they do in
/// the YAML stream, so `document 3` means the third `---` block a reader sees.
pub fn parse_yaml_documents(text: &str) -> Vec<DocChunk> {
    let mut chunks: Vec<DocChunk> = Vec::new();
    let mut current: Vec<&str> = Vec::new();
    let mut start_line = 1;

    for (i, line) in text.lines().enumerate() {
        if !is_doc_separator(line) {
            current.push(line);
            continue;
        }
        // A leading `---` opens the first document rather than closing an empty one.
        let file_start = chunks.is_empty() && current.iter().all(|l| is_empty_doc(l));
        if !file_start {
            chunks.push(DocChunk {
                document_index: chunks.len() + 1,
                start_line,
                content: current.join("\n"),
            });
        }
        current.clear();
        start_line = i + 1;
    }

    if !current.is_empty() {
        chunks.push(DocChunk {
            document_index: chunks.len() + 1,
            start_line,
            content: current.join("\n"),
        });
    }

    chunks
}
