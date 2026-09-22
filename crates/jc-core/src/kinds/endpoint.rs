//! Manifest kinds for Endpoints and cross-project sharing references (T-0113, EP-01..EP-28).

use crate::envelope::{Kind, ObjectMeta, Ref, Scope, SecretRef, TypedRef};
use crate::error::{Error, Result};
use crate::names;
use crate::urn::Urn;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::fmt;

/// The opaque endpoint slug of EP-02: at least 128 bits of entropy, lowercase RFC 4648
/// base32 without padding (`^[a-z2-7]{26,}$`).
///
/// 26 base32 characters carry 130 bits of entropy (5 bits/char), whereas 25 characters carry
/// only 125 bits. Therefore, 26 is the minimum length that satisfies the "at least 128 bits"
/// requirement of EP-02.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct EndpointSlug(String);

impl EndpointSlug {
    /// Creates a validated [`EndpointSlug`] checking length and base32 character set (EP-02).
    pub fn new(s: &str) -> Result<Self> {
        if s.len() < 26 {
            return Err(Error::Name {
                field: "slug",
                value: s.to_string(),
                reason: "endpoint slug must be at least 26 characters (>= 128 bits entropy)",
            });
        }
        if !s.chars().all(|c| matches!(c, 'a'..='z' | '2'..='7')) {
            return Err(Error::Name {
                field: "slug",
                value: s.to_string(),
                reason: "endpoint slug must contain only lowercase RFC 4648 base32 characters (`a-z`, `2-7`) without padding",
            });
        }
        Ok(Self(s.to_string()))
    }

    /// Returns a string slice of the endpoint slug.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Verifies that the slug does not encode any forbidden identifier (EP-03).
    ///
    /// Returns `false` when the lowercased slug contains any non-empty `forbidden` substring
    /// of 3 or more characters (shorter fragments would produce false positives on a random slug).
    pub fn is_opaque(&self, forbidden: &[&str]) -> bool {
        let slug_lower = self.0.to_lowercase();
        for term in forbidden {
            let term_lower = term.to_lowercase();
            if term_lower.len() >= 3 && slug_lower.contains(&term_lower) {
                return false;
            }
        }
        true
    }
}

impl AsRef<str> for EndpointSlug {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for EndpointSlug {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl Serialize for EndpointSlug {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for EndpointSlug {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        EndpointSlug::new(&s).map_err(serde::de::Error::custom)
    }
}

impl JsonSchema for EndpointSlug {
    fn schema_name() -> String {
        "EndpointSlug".to_string()
    }

    fn json_schema(_gen: &mut schemars::gen::SchemaGenerator) -> schemars::schema::Schema {
        let schema = schemars::schema::SchemaObject {
            instance_type: Some(schemars::schema::InstanceType::String.into()),
            string: Some(Box::new(schemars::schema::StringValidation {
                pattern: Some("^[a-z2-7]{26,}$".to_string()),
                ..Default::default()
            })),
            metadata: Some(Box::new(schemars::schema::Metadata {
                description: Some(
                    "Cryptographically random endpoint slug with at least 128 bits of entropy (lowercase RFC 4648 base32 without padding, >=26 chars, EP-02)"
                        .to_string(),
                ),
                ..Default::default()
            })),
            ..Default::default()
        };
        schemars::schema::Schema::Object(schema)
    }
}

/// Target audience access control setting for an endpoint (EP-14).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum Audience {
    /// Restricted to explicitly listed projects within the organization.
    ProjectList,
    /// Accessible to any authenticated member or service account of the organization.
    Organization,
    /// Accessible publicly without authentication under the synthetic public role grant.
    Public,
}

impl Audience {
    /// Returns the kebab-case wire name for this audience.
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::ProjectList => "project-list",
            Self::Organization => "organization",
            Self::Public => "public",
        }
    }
}

impl fmt::Display for Audience {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

/// Supported representation formats served under an endpoint (EP-05, Architecture/04).
///
/// Note: `schema` (EP-46) and `access` (EP-55) are not variants: they are unconditional
/// surfaces on every endpoint and cannot be toggled off.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "kebab-case")]
pub enum Representation {
    /// Canonical NGSI-LD representation (`/ngsi-ld/v1/`).
    NgsiLd,
    /// Direct Model Context Protocol streamable HTTP interface (`/mcp`).
    Mcp,
    /// GeoJSON feature collection (`/file.geojson`).
    #[serde(rename = "geojson")]
    GeoJson,
    /// Flat tabular CSV (`/file.csv`).
    Csv,
    /// Flat tabular Excel spreadsheet (`/file.xlsx`).
    Xlsx,
    /// JSON array of entities (`/file.json`).
    Json,
    /// Zipped multi-representation export bundle (`/file.zip`).
    Zip,
    /// OGC API - Features Part 1 collections and items (`/ogc/features/`).
    #[serde(rename = "ogc-features")]
    OgcFeatures,
    /// SensorThings API v1.1 read projection (`/sta/v1.1/`).
    Sta,
}

impl Representation {
    /// Returns the wire name string for this representation.
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::NgsiLd => "ngsi-ld",
            Self::Mcp => "mcp",
            Self::GeoJson => "geojson",
            Self::Csv => "csv",
            Self::Xlsx => "xlsx",
            Self::Json => "json",
            Self::Zip => "zip",
            Self::OgcFeatures => "ogc-features",
            Self::Sta => "sta",
        }
    }
}

impl fmt::Display for Representation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

/// Token-bucket rate limiting configuration per endpoint (EP-20).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct RateLimits {
    /// Maximum allowed requests per minute.
    pub requests_per_minute: u32,
    /// Optional maximum burst capacity above steady rate.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub burst: Option<u32>,
}

impl RateLimits {
    /// Validates rate limit bounds (>= 1).
    pub fn validate(&self) -> Result<()> {
        if self.requests_per_minute == 0 {
            return Err(Error::Name {
                field: "rateLimits.requestsPerMinute",
                value: "0".to_string(),
                reason: "requestsPerMinute must be >= 1",
            });
        }
        if let Some(burst) = self.burst {
            if burst == 0 {
                return Err(Error::Name {
                    field: "rateLimits.burst",
                    value: "0".to_string(),
                    reason: "burst must be >= 1",
                });
            }
        }
        Ok(())
    }
}

/// HTTP caching configuration for endpoint responses.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Caching {
    /// Maximum cache age in seconds.
    pub max_age_seconds: u32,
}

/// Desired specification of an [`Endpoint`][crate::kinds::Endpoint] resource (EP-01..EP-28).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct EndpointSpec {
    /// Reference to the underlying ContextSpace.
    pub context_space_ref: Ref,
    /// Unguessable opaque slug with >= 128 bits of entropy (EP-02).
    pub slug: EndpointSlug,
    /// Audience access scope (EP-14).
    pub audience: Audience,
    /// Whitelist of project slugs permitted when audience is `project-list` (EP-14, EP-15).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allowed_projects: Vec<String>,
    /// Optional NGSI-LD Policy URN attached to this endpoint.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub policy_ref: Option<Urn>,
    /// Set of enabled representation formats (EP-05).
    pub enabled_representations: Vec<Representation>,
    /// Optional rate limiting configuration (EP-20).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_limits: Option<RateLimits>,
    /// Optional ceiling on one `file.*` download (EP-44).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_limits: Option<FileLimits>,
    /// Optional response caching configuration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub caching: Option<Caching>,
    /// Optional publication narrowing applied to every representation (EP-61).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub projection: Option<Projection>,
    /// The named subset of the space's model this Endpoint reads (MP-02): a
    /// `ModelProjection` of the same space, intersected with the caller's grants by the
    /// gateway, so it narrows and never widens.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub projection_ref: Option<TypedRef>,
    /// Where this Endpoint is published beside its own surface: an open-data catalogue so
    /// far (EP-62).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub publish: Option<crate::kinds::ckan::Publication>,
    /// A `Mapping` whose target model this Endpoint serves instead of the space's own
    /// (EP-54, DM-51).
    ///
    /// The gateway translates every entity through the Mapping's compiled IR and rewrites
    /// `attrs`, `q` and `geoQ` back through its inverse. An Endpoint that carries one is
    /// read only: a mapping is invertible per slot, not per entity, so a write in the target
    /// model has no source entity to reconstruct.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub view_mapping_ref: Option<TypedRef>,
    /// Every caller this Endpoint admits holds the role [`endpoint_role`] names for it, on
    /// requests through this Endpoint alone (AP-96, AP-97). The App reconciler sets it, so an
    /// App's grants never reach another surface of the space.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub caller_role: bool,
    /// Roles a caller this Endpoint admits holds when a subject matches them, on requests
    /// through it alone (AP-96, AP-97).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub roles: Vec<EndpointRole>,
}

/// One role an Endpoint gives the callers it admits who match a subject (AP-96, AP-97).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct EndpointRole {
    /// The role's name, `[a-z][a-z0-9-]{0,31}`; the caller holds [`endpoint_role`] of it.
    pub name: String,
    /// Who holds it: `user` by e-mail, `group` by name, never a wildcard.
    pub subjects: Vec<crate::kinds::role::Subject>,
}

/// What every role an Endpoint gives starts with. The gateway drops a role of this prefix that
/// a token or an account asserts, so nothing outside the Endpoint can hold one (AP-97).
pub const ENDPOINT_ROLE_PREFIX: &str = "endpoint:";

/// The role a caller holds on the Endpoint `endpoint` of `project`: its caller role for `None`,
/// the named role for `Some` (AP-96, AP-97). The reconciler assigns Policies to these names and
/// the gateway gives them, so both read this one function.
pub fn endpoint_role(project: &str, endpoint: &str, role: Option<&str>) -> String {
    match role {
        None => format!("{ENDPOINT_ROLE_PREFIX}{project}/{endpoint}"),
        Some(role) => format!("{ENDPOINT_ROLE_PREFIX}{project}/{endpoint}/{role}"),
    }
}

impl Kind for EndpointSpec {
    const KIND: &'static str = "Endpoint";
    const PLURAL: &'static str = "endpoints";
    const SCOPE: Scope = Scope::Project;
    const PATH_TEMPLATE: &'static str = "projects/{project}/spaces/{space}/endpoints/{name}.yaml";

    fn validate_spec(&self, meta: &ObjectMeta) -> Result<()> {
        names::validate_dns1123_label(&meta.name)?;
        self.validate()
    }

    fn context_space(&self) -> Option<&str> {
        Some(self.context_space_ref.name())
    }
}

impl EndpointSpec {
    /// Validates endpoint audience, representations, limits, and policy reference.
    pub fn validate(&self) -> Result<()> {
        let space_name = self.context_space_ref.name();
        names::validate_space_name(space_name)?;
        if let Some(kind) = self.context_space_ref.kind() {
            if kind != "ContextSpace" {
                return Err(Error::Kind {
                    expected: "ContextSpace",
                    got: kind.to_string(),
                });
            }
        }

        match self.audience {
            Audience::ProjectList => {
                if self.allowed_projects.is_empty() {
                    return Err(Error::Name {
                        field: "allowedProjects",
                        value: String::new(),
                        reason: "allowedProjects must not be empty when audience is `project-list`",
                    });
                }
                for p in &self.allowed_projects {
                    names::validate_namespace(p)?;
                }
            }
            Audience::Organization | Audience::Public => {
                if !self.allowed_projects.is_empty() {
                    return Err(Error::Name {
                        field: "allowedProjects",
                        value: format!("{:?}", self.allowed_projects),
                        reason: "allowedProjects must be empty unless audience is `project-list`",
                    });
                }
            }
        }

        if let Some(view) = &self.view_mapping_ref {
            if view.kind != "Mapping" {
                return Err(Error::Kind {
                    expected: "Mapping",
                    got: view.kind.clone(),
                });
            }
            names::validate_dns1123_label(&view.name).map_err(|e| match e {
                Error::Name { reason, .. } => Error::Name {
                    field: "spec.viewMappingRef.name",
                    value: view.name.clone(),
                    reason,
                },
                other => other,
            })?;
        }

        if let Some(projection) = &self.projection_ref {
            if projection.kind != "ModelProjection" {
                return Err(Error::Kind {
                    expected: "ModelProjection",
                    got: projection.kind.clone(),
                });
            }
            names::validate_dns1123_label(&projection.name).map_err(|e| match e {
                Error::Name { reason, .. } => Error::Name {
                    field: "spec.projectionRef.name",
                    value: projection.name.clone(),
                    reason,
                },
                other => other,
            })?;
        }

        if self.enabled_representations.is_empty() {
            return Err(Error::Name {
                field: "enabledRepresentations",
                value: String::new(),
                reason: "enabledRepresentations must not be empty",
            });
        }
        let mut seen = std::collections::BTreeSet::new();
        for rep in &self.enabled_representations {
            if !seen.insert(*rep) {
                return Err(Error::Name {
                    field: "enabledRepresentations",
                    value: rep.as_str().to_string(),
                    reason: "duplicate representation in enabledRepresentations",
                });
            }
        }

        if let Some(ref limits) = self.rate_limits {
            limits.validate()?;
        }

        if let Some(ref limits) = self.file_limits {
            limits.validate()?;
        }

        if let Some(ref projection) = self.projection {
            projection.validate()?;
        }

        if let Some(ref publish) = self.publish {
            publish.validate(&self.enabled_representations)?;
        }

        if let Some(ref policy) = self.policy_ref {
            if policy.entity_type() != "Policy" {
                return Err(Error::Kind {
                    expected: "Policy",
                    got: policy.entity_type().to_string(),
                });
            }
        }

        let mut seen = std::collections::BTreeSet::new();
        for role in &self.roles {
            crate::kinds::app::validate_role_name("roles[].name", &role.name)?;
            if !seen.insert(role.name.as_str()) {
                return Err(Error::Name {
                    field: "roles[].name",
                    value: role.name.clone(),
                    reason: "a role is declared once (AP-96)",
                });
            }
            crate::kinds::app::validate_subjects("roles[].subjects", &role.subjects)?;
        }

        Ok(())
    }

    /// Verifies that the endpoint slug does not encode project, organization, or space names (EP-03).
    pub fn validate_opacity(&self, project: &str, organization: &str) -> Result<()> {
        let space = self.context_space_ref.name();
        if !self.slug.is_opaque(&[space, project, organization]) {
            return Err(Error::Name {
                field: "slug",
                value: self.slug.to_string(),
                reason: "slug must not encode context space, project, or organization name (EP-03)",
            });
        }
        Ok(())
    }
}

/// How much of a `file.*` representation one download may return (EP-44).
///
/// The gateway pages through the broker and stops at the first row that would cross
/// either bound, so a caller gets a refusal rather than a file that is silently short.
/// An absent field leaves the gateway's own ceiling in charge.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct FileLimits {
    /// Rows a single download may return.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_file_rows: Option<u32>,
    /// Bytes a single download may return.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_file_bytes: Option<u64>,
}

impl FileLimits {
    /// Validates that a declared bound is positive: a zero blocks the download entirely.
    pub fn validate(&self) -> Result<()> {
        if self.max_file_rows == Some(0) {
            return Err(Error::Name {
                field: "fileLimits.maxFileRows",
                value: "0".to_string(),
                reason: "a limit of zero returns no rows at all; omit the field instead",
            });
        }
        if self.max_file_bytes == Some(0) {
            return Err(Error::Name {
                field: "fileLimits.maxFileBytes",
                value: "0".to_string(),
                reason: "a limit of zero returns no bytes at all; omit the field instead",
            });
        }
        Ok(())
    }
}

/// What this Endpoint never serves, whatever the Policy set allows (EP-61).
///
/// A publication decision, not an authorization one: the gateway intersects this list with
/// the caller's policy projection, so an Endpoint can subtract from a grant and never add to
/// it. The same space can therefore be published twice with different amounts of detail
/// without writing a second Policy, and because the subtraction happens where every
/// representation reads its entities, a hidden attribute is missing from the CSV, the
/// GeoJSON, the MCP tool result and the schema surface alike (EP-06, EP-07, EP-47).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Projection {
    /// Attribute names this endpoint removes from every representation.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub hidden_attributes: Vec<String>,
}

/// The members an Endpoint cannot hide: the identity without which the answer is not an NGSI-LD
/// entity, and the timestamps the broker sets rather than an author (EP-71).
///
/// The gateway serves all ten whatever the projection says — `pdp::projection`'s `STRUCTURAL` and
/// `SYSTEM` — so a manifest naming one here used to be accepted and then hide nothing (T-2334).
/// The same name in a write payload is refused, which is the second reason the two lists may not
/// be the same list.
const ALWAYS_SERVED: &[&str] = &[
    "id",
    "type",
    "@context",
    "@id",
    "@type",
    "scope",
    "createdAt",
    "modifiedAt",
    "deletedAt",
    "expiresAt",
];

impl Projection {
    /// Validates that every hidden attribute is a usable NGSI-LD attribute name.
    ///
    /// An empty or repeated name is refused rather than ignored: a steward who writes one
    /// means to hide something, and a list that quietly drops an entry hides nothing.
    pub fn validate(&self) -> Result<()> {
        let mut seen = std::collections::BTreeSet::new();
        for attribute in &self.hidden_attributes {
            if attribute.trim().is_empty() {
                return Err(Error::Name {
                    field: "projection.hiddenAttributes",
                    value: attribute.clone(),
                    reason: "an attribute name must not be empty",
                });
            }
            if ALWAYS_SERVED.contains(&attribute.as_str()) {
                return Err(Error::Name {
                    field: "projection.hiddenAttributes",
                    value: attribute.clone(),
                    reason: "this member is served whatever a projection says (it is what makes \
                             the answer an entity, or a timestamp the broker sets): hide an \
                             attribute instead (EP-61, EP-71)",
                });
            }
            if !seen.insert(attribute.as_str()) {
                return Err(Error::Name {
                    field: "projection.hiddenAttributes",
                    value: attribute.clone(),
                    reason: "duplicate attribute in hiddenAttributes",
                });
            }
        }
        Ok(())
    }
}

/// Desired specification of a [`SharedSpaceReference`][crate::kinds::SharedSpaceReference] resource (EP-15).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct SharedSpaceReferenceSpec {
    /// Opaque slug of the target endpoint, for a source in another instance (EP-77).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint_slug: Option<EndpointSlug>,
    /// The source Endpoint by project and name, inside one Organization repository; the
    /// loader resolves it to this environment's slug (EP-77).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint_ref: Option<EndpointRef>,
    /// Local alias name for the remote context space.
    pub alias: String,
    /// How often the peer's schema surface is mirrored; 24 hours when absent (DM-49).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schedule: Option<crate::kinds::Schedule>,
    /// Optional reference to cached access token in secret store.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cached_token_secret_ref: Option<SecretRef>,
}

/// An Endpoint named by where it lives rather than by its slug (EP-77).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct EndpointRef {
    /// The project the Endpoint belongs to.
    pub project: String,
    /// The Endpoint's name in that project.
    pub name: String,
}

impl Kind for SharedSpaceReferenceSpec {
    const KIND: &'static str = "SharedSpaceReference";
    const PLURAL: &'static str = "shared";
    const SCOPE: Scope = Scope::Project;
    const PATH_TEMPLATE: &'static str = "projects/{project}/shared/{name}.yaml";

    fn validate_spec(&self, meta: &ObjectMeta) -> Result<()> {
        names::validate_dns1123_label(&meta.name)?;
        self.validate()
    }
}

impl SharedSpaceReferenceSpec {
    /// Validates that the local alias conforms to DNS-1123 label rules.
    pub fn validate(&self) -> Result<()> {
        match (&self.endpoint_slug, &self.endpoint_ref) {
            (Some(_), None) => {}
            (None, Some(reference)) => {
                names::validate_dns1123_label(&reference.project)?;
                names::validate_dns1123_label(&reference.name)?;
            }
            _ => {
                return Err(Error::Name {
                    field: "spec.endpointRef",
                    value: String::new(),
                    reason: "name the source by exactly one of endpointRef (this organization) or endpointSlug (another instance)",
                })
            }
        }
        names::validate_dns1123_label(&self.alias).map_err(|e| match e {
            Error::Name { reason, .. } => Error::Name {
                field: "spec.alias",
                value: self.alias.clone(),
                reason,
            },
            other => other,
        })?;
        crate::kinds::validate_mirror_schedule(self.schedule.as_ref())
    }
}
