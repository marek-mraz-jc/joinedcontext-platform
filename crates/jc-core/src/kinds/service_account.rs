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
    /// The public keys a device outside the cluster signs its client assertion with (PF-47,
    /// T-4015); only on an `oauth-client` credential, never beside `workload`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub jwks: Option<Jwks>,
}

/// The most keys one credential may list: enough for a rotation, not a key store.
pub const MAX_DEVICE_KEYS: usize = 5;

/// A JSON Web Key Set of public keys (RFC 7517), the device's half of its key pair.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Jwks {
    /// One to five public keys, each with its own `kid`.
    pub keys: Vec<PublicJwk>,
}

/// One public key (RFC 7517/7518). It has no member for a private or symmetric key, so a JWK
/// carrying `d`, `p`, `q`, `dp`, `dq`, `qi` or `k` does not read at all.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PublicJwk {
    /// Key type: `EC` (P-256) or `RSA` (2048 bits or more).
    pub kty: String,
    /// Key id, unique within the set; the device names it in its assertion header.
    pub kid: String,
    /// Curve of an `EC` key: `P-256`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub crv: Option<String>,
    /// X coordinate of an `EC` key, base64url.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub x: Option<String>,
    /// Y coordinate of an `EC` key, base64url.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub y: Option<String>,
    /// Modulus of an `RSA` key, base64url.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub n: Option<String>,
    /// Public exponent of an `RSA` key, base64url.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub e: Option<String>,
    /// Intended algorithm: `ES256` for an `EC` key, `RS256` for an `RSA` key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub alg: Option<String>,
    /// Intended use: `sig`.
    #[serde(default, rename = "use", skip_serializing_if = "Option::is_none")]
    pub use_: Option<String>,
}

impl Jwks {
    /// Every rule of PF-47 for a device's keys: one to five, each `kid` once, each a public
    /// P-256 or RSA-2048+ signing key whose members are what its type needs and no more.
    pub fn validate(&self) -> Result<()> {
        fn refuse(value: String, reason: &'static str) -> Result<()> {
            Err(Error::Name {
                field: "spec.credentials.jwks",
                value,
                reason,
            })
        }
        if self.keys.is_empty() || self.keys.len() > MAX_DEVICE_KEYS {
            return refuse(
                self.keys.len().to_string(),
                "a device key set holds one to five keys",
            );
        }
        let mut kids = std::collections::BTreeSet::new();
        for key in &self.keys {
            if key.kid.trim().is_empty() || key.kid.len() > 128 {
                return refuse(
                    key.kid.clone(),
                    "every key needs a kid of 1 to 128 characters",
                );
            }
            if !kids.insert(key.kid.as_str()) {
                return refuse(key.kid.clone(), "two keys share this kid");
            }
            if key.use_.as_deref().is_some_and(|u| u != "sig") {
                return refuse(
                    key.kid.clone(),
                    "a device key is for signing: `use` is `sig`",
                );
            }
            match key.kty.as_str() {
                "EC" => {
                    if key.crv.as_deref() != Some("P-256")
                        || key.alg.as_deref().is_some_and(|a| a != "ES256")
                    {
                        return refuse(key.kid.clone(), "an EC key is P-256, for ES256");
                    }
                    // A P-256 coordinate is 32 bytes: 43 base64url characters.
                    if b64(&key.x).map(str::len) != Some(43)
                        || b64(&key.y).map(str::len) != Some(43)
                    {
                        return refuse(
                            key.kid.clone(),
                            "an EC key needs x and y of 32 bytes each, base64url",
                        );
                    }
                    if key.n.is_some() || key.e.is_some() {
                        return refuse(key.kid.clone(), "an EC key carries no n or e");
                    }
                }
                "RSA" => {
                    if key.alg.as_deref().is_some_and(|a| a != "RS256") {
                        return refuse(key.kid.clone(), "an RSA key is for RS256");
                    }
                    // 2048 bits are 256 bytes: at least 342 base64url characters.
                    if b64(&key.n).is_none_or(|n| n.len() < 342) || b64(&key.e).is_none() {
                        return refuse(
                            key.kid.clone(),
                            "an RSA key needs n of 2048 bits or more and e, base64url",
                        );
                    }
                    if key.crv.is_some() || key.x.is_some() || key.y.is_some() {
                        return refuse(key.kid.clone(), "an RSA key carries no crv, x or y");
                    }
                }
                _ => return refuse(key.kty.clone(), "a device key is EC (P-256) or RSA"),
            }
        }
        Ok(())
    }
}

/// A member's value when it is non-empty base64url, the form RFC 7518 writes it in.
fn b64(value: &Option<String>) -> Option<&str> {
    value.as_deref().filter(|v| is_base64url(v))
}

fn is_base64url(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// Category of service account credential.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum CredentialKind {
    /// Keycloak confidential OAuth 2.1 client with client_credentials flow; the only kind, as
    /// the platform mints and accepts no static API key (PF-37, T-4362).
    OauthClient,
}

impl CredentialKind {
    /// Returns the kebab-case wire name for this credential kind.
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::OauthClient => "oauth-client",
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
        if crate::kinds::pipeline_identity::is_derived(&meta.name) {
            return Err(Error::Name {
                field: "metadata.name",
                value: meta.name.clone(),
                reason: "starts with `pl-`, the prefix of the account every Pipeline is given \
                         (PL-19); a ServiceAccount written by hand must choose another name",
            });
        }
        if crate::kinds::app_identity::is_derived(&meta.name) {
            return Err(Error::Name {
                field: "metadata.name",
                value: meta.name.clone(),
                reason:
                    "starts with `appjob-`, the prefix of the account a wasm App's jobs run as \
                         (AP-159); a ServiceAccount written by hand must choose another name",
            });
        }
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

            if let Some(jwks) = &cred.jwks {
                if self.workload.is_some() {
                    return Err(Error::Name {
                        field: "spec.credentials.jwks",
                        value: cred.name.clone(),
                        reason: "an account bound to a workload signs in by its pod's token, \
                                 not by a key pair as well (PF-47)",
                    });
                }
                jwks.validate()?;
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
