//! `kind: SyncSource` and the `kind: Bundle` download index (T-0121, MF-17..MF-19, MF-27..MF-32).
//!
//! Shapes follow `docs/Architecture/06-configuration-as-code.md` section 6 verbatim: the
//! source is an externally tagged `git` / `bundle` / `platformApi` object, the schedule is
//! `{ interval: 30m }` or `{ webhook: true }`, and every remote credential is a `secretRef`
//! (MF-31) — there is no field an inline token could be written into.

use crate::envelope::{Kind, ObjectMeta, Scope, SecretRef};
use crate::error::{Error, Result};
use crate::names;
use crate::registry;
use chrono::{DateTime, Utc};
use regex::Regex;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fmt;
use std::sync::LazyLock;

static INTERVAL_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^([1-9][0-9]*)([smhd])$").expect("valid regex"));
static LABEL_KEY_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^([a-z0-9]([-a-z0-9.]*[a-z0-9])?/)?[A-Za-z0-9]([-A-Za-z0-9_.]*[A-Za-z0-9])?$")
        .expect("valid regex")
});
static HEX_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[0-9a-f]{7,40}$").expect("valid regex"));

/// What a [`SyncSourceSpec`] follows (MF-27).
///
/// Exactly one of the three members is set. This is the shape the docs print
/// (`source: { git: { … } }`) and, unlike an externally tagged Rust enum, it survives the
/// YAML round-trip without a `!Git` tag.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SyncOrigin {
    /// A remote Git repository, optionally a subtree of it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git: Option<GitOrigin>,
    /// A published bundle URL (MF-17).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bundle: Option<BundleOrigin>,
    /// Another instance's public resource API (MF-32).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub platform_api: Option<PlatformApiOrigin>,
}

/// A remote Git repository as a sync origin.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct GitOrigin {
    /// Clone URL; `https://` or an `ssh://`/`git@` remote, never plain `http://`.
    pub url: String,
    /// Branch, tag or commit to follow.
    #[serde(rename = "ref")]
    pub git_ref: String,
    /// Subtree to sync; the whole repository when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// Read-only credential, resolved by the reconciler (MF-31).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secret_ref: Option<SecretRef>,
}

/// A published bundle URL as a sync origin (MF-17).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BundleOrigin {
    /// URL of the bundle archive or `kind: Bundle` index; `https://` only.
    pub url: String,
    /// Credential for a protected bundle URL (MF-31).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secret_ref: Option<SecretRef>,
}

/// Another platform instance's public resource API as a sync origin (MF-32).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PlatformApiOrigin {
    /// Base URL of the remote instance, `https://` only.
    pub base_url: String,
    /// Project slug to follow on the remote instance.
    pub project: String,
    /// Credential for the remote resource API (MF-31).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secret_ref: Option<SecretRef>,
}

impl SyncOrigin {
    /// The credential reference of whichever origin is set, if it has one.
    pub fn secret_ref(&self) -> Option<&SecretRef> {
        if let Some(o) = &self.git {
            return o.secret_ref.as_ref();
        }
        if let Some(o) = &self.bundle {
            return o.secret_ref.as_ref();
        }
        self.platform_api
            .as_ref()
            .and_then(|o| o.secret_ref.as_ref())
    }

    fn validate(&self) -> Result<()> {
        let set = [
            self.git.is_some(),
            self.bundle.is_some(),
            self.platform_api.is_some(),
        ]
        .iter()
        .filter(|b| **b)
        .count();
        if set != 1 {
            return Err(Error::Name {
                field: "source",
                value: set.to_string(),
                reason: "exactly one of git, bundle or platformApi must be set",
            });
        }

        if let Some(o) = &self.git {
            validate_remote_url("source.git.url", &o.url, true)?;
            if o.git_ref.trim().is_empty() {
                return Err(Error::Name {
                    field: "source.git.ref",
                    value: o.git_ref.clone(),
                    reason: "ref must not be empty",
                });
            }
            if let Some(ref p) = o.path {
                names::validate_relative_path("source.git.path", p)?;
            }
        }
        if let Some(o) = &self.bundle {
            validate_remote_url("source.bundle.url", &o.url, false)?;
        }
        if let Some(o) = &self.platform_api {
            validate_remote_url("source.platformApi.baseUrl", &o.base_url, false)?;
            names::validate_dns1123_label(&o.project)
                .map_err(|e| names::rename(e, "source.platformApi.project"))?;
        }
        if let Some(sec) = self.secret_ref() {
            names::validate_dns1123_label(&sec.name)
                .map_err(|e| names::rename(e, "source.secretRef.name"))?;
        }
        Ok(())
    }
}

/// How often a [`SyncSourceSpec`] runs (MF-28): `{ interval: 30m }` or `{ webhook: true }`.
///
/// Exactly one of the two members is set, for the same reason as [`SyncOrigin`].
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Schedule {
    /// Fixed interval such as `30m`, `6h` or `1d` (suffixes `s`, `m`, `h`, `d`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interval: Option<String>,
    /// Driven by a webhook from the source instead of a timer; only `true` is a schedule.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub webhook: Option<bool>,
}

impl Schedule {
    /// A fixed-interval schedule.
    pub fn interval(spec: impl Into<String>) -> Self {
        Self {
            interval: Some(spec.into()),
            webhook: None,
        }
    }

    /// A webhook-driven schedule.
    pub fn webhook() -> Self {
        Self {
            interval: None,
            webhook: Some(true),
        }
    }

    /// Length of the interval in seconds; `None` for a webhook or malformed schedule.
    pub fn interval_seconds(&self) -> Option<u64> {
        let caps = INTERVAL_RE.captures(self.interval.as_deref()?)?;
        let n: u64 = caps[1].parse().ok()?;
        let unit = match &caps[2] {
            "s" => 1,
            "m" => 60,
            "h" => 3600,
            _ => 86_400,
        };
        n.checked_mul(unit)
    }

    fn validate(&self) -> Result<()> {
        match (&self.interval, self.webhook) {
            (Some(raw), None) => match self.interval_seconds() {
                // ponytail: one minute is the floor the reconciler's import gates can keep up
                // with; raise it here if the sync loop ever gets a cheaper dry-run path.
                Some(secs) if secs >= 60 => Ok(()),
                Some(_) => Err(Error::Name {
                    field: "schedule.interval",
                    value: raw.clone(),
                    reason: "interval must be at least 1m",
                }),
                None => Err(Error::Name {
                    field: "schedule.interval",
                    value: raw.clone(),
                    reason: "interval must be a positive number followed by s, m, h or d",
                }),
            },
            (None, Some(true)) => Ok(()),
            (None, Some(false)) => Err(Error::Name {
                field: "schedule.webhook",
                value: "false".to_string(),
                reason: "webhook: false is not a schedule, give an interval instead",
            }),
            _ => Err(Error::Name {
                field: "schedule",
                value: String::new(),
                reason: "exactly one of interval or webhook must be set",
            }),
        }
    }
}

/// What a sync run does with the synced subtree.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum SyncMode {
    /// The source keeps winning; local edits show up as drift.
    Mirror,
    /// Copied once, then detached.
    Oneshot,
}

/// What an import does when a resource already exists (MF-23).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum ConflictPolicy {
    /// Abort the whole run.
    Fail,
    /// Keep the local resource.
    Skip,
    /// Overwrite with the remote resource.
    Replace,
    /// Import the remote resource under a new name.
    Rename,
}

/// The inbound credential of a webhook-driven [`SyncSourceSpec`] (MF-44).
///
/// The signature the origin sends covers the request body and not the path it posts to, so one
/// secret shared between sources would let the origin of any one of them force a run of every
/// other, in projects it has no binding in. Each source therefore carries its own, by reference
/// like every other credential (MF-24): what a department is handed opens their door and nothing
/// else.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WebhookAuth {
    /// The secret the origin signs the request body with, HMAC-SHA256.
    pub secret_ref: SecretRef,
    /// The secret being retired, accepted alongside the current one so that moving the origin's
    /// own hook to the new value is a separate step from writing it here — without a window,
    /// every rotation is an outage and nobody rotates (T-0982).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous_secret_ref: Option<SecretRef>,
}

/// Desired specification of a [`SyncSource`][crate::kinds::SyncSource] resource (MF-27..MF-32).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct SyncSourceSpec {
    /// Where the resources come from (MF-27).
    pub source: SyncOrigin,
    /// How often the run happens (MF-28).
    pub schedule: Schedule,
    /// Mirror the source continuously or copy it once.
    pub mode: SyncMode,
    /// Label selector narrowing which resources of the source are synced (MF-10).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub selector: BTreeMap<String, String>,
    /// What to do when a resource already exists locally (MF-23).
    pub conflict_policy: ConflictPolicy,
    /// Whether resources missing from the source are deleted; deletions are never implied (CC-19).
    #[serde(default)]
    pub prune: bool,
    /// Whether a clean sync merge request merges itself (CC-70, MF-29).
    #[serde(default)]
    pub auto_merge: bool,
    /// What authorises a run through the webhook route (MF-44). Required by, and only
    /// meaningful with, `schedule: { webhook: true }`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub webhook: Option<WebhookAuth>,
}

impl Kind for SyncSourceSpec {
    const KIND: &'static str = "SyncSource";
    const PLURAL: &'static str = "syncsources";
    const SCOPE: Scope = Scope::Project;
    const PATH_TEMPLATE: &'static str = "projects/{project}/sync/{name}.yaml";

    fn validate_spec(&self, meta: &ObjectMeta) -> Result<()> {
        names::validate_dns1123_label(&meta.name)?;
        self.validate()
    }
}

impl SyncSourceSpec {
    /// Validates origin URLs, schedule, selector labels and credential references.
    pub fn validate(&self) -> Result<()> {
        self.source.validate()?;
        self.schedule.validate()?;
        // A webhook schedule with no credential would be a door with no lock, and a credential
        // with no webhook schedule a lock on no door: both are refused where they are written,
        // rather than discovered as a source that never runs (MF-44).
        match (self.schedule.webhook == Some(true), &self.webhook) {
            (true, None) => {
                return Err(Error::Invalid {
                    field: "spec.webhook".to_string(),
                    reason: "a webhook-driven source is authorised by its own secret: set \
                             spec.webhook.secretRef (MF-44)"
                        .to_string(),
                })
            }
            (false, Some(_)) => {
                return Err(Error::Invalid {
                    field: "spec.webhook".to_string(),
                    reason: "only a source with schedule: { webhook: true } is reached through \
                             the webhook route (MF-44)"
                        .to_string(),
                })
            }
            (true, Some(auth)) => {
                auth.secret_ref.validate("spec.webhook.secretRef")?;
                if let Some(previous) = &auth.previous_secret_ref {
                    previous.validate("spec.webhook.previousSecretRef")?;
                }
            }
            (false, None) => {}
        }
        for (key, value) in &self.selector {
            if !LABEL_KEY_RE.is_match(key) || key.len() > 253 {
                return Err(Error::Name {
                    field: "selector",
                    value: key.clone(),
                    reason: "selector key must be a Kubernetes label key",
                });
            }
            if value.len() > 63 || (!value.is_empty() && !LABEL_KEY_RE.is_match(value)) {
                return Err(Error::Name {
                    field: "selector",
                    value: value.clone(),
                    reason: "selector value must be a Kubernetes label value",
                });
            }
        }
        Ok(())
    }

    /// Whether a change to this SyncSource lands in the red lane (CC-70, CC-19).
    ///
    /// Merging without review, and deleting what the source dropped, are both decisions a
    /// person has to make.
    pub fn requires_red_lane(&self) -> bool {
        self.auto_merge || self.prune
    }
}

/// One resource listed in a [`BundleSpec`] index.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct BundleItem {
    /// Manifest kind of the exported resource; must be a kind this crate knows.
    pub kind: String,
    /// Namespace of the resource, absent for organization-scoped kinds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub namespace: Option<String>,
    /// `metadata.name` of the resource.
    pub name: String,
    /// Path of the manifest inside the bundle archive.
    pub path: String,
}

impl BundleItem {
    fn validate(&self) -> Result<()> {
        if registry::by_kind(&self.kind).is_none() {
            return Err(Error::Name {
                field: "items.kind",
                value: self.kind.clone(),
                reason: "unknown manifest kind",
            });
        }
        names::validate_dns1123_label(&self.name).map_err(|e| names::rename(e, "items.name"))?;
        if let Some(ref ns) = self.namespace {
            names::validate_namespace(ns).map_err(|e| names::rename(e, "items.namespace"))?;
        }
        names::validate_relative_path("items.path", &self.path)
    }
}

/// Desired specification of a [`Bundle`][crate::kinds::Bundle] download index (MF-17, MF-18).
///
/// A Bundle is generated on download and never stored in the repository; it travels inside
/// the archive so the import wizard knows what it received and from where.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct BundleSpec {
    /// When the bundle was produced.
    pub exported_at: DateTime<Utc>,
    /// Who produced it (a user or ServiceAccount name).
    pub exported_by: String,
    /// Base URL of the instance it came from, so a re-import can be traced (MF-25).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_instance: Option<String>,
    /// Git revision the export was taken at, 7 to 40 lowercase hexadecimal characters.
    pub source_revision: String,
    /// The exported resources.
    pub items: Vec<BundleItem>,
    /// Native files carried next to their envelopes (`bento.yaml`, `*.linkml.yaml`) (MF-17).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub native_files: Vec<String>,
    /// The SHA-256 of every file the bundle holds, over the bytes as exported (MF-42). An import
    /// compares what it wrote against these, so a transfer between instances is verified before
    /// the source is deleted. A bundle from an older exporter carries none, and an import then
    /// reports nothing verified rather than reporting everything equal.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub files: Vec<BundleFile>,
    /// How many resources the caller was not allowed to read; names are never disclosed (MF-18).
    #[serde(default)]
    pub omitted: u32,
    /// What the bundle holds, in prose: the text of the `README.md` a complete export carries
    /// at its root, so a stream export says the same thing as an archive (MF-41).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub readme: Option<String>,
    /// The schemas a complete export carries beside its manifests (MF-41).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schemas: Option<BundleSchemas>,
    /// The git bundles of a git-native export, one per repository (MF-45): the project's and
    /// one per application of it. An import verifies each by its head commit (MF-46).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub repositories: Vec<BundleRepository>,
}

/// One repository of a git-native export (MF-45).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct BundleRepository {
    /// The repository's name, and the name it is created under at the target.
    pub name: String,
    /// What the repository is.
    pub role: BundleRole,
    /// The `git bundle` file, relative to the index.
    pub file: String,
    /// The commit the repository's default branch ends at, 40 lowercase hexadecimal characters.
    pub head: String,
}

/// What a repository of a git-native export is (MF-45, PF-85).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum BundleRole {
    /// An organization repository.
    Organization,
    /// A project repository.
    Project,
    /// The repository of one application (AP-72).
    Application,
}

/// One file of a bundle and the checksum a transfer is verified against (MF-42).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct BundleFile {
    /// Path inside the bundle, as the archive carries it.
    pub path: String,
    /// Lowercase hexadecimal SHA-256 of the file's bytes as exported.
    pub sha256: String,
}

/// The schemas of a complete export: one per kind in the bundle, and the LinkML source and
/// JSON Schema of every `DataModel` it holds (MF-41).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct BundleSchemas {
    /// Kind name to its JSON Schema (draft-07).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub kinds: BTreeMap<String, serde_json::Value>,
    /// DataModel name to `{linkml, jsonSchema}`; a member is absent when neither the
    /// repository nor Model Tools could produce it.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub models: BTreeMap<String, serde_json::Value>,
}

impl Kind for BundleSpec {
    const KIND: &'static str = "Bundle";
    const PLURAL: &'static str = "bundles";
    const SCOPE: Scope = Scope::Organization;
    const PATH_TEMPLATE: &'static str = "bundle.yaml";

    fn validate_spec(&self, meta: &ObjectMeta) -> Result<()> {
        names::validate_dns1123_label(&meta.name)?;
        self.validate()
    }
}

impl BundleSpec {
    /// Validates the index: known kinds, unique identities, relative paths, a real revision.
    pub fn validate(&self) -> Result<()> {
        if self.exported_by.trim().is_empty() {
            return Err(Error::Name {
                field: "exportedBy",
                value: self.exported_by.clone(),
                reason: "exportedBy must not be empty",
            });
        }
        if !HEX_RE.is_match(&self.source_revision) {
            return Err(Error::Name {
                field: "sourceRevision",
                value: self.source_revision.clone(),
                reason: "sourceRevision must be 7 to 40 lowercase hexadecimal characters",
            });
        }
        if let Some(ref url) = self.source_instance {
            validate_remote_url("sourceInstance", url, false)?;
        }
        if self.items.is_empty() && self.repositories.is_empty() {
            return Err(Error::Name {
                field: "items",
                value: String::new(),
                reason: "a bundle must list at least one resource, or one repository (MF-45)",
            });
        }
        let mut names_seen: Vec<&str> = Vec::with_capacity(self.repositories.len());
        for repository in &self.repositories {
            names::validate_dns1123_label(&repository.name)
                .map_err(|e| names::rename(e, "repositories.name"))?;
            if names_seen.contains(&repository.name.as_str()) {
                return Err(Error::Name {
                    field: "repositories.name",
                    value: repository.name.clone(),
                    reason: "a repository is listed once (MF-45)",
                });
            }
            names_seen.push(&repository.name);
            names::validate_relative_path("repositories.file", &repository.file)?;
            if repository.head.len() != 40 || !HEX_RE.is_match(&repository.head) {
                return Err(Error::Name {
                    field: "repositories.head",
                    value: repository.head.clone(),
                    reason: "a head is a full commit id, 40 lowercase hexadecimal characters",
                });
            }
        }

        let mut seen: Vec<(&str, Option<&str>, &str)> = Vec::with_capacity(self.items.len());
        for item in &self.items {
            item.validate()?;
            let id = (
                item.kind.as_str(),
                item.namespace.as_deref(),
                item.name.as_str(),
            );
            if seen.contains(&id) {
                return Err(Error::Name {
                    field: "items",
                    value: format!("{}/{}", item.kind, item.name),
                    reason: "duplicate (kind, namespace, name) in the bundle index (MF-06)",
                });
            }
            seen.push(id);
        }

        for file in &self.native_files {
            names::validate_relative_path("nativeFiles", file)?;
        }
        // An import reads and writes where these point, so none leaves the bundle (MF-42).
        for file in &self.files {
            names::validate_relative_path("files.path", &file.path)?;
        }
        Ok(())
    }

    /// Whether the index lists this resource identity (MF-06).
    pub fn contains(&self, kind: &str, namespace: Option<&str>, name: &str) -> bool {
        self.items
            .iter()
            .any(|i| i.kind == kind && i.namespace.as_deref() == namespace && i.name == name)
    }
}

impl fmt::Display for SyncMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Mirror => "mirror",
            Self::Oneshot => "oneshot",
        })
    }
}

/// Rejects `http://` and anything that is not a URL we are willing to fetch (MF-31, MF-32).
fn validate_remote_url(field: &'static str, url: &str, allow_ssh: bool) -> Result<()> {
    let ok = url.starts_with("https://")
        || (allow_ssh && (url.starts_with("ssh://") || url.starts_with("git@")));
    if !ok {
        return Err(Error::Name {
            field,
            value: url.to_string(),
            reason: if allow_ssh {
                "url must be https://, ssh:// or git@ — plaintext http is refused"
            } else {
                "url must be https:// — plaintext http is refused"
            },
        });
    }
    Ok(())
}
