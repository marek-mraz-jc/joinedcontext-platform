//! Deterministic NGSI-LD entity URN specification (ADR 001, PF-10, PF-12, PF-42): the four
//! segments before the local id are names, never random UUIDs, so an `idPattern` prefix routes.

use crate::error::{Error, Result, UrnError};
use crate::names;
use std::fmt;
use std::str::FromStr;

/// NGSI-LD entity URN conforming to ADR 001 and PF-42.
///
/// Structure: `urn:ngsi-ld:{Type}:{orgDomain}:{space}:{localId}`
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Urn {
    entity_type: String,
    org_domain: String,
    space: String,
    local_id: String,
}

impl Urn {
    /// Mints a new [`Urn`] after validating all four segments (PF-42, PF-44).
    pub fn new(entity_type: &str, org_domain: &str, space: &str, local_id: &str) -> Result<Self> {
        let dummy_urn = format!("urn:ngsi-ld:{entity_type}:{org_domain}:{space}:{local_id}");

        names::validate_entity_type(entity_type).map_err(|e| match e {
            Error::Name { reason, .. } => Error::Urn {
                urn: dummy_urn.clone(),
                reason: UrnError::InvalidEntityType {
                    segment: entity_type.to_string(),
                    reason,
                },
            },
            other => other,
        })?;

        names::validate_org_domain(org_domain).map_err(|e| match e {
            Error::Name { reason, .. } => Error::Urn {
                urn: dummy_urn.clone(),
                reason: UrnError::InvalidOrgDomain {
                    segment: org_domain.to_string(),
                    reason,
                },
            },
            other => other,
        })?;

        names::validate_space_name(space).map_err(|e| match e {
            Error::Name { reason, .. } => Error::Urn {
                urn: dummy_urn.clone(),
                reason: UrnError::InvalidSpace {
                    segment: space.to_string(),
                    reason,
                },
            },
            other => other,
        })?;

        names::validate_local_id(local_id).map_err(|e| match e {
            Error::Name { reason, .. } => Error::Urn {
                urn: dummy_urn.clone(),
                reason: UrnError::InvalidLocalId {
                    segment: local_id.to_string(),
                    reason,
                },
            },
            other => other,
        })?;

        Ok(Self {
            entity_type: entity_type.to_string(),
            org_domain: org_domain.to_string(),
            space: space.to_string(),
            local_id: local_id.to_string(),
        })
    }

    /// Returns the entity type short name (e.g. `AirQualityObserved`).
    pub fn entity_type(&self) -> &str {
        &self.entity_type
    }

    /// Returns the organization's verified domain (e.g. `banskabystrica.sk`).
    pub fn org_domain(&self) -> &str {
        &self.org_domain
    }

    /// Returns the Context Space name (e.g. `ovzdusie`).
    pub fn space(&self) -> &str {
        &self.space
    }

    /// Returns the local identifier within the space and type.
    pub fn local_id(&self) -> &str {
        &self.local_id
    }

    /// Checks whether this URN belongs to the target organization domain and Context Space (PF-43).
    pub fn matches_tenant(&self, org_domain: &str, space: &str) -> bool {
        self.org_domain == org_domain && self.space == space
    }

    /// Generates an anchored regex prefix for Context Source Registrations (R33).
    pub fn id_pattern_prefix(&self) -> String {
        let escaped_domain = regex::escape(&self.org_domain);
        format!(
            "^urn:ngsi-ld:{}:{}:{}:.*$",
            self.entity_type, escaped_domain, self.space
        )
    }
}

/// The id, or anchored id pattern, with its `{space}` segment prefixed by `prefix` (CC-78).
///
/// A workspace preview renders every organization-unique identity with `ws-{name}-` in front;
/// this is the one implementation of that for ids, which the loader and the gateway both call,
/// so a preview can never mint or match an id of the space it was branched from. Only the
/// `{space}` segment moves: type, domain and local id stay as they are. A string that is not an
/// NGSI-LD URN, or a URN without a `{space}` segment, comes back unchanged, as does everything
/// with an empty prefix or one the segment already carries.
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
        write!(
            f,
            "urn:ngsi-ld:{}:{}:{}:{}",
            self.entity_type, self.org_domain, self.space, self.local_id
        )
    }
}

impl FromStr for Urn {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self> {
        let prefix = "urn:ngsi-ld:";
        // `get` rather than a slice: a multi-byte character straddling byte 12 would panic.
        let (head, rest) = match (s.get(..prefix.len()), s.get(prefix.len()..)) {
            (Some(head), Some(rest)) => (head, rest),
            _ => {
                return Err(Error::Urn {
                    urn: s.to_string(),
                    reason: UrnError::InvalidPrefix,
                })
            }
        };
        if !head.eq_ignore_ascii_case(prefix) {
            return Err(Error::Urn {
                urn: s.to_string(),
                reason: UrnError::InvalidPrefix,
            });
        }

        let segments: Vec<&str> = rest.split(':').collect();
        if segments.len() != 4 {
            return Err(Error::Urn {
                urn: s.to_string(),
                reason: UrnError::InvalidSegmentCount {
                    got: segments.len(),
                },
            });
        }

        for (i, seg) in segments.iter().enumerate() {
            if seg.is_empty() {
                return Err(Error::Urn {
                    urn: s.to_string(),
                    reason: UrnError::EmptySegment { index: i },
                });
            }
        }

        let entity_type = segments[0];
        let org_domain = segments[1];
        let space = segments[2];
        let local_id = segments[3];

        names::validate_entity_type(entity_type).map_err(|e| match e {
            Error::Name { reason, .. } => Error::Urn {
                urn: s.to_string(),
                reason: UrnError::InvalidEntityType {
                    segment: entity_type.to_string(),
                    reason,
                },
            },
            other => other,
        })?;

        names::validate_org_domain(org_domain).map_err(|e| match e {
            Error::Name { reason, .. } => Error::Urn {
                urn: s.to_string(),
                reason: UrnError::InvalidOrgDomain {
                    segment: org_domain.to_string(),
                    reason,
                },
            },
            other => other,
        })?;

        names::validate_space_name(space).map_err(|e| match e {
            Error::Name { reason, .. } => Error::Urn {
                urn: s.to_string(),
                reason: UrnError::InvalidSpace {
                    segment: space.to_string(),
                    reason,
                },
            },
            other => other,
        })?;

        names::validate_local_id(local_id).map_err(|e| match e {
            Error::Name { reason, .. } => Error::Urn {
                urn: s.to_string(),
                reason: UrnError::InvalidLocalId {
                    segment: local_id.to_string(),
                    reason,
                },
            },
            other => other,
        })?;

        Ok(Self {
            entity_type: entity_type.to_string(),
            org_domain: org_domain.to_string(),
            space: space.to_string(),
            local_id: local_id.to_string(),
        })
    }
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
                    r"^urn:ngsi-ld:[A-Z][A-Za-z0-9]{1,63}:[a-z0-9]([a-z0-9-]{0,61}[a-z0-9])?(\.[a-z0-9]([a-z0-9-]{0,61}[a-z0-9])?)+:[a-z0-9][a-z0-9-]{0,62}:[A-Za-z0-9._~-]{1,128}$"
                        .to_string(),
                ),
                ..Default::default()
            })),
            metadata: Some(Box::new(schemars::schema::Metadata {
                description: Some(
                    "NGSI-LD entity URN conforming to ADR 001 / PF-42: urn:ngsi-ld:{Type}:{orgDomain}:{space}:{localId}"
                        .to_string(),
                ),
                ..Default::default()
            })),
            ..Default::default()
        };
        schemars::schema::Schema::Object(schema)
    }
}
