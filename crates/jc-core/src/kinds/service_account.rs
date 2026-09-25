//! Manifest kinds for ServiceAccounts and credentials (T-0115, PF-34..PF-37, PF-47).

use crate::envelope::{Kind, ObjectMeta, Scope};
use crate::error::{Error, Result};
use crate::kinds::policy::OperationRef;
use crate::names;
use chrono::{DateTime, Utc};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::fmt;

/// Desired specification of a [`ServiceAccount`][crate::kinds::ServiceAccount] resource (PF-34, PF-35, PF-47).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct ServiceAccountSpec {
    /// Accountable human owner of this service identity (PF-34).
    pub owner: Owner,
    /// Reason why this identity exists, shown in audit reviews.
    pub purpose: String,
    /// Scoped platform role bindings granted to this identity.
    pub roles: Vec<RoleBinding>,
    /// Authentication credentials registered for this identity.
    pub credentials: Vec<Credential>,
    /// Optional rate limiting configuration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limits: Option<ServiceAccountLimits>,
    /// Optional Kubernetes workload binding (PF-47).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workload: Option<Workload>,
    /// Whether tokens this account's client obtains by exchanging a person's own act for that
    /// person (ADR-N-038, AG-95). Absent, a token of this client whose subject is a person is
    /// refused.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delegation: Option<Delegation>,
}

/// How an account's client may act for a person (ADR-N-038, AG-95).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum Delegation {
    /// RFC 8693 token exchange: the client turns the person's token into one of its own whose
    /// subject is still the person, and the gateway decides it as that person.
    TokenExchange,
}

/// Accountable human owner reference (PF-34).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Owner {
    /// Identifier or username of the accountable human user.
    pub user: String,
}

/// Scoped role grant assignment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct RoleBinding {
    /// Role template name (DNS-1123 label).
    pub role: String,
    /// Target hierarchical scope of this grant.
    pub scope: RoleScope,
    /// Optional entity types whitelist.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub types: Vec<String>,
    /// CIM 009 operations or operation groups granted (R8).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub operations: Vec<OperationRef>,
}

/// Target scope of a role grant: exactly one of `contextSpace`, `project`, or `organization`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct RoleScope {
    /// Context Space scope target.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_space: Option<String>,
    /// Project scope target.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    /// Organization scope target.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub organization: Option<String>,
}

impl RoleScope {
    /// Exactly one target, and it is a well-formed name of its level (shared with `RoleBinding`, PF-49).
    pub fn validate(&self, field: &'static str) -> Result<()> {
        let count = usize::from(self.context_space.is_some())
            + usize::from(self.project.is_some())
            + usize::from(self.organization.is_some());
        if count != 1 {
            return Err(Error::Name {
                field,
                value: format!("{count} scopes defined"),
                reason: "role scope must specify exactly one of `contextSpace`, `project`, or `organization`",
            });
        }
        if let Some(space) = &self.context_space {
            names::validate_space_name(space)?;
        }
        if let Some(project) = &self.project {
            names::validate_namespace(project)?;
        }
        if let Some(org) = &self.organization {
            names::validate_dns1123_label(org)?;
        }
        Ok(())
    }
}

/// Declared credential for a service account (PF-36, PF-37).
///
/// **Security note**: The security property of this kind is the complete absence of secret fields.
/// `Credential` has nowhere to put a secret and carries `deny_unknown_fields`, so `secret:`,
/// `value:`, `password:`, `clientSecret:` and `apiKey:` are all rejected at parse time (PF-36).
/// The key id and the Argon2id hash are stored in the database / Keycloak / secret store,
/// never in manifest state or Git.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Credential {
    /// Type of credential.
    pub kind: CredentialKind,
    /// Credential identifier name (DNS-1123 label).
    pub name: String,
    /// Optional expiration timestamp (UTC).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<DateTime<Utc>>,
    /// Whitelist of allowed client IP CIDR blocks (PF-36).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ip_allow_list: Vec<String>,
}

/// Category of service account credential.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum CredentialKind {
    /// Keycloak confidential OAuth 2.1 client with client_credentials flow.
    OauthClient,
    /// Static hashed API bearer key for external legacy systems (PF-36, PF-37).
    ApiKey,
}

impl CredentialKind {
    /// Returns the kebab-case wire name for this credential kind.
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::OauthClient => "oauth-client",
            Self::ApiKey => "api-key",
        }
    }
}

impl fmt::Display for CredentialKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

/// Rate limiting bounds for a service account.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct ServiceAccountLimits {
    /// Maximum allowed requests per minute.
    pub requests_per_minute: u32,
}

/// Cluster workload binding container (PF-47).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Workload {
    /// Kubernetes pod workload binding.
    pub kubernetes: KubernetesBinding,
}

/// Kubernetes ServiceAccount binding coordinates (PF-47).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct KubernetesBinding {
    /// Kubernetes namespace name.
    pub namespace: String,
    /// Kubernetes ServiceAccount name.
    pub service_account: String,
}

impl Kind for ServiceAccountSpec {
    const KIND: &'static str = "ServiceAccount";
    const PLURAL: &'static str = "serviceaccounts";
    const SCOPE: Scope = Scope::Project;
    const PATH_TEMPLATE: &'static str = "projects/{project}/access/serviceaccounts/{name}.yaml";

    fn validate_spec(&self, meta: &ObjectMeta) -> Result<()> {
        names::validate_dns1123_label(&meta.name)?;
        if let Some(project) = meta.namespace.as_deref() {
            let id = keycloak_client_id(project, &meta.name);
            if PLATFORM_CLIENTS.contains(&id.as_str()) {
                return Err(Error::Name {
                    field: "metadata.name",
                    value: id,
                    reason: "is the Keycloak client id of a platform workload, which a \
                             ServiceAccount must not take (T-1454); choose another name",
                });
            }
        }
        self.validate()
    }
}

/// The Keycloak client id of a service account: derived, never chosen (Architecture/12 §3).
pub fn keycloak_client_id(project: &str, name: &str) -> String {
    format!("{project}-{name}")
}

/// The realm's clients that belong to the platform itself and have no `ServiceAccount` manifest:
/// the keys of `components/*/keycloak-clients.yaml` in `joinedcontext-deployment`, less the ones
/// the seed declares as accounts (`helsinki-pipelines`, `helsinki-agent-proxy`,
/// `banskabystrica-conformance`, `mcp-mobile`). A token's `azp` is the one claim that tells these
/// workloads apart (the Portal's activity ingest admits `activity-ingest` alone), so an account
/// that derives one of them is refused (T-1454). A new platform client is added here too.
pub const PLATFORM_CLIENTS: &[&str] = &[
    "activity-ingest",
    "apisix-gateway",
    "ckan",
    "edge",
    "gitea",
    "grafana",
    "portal-api",
    "portal-reconciler",
    "portal-ui",
];

impl ServiceAccountSpec {
    /// Validates owner, purpose, roles, credential formats, and limits.
    pub fn validate(&self) -> Result<()> {
        if self.owner.user.trim().is_empty() {
            return Err(Error::Name {
                field: "spec.owner.user",
                value: self.owner.user.clone(),
                reason: "owner user must not be empty",
            });
        }
        if self.purpose.trim().is_empty() {
            return Err(Error::Name {
                field: "spec.purpose",
                value: self.purpose.clone(),
                reason: "purpose must not be empty",
            });
        }

        if self.roles.is_empty() {
            return Err(Error::Name {
                field: "spec.roles",
                value: String::new(),
                reason: "roles must not be empty",
            });
        }

        for role_binding in &self.roles {
            names::validate_dns1123_label(&role_binding.role)?;

            role_binding.scope.validate("spec.roles.scope")?;

            for t in &role_binding.types {
                names::validate_entity_type(t)?;
            }

            let mut seen_ops = std::collections::BTreeSet::new();
            for op in &role_binding.operations {
                if !seen_ops.insert(op.as_str()) {
                    return Err(Error::Name {
                        field: "spec.roles.operations",
                        value: op.as_str().to_string(),
                        reason: "duplicate operation in role operations list",
                    });
                }
            }
        }

        if self.credentials.is_empty() {
            return Err(Error::Name {
                field: "spec.credentials",
                value: String::new(),
                reason: "credentials must not be empty",
            });
        }

        let mut seen_creds = std::collections::BTreeSet::new();
        for cred in &self.credentials {
            names::validate_dns1123_label(&cred.name)?;
            if !seen_creds.insert(&cred.name) {
                return Err(Error::Name {
                    field: "spec.credentials.name",
                    value: cred.name.clone(),
                    reason: "duplicate credential name in credentials list",
                });
            }

            for cidr in &cred.ip_allow_list {
                validate_cidr(cidr)?;
            }
        }

        if let Some(ref limits) = self.limits {
            if limits.requests_per_minute == 0 {
                return Err(Error::Name {
                    field: "spec.limits.requestsPerMinute",
                    value: "0".to_string(),
                    reason: "requestsPerMinute must be >= 1",
                });
            }
        }

        if let Some(ref wl) = self.workload {
            names::validate_dns1123_label(&wl.kubernetes.namespace)?;
            names::validate_dns1123_label(&wl.kubernetes.service_account)?;
        }

        Ok(())
    }

    /// Returns `true` if this service account registers any credential of kind [`CredentialKind::ApiKey`] (PF-37).
    pub fn has_api_key(&self) -> bool {
        self.credentials
            .iter()
            .any(|c| c.kind == CredentialKind::ApiKey)
    }
}

/// Validates an IPv4 or IPv6 CIDR block notation (`<ip>/<prefix>`).
fn validate_cidr(cidr: &str) -> Result<()> {
    let parts: Vec<&str> = cidr.split('/').collect();
    if parts.len() != 2 {
        return Err(Error::Name {
            field: "ipAllowList",
            value: cidr.to_string(),
            reason: "CIDR must contain exactly one `/` separating IP address and prefix length",
        });
    }

    let ip: std::net::IpAddr = parts[0].parse().map_err(|_| Error::Name {
        field: "ipAllowList",
        value: cidr.to_string(),
        reason: "invalid IP address in CIDR block",
    })?;

    if !parts[1].chars().all(|c| c.is_ascii_digit()) {
        return Err(Error::Name {
            field: "ipAllowList",
            value: cidr.to_string(),
            reason: "invalid prefix length in CIDR block (must contain only ascii digits)",
        });
    }

    let prefix: u8 = parts[1].parse().map_err(|_| Error::Name {
        field: "ipAllowList",
        value: cidr.to_string(),
        reason: "prefix length is outside valid integer range",
    })?;

    let max_prefix = match ip {
        std::net::IpAddr::V4(_) => 32,
        std::net::IpAddr::V6(_) => 128,
    };

    if prefix > max_prefix {
        return Err(Error::Name {
            field: "ipAllowList",
            value: cidr.to_string(),
            reason: "CIDR prefix length exceeds address bit length (32 for IPv4, 128 for IPv6)",
        });
    }

    Ok(())
}
