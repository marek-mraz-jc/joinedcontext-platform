//! `kind: ContextBroker`, one Antares broker instance of the organization (PF-110, ADR-N-047).
//!
//! A space lives on one broker, named by its `spec.brokerRef` or else the default broker
//! (PF-111). [`broker_of`] is the one lookup the gateway, the reconciler and the Portal call, so
//! no caller keeps an address of its own. An organization with no `ContextBroker` manifest has
//! the implicit [`IMPLICIT_BROKER`], today's single deployment.

use crate::envelope::{Kind, ObjectMeta, Scope};
use crate::error::{Error, Result};
use crate::names;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// The broker of an organization that declares none (ADR-N-047 §2.1).
pub const IMPLICIT_BROKER: &str = "main";

/// One broker instance (PF-110).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct ContextBrokerSpec {
    /// The broker spaces without a `brokerRef` live on; exactly one per organization.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub default: bool,
    /// The Antares image, pinned by digest: `{repository}@sha256:{64 hex}`.
    pub image: String,
    /// A resource preset of the chart; a manifest never asks the node for raw resources.
    #[serde(default)]
    pub size: BrokerSize,
    /// The storage class of the broker's database; the cluster default when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub storage_class: Option<String>,
    /// The surfaces this instance serves; an Endpoint needing one its broker lacks is refused.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub features: Vec<BrokerFeature>,
}

/// A resource preset of the broker chart (ADR-N-047 §2.7).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum BrokerSize {
    /// The development preset: 100m CPU and 128 Mi requested.
    #[default]
    Small,
    /// Between the two, for a broker of a few busy spaces.
    Medium,
    /// The production preset: 200m CPU and 256 Mi requested, 2 CPU and 1 Gi at most.
    Large,
}

/// A surface of a broker instance.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "lowercase")]
pub enum BrokerFeature {
    /// NGSI-LD temporal history.
    Temporal,
    /// OGC API Features.
    Ogc,
    /// OData.
    Odata,
    /// The broker's own MCP server.
    Mcp,
    /// Context source registrations, the federation between brokers (PF-113).
    Csr,
}

impl Kind for ContextBrokerSpec {
    const KIND: &'static str = "ContextBroker";
    const PLURAL: &'static str = "contextbrokers";
    const SCOPE: Scope = Scope::Organization;
    const PATH_TEMPLATE: &'static str = "brokers/{name}.yaml";

    fn validate_spec(&self, meta: &ObjectMeta) -> Result<()> {
        names::validate_dns1123_label(&meta.name)?;
        self.validate()
    }
}

impl ContextBrokerSpec {
    /// The image is pinned by digest, the storage class is a name, no feature is listed twice.
    pub fn validate(&self) -> Result<()> {
        let pinned = self
            .image
            .split_once("@sha256:")
            .is_some_and(|(repo, hex)| {
                !repo.trim().is_empty()
                    && hex.len() == 64
                    && hex
                        .bytes()
                        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            });
        if !pinned {
            return Err(Error::Name {
                field: "spec.image",
                value: self.image.clone(),
                reason: "a broker image is pinned by digest: {repository}@sha256:{64 hex} (PF-110)",
            });
        }
        if let Some(class) = &self.storage_class {
            names::validate_dns1123_label(class)?;
        }
        let mut features = self.features.clone();
        features.sort();
        features.dedup();
        if features.len() != self.features.len() {
            return Err(Error::Invalid {
                field: "spec.features".to_owned(),
                reason: "a feature is listed twice".to_owned(),
            });
        }
        Ok(())
    }
}

/// The organization's brokers hold exactly one default, or none are declared at all (PF-110).
pub fn validate_brokers<'a>(
    brokers: impl IntoIterator<Item = (&'a str, &'a ContextBrokerSpec)>,
) -> Result<()> {
    let brokers: Vec<_> = brokers.into_iter().collect();
    let defaults: Vec<&str> = brokers
        .iter()
        .filter(|(_, spec)| spec.default)
        .map(|(name, _)| *name)
        .collect();
    if !brokers.is_empty() && defaults.len() != 1 {
        return Err(Error::Invalid {
            field: "ContextBroker.spec.default".to_owned(),
            reason: format!(
                "exactly one broker is the default, not {} ({})",
                defaults.len(),
                defaults.join(", ")
            ),
        });
    }
    Ok(())
}

/// The broker a space lives on (PF-111): its `brokerRef`, else the default broker, else the
/// implicit `main` of an organization that declares none. A `brokerRef` naming no broker is an
/// error, never a fallback to the default.
pub fn broker_of<'a>(
    broker_ref: Option<&'a str>,
    brokers: impl IntoIterator<Item = (&'a str, &'a ContextBrokerSpec)>,
) -> Result<&'a str> {
    let brokers: Vec<_> = brokers.into_iter().collect();
    if brokers.is_empty() {
        return match broker_ref {
            None | Some(IMPLICIT_BROKER) => Ok(IMPLICIT_BROKER),
            Some(name) => Err(no_such_broker(name)),
        };
    }
    match broker_ref {
        Some(name) => brokers
            .iter()
            .find(|(declared, _)| *declared == name)
            .map(|(declared, _)| *declared)
            .ok_or_else(|| no_such_broker(name)),
        None => {
            validate_brokers(brokers.iter().copied())?;
            Ok(brokers
                .iter()
                .find(|(_, spec)| spec.default)
                .map(|(name, _)| *name)
                .unwrap_or(IMPLICIT_BROKER))
        }
    }
}

fn no_such_broker(name: &str) -> Error {
    Error::Invalid {
        field: "spec.brokerRef".to_owned(),
        reason: format!("no ContextBroker is named '{name}' (PF-111)"),
    }
}
