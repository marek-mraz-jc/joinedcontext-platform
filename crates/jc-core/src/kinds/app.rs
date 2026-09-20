//! `kind: App`, an AI-generated application with a least-privilege endpoint (T-0119, AP-01..AP-20).
//!
//! **Security note**: the security property of this kind is that an App has nowhere to put a
//! secret. `AppSpec` and every nested struct carry `deny_unknown_fields`, so `secretRef:`,
//! `secret:`, `token:` and friends are rejected at parse time (AP-16). An app that needs
//! external data declares a Pipeline instead.

use crate::envelope::{Kind, ObjectMeta, Ref, Scope};
use crate::error::{Error, Result};
use crate::kinds::endpoint::Representation;
use crate::kinds::policy::OperationRef;
use crate::names;
use regex::Regex;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::fmt;
use std::sync::LazyLock;

static TOOLCHAIN_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[a-z][a-z0-9-]*$").expect("valid regex"));
static DURATION_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^P(?:\d+Y)?(?:\d+M)?(?:\d+W)?(?:\d+D)?(?:T(?:\d+H)?(?:\d+M)?(?:\d+S)?)?$")
        .expect("valid regex")
});
static ATTR_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[A-Za-z][A-Za-z0-9_]{0,63}$").expect("valid regex"));

/// How an app is built and served (AP-01).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum AppClass {
    /// A built front end served from the Portal static host, acting with the user's token (AP-07, AP-14).
    Static,
    /// A backend running in the instance namespace with its own service account (AP-08, AP-15).
    Service,
    /// Backend and front end in one image.
    Fullstack,
}

/// Who may reach a published app (AP-18).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum AppVisibility {
    /// Only the author.
    Private,
    /// Members of the owning project.
    Project,
    /// Anyone in the organization.
    Organization,
    /// Everyone, unauthenticated.
    Public,
}

/// Lifecycle state of an app (AP-18, AP-19, AP-20).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum AppLifecycle {
    /// Being written; not deployed.
    #[default]
    Draft,
    /// Bound to a sandbox space, reachable by the author and reviewers only (AP-19).
    Preview,
    /// Bound to the real space and reachable by its `visibility` audience (AP-18).
    Published,
    /// Withdrawn; kept for the record.
    Retired,
}

impl AppLifecycle {
    /// Wire name of this state.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Draft => "draft",
            Self::Preview => "preview",
            Self::Published => "published",
            Self::Retired => "retired",
        }
    }

    /// Whether `draft → preview → published → retired` allows this step (AP-18).
    ///
    /// Every state may stay itself; nothing moves backwards; `retired` is terminal.
    pub fn allows_transition_to(&self, next: Self) -> bool {
        if *self == next {
            return true;
        }
        matches!(
            (self, next),
            (Self::Draft, Self::Preview)
                | (Self::Preview, Self::Published)
                | (Self::Published, Self::Retired)
                | (Self::Preview, Self::Retired)
                | (Self::Draft, Self::Retired)
        )
    }
}

/// Where the app's source lives; exactly one member is set (AP-02).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct AppSource {
    /// Path beside the manifest in the org repository.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// A repository of the same forge (AP-02).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git: Option<GitSource>,
}

/// A source repository on the organization's own forge (AP-02).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct GitSource {
    /// Clone URL on the organization's forge; `https://` only.
    pub url: String,
    /// Branch, tag or commit.
    #[serde(rename = "ref")]
    pub git_ref: String,
    /// Subdirectory holding the app.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
}

impl AppSource {
    fn validate(&self) -> Result<()> {
        match (&self.path, &self.git) {
            (Some(path), None) => validate_relative_path("source.path", path),
            (None, Some(git)) => {
                if !git.url.starts_with("https://") {
                    return Err(Error::Name {
                        field: "source.git.url",
                        value: git.url.clone(),
                        reason: "the forge URL must be https://",
                    });
                }
                if git.git_ref.trim().is_empty() {
                    return Err(Error::Name {
                        field: "source.git.ref",
                        value: git.git_ref.clone(),
                        reason: "ref must not be empty",
                    });
                }
                match &git.path {
                    Some(p) => validate_relative_path("source.git.path", p),
                    None => Ok(()),
                }
            }
            _ => Err(Error::Name {
                field: "source",
                value: String::new(),
                reason: "exactly one of path or git must be set (AP-02)",
            }),
        }
    }
}

/// Toolchain versions CI builds the app with, e.g. `{ rust: "1.90", node: "22" }` (AP-01, AP-11).
///
/// Kept as a map rather than a fixed set of fields: the build image, not this crate, decides
/// which toolchains exist.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(transparent)]
pub struct AppBuild(pub BTreeMap<String, String>);

impl AppBuild {
    fn validate(&self) -> Result<()> {
        if self.0.is_empty() {
            return Err(Error::Name {
                field: "build",
                value: String::new(),
                reason: "build must pin at least one toolchain version (AP-11)",
            });
        }
        for (toolchain, version) in &self.0 {
            if !TOOLCHAIN_RE.is_match(toolchain) {
                return Err(Error::Name {
                    field: "build",
                    value: toolchain.clone(),
                    reason: "toolchain name must be a lowercase identifier",
                });
            }
            if version.trim().is_empty() {
                return Err(Error::Name {
                    field: "build",
                    value: toolchain.clone(),
                    reason: "toolchain version must be pinned, not empty (AP-11)",
                });
            }
        }
        Ok(())
    }
}

/// Temporal narrowing of a data need, e.g. `{ window: P1D }` (AP-05).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct TemporalConstraint {
    /// ISO 8601 duration reaching back from now.
    pub window: String,
}

/// Geographic narrowing of a data need, e.g. `{ within: { scopeRef: /geo/SK/BB } }` (AP-05).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct GeoConstraint {
    /// Scope tree the app may read inside.
    pub within: GeoWithin,
}

/// The scope a [`GeoConstraint`] confines the app to (ADR 005).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct GeoWithin {
    /// Absolute scope string, e.g. `/geo/SK/BB`.
    pub scope_ref: String,
}

/// One declared data need, the input the reconciler renders a Policy from (AP-04, AP-05).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct DataNeed {
    /// The context space the app reads.
    pub context_space_ref: Ref,
    /// NGSI-LD entity types the app needs.
    pub types: Vec<String>,
    /// Attributes the app needs; empty means every readable attribute of those types.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attrs: Vec<String>,
    /// CIM 009 clause 4.20 operation names the app performs (R8).
    pub operations: Vec<OperationRef>,
    /// NGSI-LD query narrowing the readable set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub q: Option<String>,
    /// Scope query narrowing the readable set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope_q: Option<String>,
    /// Geographic narrowing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub geo_q: Option<GeoConstraint>,
    /// Temporal narrowing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temporal_q: Option<TemporalConstraint>,
    /// Representations of the rendered endpoint this need contributes (AP-05, EP-08).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub representations: Vec<Representation>,
}

impl DataNeed {
    /// Whether this need asks for an operation that changes context data (AP-09).
    pub fn has_write(&self) -> bool {
        self.operations.iter().any(OperationRef::is_write)
    }

    fn validate(&self) -> Result<()> {
        names::validate_space_name(self.context_space_ref.name())
            .map_err(|e| rename(e, "dataNeeds.contextSpaceRef"))?;
        if let Some(kind) = self.context_space_ref.kind() {
            if kind != "ContextSpace" {
                return Err(Error::Kind {
                    expected: "ContextSpace",
                    got: kind.to_string(),
                });
            }
        }

        if self.types.is_empty() {
            return Err(Error::Name {
                field: "dataNeeds.types",
                value: String::new(),
                reason: "a data need must name at least one entity type (AP-05)",
            });
        }
        let mut seen = BTreeSet::new();
        for entity_type in &self.types {
            names::validate_entity_type(entity_type).map_err(|e| rename(e, "dataNeeds.types"))?;
            if !seen.insert(entity_type) {
                return Err(Error::Name {
                    field: "dataNeeds.types",
                    value: entity_type.clone(),
                    reason: "duplicate entity type",
                });
            }
        }

        let mut seen_attrs = BTreeSet::new();
        for attr in &self.attrs {
            if !ATTR_RE.is_match(attr) {
                return Err(Error::Name {
                    field: "dataNeeds.attrs",
                    value: attr.clone(),
                    reason: "attribute name must be an NGSI-LD term",
                });
            }
            if !seen_attrs.insert(attr) {
                return Err(Error::Name {
                    field: "dataNeeds.attrs",
                    value: attr.clone(),
                    reason: "duplicate attribute",
                });
            }
        }

        if self.operations.is_empty() {
            return Err(Error::Name {
                field: "dataNeeds.operations",
                value: String::new(),
                reason: "a data need must name at least one operation (AP-05, R8)",
            });
        }

        let mut seen_reps = BTreeSet::new();
        for representation in &self.representations {
            if !seen_reps.insert(*representation) {
                return Err(Error::Name {
                    field: "dataNeeds.representations",
                    value: representation.as_str().to_string(),
                    reason: "duplicate representation",
                });
            }
        }

        if let Some(temporal) = &self.temporal_q {
            if !DURATION_RE.is_match(&temporal.window) || temporal.window == "P" {
                return Err(Error::Name {
                    field: "dataNeeds.temporalQ.window",
                    value: temporal.window.clone(),
                    reason: "window must be an ISO 8601 duration such as P1D",
                });
            }
        }

        if let Some(geo) = &self.geo_q {
            if !geo.within.scope_ref.starts_with('/') || geo.within.scope_ref.contains("//") {
                return Err(Error::Name {
                    field: "dataNeeds.geoQ.within.scopeRef",
                    value: geo.within.scope_ref.clone(),
                    reason: "scopeRef must be an absolute scope string such as /geo/SK/BB",
                });
            }
        }

        Ok(())
    }
}

/// Per-app runtime limits enforced on its own endpoint (AP-17).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct AppLimits {
    /// Requests per minute allowed on the app endpoint.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requests_per_minute: Option<u32>,
    /// Rows a single file representation download may return.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_file_rows: Option<u32>,
}

impl AppLimits {
    fn validate(&self) -> Result<()> {
        for (field, value) in [
            ("limits.requestsPerMinute", self.requests_per_minute),
            ("limits.maxFileRows", self.max_file_rows),
        ] {
            if value == Some(0) {
                return Err(Error::Name {
                    field,
                    value: "0".to_string(),
                    reason: "a limit of zero blocks the app; omit the field instead",
                });
            }
        }
        Ok(())
    }
}

/// Content Security Policy of a served app (AP-12).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct ContentSecurityPolicy {
    /// `connect-src`; only `self` and https origins, never `*` (AP-12).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub connect_src: Vec<String>,
    /// `frame-ancestors`; defaults to `none` (AP-12).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub frame_ancestors: Vec<String>,
}

impl ContentSecurityPolicy {
    fn validate(&self) -> Result<()> {
        for (field, sources) in [
            ("csp.connectSrc", &self.connect_src),
            ("csp.frameAncestors", &self.frame_ancestors),
        ] {
            for source in sources {
                let ok = source == "self"
                    || source == "none"
                    || source.starts_with("https://") && !source.contains('*');
                if !ok {
                    return Err(Error::Name {
                        field,
                        value: source.clone(),
                        reason: "a CSP source must be `self`, `none` or an https origin without a wildcard (AP-12)",
                    });
                }
            }
        }
        Ok(())
    }
}

/// Desired specification of an [`App`][crate::kinds::App] resource (AP-01..AP-20).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct AppSpec {
    /// How the app is built and served; `spec.kind` in the manifest (AP-01).
    #[serde(rename = "kind")]
    pub class: AppClass,
    /// Where the source lives (AP-02).
    pub source: AppSource,
    /// Toolchain versions CI builds with (AP-11).
    pub build: AppBuild,
    /// Who may reach the published app (AP-18).
    pub visibility: AppVisibility,
    /// Lifecycle state; a manifest without one is a draft (AP-18).
    #[serde(default)]
    pub lifecycle: AppLifecycle,
    /// What the app needs to read or write; the reconciler renders its Endpoint and Policies from this (AP-04, AP-05).
    pub data_needs: Vec<DataNeed>,
    /// Per-app rate limits (AP-17).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limits: Option<AppLimits>,
    /// Content Security Policy of the served app (AP-12).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub csp: Option<ContentSecurityPolicy>,
    /// Whether the Portal may embed the app in a frame (AP-12).
    #[serde(default)]
    pub embeddable: bool,
}

impl Kind for AppSpec {
    const KIND: &'static str = "App";
    const PLURAL: &'static str = "apps";
    const SCOPE: Scope = Scope::Project;
    const PATH_TEMPLATE: &'static str = "projects/{project}/apps/{name}/app.yaml";

    fn validate_spec(&self, meta: &ObjectMeta) -> Result<()> {
        names::validate_dns1123_label(&meta.name)?;
        // The artifact an App runs is named in `status.build`, written back by the build lane
        // in the commit that publishes it. An annotation naming an image or a module is a
        // digest somebody typed, and it would deploy something this platform never built
        // (AP-11, AP-13a).
        for key in BUILT_ANNOTATIONS {
            if meta.annotations.contains_key(key) {
                return Err(Error::Name {
                    field: "metadata.annotations",
                    value: key.to_owned(),
                    reason: "the digest lives in status.build, written by the build lane (AP-13a)",
                });
            }
        }
        self.validate()
    }
}

/// The digest the build lane writes back when it publishes an image (AP-13a), and the digest of
/// a compiled module. Neither is ever written by hand, and neither travels in a bundle.
pub const BUILT_ANNOTATIONS: [&str; 2] = ["joinedcontext.com/image", "joinedcontext.com/module"];

impl AppSpec {
    /// Validates source, build, data needs, limits and CSP.
    pub fn validate(&self) -> Result<()> {
        self.source.validate()?;
        self.build.validate()?;

        if self.data_needs.is_empty() {
            return Err(Error::Name {
                field: "dataNeeds",
                value: String::new(),
                reason:
                    "an app declares what it needs; the endpoint is rendered from it (AP-01, AP-04)",
            });
        }
        for need in &self.data_needs {
            need.validate()?;
        }

        if let Some(limits) = &self.limits {
            limits.validate()?;
        }
        if let Some(csp) = &self.csp {
            csp.validate()?;
        }

        if self.lifecycle == AppLifecycle::Published && self.visibility == AppVisibility::Private {
            return Err(Error::Name {
                field: "visibility",
                value: "private".to_string(),
                reason: "a published app is reachable by its audience; private has none (AP-18)",
            });
        }

        Ok(())
    }

    /// Whether any data need asks for a write operation, which makes the change red lane (AP-09).
    pub fn write_operations(&self) -> bool {
        self.data_needs.iter().any(DataNeed::has_write)
    }

    /// Whether a change to this app must be reviewed in the red lane (AP-09, AP-10).
    pub fn requires_red_lane(&self) -> bool {
        self.write_operations() || self.visibility == AppVisibility::Public
    }

    /// Union of the representations the data needs ask for, the rendered endpoint's set (AP-05).
    pub fn representations(&self) -> BTreeSet<Representation> {
        self.data_needs
            .iter()
            .flat_map(|n| n.representations.iter().copied())
            .collect()
    }
}

impl fmt::Display for AppClass {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Static => "static",
            Self::Service => "service",
            Self::Fullstack => "fullstack",
        })
    }
}

impl fmt::Display for AppLifecycle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Rewrites the `field` of an [`Error::Name`] so the caller sees the manifest path.
fn rename(err: Error, field: &'static str) -> Error {
    match err {
        Error::Name { reason, value, .. } => Error::Name {
            field,
            value,
            reason,
        },
        other => other,
    }
}

fn validate_relative_path(field: &'static str, path: &str) -> Result<()> {
    let trimmed = path.strip_prefix("./").unwrap_or(path);
    if trimmed.is_empty() || trimmed.starts_with('/') || trimmed.split('/').any(|seg| seg == "..") {
        return Err(Error::Name {
            field,
            value: path.to_string(),
            reason: "path must be relative and must not contain a `..` segment",
        });
    }
    Ok(())
}
