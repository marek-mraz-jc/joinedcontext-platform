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
use crate::names;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

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
    /// The removed row mirror (T-5060). Read only so that `validate` can refuse the block
    /// with the fix instead of serde's bare "unknown field"; absent from the schema.
    #[doc(hidden)]
    #[serde(default, skip_serializing)]
    #[schemars(skip)]
    pub datastore: Option<serde_json::Value>,
    /// The CKAN licence id the organization chose, such as `cc-by` for CC-BY 4.0. Used when the
    /// Endpoint's DCAT-AP record names no `dct:license`, which is the one term the record does
    /// not carry (EP-62, EP-63).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub license: Option<String>,
}

impl CkanPublication {
    /// The CKAN dataset name of an Endpoint called `endpoint`.
    pub fn dataset_name<'a>(&'a self, endpoint: &'a str) -> &'a str {
        self.name.as_deref().unwrap_or(endpoint)
    }

    fn validate(&self) -> Result<()> {
        names::validate_dns1123_label(self.instance_ref.name())
            .map_err(|e| names::rename(e, "publish.ckan.instanceRef"))?;
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
        if let Some(license) = &self.license {
            validate_license(license)?;
        }
        if self.datastore.is_some() {
            return Err(Error::Name {
                field: "publish.ckan.datastore",
                value: String::new(),
                reason: "publish.ckan.datastore was removed (T-5060): CKAN holds no rows, the \
                         catalogue reads the Endpoint live; delete the block",
            });
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
    /// Validates every declared target.
    pub fn validate(&self) -> Result<()> {
        match &self.ckan {
            Some(ckan) => ckan.validate(),
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

/// A licence register id: what CKAN stores in `license_id`. An IRI is not one — it belongs in
/// the record's `dct:license`, which the publisher maps to the `license_url` extra.
fn validate_license(value: &str) -> Result<()> {
    let ok = !value.is_empty()
        && value.len() <= 100
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'));
    if !ok {
        return Err(Error::Name {
            field: "publish.ckan.license",
            value: value.to_string(),
            reason: "a CKAN licence id is 1 to 100 characters of letters, digits, `-`, `_` and \
                     `.`, such as `cc-by` for CC-BY 4.0",
        });
    }
    Ok(())
}
