//! NGSI-LD entity URNs (ADR-N-041, PF-10, PF-43): `urn:ngsi-ld:{Type}:{id}`, where `{id}` is any
//! RFC 8141 namespace-specific string. An entity is the pair of its Context Space and its URN
//! ([`EntityRef`]); the URN names no space and grants nothing (PF-42). The platform's own
//! `prefixed` shape, `urn:ngsi-ld:{Type}:{orgDomain}:{space}:{localId}`, is one valid shape among
//! others: [`Urn::new`] mints it, and a parsed URN in that shape exposes its parts, as text.

use crate::error::{Error, Result, UrnError};
use crate::names;
use std::fmt;
use std::str::FromStr;
use std::sync::LazyLock;

/// The longest `{id}` part accepted (ADR-N-041 §3.1).
pub const MAX_ID_CHARS: usize = 256;

/// RFC 8141: NSS = pchar *( pchar / "/" ), pchar = unreserved / pct-encoded / sub-delims / ":" / "@".
static ID_RE: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(
        r"^(?:[A-Za-z0-9._~!$&'()*+,;=:@-]|%[0-9A-Fa-f]{2})(?:[A-Za-z0-9._~!$&'()*+,;=:@/-]|%[0-9A-Fa-f]{2})*$",
    )
    .expect("a valid regex")
});

/// The parts of a URN in the platform's `prefixed` shape: text, never a space or a right.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Prefixed {
    /// The domain segment (`hel.fi`).
    pub org_domain: String,
    /// The space segment (`air-quality`); text, never the entity's space (PF-42).
    pub space: String,
    /// The local id segment (`station-kallio-01`).
    pub local_id: String,
}

/// An NGSI-LD entity URN (ADR-N-041).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Urn {
    entity_type: String,
    id: String,
    prefixed: Option<Prefixed>,
}

impl Urn {
    /// Mints the `prefixed` shape `urn:ngsi-ld:{Type}:{orgDomain}:{space}:{localId}` after
    /// validating each part (PF-44, the default mint option of ADR-N-041 §3.4).
    pub fn new(entity_type: &str, org_domain: &str, space: &str, local_id: &str) -> Result<Self> {
        let urn = format!("urn:ngsi-ld:{entity_type}:{org_domain}:{space}:{local_id}");
        let fail = |reason: fn(String, &'static str) -> UrnError, segment: &str| {
            let urn = urn.clone();
            let segment = segment.to_string();
            move |e: Error| match e {
                Error::Name { reason: why, .. } => Error::Urn {
                    urn,
                    reason: reason(segment, why),
                },
                other => other,
            }
        };
        names::validate_entity_type(entity_type).map_err(fail(
            |segment, reason| UrnError::InvalidEntityType { segment, reason },
            entity_type,
        ))?;
        names::validate_org_domain(org_domain).map_err(fail(
            |segment, reason| UrnError::InvalidOrgDomain { segment, reason },
            org_domain,
        ))?;
        names::validate_space_name(space).map_err(fail(
            |segment, reason| UrnError::InvalidSpace { segment, reason },
            space,
        ))?;
        names::validate_local_id(local_id).map_err(fail(
            |segment, reason| UrnError::InvalidLocalId { segment, reason },
            local_id,
        ))?;
        Ok(Self {
            entity_type: entity_type.to_string(),
            id: format!("{org_domain}:{space}:{local_id}"),
            prefixed: Some(Prefixed {
                org_domain: org_domain.to_string(),
                space: space.to_string(),
                local_id: local_id.to_string(),
            }),
        })
    }

    /// The entity type short name (e.g. `AirQualityObserved`).
    pub fn entity_type(&self) -> &str {
        &self.entity_type
    }

    /// Everything after the type (e.g. `Helsinki-001`, or `hel.fi:air:st-1` when prefixed).
    pub fn id(&self) -> &str {
        &self.id
    }

    /// The parts of the `prefixed` shape, when the URN is in it.
    pub fn prefixed(&self) -> Option<&Prefixed> {
        self.prefixed.as_ref()
    }

    /// The domain segment of a prefixed URN. Text only: never the owner of anything (PF-42).
    pub fn org_domain(&self) -> Option<&str> {
        self.prefixed.as_ref().map(|p| p.org_domain.as_str())
    }

    /// The space segment of a prefixed URN. Text only: an entity's space is its address's
    /// (PF-42), never this.
    pub fn space(&self) -> Option<&str> {
        self.prefixed.as_ref().map(|p| p.space.as_str())
    }

    /// The local id: the last segment of a prefixed URN, else the whole id part.
    pub fn local_id(&self) -> &str {
        self.prefixed
            .as_ref()
            .map_or(self.id.as_str(), |p| p.local_id.as_str())
    }
}

/// An entity: its Context Space and its URN (ADR-N-041 §3.1). The same URN in two spaces is two
/// entities.
#[derive(
    Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
pub struct EntityRef {
    space: String,
    urn: Urn,
}

impl EntityRef {
    /// The entity `urn` of the space `space`, the space name checked (PF-09).
    pub fn new(space: &str, urn: Urn) -> Result<Self> {
        names::validate_space_name(space)?;
        Ok(Self {
            space: space.to_string(),
            urn,
        })
    }

    /// The Context Space the entity lives in.
    pub fn space(&self) -> &str {
        &self.space
    }

    /// The entity's URN within that space.
    pub fn urn(&self) -> &Urn {
        &self.urn
    }
}

impl fmt::Display for EntityRef {
    /// The entity's address under the space surface (SP-02).
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "/cs/{}/ngsi-ld/v1/entities/{}", self.space, self.urn)
    }
}

/// A manifest reference in the prefixed shape, with its `{space}` segment prefixed by `prefix`
/// (CC-78).
///
/// A workspace preview renders every organization-unique identity with `ws-{name}-` in front. A
/// reference between manifests written as a prefixed URN (a Pipeline's `targetEndpoint`, an
/// Endpoint's `policyRef`) names another manifest by its project and name, so the preview's copy
/// has to point at the preview's object, never at the one it was branched from. Entity ids in
/// data are not references and stay as written (ADR-N-041 §3.4). Only the `{space}` segment
/// moves; a string that is not an NGSI-LD URN, a URN without a `{space}` segment, an empty prefix
/// or a segment that already carries it comes back unchanged.
pub fn apply_render_prefix(urn: &str, prefix: &str) -> String {
    let (anchor, body) = match urn.strip_prefix('^') {
        Some(body) => ("^", body),
        None => ("", urn),
    };
    if prefix.is_empty() || !body.starts_with("urn:ngsi-ld:") {
        return urn.to_owned();
    }
    let mut parts: Vec<&str> = body.splitn(6, ':').collect();
    if parts.len() < 6 || parts[4].is_empty() || parts[4].starts_with(prefix) {
        return urn.to_owned();
    }
    let prefixed = format!("{prefix}{}", parts[4]);
    parts[4] = &prefixed;
    format!("{anchor}{}", parts.join(":"))
}

impl fmt::Display for Urn {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "urn:ngsi-ld:{}:{}", self.entity_type, self.id)
    }
}

impl FromStr for Urn {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self> {
        let invalid = |reason: UrnError| Error::Urn {
            urn: s.to_string(),
            reason,
        };
        let prefix = "urn:ngsi-ld:";
        // `get` rather than a slice: a multi-byte character straddling byte 12 would panic.
        let rest = match (s.get(..prefix.len()), s.get(prefix.len()..)) {
            (Some(head), Some(rest)) if head.eq_ignore_ascii_case(prefix) => rest,
            _ => return Err(invalid(UrnError::InvalidPrefix)),
        };
        let Some((entity_type, id)) = rest.split_once(':') else {
            return Err(invalid(UrnError::MissingId));
        };
        names::validate_entity_type(entity_type).map_err(|e| match e {
            Error::Name { reason, .. } => invalid(UrnError::InvalidEntityType {
                segment: entity_type.to_string(),
                reason,
            }),
            other => other,
        })?;
        if id.is_empty() {
            return Err(invalid(UrnError::MissingId));
        }
        if id.chars().count() > MAX_ID_CHARS {
            return Err(invalid(UrnError::InvalidId {
                segment: id.chars().take(40).collect::<String>() + "…",
                reason: "is longer than 256 characters",
            }));
        }
        if !ID_RE.is_match(id) {
            return Err(invalid(UrnError::InvalidId {
                segment: id.to_string(),
                reason:
                    "may hold only letters, digits, `-._~!$&'()*+,;=:@/` and %-escapes (RFC 8141)",
            }));
        }
        Ok(Self {
            entity_type: entity_type.to_string(),
            id: id.to_string(),
            prefixed: prefixed_parts(id),
        })
    }
}

/// The parts of the `prefixed` shape when `id` is in it: three segments, a domain, a space name
/// and a local id that each validate.
fn prefixed_parts(id: &str) -> Option<Prefixed> {
    let mut parts = id.split(':');
    let (org_domain, space, local_id) = (parts.next()?, parts.next()?, parts.next()?);
    if parts.next().is_some()
        || names::validate_org_domain(org_domain).is_err()
        || names::validate_space_name(space).is_err()
        || names::validate_local_id(local_id).is_err()
    {
        return None;
    }
    Some(Prefixed {
        org_domain: org_domain.to_string(),
        space: space.to_string(),
        local_id: local_id.to_string(),
    })
}

impl serde::Serialize for Urn {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> serde::Deserialize<'de> for Urn {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        s.parse::<Urn>().map_err(serde::de::Error::custom)
    }
}

impl schemars::JsonSchema for Urn {
    fn schema_name() -> String {
        "Urn".to_string()
    }

    fn json_schema(_gen: &mut schemars::gen::SchemaGenerator) -> schemars::schema::Schema {
        let schema = schemars::schema::SchemaObject {
            instance_type: Some(schemars::schema::InstanceType::String.into()),
            string: Some(Box::new(schemars::schema::StringValidation {
                pattern: Some(
                    r"^urn:ngsi-ld:[A-Z][A-Za-z0-9]{1,63}:(?:[A-Za-z0-9._~!$&'()*+,;=:@-]|%[0-9A-Fa-f]{2})(?:[A-Za-z0-9._~!$&'()*+,;=:@/-]|%[0-9A-Fa-f]{2}){0,255}$"
                        .to_string(),
                ),
                ..Default::default()
            })),
            metadata: Some(Box::new(schemars::schema::Metadata {
                description: Some(
                    "NGSI-LD entity URN (ADR-N-041, PF-43): urn:ngsi-ld:{Type}:{id}; the prefixed shape urn:ngsi-ld:{Type}:{orgDomain}:{space}:{localId} is optional"
                        .to_string(),
                ),
                ..Default::default()
            })),
            ..Default::default()
        };
        schemars::schema::Schema::Object(schema)
    }
}
