//! `kind: Environment`: the values one environment renders the repository with (CC-73…CC-75).
//!
//! What differs between `dev`, `staging` and a city's production is not configuration but
//! values: the organization's domain there, the hosts, the image digests, which secret backend
//! answers a `secretRef`, a feature flag. One overlay per environment lives in
//! `environments/{name}.yaml`, and every loader merges the one `JC_ENVIRONMENT` names over the
//! manifests before validation, so one repository renders every environment and a manifest
//! carries `{orgDomain}` rather than a host of its own (CC-74).
//!
//! An overlay names a secret backend, never a secret value (CC-75); `deny_unknown_fields` is
//! what refuses one.

use std::collections::BTreeMap;

use crate::envelope::{Kind, ObjectMeta, Scope};
use crate::error::{Error, Result};
use crate::names;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Where a `secretRef` is resolved in this environment (CC-06, CC-75).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct SecretBackend {
    /// The backend that answers: `openbao`, `kubernetes`, `sops`.
    pub backend: String,
    /// Where inside it, e.g. `jc/staging`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mount: Option<String>,
}

/// Desired specification of an [`Environment`][crate::kinds::Environment] (CC-73).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct EnvironmentSpec {
    /// The organization's domain in this environment; every `{orgDomain}` of every manifest is
    /// rendered with it before validation (CC-74).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub org_domain: Option<String>,
    /// The hosts of this environment by component, e.g. `portal: portal.staging.bb.example`.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub hosts: BTreeMap<String, String>,
    /// The image digest each component runs here, by name; a digest, never a tag.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub images: BTreeMap<String, String>,
    /// Where a `secretRef` is resolved in this environment; a backend, never a value (CC-75).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secrets: Option<SecretBackend>,
    /// Feature flags of this environment, e.g. `publicEndpoints: false`.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub features: BTreeMap<String, bool>,
}

impl Kind for EnvironmentSpec {
    const KIND: &'static str = "Environment";
    const PLURAL: &'static str = "environments";
    const SCOPE: Scope = Scope::Organization;
    const PATH_TEMPLATE: &'static str = "environments/{name}.yaml";

    fn validate_spec(&self, meta: &ObjectMeta) -> Result<()> {
        names::validate_dns1123_label(&meta.name)?;
        self.validate()
    }
}

impl EnvironmentSpec {
    /// The domain is a domain, every image is pinned by digest, and the secret backend is named.
    pub fn validate(&self) -> Result<()> {
        if let Some(domain) = self.org_domain.as_deref() {
            names::validate_org_domain(domain).map_err(|_| Error::Name {
                field: "spec.orgDomain",
                value: domain.to_owned(),
                reason: "the organization's domain in this environment, e.g. staging.bb.sk",
            })?;
        }
        for (component, host) in &self.hosts {
            names::validate_org_domain(host).map_err(|_| Error::Name {
                field: "spec.hosts",
                value: format!("{component}: {host}"),
                reason: "a host is a domain name, without a scheme or a path",
            })?;
        }
        // A tag moves and a digest does not; an environment that pins by tag is not reproducible
        // (CC-74, OPS-13).
        for (component, image) in &self.images {
            if !image.starts_with("sha256:") || image.len() != "sha256:".len() + 64 {
                return Err(Error::Name {
                    field: "spec.images",
                    value: format!("{component}: {image}"),
                    reason: "an image is pinned by digest: sha256: and 64 hex characters",
                });
            }
        }
        if let Some(secrets) = &self.secrets {
            if secrets.backend.trim().is_empty() {
                return Err(Error::Name {
                    field: "spec.secrets.backend",
                    value: secrets.backend.clone(),
                    reason: "an overlay names the backend that answers a secretRef (CC-75)",
                });
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta() -> ObjectMeta {
        ObjectMeta {
            name: "staging".to_owned(),
            namespace: Some(crate::envelope::ORG_NAMESPACE.to_owned()),
            ..Default::default()
        }
    }

    fn overlay() -> EnvironmentSpec {
        EnvironmentSpec {
            org_domain: Some("staging.banskabystrica.sk".to_owned()),
            hosts: [("portal".to_owned(), "portal.staging.bb.example".to_owned())]
                .into_iter()
                .collect(),
            images: [("portal".to_owned(), format!("sha256:{}", "a".repeat(64)))]
                .into_iter()
                .collect(),
            secrets: Some(SecretBackend {
                backend: "openbao".to_owned(),
                mount: Some("jc/staging".to_owned()),
            }),
            features: [("publicEndpoints".to_owned(), false)]
                .into_iter()
                .collect(),
        }
    }

    #[test]
    fn the_overlay_of_the_architecture_chapter_validates() {
        overlay().validate_spec(&meta()).expect("the example");
    }

    #[test]
    fn an_image_pinned_by_tag_is_refused() {
        let mut spec = overlay();
        spec.images
            .insert("portal".to_owned(), "ghcr.io/x/portal:main".to_owned());
        let err = spec.validate().expect_err("a tag is not a digest");
        assert!(format!("{err}").contains("digest"), "{err}");
    }

    #[test]
    fn a_secret_value_is_not_a_field_of_an_overlay() {
        // `deny_unknown_fields` is the refusal; CC-75 is what it enforces.
        let refused: std::result::Result<EnvironmentSpec, _> = serde_json::from_value(
            serde_json::json!({ "secrets": { "backend": "openbao", "value": "hunter2" } }),
        );
        assert!(refused.is_err(), "an overlay never carries a secret value");
    }

    #[test]
    fn a_host_with_a_scheme_is_not_a_host() {
        let mut spec = overlay();
        spec.hosts.insert(
            "gateway".to_owned(),
            "https://api.staging.bb.example".to_owned(),
        );
        assert!(spec.validate().is_err());
    }
}
