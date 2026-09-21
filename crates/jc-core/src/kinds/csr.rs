//! Manifest kind for federation: where part of a space's data actually lives (T-0303, MF-36).
//!
//! A `ContextSourceRegistration` tells one Context Space's broker that entities matching a
//! claim are held somewhere else, so a query can be forwarded, merged and loop-protected by
//! the rules NGSI-LD already defines (CIM 009 clause 4.3.6). It is the manifest behind the hub
//! pattern of `Architecture/04 §5a`: a hub space holds only registrations and one Endpoint,
//! and that Endpoint answers over the union of the members.
//!
//! Two things this type refuses on purpose. A registration with no coverage claim, because a
//! source that claims nothing is matched by nothing and would be a silent no-op rather than a
//! federation. And a registration naming both a local Endpoint and an external address,
//! because the reconciler would have to choose one and the file would mean two things.

use crate::envelope::{Kind, ObjectMeta, Ref, Scope};
use crate::error::{Error, Result};
use crate::kinds::policy::RegistrationInfo;
use crate::names;
use chrono::{DateTime, Utc};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Which identity a forwarded request carries (PF-48).
///
/// Neither mode is a grant. `ServiceAccount` reads a source with the grants a `Policy` on that
/// source gave the named account; `Caller` reads it with the caller's own. In both, the hub
/// Endpoint's policy set applies as well and the narrower one wins.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum FederationIdentity {
    /// The hub forwards as a `ServiceAccount` of its own, named by `serviceAccountRef`.
    #[default]
    ServiceAccount,
    /// The caller's own token is forwarded, rewritten for the source's audience.
    Caller,
}

/// How the broker treats a source's answer relative to its own data (CIM 009 clause 5.2.9).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum RegistrationMode {
    /// The source is one of several answers and the broker merges it with the rest.
    #[default]
    Inclusive,
    /// The source is the only answer for what it covers.
    Exclusive,
    /// The source answers alongside the broker's own data without overriding it.
    Auxiliary,
    /// The caller is redirected to the source rather than the broker forwarding.
    Redirect,
}

/// Who this platform is when it forwards to the source (PF-48).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Federation {
    /// Which identity the forward carries.
    #[serde(default)]
    pub identity: FederationIdentity,
    /// The `ServiceAccount` the hub forwards as, required in `serviceAccount` mode.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service_account_ref: Option<Ref>,
}

/// Desired specification of a `ContextSourceRegistration` resource (MF-36, SP-09).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct ContextSourceRegistrationSpec {
    /// The Context Space whose broker learns of the source. The registration is written into
    /// this space's tenant and no other (SP-08, SP-09).
    pub context_space_ref: Ref,
    /// A source on this platform, as an `Endpoint`. Mutually exclusive with `endpoint`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint_ref: Option<Ref>,
    /// A source elsewhere, as the base URL of its NGSI-LD API. Mutually exclusive with
    /// `endpointRef`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
    /// What the source is claimed to hold, in CIM 009's own `information` shape: this is what
    /// the broker matches a query against, so it is the specification's member and not a shape
    /// of ours.
    pub information: Vec<RegistrationInfo>,
    /// Who this platform is when it forwards (PF-48).
    #[serde(default)]
    pub federation: Federation,
    /// How the source's answer relates to the broker's own data.
    #[serde(default)]
    pub mode: RegistrationMode,
    /// Which operations the source is registered for, as CIM 009 clause 5.2.9 names them: an
    /// operation (`retrieveEntity`) or one of its groups (`federationOps`). Left out means the
    /// specification's own default, which is why this is not an enum here: the vocabulary is
    /// the broker's and a list of ours would go stale the first time CIM 009 adds a member.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub operations: Vec<String>,
    /// When the registration stops being used, if it is not open-ended.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<DateTime<Utc>>,
    /// How often the source's schema surface is mirrored; 24 hours when absent (DM-49).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schedule: Option<crate::kinds::Schedule>,
}

impl Kind for ContextSourceRegistrationSpec {
    const KIND: &'static str = "ContextSourceRegistration";
    const PLURAL: &'static str = "csrs";
    const SCOPE: Scope = Scope::Project;
    const PATH_TEMPLATE: &'static str =
        "projects/{project}/spaces/{space}/registrations/{name}.yaml";

    fn validate_spec(&self, meta: &ObjectMeta) -> Result<()> {
        names::validate_dns1123_label(&meta.name)?;
        self.validate()?;
        self.validate_scope(meta.namespace.as_deref())
    }
}

impl ContextSourceRegistrationSpec {
    /// Validates the target, the coverage claim and the identity (MF-36, PF-48).
    pub fn validate(&self) -> Result<()> {
        match (&self.endpoint_ref, &self.endpoint) {
            (Some(_), Some(url)) => {
                return Err(Error::Name {
                    field: "spec.endpoint",
                    value: url.clone(),
                    reason: "a registration names an Endpoint on this platform or an address \
                             elsewhere, not both (MF-36)",
                })
            }
            (None, None) => {
                return Err(Error::Name {
                    field: "spec.endpointRef",
                    value: String::new(),
                    reason: "a registration has to say where the data is: endpointRef or \
                             endpoint (MF-36)",
                })
            }
            _ => {}
        }
        if let Some(reference) = &self.endpoint_ref {
            if let Some(kind) = reference.kind() {
                if kind != "Endpoint" {
                    return Err(Error::Name {
                        field: "spec.endpointRef.kind",
                        value: kind.to_owned(),
                        reason: "endpointRef names an Endpoint",
                    });
                }
            }
        }
        if let Some(url) = &self.endpoint {
            // http is allowed: a member broker on the same cluster is reached over the mesh,
            // which carries its own mTLS, and forcing https there would mean a certificate for
            // a Service name nothing outside the cluster can resolve.
            let Some(rest) = url
                .strip_prefix("https://")
                .or_else(|| url.strip_prefix("http://"))
            else {
                return Err(Error::Name {
                    field: "spec.endpoint",
                    value: url.clone(),
                    reason: "an external source is an http or https base URL",
                });
            };
            // A user or password in the address is a credential committed to Git and written
            // into the broker (MF-24). The address is not repeated: it carries the secret.
            let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
            if authority.contains('@') {
                return Err(Error::Name {
                    field: "spec.endpoint",
                    value: String::new(),
                    reason: "the address carries a user or password; a credential is a \
                             secretRef, never part of the manifest (MF-24)",
                });
            }
        }
        for operation in &self.operations {
            if operation.trim().is_empty() {
                return Err(Error::Name {
                    field: "spec.operations",
                    value: operation.clone(),
                    reason: "an operation is named, and an empty name would be forwarded to the \
                             broker as one",
                });
            }
        }
        // A claim of nothing matches nothing. Refusing it here is the difference between a
        // manifest that federates no data and a manifest the reviewer can see is wrong.
        if self.information.is_empty() {
            return Err(Error::Name {
                field: "spec.information",
                value: String::new(),
                reason: "a registration claims at least one entity selector (MF-36)",
            });
        }
        for info in &self.information {
            if info.entities.is_empty() {
                return Err(Error::Name {
                    field: "spec.information.entities",
                    value: String::new(),
                    reason: "an information entry selects at least one entity type",
                });
            }
            // A registration claims ids for its hub the way a policy grants them, so its
            // selectors keep the same rules: an unanchored idPattern would route ids of other
            // members to this one (CC-13, R24).
            for entity in &info.entities {
                entity.validate()?;
            }
        }
        crate::kinds::validate_mirror_schedule(self.schedule.as_ref())?;
        self.federation.validate()
    }

    /// Refuses a member outside the hub's own project (PF-48).
    ///
    /// A hub space's broker tenant reads its members' tenants directly (CIM 009 clause 4.3.6.5),
    /// and the hub Endpoint's policy set is the only gate on what comes back. Registering a
    /// member is therefore the act that exposes it to the hub's audience, and that act has to
    /// stay inside one steward's domain: a project is where a steward's authority ends, so a
    /// reference across one is refused here rather than at the broker, where it would already
    /// have been a grant.
    ///
    /// `project` is the registration's own namespace; a reference that names no namespace is
    /// the project's own by definition.
    pub fn validate_scope(&self, project: Option<&str>) -> Result<()> {
        for (field, reference) in [
            (
                "spec.contextSpaceRef.namespace",
                Some(&self.context_space_ref),
            ),
            ("spec.endpointRef.namespace", self.endpoint_ref.as_ref()),
            // The account a forward would act as is the hub's project's too (T-2550).
            (
                "spec.federation.serviceAccountRef.namespace",
                self.federation.service_account_ref.as_ref(),
            ),
        ] {
            let Some(namespace) = reference.and_then(Ref::namespace) else {
                continue;
            };
            if Some(namespace) != project {
                return Err(Error::Name {
                    field,
                    value: namespace.to_owned(),
                    reason: "a hub federates spaces of its own project: registering a member is \
                             what exposes it to the hub's audience (PF-48)",
                });
            }
        }
        Ok(())
    }
}

impl Federation {
    /// Validates that the chosen identity has what it needs (PF-48).
    pub fn validate(&self) -> Result<()> {
        match self.identity {
            // Without the account there is no identity to forward as, and a hub that fell back
            // to the caller's token because a field was missing would be a widening nobody
            // wrote down.
            FederationIdentity::ServiceAccount if self.service_account_ref.is_none() => {
                Err(Error::Name {
                    field: "spec.federation.serviceAccountRef",
                    value: String::new(),
                    reason: "serviceAccount identity names the ServiceAccount to forward as \
                             (PF-48)",
                })
            }
            FederationIdentity::Caller if self.service_account_ref.is_some() => Err(Error::Name {
                field: "spec.federation.serviceAccountRef",
                value: self
                    .service_account_ref
                    .as_ref()
                    .map(|r| r.name().to_owned())
                    .unwrap_or_default(),
                reason: "caller identity forwards the caller's token, so a ServiceAccount here \
                         would never be used (PF-48)",
            }),
            _ => {
                if let Some(reference) = &self.service_account_ref {
                    if let Some(kind) = reference.kind() {
                        if kind != "ServiceAccount" {
                            return Err(Error::Name {
                                field: "spec.federation.serviceAccountRef.kind",
                                value: kind.to_owned(),
                                reason: "serviceAccountRef names a ServiceAccount",
                            });
                        }
                    }
                }
                Ok(())
            }
        }
    }
}
