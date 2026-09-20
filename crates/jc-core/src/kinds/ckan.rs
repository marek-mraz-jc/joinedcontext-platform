//! `kind: CkanInstance` and the `spec.publish.ckan` block of an Endpoint (T-0314, T-0316,
//! EP-62…EP-67).
//!
//! An open-data catalogue is a place an Endpoint is published to, so the catalogue is a
//! manifest like everything else and the publication is a field on the Endpoint that names it.
//! Neither carries a credential: the API token is a [`SecretRef`] the reconciler resolves at
//! run time, and `deny_unknown_fields` refuses an inline `token:` or `apiKey:` at parse time
//! (EP-67, CC-06).

use crate::envelope::{Kind, ObjectMeta, Ref, Scope, SecretRef};
use crate::error::{Error, Result};
use crate::kinds::endpoint::Representation;
use crate::names;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::fmt;

/// Desired specification of a [`CkanInstance`][crate::kinds::CkanInstance] resource (EP-62).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct CkanInstanceSpec {
    /// Base URL of the CKAN instance, `https://` only.
    pub url: String,
    /// The organization slug a dataset lands in when the Endpoint names none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub organization_default: Option<String>,
    /// Where the API token lives; never the token itself (EP-67, CC-06).
    pub api_token_ref: SecretRef,
}

impl Kind for CkanInstanceSpec {
    const KIND: &'static str = "CkanInstance";
    const PLURAL: &'static str = "ckaninstances";
    const SCOPE: Scope = Scope::Project;
    const PATH_TEMPLATE: &'static str = "projects/{project}/ckan/{name}.yaml";

    fn validate_spec(&self, meta: &ObjectMeta) -> Result<()> {
        names::validate_dns1123_label(&meta.name)?;
        self.validate()
    }
}

impl CkanInstanceSpec {
    /// Validates the URL, the default organization and the token reference.
    pub fn validate(&self) -> Result<()> {
        // Plain HTTP would carry the API token in the clear on every publication.
        if !self.url.starts_with("https://") {
            return Err(Error::Name {
                field: "url",
                value: self.url.clone(),
                reason: "the CKAN URL must be https:// (EP-67)",
            });
        }
        if self.url.trim_end_matches('/').len() <= "https://".len() {
            return Err(Error::Name {
                field: "url",
                value: self.url.clone(),
                reason: "the CKAN URL must name a host",
            });
        }
        if let Some(organization) = &self.organization_default {
            validate_organization(organization)?;
        }
        if self.api_token_ref.name.trim().is_empty() {
            return Err(Error::Name {
                field: "apiTokenRef.name",
                value: String::new(),
                reason: "the API token reference must name a secret (EP-67)",
            });
        }
        Ok(())
    }

    /// The instance URL without its trailing slash, which is what an API path is joined to.
    pub fn base_url(&self) -> &str {
        self.url.trim_end_matches('/')
    }
}

/// How often the DataStore mirror is refreshed (EP-65).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum DataStoreRefresh {
    /// Driven by the Endpoint's own subscription: one upsert per changed entity (the default).
    #[default]
    OnChange,
    /// Reloaded whenever the Endpoint is reconciled, for a space without a subscription.
    OnReconcile,
}

impl DataStoreRefresh {
    /// Wire name of this cadence.
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::OnChange => "onChange",
            Self::OnReconcile => "onReconcile",
        }
    }
}

/// The optional row mirror in the CKAN DataStore (EP-65).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct DataStore {
    /// The tabular representation the rows are read through; it must be enabled (EP-44).
    pub representation: Representation,
    /// How the mirror is kept current.
    #[serde(default)]
    pub refresh: DataStoreRefresh,
}

/// Publication of one Endpoint as one CKAN dataset (EP-62…EP-65).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct CkanPublication {
    /// The `CkanInstance` this Endpoint publishes to.
    pub instance_ref: Ref,
    /// The CKAN organization slug; the instance's default when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub organization: Option<String>,
    /// The CKAN dataset name; the Endpoint's own name when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// The optional row mirror (EP-65).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub datastore: Option<DataStore>,
}

impl CkanPublication {
    /// The CKAN dataset name of an Endpoint called `endpoint`.
    pub fn dataset_name<'a>(&'a self, endpoint: &'a str) -> &'a str {
        self.name.as_deref().unwrap_or(endpoint)
    }

    fn validate(&self, enabled: &[Representation]) -> Result<()> {
        names::validate_dns1123_label(self.instance_ref.name())
            .map_err(|e| rename(e, "publish.ckan.instanceRef"))?;
        if let Some(kind) = self.instance_ref.kind() {
            if kind != CkanInstanceSpec::KIND {
                return Err(Error::Kind {
                    expected: CkanInstanceSpec::KIND,
                    got: kind.to_string(),
                });
            }
        }
        if let Some(organization) = &self.organization {
            validate_organization(organization)?;
        }
        if let Some(name) = &self.name {
            validate_dataset_name(name)?;
        }
        if let Some(datastore) = &self.datastore {
            // A mirror of a representation the Endpoint does not serve would have nothing to
            // read: the publisher is an ordinary consumer of this Endpoint (EP-66).
            if !enabled.contains(&datastore.representation) {
                return Err(Error::Name {
                    field: "publish.ckan.datastore.representation",
                    value: datastore.representation.as_str().to_string(),
                    reason:
                        "the mirror reads a representation the endpoint does not enable (EP-65)",
                });
            }
            if !matches!(
                datastore.representation,
                Representation::Csv | Representation::Xlsx | Representation::Json
            ) {
                return Err(Error::Name {
                    field: "publish.ckan.datastore.representation",
                    value: datastore.representation.as_str().to_string(),
                    reason: "a DataStore table is filled from a tabular representation (EP-44)",
                });
            }
        }
        Ok(())
    }
}

/// Where an Endpoint is published beside its own surface (EP-62).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Publication {
    /// Publication to an open-data catalogue.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ckan: Option<CkanPublication>,
}

impl Publication {
    /// Validates every declared target against the representations the Endpoint enables.
    pub fn validate(&self, enabled: &[Representation]) -> Result<()> {
        match &self.ckan {
            Some(ckan) => ckan.validate(enabled),
            None => Ok(()),
        }
    }
}

/// A CKAN organization or dataset slug: lowercase, digits, `-` and `_`, at least two
/// characters, which is what CKAN's own name validator accepts.
fn validate_slug(field: &'static str, value: &str) -> Result<()> {
    let ok = value.len() >= 2
        && value.len() <= 100
        && value
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_')
        && !value.starts_with(['-', '_'])
        && !value.ends_with(['-', '_']);
    if !ok {
        return Err(Error::Name {
            field,
            value: value.to_string(),
            reason: "a CKAN name is 2 to 100 characters of a-z, 0-9, `-` and `_`",
        });
    }
    Ok(())
}

fn validate_organization(value: &str) -> Result<()> {
    validate_slug("publish.ckan.organization", value)
}

fn validate_dataset_name(value: &str) -> Result<()> {
    validate_slug("publish.ckan.name", value)
}

impl fmt::Display for DataStoreRefresh {
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
