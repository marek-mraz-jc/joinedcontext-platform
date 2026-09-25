//! Kubernetes-style resource envelope, metadata, and status types (MF-01..MF-08).

use crate::error::{Error, Result};
use crate::i18n::Text;
use crate::names;
use regex::Regex;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::sync::LazyLock;

/// A key inside a Kubernetes secret (`^[-._a-zA-Z0-9]+$`), which is what `SecretRef.key` names.
static SECRET_KEY_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[-._a-zA-Z0-9]+$").expect("valid regex"));

/// The canonical API version for joinedcontext resources (MF-01).
pub const API_VERSION: &str = "joinedcontext.com/v1alpha1";

/// The version `Pipeline` alone is also served at: sources, steps and outputs (PL-54,
/// ADR-N-023). A `v1alpha1` Pipeline stays valid and reads as the `v1alpha2` one it means.
pub const API_VERSION_V1ALPHA2: &str = "joinedcontext.com/v1alpha2";

/// Whether this platform serves `kind` at `api_version` (MF-01, PL-54): every kind at
/// [`API_VERSION`], and `Pipeline` at [`API_VERSION_V1ALPHA2`] as well.
pub fn serves(kind: &str, api_version: &str) -> bool {
    api_version == API_VERSION || (kind == "Pipeline" && api_version == API_VERSION_V1ALPHA2)
}

/// Trait implemented by every manifest spec kind.
pub trait Kind:
    Serialize + serde::de::DeserializeOwned + JsonSchema + Clone + std::fmt::Debug + PartialEq
{
    /// Manifest kind name (e.g. "ContextSpace").
    const KIND: &'static str;
    /// Plural resource name for URL routing (e.g. "spaces"), the `{plural}` of `/api/v1/projects/{project}/{plural}`.
    const PLURAL: &'static str;
    /// Target scope (Organization or Project).
    const SCOPE: Scope;
    /// Repository path template of this kind (MF-06, Architecture/06 section 2).
    ///
    /// Placeholders are `{project}` (`metadata.namespace`), `{space}` (the owning
    /// ContextSpace, see [`Kind::context_space`]) and `{name}` (`metadata.name`).
    const PATH_TEMPLATE: &'static str;

    /// Where a kind of scope [`Scope::OrganizationOrProject`] lives when it is in a project
    /// (PF-68). `None` for every kind that lives in one place, which is all but `Role`.
    const PROJECT_PATH_TEMPLATE: Option<&'static str> = None;

    /// Where a kind whose project path names a `{space}` lives in a project when no space owns
    /// the manifest (DM-75). `None` for every kind but `DataModel`.
    const SPACELESS_PATH_TEMPLATE: Option<&'static str> = None;

    /// Kind-specific validation of the spec against its own metadata.
    ///
    /// The default checks nothing; every kind that has invariants of its own implements it,
    /// so [`ResourceEnvelope::validate`] is the single entry point for `jcctl`, the Portal
    /// API and CI.
    fn validate_spec(&self, _meta: &ObjectMeta) -> Result<()> {
        Ok(())
    }

    /// Kind-specific agreement of the spec with the envelope's `apiVersion` (PL-54). The
    /// default accepts what [`serves`] accepts; a kind with two versions says which fields
    /// belong to which.
    fn validate_api_version(&self, _api_version: &str) -> Result<()> {
        Ok(())
    }

    /// Name of the ContextSpace this resource lives in, for the `{space}` placeholder.
    ///
    /// Only space-scoped kinds (Endpoint, Policy, DataModel, Mapping) override it.
    fn context_space(&self) -> Option<&str> {
        None
    }

    /// Path of this resource inside the organization repository (MF-06, Architecture/06 section 2).
    ///
    /// Renders [`Kind::PATH_TEMPLATE`], which stays the single source of truth so that
    /// `jcctl`, the reconciler and the Portal (through [`crate::registry`]) all derive the
    /// same path.
    fn repo_path(&self, meta: &ObjectMeta) -> String {
        let namespace = meta.namespace.as_deref().unwrap_or_default();
        let in_project = !namespace.is_empty() && namespace != ORG_NAMESPACE;
        let spaceless = self.context_space().is_none_or(str::is_empty);
        let template = match (Self::PROJECT_PATH_TEMPLATE, Self::SPACELESS_PATH_TEMPLATE) {
            (_, Some(no_space)) if in_project && spaceless => no_space,
            (Some(project), _) if in_project => project,
            _ => Self::PATH_TEMPLATE,
        };
        template
            .replace("{project}", namespace)
            .replace("{space}", self.context_space().unwrap_or_default())
            .replace("{name}", &meta.name)
    }
}

/// The namespace of everything that belongs to the organization rather than to one project.
pub const ORG_NAMESPACE: &str = "org";

/// Target scope hierarchy of a manifest kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub enum Scope {
    /// Organization-level resource (namespace must be "org").
    Organization,
    /// Project-level resource (namespace must be a project slug).
    Project,
    /// A kind that lives in either: the organization's own copy in `org`, or a project's own
    /// copy in that project's namespace. `Role` is the one, so a project defines roles of its
    /// own without reaching outside it (PF-68).
    OrganizationOrProject,
}

impl Scope {
    /// Whether a manifest of this scope may carry the organization's namespace.
    pub fn allows_organization(self) -> bool {
        matches!(self, Scope::Organization | Scope::OrganizationOrProject)
    }

    /// Whether a manifest of this scope may carry a project's namespace.
    pub fn allows_project(self) -> bool {
        matches!(self, Scope::Project | Scope::OrganizationOrProject)
    }
}

/// Kubernetes-style resource envelope (MF-01..MF-04).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(
    deny_unknown_fields,
    rename_all = "camelCase",
    bound(serialize = "S: Kind", deserialize = "S: Kind")
)]
pub struct ResourceEnvelope<S: Kind> {
    /// API version: [`API_VERSION`], or a version [`serves`] accepts for this kind.
    #[serde(deserialize_with = "de_api_version_of::<S, _>")]
    pub api_version: String,
    /// Resource kind name (must match [`Kind::KIND`]).
    #[serde(deserialize_with = "de_kind::<S, _>")]
    pub kind: String,
    /// Metadata identifying and labeling the resource.
    pub metadata: ObjectMeta,
    /// Desired spec state.
    pub spec: S,
    /// Server-computed status. Stripped during export and serialization.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<Status>,
}

/// Deserializes and validates that `apiVersion` matches [`API_VERSION`].
pub fn de_api_version<'de, D>(deserializer: D) -> std::result::Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let s = String::deserialize(deserializer)?;
    if s != API_VERSION {
        return Err(serde::de::Error::custom(Error::ApiVersion(s)));
    }
    Ok(s)
}

/// Deserializes and validates that this platform serves the kind `S` at `apiVersion` (PL-54).
pub fn de_api_version_of<'de, S: Kind, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<String, D::Error> {
    let s = String::deserialize(deserializer)?;
    if !serves(S::KIND, &s) {
        return Err(serde::de::Error::custom(Error::ApiVersion(s)));
    }
    Ok(s)
}

/// Deserializes and validates that `kind` matches [`Kind::KIND`].
pub fn de_kind<'de, S: Kind, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<String, D::Error> {
    let s = String::deserialize(deserializer)?;
    if s != S::KIND {
        return Err(serde::de::Error::custom(Error::Kind {
            expected: S::KIND,
            got: s,
        }));
    }
    Ok(s)
}

/// Validates a list of ISO 639-1 locales and the fallback that must be among them (PF-25).
pub fn validate_locales(locales: &[String], default_locale: &str) -> Result<()> {
    if locales.is_empty() {
        return Err(Error::Name {
            field: "locales",
            value: String::new(),
            reason: "locales list must not be empty",
        });
    }
    names::validate_locale(default_locale)?;
    let mut seen = std::collections::BTreeSet::new();
    for loc in locales {
        names::validate_locale(loc)?;
        if !seen.insert(loc.as_str()) {
            return Err(Error::Name {
                field: "locales",
                value: loc.clone(),
                reason: "duplicate locale in locales list",
            });
        }
    }
    if !seen.contains(default_locale) {
        return Err(Error::MissingFallbackLocale(default_locale.to_string()));
    }
    Ok(())
}

impl<S: Kind> ResourceEnvelope<S> {
    /// Creates a new resource envelope with default [`API_VERSION`] and [`Kind::KIND`].
    pub fn new(metadata: ObjectMeta, spec: S) -> Self {
        Self {
            api_version: API_VERSION.to_string(),
            kind: S::KIND.to_string(),
            metadata,
            spec,
            status: None,
        }
    }

    /// Validates envelope metadata and scope agreement (MF-02).
    pub fn validate(&self) -> Result<()> {
        self.metadata.validate()?;
        let ns = match &self.metadata.namespace {
            Some(ns) => ns.as_str(),
            None => {
                return Err(Error::Name {
                    field: "metadata.namespace",
                    value: String::new(),
                    reason: "namespace is required",
                });
            }
        };

        match S::SCOPE {
            Scope::OrganizationOrProject => {}
            Scope::Organization => {
                if ns != ORG_NAMESPACE {
                    return Err(Error::Name {
                        field: "metadata.namespace",
                        value: ns.to_string(),
                        reason: "organization-scoped resources must have namespace `org`",
                    });
                }
            }
            Scope::Project => {
                if ns == ORG_NAMESPACE {
                    return Err(Error::Name {
                        field: "metadata.namespace",
                        value: ns.to_string(),
                        reason: "project-scoped resources must not have namespace `org`",
                    });
                }
            }
        }

        if let Some(status) = &self.status {
            status.validate()?;
        }
        if !serves(S::KIND, &self.api_version) {
            return Err(Error::ApiVersion(self.api_version.clone()));
        }
        self.spec.validate_api_version(&self.api_version)?;
        self.spec.validate_spec(&self.metadata)
    }

    /// Strips status in-place (MF-04).
    pub fn strip_status(&mut self) {
        self.status = None;
    }

    /// Returns a copy of the envelope without status (MF-04).
    pub fn without_status(mut self) -> Self {
        self.status = None;
        self
    }

    /// Derives the canonical repository file path for this resource (MF-06).
    pub fn resource_path(&self) -> Result<String> {
        self.validate()?;
        Ok(self.spec.repo_path(&self.metadata))
    }

    /// Serializes envelope to YAML, unconditionally stripping server status (MF-04).
    pub fn to_yaml(&self) -> std::result::Result<String, serde_norway::Error> {
        let mut stripped = self.clone();
        stripped.strip_status();
        serde_norway::to_string(&stripped)
    }

    /// Deserializes envelope from YAML.
    pub fn from_yaml(s: &str) -> std::result::Result<Self, serde_norway::Error> {
        serde_norway::from_str(s)
    }

    /// Serializes envelope to pretty JSON, unconditionally stripping server status (MF-04).
    pub fn to_json(&self) -> std::result::Result<String, serde_json::Error> {
        let mut stripped = self.clone();
        stripped.strip_status();
        serde_json::to_string_pretty(&stripped)
    }

    /// Deserializes envelope from JSON.
    pub fn from_json(s: &str) -> std::result::Result<Self, serde_json::Error> {
        serde_json::from_str(s)
    }
}

/// Metadata envelope attached to every platform resource (MF-02).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct ObjectMeta {
    /// Resource name (DNS-1123 label).
    pub name: String,
    /// Resource namespace ("org" or project slug).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub namespace: Option<String>,
    /// Key-value labels for filtering (MF-10).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub labels: BTreeMap<String, String>,
    /// Key-value annotations for provenance and ownership (MF-08).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub annotations: BTreeMap<String, String>,
    /// Human-readable title: one string, or the legacy map per locale (UI-50, PF-24).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<Text>,
    /// Human-readable description: one string, or the legacy map per locale (UI-50, PF-24).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<Text>,
}

impl ObjectMeta {
    /// Creates an [`ObjectMeta`] with name and namespace.
    pub fn new(name: impl Into<String>, namespace: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            namespace: Some(namespace.into()),
            labels: BTreeMap::new(),
            annotations: BTreeMap::new(),
            title: None,
            description: None,
        }
    }

    /// Validates DNS-1123 name and optional namespace.
    pub fn validate(&self) -> Result<()> {
        names::validate_dns1123_label(&self.name)?;
        if let Some(ref ns) = self.namespace {
            names::validate_namespace(ns)?;
        }
        Ok(())
    }
}

/// Well-known platform annotations (MF-08, CC-69).
pub mod annotations {
    /// Managed attributes annotation for server-side attribute ownership (MF-08, CC-69).
    pub const MANAGED_ATTRIBUTES: &str = "joinedcontext.com/managed-attributes";
    /// Provenance tracking annotation for imported bundles (MF-08, MF-20).
    pub const IMPORTED_FROM: &str = "joinedcontext.com/imported-from";
    /// Provenance tracking annotation for SyncSource instances (MF-08, MF-27).
    pub const SYNC_SOURCE: &str = "joinedcontext.com/sync-source";
    /// Provenance tracking annotation for blueprint instantiations (MF-08).
    pub const BLUEPRINT: &str = "joinedcontext.com/blueprint";
    /// Provenance tracking annotation for generator tools (MF-08).
    pub const GENERATED_BY: &str = "joinedcontext.com/generated-by";
    /// Digest of the prompt that created the resource (MF-08).
    pub const PROMPT_DIGEST: &str = "joinedcontext.com/prompt-digest";
}

/// Reference to another resource, either bare name or typed reference (MF-07).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum Ref {
    /// Bare string resource name.
    Name(String),
    /// Typed reference with kind, name, and optional namespace.
    Typed(TypedRef),
}

impl Ref {
    /// Returns the referenced resource name.
    pub fn name(&self) -> &str {
        match self {
            Ref::Name(name) => name.as_str(),
            Ref::Typed(typed) => typed.name.as_str(),
        }
    }

    /// Returns the referenced resource kind if typed.
    pub fn kind(&self) -> Option<&str> {
        match self {
            Ref::Name(_) => None,
            Ref::Typed(typed) => Some(typed.kind.as_str()),
        }
    }

    /// Returns the referenced resource namespace if specified.
    pub fn namespace(&self) -> Option<&str> {
        match self {
            Ref::Name(_) => None,
            Ref::Typed(typed) => typed.namespace.as_deref(),
        }
    }
}

/// Structured typed resource reference (MF-07).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct TypedRef {
    /// Target resource kind.
    pub kind: String,
    /// Target resource name.
    pub name: String,
    /// Target resource namespace (defaults to caller namespace if omitted).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub namespace: Option<String>,
}

/// Reference to a credential stored in SOPS or OpenBao (PF-36).
///
/// `deny_unknown_fields` is a security control: it rejects inline `value:` or `password:`
/// fields at parse time, enforcing that secrets never appear in Git or manifest files.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct SecretRef {
    /// Secret name in secret store.
    pub name: String,
    /// Optional key within secret.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    /// Optional environment variable name for injection into runners.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub env_var: Option<String>,
}

impl SecretRef {
    /// Checks that a reference names a secret and does not carry one (MF-24, MF-35, T-2238).
    ///
    /// `field` is the path a person typed into, e.g. `spec.mqtt.passwordRef`. Two rules, because
    /// either alone lets a credential through: the name is a DNS-1123 label, which is what a
    /// secret in the store can be called, and neither the name nor the key wears the shape of a
    /// minted token — `glpat-…`, `xoxb-…` and `sk-…` are all valid labels, and a token pasted into
    /// the name box would otherwise be committed to Git in the clear.
    ///
    /// No refusal repeats what it refused: the thing pasted is live, and an error message travels
    /// into logs and chats. The field is what the person is told.
    pub fn validate(&self, field: &str) -> Result<()> {
        let refuse = |what: &str, reason: &str| Error::Invalid {
            field: format!("{field}.{what}"),
            reason: reason.to_string(),
        };

        if names::looks_like_a_credential(&self.name) {
            return Err(refuse(
                "name",
                "carries the shape of a credential; this field takes the name of the secret that \
                 holds it, never the credential itself (MF-24)",
            ));
        }
        if names::validate_dns1123_label(&self.name).is_err() {
            return Err(refuse(
                "name",
                "must be the name of a secret in the store: a DNS-1123 label of 1 to 63 \
                 characters (MF-35)",
            ));
        }
        if let Some(key) = &self.key {
            if names::looks_like_a_credential(key) {
                return Err(refuse(
                    "key",
                    "carries the shape of a credential; this field takes the key inside the \
                     secret, never the credential itself (MF-24)",
                ));
            }
            if key.is_empty() || key.len() > 253 || !SECRET_KEY_RE.is_match(key) {
                return Err(refuse(
                    "key",
                    "must be a key inside a Kubernetes secret: ^[-._a-zA-Z0-9]+$, 1 to 253 \
                     characters (MF-35)",
                ));
            }
        }
        Ok(())
    }
}

/// Server-computed lifecycle and reconciliation status (MF-04). Never stored in Git.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Status {
    /// Current lifecycle phase.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phase: Option<Phase>,
    /// Git commit revision observed during reconciliation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_revision: Option<String>,
    /// Reconciler condition transitions.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub conditions: Vec<Condition>,
    /// What the build lane published for an `App`, and the only place its artifact is named
    /// (AP-13a). Written back by the build lane in the commit that publishes the artifact; an
    /// App without it renders no pod, and no other principal may set it (AP-73).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub build: Option<Build>,
}

impl Status {
    /// Checks what a status carries that a person could get wrong (AP-13a).
    pub fn validate(&self) -> Result<()> {
        match &self.build {
            Some(build) => build.validate(),
            None => Ok(()),
        }
    }
}

/// The artifact one build of an `App` published, as the build lane writes it back (AP-13a).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Build {
    /// The artifact's digest, `sha256:` and 64 lowercase hexadecimal characters. The reconciler
    /// deploys this and nothing else (AP-72).
    pub digest: String,
    /// The commit of the source the artifact was built from, 7 to 40 lowercase hexadecimal
    /// characters.
    pub commit: String,
    /// The version of the app SDK the artifact was built against.
    pub sdk_version: String,
    /// When the build lane published it.
    pub built_at: chrono::DateTime<chrono::Utc>,
}

impl Build {
    /// Refuses a digest, a commit or an SDK version nothing could have built (AP-13a).
    pub fn validate(&self) -> Result<()> {
        let hex = |text: &str| {
            text.chars()
                .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c))
        };
        let digest = self
            .digest
            .strip_prefix("sha256:")
            .filter(|rest| rest.len() == 64 && hex(rest));
        if digest.is_none() {
            return Err(Error::Name {
                field: "status.build.digest",
                value: self.digest.clone(),
                reason: "a digest is `sha256:` and 64 lowercase hexadecimal characters (AP-13a)",
            });
        }
        if !(7..=40).contains(&self.commit.len()) || !hex(&self.commit) {
            return Err(Error::Name {
                field: "status.build.commit",
                value: self.commit.clone(),
                reason: "a commit is 7 to 40 lowercase hexadecimal characters (AP-13a)",
            });
        }
        if self.sdk_version.trim().is_empty() {
            return Err(Error::Name {
                field: "status.build.sdkVersion",
                value: self.sdk_version.clone(),
                reason: "the SDK version the artifact was built against is required (AP-13a)",
            });
        }
        Ok(())
    }
}

/// Lifecycle phase enumeration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "PascalCase")]
pub enum Phase {
    /// Resource manifest is in draft state.
    Draft,
    /// Resource is pending review or approval.
    Pending,
    /// Resource deployment is in progress.
    Deploying,
    /// Resource is live and active in cluster.
    Live,
    /// Reconciliation error.
    Error,
    /// The platform holds something the repository does not declare, or no longer holds what it
    /// does (CC-21, UI-25). Configuration cannot reach this phase — every component reads it
    /// from the repository (CC-72) — so it is a space whose seed entities the broker answers
    /// differently, and the two resolutions of UI-26 are what a person does about it.
    Drifted,
}

/// Status condition entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Condition {
    /// Type of condition (e.g. Reconciled, Ready).
    pub r#type: String,
    /// Status value ("True", "False", "Unknown").
    pub status: String,
    /// Machine-readable reason code.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Human-readable message.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    /// Last transition timestamp.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_transition_time: Option<chrono::DateTime<chrono::Utc>>,
}
