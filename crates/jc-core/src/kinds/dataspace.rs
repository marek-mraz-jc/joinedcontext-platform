//! Manifest kinds for the Data Space Connector addon (T-0120, DS-01..DS-20, Architecture/18).

use crate::envelope::{Kind, ObjectMeta, Ref, Scope, SecretRef};
use crate::error::{Error, Result};
use crate::kinds::policy::Validity;
use crate::names;
use chrono::{DateTime, Utc};
use regex::Regex;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::fmt;
use std::str::FromStr;
use std::sync::LazyLock;

static DID_WEB_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^did:web:(.*)$").expect("valid regex for did:web"));

/// Decentralized identifier in the `did:web` method bound to a verified organization domain (DS-04, PF-41).
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Did(String);

impl Did {
    /// Creates a validated [`Did`] instance conforming to `did:web:{orgDomain}` (DS-04, PF-41).
    pub fn new(s: &str) -> Result<Self> {
        let caps = DID_WEB_RE.captures(s).ok_or_else(|| Error::Name {
            field: "did",
            value: s.to_string(),
            reason:
                "DID must start with `did:web:` followed by a verified organization domain (DS-04)",
        })?;

        let domain = &caps[1];
        names::validate_org_domain(domain).map_err(|e| match e {
            Error::Name { reason, .. } => Error::Name {
                field: "did",
                value: s.to_string(),
                reason,
            },
            other => other,
        })?;

        Ok(Self(s.to_string()))
    }

    /// Returns a string slice of the complete DID.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Returns the verified organization domain segment of the DID (DS-04, PF-41).
    pub fn org_domain(&self) -> &str {
        &self.0["did:web:".len()..]
    }
}

impl AsRef<str> for Did {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Did {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl FromStr for Did {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self> {
        Self::new(s)
    }
}

impl Serialize for Did {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for Did {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        Did::new(&s).map_err(serde::de::Error::custom)
    }
}

impl JsonSchema for Did {
    fn schema_name() -> String {
        "Did".to_string()
    }

    fn json_schema(_gen: &mut schemars::gen::SchemaGenerator) -> schemars::schema::Schema {
        let schema = schemars::schema::SchemaObject {
            instance_type: Some(schemars::schema::InstanceType::String.into()),
            string: Some(Box::new(schemars::schema::StringValidation {
                pattern: Some(
                    r"^did:web:[a-z0-9]([a-z0-9-]{0,61}[a-z0-9])?(\.[a-z0-9]([a-z0-9-]{0,61}[a-z0-9])?)+$"
                        .to_string(),
                ),
                ..Default::default()
            })),
            metadata: Some(Box::new(schemars::schema::Metadata {
                description: Some(
                    "Decentralized Identifier in the did:web method bound to an organization domain (DS-04, PF-41)"
                        .to_string(),
                ),
                ..Default::default()
            })),
            ..Default::default()
        };
        schemars::schema::Schema::Object(schema)
    }
}

/// Supported data space connector runtime engine (DS-06).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum ConnectorEngine {
    /// FIWARE Data Space Connector Rainbow engine implemented in Rust (DS-06).
    #[default]
    Rainbow,
    /// Eclipse Dataspace Components (EDC) engine implemented in Java (DS-06).
    Edc,
}

impl ConnectorEngine {
    /// Returns the kebab-case wire name for this connector engine.
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Rainbow => "rainbow",
            Self::Edc => "edc",
        }
    }
}

impl fmt::Display for ConnectorEngine {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

/// Desired specification of a [`DataSpaceParticipant`][crate::kinds::DataSpaceParticipant] resource (DS-04, DS-06, DS-07).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct DataSpaceParticipantSpec {
    /// Participant decentralized identifier `did:web:{orgDomain}` (DS-04, DS-07).
    pub did: Did,
    /// HTTPS base URL of the Verifiable Credential issuer (DS-04, DS-07).
    pub credential_issuer: String,
    /// HTTPS base URL of the local connector protocol endpoint (DS-07).
    pub connector_url: String,
    /// Connector protocol engine implementation (DS-06, DS-07).
    #[serde(default)]
    pub engine: ConnectorEngine,
    /// Trusted credential issuer and authority DIDs (DS-07).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub trust_anchors: Vec<Did>,
}

impl Kind for DataSpaceParticipantSpec {
    const KIND: &'static str = "DataSpaceParticipant";
    const PLURAL: &'static str = "dataspaceparticipants";
    const SCOPE: Scope = Scope::Organization;
    const PATH_TEMPLATE: &'static str = "dataspace/participant.yaml";

    fn validate_spec(&self, meta: &ObjectMeta) -> Result<()> {
        names::validate_dns1123_label(&meta.name)?;
        self.validate()
    }
}

impl DataSpaceParticipantSpec {
    /// Validates credential issuer URL, connector URL, and trust anchor uniqueness (DS-07).
    pub fn validate(&self) -> Result<()> {
        if self.credential_issuer.trim().is_empty()
            || !self.credential_issuer.starts_with("https://")
        {
            return Err(Error::Name {
                field: "spec.credentialIssuer",
                value: self.credential_issuer.clone(),
                reason: "credentialIssuer must be a non-empty HTTPS URL (DS-07)",
            });
        }

        if self.connector_url.trim().is_empty() || !self.connector_url.starts_with("https://") {
            return Err(Error::Name {
                field: "spec.connectorUrl",
                value: self.connector_url.clone(),
                reason: "connectorUrl must be a non-empty HTTPS URL (DS-07)",
            });
        }

        let mut seen = std::collections::BTreeSet::new();
        for anchor in &self.trust_anchors {
            if !seen.insert(anchor.as_str()) {
                return Err(Error::Name {
                    field: "spec.trustAnchors",
                    value: anchor.to_string(),
                    reason: "duplicate trust anchor in trustAnchors list (DS-07)",
                });
            }
        }

        Ok(())
    }
}

/// Desired specification of a [`DataOffer`][crate::kinds::DataOffer] resource (DS-07, DS-08, DS-14).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct DataOfferSpec {
    /// Target Context Space name offering context data (DS-07).
    pub context_space_ref: String,
    /// References to offered Endpoint resources (DS-07, DS-08).
    pub endpoint_refs: Vec<Ref>,
    /// Verbatim ODRL 2.2 offer in the `ngsi-ld:` profile (DS-07, R52).
    pub policy: serde_json::Value,
    /// Whether the offered data contains personal data (DS-14).
    #[serde(default)]
    pub contains_personal_data: bool,
    /// Optional Data Privacy Vocabulary (DPV) purpose IRI (DS-14).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub purpose: Option<String>,
}

impl Kind for DataOfferSpec {
    const KIND: &'static str = "DataOffer";
    const PLURAL: &'static str = "dataoffers";
    const SCOPE: Scope = Scope::Project;
    const PATH_TEMPLATE: &'static str =
        "projects/{project}/spaces/{space}/dataspace/offers/{name}.yaml";

    fn validate_spec(&self, meta: &ObjectMeta) -> Result<()> {
        names::validate_dns1123_label(&meta.name)?;
        self.validate()
    }

    fn context_space(&self) -> Option<&str> {
        Some(&self.context_space_ref)
    }
}

impl DataOfferSpec {
    /// Validates context space ref, endpoint refs, ODRL policy structure, and personal data rules (DS-07, DS-14).
    pub fn validate(&self) -> Result<()> {
        names::validate_dns1123_label(&self.context_space_ref).map_err(|e| match e {
            Error::Name { reason, .. } => Error::Name {
                field: "spec.contextSpaceRef",
                value: self.context_space_ref.clone(),
                reason,
            },
            other => other,
        })?;

        if self.endpoint_refs.is_empty() {
            return Err(Error::Name {
                field: "spec.endpointRefs",
                value: String::new(),
                reason: "endpointRefs must contain at least one endpoint reference (DS-07)",
            });
        }

        let mut seen = std::collections::BTreeSet::new();
        for ep in &self.endpoint_refs {
            let name = ep.name();
            names::validate_dns1123_label(name).map_err(|e| match e {
                Error::Name { reason, .. } => Error::Name {
                    field: "spec.endpointRefs.name",
                    value: name.to_string(),
                    reason,
                },
                other => other,
            })?;
            if let Some(kind) = ep.kind() {
                if kind != "Endpoint" {
                    return Err(Error::Kind {
                        expected: "Endpoint",
                        got: kind.to_string(),
                    });
                }
            }
            if !seen.insert(name) {
                return Err(Error::Name {
                    field: "spec.endpointRefs",
                    value: name.to_string(),
                    reason: "duplicate endpoint reference in endpointRefs list (DS-07)",
                });
            }
        }

        let policy_obj = self.policy.as_object().ok_or_else(|| Error::Name {
            field: "spec.policy",
            value: self.policy.to_string(),
            reason: "policy must be a JSON object (DS-07)",
        })?;

        match policy_obj.get("permission") {
            Some(perm) => {
                let perm_arr = perm.as_array().ok_or_else(|| Error::Name {
                    field: "spec.policy.permission",
                    value: perm.to_string(),
                    reason: "policy permission must be a non-empty array (DS-07)",
                })?;
                if perm_arr.is_empty() {
                    return Err(Error::Name {
                        field: "spec.policy.permission",
                        value: "[]".to_string(),
                        reason: "policy permission must be a non-empty array (DS-07)",
                    });
                }
            }
            None => {
                return Err(Error::Name {
                    field: "spec.policy.permission",
                    value: String::new(),
                    reason: "policy must contain a non-empty `permission` array (DS-07)",
                });
            }
        }

        if self.contains_personal_data {
            match &self.purpose {
                Some(p) if p.starts_with("https://w3id.org/dpv") => {}
                Some(p) => {
                    return Err(Error::Name {
                        field: "spec.purpose",
                        value: p.clone(),
                        reason: "purpose must start with `https://w3id.org/dpv` when containsPersonalData is true (DS-14)",
                    });
                }
                None => {
                    return Err(Error::Name {
                        field: "spec.purpose",
                        value: String::new(),
                        reason: "purpose is required when containsPersonalData is true (DS-14)",
                    });
                }
            }
        } else if let Some(ref p) = self.purpose {
            if !p.starts_with("https://w3id.org/dpv") {
                return Err(Error::Name {
                    field: "spec.purpose",
                    value: p.clone(),
                    reason: "purpose must start with `https://w3id.org/dpv` (DS-14)",
                });
            }
        }

        Ok(())
    }
}

/// Role of this organization within a data space agreement (DS-07, DS-15).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum AgreementRole {
    /// Provider sharing data through an offered endpoint (DS-07).
    Provider,
    /// Consumer accessing data from another participant (DS-15).
    Consumer,
}

impl AgreementRole {
    /// Returns the kebab-case wire name for this agreement role.
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Provider => "provider",
            Self::Consumer => "consumer",
        }
    }
}

impl fmt::Display for AgreementRole {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

/// Negotiation state of a Dataspace Protocol contract agreement (DS-09, DS-12).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum AgreementState {
    /// Initial contract negotiation request submitted (DS-09).
    Requested,
    /// Counter-offer proposed by provider (DS-09).
    Offered,
    /// Agreement terms accepted by both parties (DS-09).
    Accepted,
    /// Agreement confirmed and active in gateway PDP (DS-09, DS-10).
    Finalized,
    /// Agreement expired or explicitly terminated (DS-12).
    Terminated,
}

impl AgreementState {
    /// Returns the kebab-case wire name for this agreement state.
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Requested => "requested",
            Self::Offered => "offered",
            Self::Accepted => "accepted",
            Self::Finalized => "finalized",
            Self::Terminated => "terminated",
        }
    }

    /// Checks whether transitioning from `self` to `next` state is permitted (DS-09, DS-12).
    ///
    /// Permitted transitions:
    /// - `Requested -> Offered -> Accepted -> Finalized`
    /// - Any state may transition to `Terminated`
    /// - Each state may transition to itself
    /// - `Terminated` is terminal (only self-transition allowed)
    pub fn allows_transition_to(&self, next: AgreementState) -> bool {
        if *self == next {
            return true;
        }
        if *self == Self::Terminated {
            return false;
        }
        if next == Self::Terminated {
            return true;
        }
        matches!(
            (*self, next),
            (Self::Requested, Self::Offered)
                | (Self::Offered, Self::Accepted)
                | (Self::Accepted, Self::Finalized)
        )
    }
}

impl fmt::Display for AgreementState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

/// Lossless query filter constraints mapped from an ODRL agreement (DS-10).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct AgreementConstraints {
    /// Residual NGSI-LD query filter string (DS-10).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub q: Option<String>,
    /// Residual scope query filter string (DS-10).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope_q: Option<String>,
    /// Residual geographic query filter string (DS-10).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub geo_q: Option<String>,
    /// Residual temporal query filter string (DS-10).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temporal_q: Option<String>,
}

/// Desired specification of a [`DataAgreement`][crate::kinds::DataAgreement] resource (DS-09, DS-10, DS-15).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct DataAgreementSpec {
    /// Operational role in this agreement (provider or consumer) (DS-07, DS-15).
    pub role: AgreementRole,
    /// Reference to local DataOffer for provider role (DS-07).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub offer_ref: Option<Ref>,
    /// Decentralized identifier of the remote participant (DS-04, DS-10).
    pub remote_participant: Did,
    /// Unique Dataspace Protocol agreement identifier (DS-09, DS-13).
    pub agreement_id: String,
    /// Negotiation lifecycle state of this agreement (DS-09, DS-12).
    pub state: AgreementState,
    /// Temporal validity window for this agreement (DS-10).
    pub validity: Validity,
    /// Reference to transfer token in secret store for consumer role (DS-15).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_secret_ref: Option<SecretRef>,
    /// Lossless query constraints mapped from contract agreement (DS-10).
    #[serde(default)]
    pub constraints: AgreementConstraints,
}

impl Kind for DataAgreementSpec {
    const KIND: &'static str = "DataAgreement";
    const PLURAL: &'static str = "dataagreements";
    const SCOPE: Scope = Scope::Project;
    const PATH_TEMPLATE: &'static str = "projects/{project}/dataspace/agreements/{name}.yaml";

    fn validate_spec(&self, meta: &ObjectMeta) -> Result<()> {
        names::validate_dns1123_label(&meta.name)?;
        self.validate()
    }
}

impl DataAgreementSpec {
    /// Validates agreement identifier, provider offer ref, consumer token secret ref, and validity window (DS-09, DS-10, DS-15).
    pub fn validate(&self) -> Result<()> {
        if self.agreement_id.trim().is_empty() {
            return Err(Error::Name {
                field: "spec.agreementId",
                value: self.agreement_id.clone(),
                reason: "agreementId must not be empty (DS-09)",
            });
        }

        if self.role == AgreementRole::Provider {
            match &self.offer_ref {
                Some(offer) => {
                    names::validate_dns1123_label(offer.name()).map_err(|e| match e {
                        Error::Name { reason, .. } => Error::Name {
                            field: "spec.offerRef.name",
                            value: offer.name().to_string(),
                            reason,
                        },
                        other => other,
                    })?;
                    if let Some(kind) = offer.kind() {
                        if kind != "DataOffer" {
                            return Err(Error::Kind {
                                expected: "DataOffer",
                                got: kind.to_string(),
                            });
                        }
                    }
                }
                None => {
                    return Err(Error::Name {
                        field: "spec.offerRef",
                        value: String::new(),
                        reason: "offerRef is required for provider role (DS-07)",
                    });
                }
            }
        }

        if self.role == AgreementRole::Consumer
            && self.state == AgreementState::Finalized
            && self.token_secret_ref.is_none()
        {
            return Err(Error::Name {
                field: "spec.tokenSecretRef",
                value: String::new(),
                reason:
                    "tokenSecretRef is required for consumer role when state is finalized (DS-15)",
            });
        }

        if let Some(ref sec) = self.token_secret_ref {
            names::validate_dns1123_label(&sec.name).map_err(|e| match e {
                Error::Name { reason, .. } => Error::Name {
                    field: "spec.tokenSecretRef.name",
                    value: sec.name.clone(),
                    reason,
                },
                other => other,
            })?;
        }

        if let (Some(from), Some(to)) = (self.validity.from, self.validity.to) {
            if from > to {
                return Err(Error::Name {
                    field: "spec.validity",
                    value: format!("{from} > {to}"),
                    reason: "validity `from` must be less than or equal to `to`",
                });
            }
        }

        Ok(())
    }

    /// Checks whether transitioning to `next` agreement state is permitted (DS-09, DS-12).
    pub fn allows_transition_to(&self, next: AgreementState) -> bool {
        self.state.allows_transition_to(next)
    }

    /// Returns `true` if the agreement is finalized and `now` falls within its validity window (DS-12).
    pub fn is_active(&self, now: DateTime<Utc>) -> bool {
        if self.state != AgreementState::Finalized {
            return false;
        }
        if let Some(from) = self.validity.from {
            if now < from {
                return false;
            }
        }
        if let Some(to) = self.validity.to {
            if now > to {
                return false;
            }
        }
        true
    }
}
