//! Manifest kind for LinkML Data Models and generated schema artifacts (T-0117, DM-01..DM-53).

use crate::envelope::{Kind, ObjectMeta, Scope, TypedRef};
use crate::error::{Error, Result};
use crate::names;
use chrono::{DateTime, Utc};
use regex::Regex;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::fmt;
use std::str::FromStr;
use std::sync::LazyLock;

static SEMVER_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$")
        .expect("valid regex for SemVer")
});
static COMMIT_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[0-9a-f]{7,40}$").expect("valid regex"));
static SHA256_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[0-9a-f]{64}$").expect("valid regex"));

/// Semantic version string conforming to `major.minor.patch` (DM-22).
///
/// Pre-release identifiers and build metadata are not used by the platform and are rejected.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SemVer(String);

impl SemVer {
    /// Creates a validated [`SemVer`] instance (DM-22).
    pub fn new(s: &str) -> Result<Self> {
        let caps = SEMVER_RE.captures(s).ok_or_else(|| Error::Name {
            field: "version",
            value: s.to_string(),
            reason: "version must follow semantic version format `major.minor.patch` with non-negative integers and no leading zeros (DM-22)",
        })?;

        // Ensure major, minor, and patch values fit within u32.
        caps[1].parse::<u32>().map_err(|_| Error::Name {
            field: "version",
            value: s.to_string(),
            reason: "major version exceeds maximum u32",
        })?;
        caps[2].parse::<u32>().map_err(|_| Error::Name {
            field: "version",
            value: s.to_string(),
            reason: "minor version exceeds maximum u32",
        })?;
        caps[3].parse::<u32>().map_err(|_| Error::Name {
            field: "version",
            value: s.to_string(),
            reason: "patch version exceeds maximum u32",
        })?;

        Ok(Self(s.to_string()))
    }

    /// Returns a string slice of the semantic version.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Returns the major version number (DM-22).
    pub fn major(&self) -> u32 {
        let mut parts = self.0.split('.');
        parts.next().and_then(|p| p.parse().ok()).unwrap_or(0)
    }

    /// Returns the minor version number (DM-22).
    pub fn minor(&self) -> u32 {
        let mut parts = self.0.split('.');
        parts.next();
        parts.next().and_then(|p| p.parse().ok()).unwrap_or(0)
    }

    /// Returns the patch version number (DM-22).
    pub fn patch(&self) -> u32 {
        let mut parts = self.0.split('.');
        parts.next();
        parts.next();
        parts.next().and_then(|p| p.parse().ok()).unwrap_or(0)
    }
}

impl AsRef<str> for SemVer {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for SemVer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl FromStr for SemVer {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self> {
        Self::new(s)
    }
}

impl Serialize for SemVer {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for SemVer {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        SemVer::new(&s).map_err(serde::de::Error::custom)
    }
}

impl JsonSchema for SemVer {
    fn schema_name() -> String {
        "SemVer".to_string()
    }

    fn json_schema(_gen: &mut schemars::gen::SchemaGenerator) -> schemars::schema::Schema {
        let schema = schemars::schema::SchemaObject {
            instance_type: Some(schemars::schema::InstanceType::String.into()),
            string: Some(Box::new(schemars::schema::StringValidation {
                pattern: Some(r"^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$".to_string()),
                ..Default::default()
            })),
            metadata: Some(Box::new(schemars::schema::Metadata {
                description: Some(
                    "Semantic version string conforming to major.minor.patch (DM-22)".to_string(),
                ),
                ..Default::default()
            })),
            ..Default::default()
        };
        schemars::schema::Schema::Object(schema)
    }
}

/// Lifecycle state of a DataModel version (DM-26, DM-48).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum DataModelLifecycle {
    /// Draft data model under active development (DM-26).
    Draft,
    /// Published immutable data model version (DM-22, DM-26).
    Published,
    /// Deprecated data model version superseded by a newer release (DM-26).
    Deprecated,
    /// Retired data model version accepting no new references (DM-26).
    Retired,
    /// Read-only mirror of a foreign data model (DM-48, DM-49).
    Mirrored,
}

impl DataModelLifecycle {
    /// Returns the kebab-case wire name for this lifecycle state.
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Draft => "draft",
            Self::Published => "published",
            Self::Deprecated => "deprecated",
            Self::Retired => "retired",
            Self::Mirrored => "mirrored",
        }
    }

    /// Returns `true` if transitioning from `self` to `next` is allowed by the DM-26 state machine.
    pub fn allows_transition_to(&self, next: Self) -> bool {
        if *self == next {
            return true;
        }
        matches!(
            (*self, next),
            (Self::Draft, Self::Published)
                | (Self::Published, Self::Deprecated)
                | (Self::Deprecated, Self::Retired)
        )
    }
}

impl fmt::Display for DataModelLifecycle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

/// Provenance of a data model, absent for a hand-authored one (DM-08, DM-48).
///
/// Either the upstream `repository`/`path`/`commit` triple of an imported model or the
/// `remote` block of a mirrored foreign model — never both.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct DataModelSource {
    /// Upstream repository the model was imported from (DM-08).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repository: Option<String>,
    /// Path inside the upstream repository (DM-08).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// Pinned upstream commit, 7 to 40 lowercase hexadecimal characters (DM-08).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
    /// Remote endpoint a foreign model was mirrored from (DM-48).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote: Option<RemoteSource>,
}

/// Where a mirrored foreign data model came from (DM-48, DM-49).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct RemoteSource {
    /// Schema surface the model was fetched from; `https://` only.
    pub url: String,
    /// Version the peer published.
    pub version: SemVer,
    /// SHA-256 of the fetched source, 64 lowercase hexadecimal characters; a change opens a merge request.
    pub sha256: String,
    /// When the fetch happened.
    pub fetched_at: DateTime<Utc>,
}

impl DataModelSource {
    /// Whether this model is a mirrored foreign one (DM-48).
    pub fn is_remote(&self) -> bool {
        self.remote.is_some()
    }

    fn validate(&self) -> Result<()> {
        let imported = self.repository.is_some() || self.path.is_some() || self.commit.is_some();
        if imported && self.remote.is_some() {
            return Err(Error::Name {
                field: "source",
                value: String::new(),
                reason: "a model is either imported or mirrored, never both (DM-08, DM-48)",
            });
        }
        if !imported && self.remote.is_none() {
            return Err(Error::Name {
                field: "source",
                value: String::new(),
                reason: "source must carry either repository/path/commit or remote; omit it for a hand-authored model",
            });
        }

        if imported {
            let (Some(repository), Some(path), Some(commit)) =
                (&self.repository, &self.path, &self.commit)
            else {
                return Err(Error::Name {
                    field: "source",
                    value: String::new(),
                    reason: "an imported model needs repository, path and commit together (DM-08)",
                });
            };
            if !repository.starts_with("https://") {
                return Err(Error::Name {
                    field: "source.repository",
                    value: repository.clone(),
                    reason: "repository must be an https:// URL",
                });
            }
            names::validate_relative_path("source.path", path)?;
            if !COMMIT_RE.is_match(commit) {
                return Err(Error::Name {
                    field: "source.commit",
                    value: commit.clone(),
                    reason: "commit must be 7 to 40 lowercase hexadecimal characters",
                });
            }
        }

        if let Some(remote) = &self.remote {
            if !remote.url.starts_with("https://") {
                return Err(Error::Name {
                    field: "source.remote.url",
                    value: remote.url.clone(),
                    reason: "remote url must be an https:// URL",
                });
            }
            if !SHA256_RE.is_match(&remote.sha256) {
                return Err(Error::Name {
                    field: "source.remote.sha256",
                    value: remote.sha256.clone(),
                    reason: "sha256 must be 64 lowercase hexadecimal characters",
                });
            }
        }
        Ok(())
    }
}

/// Artifacts generated beside the LinkML source and committed in the same change (DM-02).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct GeneratedArtifacts {
    /// Generated JSON Schema draft-07 (DM-02, DM-03).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub json_schema: Option<String>,
    /// Generated JSON-LD `@context` (DM-02, DM-05).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<String>,
    /// Generated Markdown documentation (DM-02).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub docs: Option<String>,
    /// Generated and validated example entity (DM-02, DM-21).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub example: Option<String>,
}

impl GeneratedArtifacts {
    /// The four artifact paths, in DM-02 order, with the field name each belongs to.
    fn entries(&self) -> [(&'static str, Option<&String>); 4] {
        [
            ("artifacts.jsonSchema", self.json_schema.as_ref()),
            ("artifacts.context", self.context.as_ref()),
            ("artifacts.docs", self.docs.as_ref()),
            ("artifacts.example", self.example.as_ref()),
        ]
    }
}

/// Desired specification of a [`DataModel`][crate::kinds::DataModel] resource (DM-01..DM-53).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct DataModelSpec {
    /// DNS-1123 label of the owning ContextSpace; with `metadata.namespace` it derives the path (MF-06).
    pub context_space_ref: String,
    /// Path of the authoring LinkML source, relative to this manifest and ending `.linkml.yaml` (DM-01).
    pub linkml: String,
    /// Semantic version; the major is the served `schema/v{major}` (DM-22).
    pub version: SemVer,
    /// Lifecycle state of this version (DM-26, DM-48).
    pub lifecycle: DataModelLifecycle,
    /// NGSI-LD entity types this model defines.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub classes: Vec<String>,
    /// Provenance; absent for a hand-authored model (DM-08, DM-48).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<DataModelSource>,
    /// Artifacts generated beside the source in the same commit (DM-02).
    #[serde(default)]
    pub artifacts: GeneratedArtifacts,
    /// Whether entities of this model may carry undeclared attributes (DM-28).
    #[serde(default)]
    pub open_world: bool,
    /// Informational list of resources consuming this version (DM-25).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub consumers: Vec<TypedRef>,
}

impl Kind for DataModelSpec {
    const KIND: &'static str = "DataModel";
    const PLURAL: &'static str = "datamodels";
    const SCOPE: Scope = Scope::Project;
    const PATH_TEMPLATE: &'static str = "projects/{project}/spaces/{space}/datamodels/{name}.yaml";

    fn validate_spec(&self, meta: &ObjectMeta) -> Result<()> {
        names::validate_dns1123_label(&meta.name)?;
        self.validate()
    }

    fn context_space(&self) -> Option<&str> {
        Some(&self.context_space_ref)
    }
}

impl DataModelSpec {
    /// Validates the space reference, paths, lifecycle invariants and provenance.
    pub fn validate(&self) -> Result<()> {
        names::validate_dns1123_label(&self.context_space_ref)
            .map_err(|e| names::rename(e, "contextSpaceRef"))?;

        names::validate_relative_path("linkml", &self.linkml)?;
        if !self.linkml.ends_with(".linkml.yaml") {
            return Err(Error::Name {
                field: "linkml",
                value: self.linkml.clone(),
                reason: "the authoring source must be a `.linkml.yaml` file (DM-01)",
            });
        }

        for class in &self.classes {
            names::validate_entity_type(class).map_err(|e| names::rename(e, "classes"))?;
        }

        for (field, path) in self.artifacts.entries() {
            match path {
                Some(p) => names::validate_relative_path(field, p)?,
                None if self.lifecycle == DataModelLifecycle::Published => {
                    return Err(Error::Name {
                        field,
                        value: String::new(),
                        reason: "a published model commits all four generated artifacts (DM-02)",
                    })
                }
                None => {}
            }
        }

        let mirrored = self.lifecycle == DataModelLifecycle::Mirrored;
        let remote = self.source.as_ref().is_some_and(DataModelSource::is_remote);
        if mirrored != remote {
            return Err(Error::Name {
                field: "source.remote",
                value: self.lifecycle.as_str().to_string(),
                reason: "lifecycle `mirrored` and `source.remote` imply each other (DM-48)",
            });
        }

        if let Some(source) = &self.source {
            source.validate()?;
        }

        for consumer in &self.consumers {
            names::validate_dns1123_label(&consumer.name)?;
            if let Some(ref ns) = consumer.namespace {
                names::validate_namespace(ns)?;
            }
        }

        Ok(())
    }

    /// Returns `true` if this data model version can be referenced by Endpoints and Pipelines (DM-26).
    pub fn can_be_referenced(&self) -> bool {
        self.lifecycle == DataModelLifecycle::Published
    }

    /// Returns `true` if transitioning from current lifecycle to `next` is permitted (DM-26).
    pub fn allows_transition_to(&self, next: DataModelLifecycle) -> bool {
        self.lifecycle.allows_transition_to(next)
    }
}
