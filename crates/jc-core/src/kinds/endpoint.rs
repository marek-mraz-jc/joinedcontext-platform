//! Manifest kinds for Endpoints and cross-project sharing references (T-0113, EP-01..EP-28).

use crate::envelope::{Kind, ObjectMeta, Ref, Scope, SecretRef, TypedRef};
use crate::error::{Error, Result};
use crate::names;
use crate::system_members::is_entity_system_member;
use crate::urn::Urn;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
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
/// surfaces on every endpoint and cannot be toggled off. `file.csv` is the one file an Endpoint
/// publishes; the names of the removed files are refused by name ([`REMOVED_REPRESENTATIONS`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum Representation {
    /// Canonical NGSI-LD representation (`/ngsi-ld/v1/`).
    NgsiLd,
    /// Direct Model Context Protocol streamable HTTP interface (`/mcp`).
    Mcp,
    /// Flat tabular CSV (`/file.csv`), the one file an Endpoint publishes.
    Csv,
    /// OGC API - Features Part 1 collections and items (`/ogc/features/`).
    #[serde(rename = "ogc-features")]
    OgcFeatures,
    /// SensorThings API v1.1 read projection (`/sta/v1.1/`); served only where listed (EP-104),
    /// and only on a space whose model has a numeric attribute with a unit (EP-125).
    Sta,
    /// OData v4 service of the Endpoint's view (`/odata/v4/`, EP-104).
    Odata,
    /// Notifications over one WebSocket (`/ws`, EP-106).
    #[serde(rename = "websocket")]
    WebSocket,
    /// iCalendar (RFC 5545) feed of the view's entities (`/ical/v1/`); served only where listed,
    /// through the mapping `spec.representations.ical` checked against the space's model (EP-125).
    Ical,
}

/// What a domain-standard representation with a fixed mapping needs of the space's data model
/// (EP-125): `sta` today; `ical` maps through `spec.representations.ical` instead
/// ([`super::ical_mapping`]). Static code, never read from a manifest.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SurfaceRequirement {
    /// The Smart Data Models type, by the NGSI-LD type name the view filters on; [`ANY_TYPE`]
    /// when the standard maps every type that has a measurement.
    pub entity_type: &'static str,
    /// Its IRI, for the documentation.
    pub type_iri: &'static str,
    /// The standard and the section that makes the fields below mandatory.
    pub standard: &'static str,
    /// `(field, standard section, source attribute, mandatory)`.
    pub fields: &'static [(&'static str, &'static str, &'static str, bool)],
}

/// The [`SurfaceRequirement::entity_type`] of a standard that maps no fixed type: it serves any
/// class of the model with a slot whose `range` is numeric and that declares its `unit`.
pub const ANY_TYPE: &str = "*";

/// The source column of a measurement in the SensorThings fields.
pub const MEASUREMENT: &str = "a numeric slot with its unit";

// Every entity is a Thing, every numeric Property a Datastream (antares-sta mapping rules 1-6).
const STA: SurfaceRequirement = SurfaceRequirement {
    entity_type: ANY_TYPE,
    type_iri: "https://docs.ogc.org/is/18-088/18-088.html",
    standard: "OGC SensorThings API v1.1",
    fields: &[
        ("Thing", "§9.2.1", "id", true),
        ("Datastream", "§9.2.5", MEASUREMENT, true),
        ("Datastream.unitOfMeasurement", "§9.2.5", MEASUREMENT, true),
        ("Observation.result", "§9.2.7", MEASUREMENT, true),
        ("Observation.phenomenonTime", "§9.2.7", "observedAt", true),
        ("ObservedProperty", "§9.2.4", "type", true),
        ("Location", "§9.2.2", "location", false),
    ],
};

impl Representation {
    /// Returns the wire name string for this representation.
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::NgsiLd => "ngsi-ld",
            Self::Mcp => "mcp",
            Self::Csv => "csv",
            Self::OgcFeatures => "ogc-features",
            Self::Sta => "sta",
            Self::Odata => "odata",
            Self::WebSocket => "websocket",
            Self::Ical => "ical",
        }
    }

    /// Whether the Endpoint's space model must fit the representation: its fixed requirement
    /// (`sta`) or its mapping (`ical`, EP-125).
    pub const fn needs_model(&self) -> bool {
        matches!(self, Self::Ical) || self.requirement().is_some()
    }

    /// What this representation needs of the space's data model; `None` for the generic ones,
    /// which serve any type, and for `ical`, whose mapping says it (EP-125).
    pub const fn requirement(&self) -> Option<&'static SurfaceRequirement> {
        match self {
            Self::Sta => Some(&STA),
            _ => None,
        }
    }

    /// Served only where `enabledRepresentations` lists it, never by the default (EP-104).
    pub const fn opt_in(&self) -> bool {
        matches!(self, Self::Sta | Self::Ical)
    }

    /// Why a space whose model is `linkml` cannot serve this representation's fixed requirement,
    /// or `None` when it can (EP-125): for `sta`, no slot that is a measurement.
    pub fn model_gap(&self, linkml: &str) -> std::result::Result<Option<String>, String> {
        if self.requirement().is_none() {
            return Ok(None);
        }
        let measured = super::model_projection::linkml_measurements(linkml)?;
        Ok(measured
            .is_empty()
            .then(|| self.missing_model_gap())
            .flatten())
    }

    /// What a space without a usable data model lacks for this representation's fixed
    /// requirement, `None` for the others (EP-125).
    pub fn missing_model_gap(&self) -> Option<String> {
        self.requirement().map(|need| match need.entity_type {
            ANY_TYPE => "a type with a numeric attribute that declares its unit".to_owned(),
            named => format!("the type {named}"),
        })
    }

    /// Every representation, in the order a record lists them.
    pub const ALL: [Self; 8] = [
        Self::NgsiLd,
        Self::Mcp,
        Self::Csv,
        Self::OgcFeatures,
        Self::Sta,
        Self::Odata,
        Self::WebSocket,
        Self::Ical,
    ];
}

impl fmt::Display for Representation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

/// The representations an Endpoint no longer publishes (owner 2026-10-18, T-4597, API/02 §5): a
/// manifest that still names one is refused with a sentence that says so, not as a typo.
pub const REMOVED_REPRESENTATIONS: [&str; 5] = ["geojson", "json", "xlsx", "parquet", "zip"];

/// The wire names of [`Representation::ALL`], for serde's "expected one of".
const REPRESENTATION_NAMES: [&str; Representation::ALL.len()] = {
    let mut names = [""; Representation::ALL.len()];
    let mut i = 0;
    while i < names.len() {
        names[i] = Representation::ALL[i].as_str();
        i += 1;
    }
    names
};

impl<'de> Deserialize<'de> for Representation {
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        let name = std::borrow::Cow::<'de, str>::deserialize(deserializer)?;
        if let Some(known) = Self::ALL.into_iter().find(|r| r.as_str() == name) {
            return Ok(known);
        }
        if REMOVED_REPRESENTATIONS.contains(&name.as_ref()) {
            return Err(serde::de::Error::custom(format_args!(
                "representation `{name}` was removed (T-4597): an Endpoint publishes file.csv and \
                 its APIs (ngsi-ld, ogc-features, odata, sta, mcp); drop it from the list"
            )));
        }
        Err(serde::de::Error::unknown_variant(
            &name,
            &REPRESENTATION_NAMES,
        ))
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

/// How an Endpoint takes new entities from callers who do not choose their ids: a public form
/// (EP-97).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Creates {
    /// The gateway replaces the id a create names with one it mints,
    /// `urn:ngsi-ld:{Type}:{uuid}`, so a caller can neither pick nor overwrite one.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub mint_ids: bool,
    /// At most this many creates through the Endpoint per UTC day, 1..=10000; past it the
    /// gateway answers 429 with `Retry-After` until midnight UTC.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub per_day: Option<u32>,
    /// The sites that may frame the form's page `/f/{slug}`, each an `https` origin exactly as a
    /// browser names it (`https://www.example.org`, an optional non-default port, no path, no
    /// wildcard), at most 20. The Portal sends them in that page's `frame-ancestors` and nowhere
    /// else; empty, only the Portal frames the form (EP-101).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub embed_origins: Vec<String>,
}

/// The largest `spec.creates.perDay` an Endpoint may name (EP-97).
pub const MAX_CREATES_PER_DAY: u32 = 10_000;

/// The most sites `spec.creates.embedOrigins` may name (EP-101).
pub const MAX_EMBED_ORIGINS: usize = 20;

/// Whether `origin` is an `https` origin as a browser serializes it, with a DNS host: lower case,
/// ASCII (punycode), no default port, no path, no wildcard. Anything else could not match a
/// framing page, or would carry more than one source into a `frame-ancestors` header.
fn is_embed_origin(origin: &str) -> bool {
    let Some(authority) = origin.strip_prefix("https://") else {
        return false;
    };
    let (host, port) = match authority.split_once(':') {
        Some((host, port)) => (host, Some(port)),
        None => (authority, None),
    };
    let label_ok = |label: &str| {
        !label.is_empty()
            && label.len() <= 63
            && !label.starts_with('-')
            && !label.ends_with('-')
            && label
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
    };
    // A port as the browser writes it: digits without a leading zero, never the default 443.
    let port_ok = port.is_none_or(|port| {
        !port.starts_with('0')
            && port.parse::<u16>().is_ok_and(|n| n != 0 && n != 443)
            && port.bytes().all(|b| b.is_ascii_digit())
    });
    host.len() <= 253 && host.contains('.') && host.split('.').all(label_ok) && port_ok
}

impl Creates {
    /// Validates `perDay` is within 1..=[`MAX_CREATES_PER_DAY`] and `embedOrigins` names at most
    /// [`MAX_EMBED_ORIGINS`] distinct `https` origins.
    pub fn validate(&self) -> Result<()> {
        if let Some(n) = self.per_day {
            if n == 0 || n > MAX_CREATES_PER_DAY {
                return Err(Error::Name {
                    field: "creates.perDay",
                    value: n.to_string(),
                    reason: "perDay must be between 1 and 10000",
                });
            }
        }
        if self.embed_origins.len() > MAX_EMBED_ORIGINS {
            return Err(Error::Name {
                field: "creates.embedOrigins",
                value: self.embed_origins.len().to_string(),
                reason: "at most 20 sites may frame a form",
            });
        }
        let mut seen = std::collections::BTreeSet::new();
        for origin in &self.embed_origins {
            if !is_embed_origin(origin) {
                return Err(Error::Name {
                    field: "creates.embedOrigins",
                    value: origin.clone(),
                    reason: "a site is an https origin as the browser names it: \
                             https://host or https://host:port, lower case, punycode for a \
                             non-ASCII name, no path, no default port, no wildcard",
                });
            }
            if !seen.insert(origin) {
                return Err(Error::Name {
                    field: "creates.embedOrigins",
                    value: origin.clone(),
                    reason: "a site is named once",
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
    /// The representations this Endpoint serves, listed (EP-05). Omitted: every one but `sta`,
    /// less `disabledRepresentations` (EP-104); see [`EndpointSpec::served_representations`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled_representations: Option<Vec<Representation>>,
    /// Representations an Endpoint without `enabledRepresentations` does not serve (EP-104).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub disabled_representations: Vec<Representation>,
    /// `false` turns the Endpoint's MCP instance off. Every Endpoint serves `mcp` without
    /// listing it, public or internal, gated by its own Policy (EP-24, T-2901); see
    /// [`EndpointSpec::served_representations`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mcp: Option<bool>,
    /// Optional rate limiting configuration (EP-20).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_limits: Option<RateLimits>,
    /// How the Endpoint takes new entities: minted ids and a daily cap (EP-97).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub creates: Option<Creates>,
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
    /// What a DCAT-AP catalogue needs beyond the distributions: publisher, contact, licence,
    /// themes, coverage, frequency and provenance (EP-78, EP-79).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub catalog: Option<crate::kinds::catalog::Catalog>,
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
    /// Per entity type, the slots this Endpoint's tables, popups and CKAN columns show and its
    /// key attributes, reordering and narrowing the model's display order (EP-135, DM-98).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub display: BTreeMap<String, crate::display_order::Display>,
    /// The mappings of the domain-standard representations, `ical` first (EP-125, API/02 §8b).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub representations: Option<super::ical_mapping::Representations>,
    /// A breaking change announced ahead: the gateway answers with `Deprecation` and `Sunset`
    /// headers until the deprecated part is removed (EP-146, EP-147).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deprecation: Option<super::endpoint_deprecation::Deprecation>,
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
    /// The representations this Endpoint serves: the ones it lists, or, when it lists none,
    /// every one but `sta` less the disabled ones (EP-104); then `ngsi-ld` and `geojson`, which
    /// every Endpoint serves (EP-10, T-2939), and `mcp` unless `mcp: false` (EP-24, T-2901).
    /// The gateway, the catalogue records and the publishers all read this, never the list
    /// alone.
    pub fn served_representations(&self) -> Vec<Representation> {
        let mut served: Vec<Representation> = match &self.enabled_representations {
            Some(listed) => listed.clone(),
            None => Representation::ALL
                .into_iter()
                .filter(|r| !r.opt_in())
                .filter(|r| !self.disabled_representations.contains(r))
                .filter(|r| *r != Representation::Mcp || self.mcp != Some(false))
                .collect(),
        };
        let mut always = vec![Representation::NgsiLd];
        if self.mcp != Some(false) {
            always.push(Representation::Mcp);
        }
        for representation in always {
            if !served.contains(&representation) {
                served.push(representation);
            }
        }
        served
    }

    /// The iCalendar outputs the Endpoint serves (EP-125, EP-159): none unless it serves `ical`
    /// and lists them (owner 2026-10-22T09:25Z).
    pub fn ical_outputs(&self) -> &[super::ical_mapping::IcalOutput] {
        match &self.representations {
            Some(r)
                if self
                    .served_representations()
                    .contains(&Representation::Ical) =>
            {
                &r.ical
            }
            _ => &[],
        }
    }

    /// Every gap between the mappings of the representations this Endpoint serves and its
    /// space's LinkML model (`None`: the space has none), one sentence each (EP-125, API/02
    /// §8b). `Err` when the model does not parse.
    pub fn representation_gaps(
        &self,
        linkml: Option<&str>,
    ) -> std::result::Result<Vec<String>, String> {
        let mut gaps = Vec::new();
        for output in self.ical_outputs() {
            gaps.extend(output.model_gaps(linkml)?);
        }
        for representation in self.served_representations() {
            let gap = match linkml {
                None => representation.missing_model_gap(),
                Some(text) => representation.model_gap(text)?,
            };
            gaps.extend(gap.map(|gap| format!("`{representation}` needs {gap}")));
        }
        Ok(gaps)
    }

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

        if let Some(ical) = self
            .representations
            .as_ref()
            .map(|r| r.ical.as_slice())
            .filter(|ical| !ical.is_empty())
        {
            if !self
                .served_representations()
                .contains(&Representation::Ical)
            {
                return Err(Error::Invalid {
                    field: "spec.representations.ical".to_owned(),
                    reason: "the Endpoint does not serve `ical`: list it in \
                             spec.enabledRepresentations or drop the mapping"
                        .to_owned(),
                });
            }
            let gaps = super::ical_mapping::outputs_gaps(ical);
            if !gaps.is_empty() {
                return Err(Error::Invalid {
                    field: "spec.representations.ical".to_owned(),
                    reason: gaps.join("; "),
                });
            }
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

        for (class, display) in &self.display {
            names::validate_entity_type(class)?;
            for (field, list) in [
                ("spec.display.fields", &display.fields),
                ("spec.display.summary", &display.summary),
            ] {
                let mut seen = std::collections::BTreeSet::new();
                if let Some(twice) = list.iter().find(|name| !seen.insert(name.as_str())) {
                    return Err(Error::Name {
                        field,
                        value: format!("{class}.{twice}"),
                        reason: "a slot is listed once (EP-135)",
                    });
                }
            }
        }

        let listed = self.enabled_representations.as_deref().unwrap_or_default();
        if self.enabled_representations.is_some() && listed.is_empty() {
            return Err(Error::Name {
                field: "enabledRepresentations",
                value: String::new(),
                reason: "enabledRepresentations must not be empty; omit it to serve every standard but sta (EP-104)",
            });
        }
        if self.enabled_representations.is_some() && !self.disabled_representations.is_empty() {
            return Err(Error::Name {
                field: "disabledRepresentations",
                value: String::new(),
                reason: "disabledRepresentations narrows the default set; beside enabledRepresentations drop one of the two (EP-104)",
            });
        }
        let mut disabled = std::collections::BTreeSet::new();
        for rep in &self.disabled_representations {
            let reason = match rep {
                Representation::NgsiLd => "every Endpoint serves ngsi-ld (EP-10)",
                Representation::Mcp => "mcp: false turns the MCP instance off (EP-24)",
                _ if !disabled.insert(*rep) => {
                    "duplicate representation in disabledRepresentations"
                }
                _ => continue,
            };
            return Err(Error::Name {
                field: "disabledRepresentations",
                value: rep.as_str().to_string(),
                reason,
            });
        }
        if self.mcp == Some(false) && listed.contains(&Representation::Mcp) {
            return Err(Error::Name {
                field: "mcp",
                value: "false".to_string(),
                reason: "mcp: false turns off the MCP instance that enabledRepresentations lists; drop one of the two (EP-24)",
            });
        }
        let mut seen = std::collections::BTreeSet::new();
        for rep in listed {
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

        if let Some(ref creates) = self.creates {
            creates.validate()?;
        }

        if let Some(ref limits) = self.file_limits {
            limits.validate()?;
        }

        if let Some(ref projection) = self.projection {
            projection.validate()?;
        }

        if let Some(ref publish) = self.publish {
            publish.validate(&self.served_representations())?;
        }

        if let Some(ref deprecation) = self.deprecation {
            deprecation.validate(&self.served_representations())?;
        }

        if let Some(ref catalog) = self.catalog {
            catalog.validate()?;
            // EP-139: a record the feeds leave out would be a dataset in CKAN alone.
            if !catalog.harvested() && self.publish.as_ref().is_some_and(|p| p.ckan.is_some()) {
                return Err(Error::Name {
                    field: "spec.catalog.harvest",
                    value: "false".to_owned(),
                    reason: "a door the feeds leave out is no catalogue dataset: drop spec.publish.ckan, or drop harvest: false (EP-139)",
                });
            }
            crate::kinds::visualization::validate_list(
                &catalog.visualizations,
                &self.served_representations(),
            )?;
        }

        if let Some(ref policy) = self.policy_ref {
            if policy.entity_type() != "Policy" {
                return Err(Error::Kind {
                    expected: "Policy",
                    got: policy.entity_type().to_string(),
                });
            }
            // EP-117: the gateway and the Portal bind the Policy named by the prefixed shape;
            // any other shape would bind none and the Endpoint would grant nothing, silently.
            if policy.prefixed().is_none() {
                return Err(Error::Name {
                    field: "policyRef",
                    value: policy.to_string(),
                    reason: "a policyRef is urn:ngsi-ld:Policy:{orgDomain}:{space}:{name} (EP-117)",
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
            // The system members of DM-130 are served whatever a projection says (EP-71), so a
            // manifest naming one would be accepted and then hide nothing (T-2334).
            if is_entity_system_member(attribute) {
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
