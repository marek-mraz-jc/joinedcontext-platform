//! Configuration parameters for `jc-agent-proxy`.
//!
//! Credentials and secrets are loaded from file paths or environment variables,
//! and are strictly redacted in `Debug` implementations to prevent log leakage.

use std::net::SocketAddr;
use std::path::PathBuf;
use url::Url;

#[derive(Clone)]
pub struct Config {
    /// The address to listen on (`JC_PROXY_BIND`, default `0.0.0.0:8080`).
    pub bind: SocketAddr,
    /// The Portal as the proxy reaches it inside the cluster (`JC_PORTAL_BASE`, default
    /// `http://portal:8080`), where it reads a run's plan and posts its callbacks.
    pub portal_base: Url,
    /// The Context Gateway as the proxy reaches it inside the cluster (`JC_GATEWAY_BASE`,
    /// default `http://context-gateway:8080`), the only address a run's context call is
    /// forwarded to.
    pub gateway_base: Url,
    /// The forge as the proxy reaches it inside the cluster (`JC_FORGE_BASE`, default
    /// `http://gitea-http:3000`), where a run's branch and its merge request are written.
    pub forge_base: Url,
    /// The configuration repository a run proposes its change to (`JC_FORGE_REPO`, default
    /// `joinedcontext/configuration`), as `owner/name`.
    pub forge_repo: String,
    /// The realm every token the proxy mints and verifies is issued by (`JC_OIDC_ISSUER`,
    /// default `http://keycloak:8080/realms/joinedcontext`).
    pub oidc_issuer: Url,
    /// The proxy's own Keycloak client (`JC_OIDC_CLIENT_ID`, default `agent-proxy`), whose
    /// token the Portal's internal listener accepts (AG-52).
    pub oidc_client_id: String,
    /// That client's secret, read from the file named by `JC_OIDC_CLIENT_SECRET_FILE` or,
    /// when no file is named, from `JC_OIDC_CLIENT_SECRET` itself. A secret: it comes from a
    /// `secretRef` the deployment mounts, it is redacted in `Debug`, and an empty value is a
    /// startup failure rather than a proxy that presents no credential (see
    /// `Config::require_secrets`).
    pub oidc_client_secret: String,
    /// The pod's projected ServiceAccount token (`JC_OIDC_CLIENT_ASSERTION_FILE`), presented as
    /// `client_assertion` to a `federated-jwt` client in place of a secret (PF-47, T-2868). Read
    /// at every call, because the kubelet rewrites the file before the token expires; set beside
    /// a secret it is a startup failure.
    pub oidc_client_assertion_file: Option<PathBuf>,
    /// The forge token the proxy writes a run's branch with, read from the file named by
    /// `JC_FORGE_TOKEN_FILE` or from `JC_FORGE_TOKEN`. A secret, on the same terms as
    /// `oidc_client_secret`.
    pub forge_token: String,
    /// The model API the proxy forwards a run's completions to (`JC_MODEL_BASE`, default
    /// `https://api.anthropic.com`).
    pub model_base: Url,
    /// The key for that API, read from the file named by `JC_MODEL_KEY_FILE` or from
    /// `JC_MODEL_KEY`. A secret, on the same terms as `oidc_client_secret`: it is the one
    /// credential a run never holds, which is why the run talks to this proxy at all.
    pub model_key: String,
    /// Which provider's protocol `model_base` speaks (`JC_MODEL_PROVIDER`, default
    /// `anthropic`).
    pub model_provider: String,
    /// Where the realm's token endpoint is, for the same reason the gateway needs one
    /// (`JC_OIDC_TOKEN_URL`, T-2272): the issuer is the address a browser uses, and a pod cannot
    /// dial its own cluster's ingress hostname. The issuer's own endpoint when none is named.
    pub oidc_token_url: Option<String>,
    /// Whether a caller must arrive through the mesh with a Linkerd identity
    /// (`JC_REQUIRE_MESH_IDENTITY`, `true` or `false`; default on, F-PLT1-008). A run's ticket is
    /// then honoured from the run's own workload or the Portal's alone, so a ticket copied out of
    /// a run works from no other pod. `false` is for a proxy outside a mesh (a laptop, a test).
    pub require_mesh_identity: bool,
    /// The Portal's Keycloak client (`JC_PORTAL_CLIENT_ID`, default `portal-api`): the one
    /// caller whose service-account token may hand this proxy a person's token for a run
    /// (`POST /internal/runs/{run}/identity`, ADR-N-038, AG-52, AG-94).
    pub portal_client_id: String,
    /// The `jc-functions` workload's ServiceAccount (`JC_PROXY_FUNCTIONS_IDENTITY`, default
    /// `jc-functions`): with mesh identity required, the one workload a run's data credential is
    /// honoured from (ADR-N-038 decision 6).
    pub functions_identity: String,
    /// The Portal's workload (`JC_PROXY_PORTAL_IDENTITY`, `<serviceaccount>` or
    /// `<serviceaccount>.<namespace>`, default `portal`): with mesh identity required, the one
    /// workload besides a run's own whose ticket is honoured. The Portal drives conversations and
    /// `ui`/`wasm` builds in-process and holds those tickets (AP-56, AG-54).
    pub portal_identity: String,
    /// The knowledge assistant's Keycloak client (`JC_ASSISTANT_CLIENT_ID`, default
    /// `jc-assistant`): its service account's token buys model calls for an `AssistantDeployment`,
    /// counted against that deployment's day (AG-109, AG-110).
    pub assistant_client_id: String,
    /// Daily model token caps (AG-97): `JC_DAILY_TOKENS_ASSISTANT`, `JC_DAILY_TOKENS_APP_BUILDER`,
    /// `JC_DAILY_TOKENS_OTHER` and `JC_DAILY_TOKENS_PER_PERSON`; absent or `0` is no cap.
    pub daily_caps: DailyCaps,
    /// Seconds between two probes of the model key (`JC_MODEL_PROBE_SECS`, default 900; `0` turns
    /// the probe off, AG-96).
    pub model_probe_secs: u64,
}

/// The daily token caps of AG-97, `0` for none.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DailyCaps {
    pub assistant: u64,
    pub app_builder: u64,
    pub other: u64,
    pub per_person: u64,
}

impl std::fmt::Debug for Config {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Config")
            .field("bind", &self.bind)
            .field("portal_base", &self.portal_base.as_str())
            .field("gateway_base", &self.gateway_base.as_str())
            .field("forge_base", &self.forge_base.as_str())
            .field("forge_repo", &self.forge_repo)
            .field("oidc_issuer", &self.oidc_issuer.as_str())
            .field("oidc_client_id", &self.oidc_client_id)
            .field("oidc_client_secret", &"[redacted]")
            .field(
                "oidc_client_assertion_file",
                &self.oidc_client_assertion_file,
            )
            .field("forge_token", &"[redacted]")
            .field("model_base", &self.model_base.as_str())
            .field("model_key", &"[redacted]")
            .field("model_provider", &self.model_provider)
            .field("oidc_token_url", &self.oidc_token_url)
            .field("require_mesh_identity", &self.require_mesh_identity)
            .field("portal_client_id", &self.portal_client_id)
            .field("functions_identity", &self.functions_identity)
            .field("portal_identity", &self.portal_identity)
            .field("assistant_client_id", &self.assistant_client_id)
            .field("daily_caps", &self.daily_caps)
            .field("model_probe_secs", &self.model_probe_secs)
            .finish()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("missing environment variable: {0}")]
    Missing(&'static str),
    #[error("invalid URL for {var}: {reason}")]
    InvalidUrl { var: &'static str, reason: String },
    #[error("invalid address for {var}: {reason}")]
    InvalidAddr { var: &'static str, reason: String },
    #[error("{var} and {other} are both set: {reason}")]
    Conflict {
        var: &'static str,
        other: &'static str,
        reason: &'static str,
    },
    #[error("invalid value for {var}: {reason}")]
    InvalidValue { var: &'static str, reason: String },
    #[error("invalid number for {var}: {reason}")]
    InvalidNumber { var: &'static str, reason: String },
    #[error("failed to read secret file {path:?}: {source}")]
    SecretFile {
        path: PathBuf,
        source: std::io::Error,
    },
}

impl Config {
    /// Refuses to run without the three credentials the proxy exists to hold.
    ///
    /// The proxy is the only holder of the model key, the forge token and the client secret it
    /// mints endpoint tokens with. Starting without one of them would mean proxying with no
    /// credential at all, which is a silent downgrade rather than an outage, so it is a startup
    /// failure instead (AG-35).
    pub fn require_secrets(&self) -> Result<(), ConfigError> {
        if self.oidc_client_secret.is_empty() && self.oidc_client_assertion_file.is_none() {
            return Err(ConfigError::Missing("JC_OIDC_CLIENT_ASSERTION_FILE"));
        }
        for (value, var) in [
            (&self.forge_token, "JC_FORGE_TOKEN_FILE"),
            (&self.model_key, "JC_MODEL_KEY_FILE"),
        ] {
            if value.is_empty() {
                return Err(ConfigError::Missing(var));
            }
        }
        Ok(())
    }

    pub fn from_env() -> Result<Self, ConfigError> {
        Self::from_lookup(|k| std::env::var(k).ok())
    }

    pub fn from_lookup(lookup: impl Fn(&str) -> Option<String>) -> Result<Self, ConfigError> {
        let bind = lookup("JC_PROXY_BIND").unwrap_or_else(|| "0.0.0.0:8080".to_string());
        let bind =
            bind.parse()
                .map_err(|e: std::net::AddrParseError| ConfigError::InvalidAddr {
                    var: "JC_PROXY_BIND",
                    reason: e.to_string(),
                })?;

        let portal_base =
            lookup("JC_PORTAL_BASE").unwrap_or_else(|| "http://portal:8080".to_string());
        let portal_base = Url::parse(&portal_base).map_err(|e| ConfigError::InvalidUrl {
            var: "JC_PORTAL_BASE",
            reason: e.to_string(),
        })?;

        let gateway_base =
            lookup("JC_GATEWAY_BASE").unwrap_or_else(|| "http://context-gateway:8080".to_string());
        let gateway_base = Url::parse(&gateway_base).map_err(|e| ConfigError::InvalidUrl {
            var: "JC_GATEWAY_BASE",
            reason: e.to_string(),
        })?;

        let forge_base =
            lookup("JC_FORGE_BASE").unwrap_or_else(|| "http://gitea-http:3000".to_string());
        let forge_base = Url::parse(&forge_base).map_err(|e| ConfigError::InvalidUrl {
            var: "JC_FORGE_BASE",
            reason: e.to_string(),
        })?;

        let forge_repo =
            lookup("JC_FORGE_REPO").unwrap_or_else(|| "joinedcontext/configuration".to_string());

        let oidc_issuer = lookup("JC_OIDC_ISSUER")
            .unwrap_or_else(|| "http://keycloak:8080/realms/joinedcontext".to_string());
        let oidc_issuer = Url::parse(&oidc_issuer).map_err(|e| ConfigError::InvalidUrl {
            var: "JC_OIDC_ISSUER",
            reason: e.to_string(),
        })?;

        let oidc_client_id =
            lookup("JC_OIDC_CLIENT_ID").unwrap_or_else(|| "agent-proxy".to_string());
        let oidc_client_secret = read_secret(
            &lookup,
            "JC_OIDC_CLIENT_SECRET_FILE",
            "JC_OIDC_CLIENT_SECRET",
        )?;

        let oidc_client_assertion_file = lookup("JC_OIDC_CLIENT_ASSERTION_FILE")
            .filter(|v| !v.trim().is_empty())
            .map(PathBuf::from);
        if oidc_client_assertion_file.is_some() && !oidc_client_secret.is_empty() {
            return Err(ConfigError::Conflict {
                var: "JC_OIDC_CLIENT_ASSERTION_FILE",
                other: "JC_OIDC_CLIENT_SECRET",
                reason: "a federated client holds no secret beside it",
            });
        }

        let forge_token = read_secret(&lookup, "JC_FORGE_TOKEN_FILE", "JC_FORGE_TOKEN")?;

        let model_base =
            lookup("JC_MODEL_BASE").unwrap_or_else(|| "https://api.anthropic.com".to_string());
        let model_base = Url::parse(&model_base).map_err(|e| ConfigError::InvalidUrl {
            var: "JC_MODEL_BASE",
            reason: e.to_string(),
        })?;

        let model_key = read_secret(&lookup, "JC_MODEL_KEY_FILE", "JC_MODEL_KEY")?;
        let model_provider = lookup("JC_MODEL_PROVIDER").unwrap_or_else(|| "anthropic".to_string());

        // `JC_PROXY_TOKEN` was here until T-2271: one string this proxy and the Portal both held,
        // presented on every callback. The proxy asks the realm for a token of its own client now,
        // audience-bound to the Portal's internal listener, and it expires by itself.
        let oidc_token_url = lookup("JC_OIDC_TOKEN_URL").filter(|v| !v.trim().is_empty());
        let require_mesh_identity = match lookup("JC_REQUIRE_MESH_IDENTITY").as_deref() {
            None | Some("true") => true,
            Some("false") => false,
            // A typo must not turn the check off: refuse to start instead (F-PLT1-008).
            Some(other) => {
                return Err(ConfigError::InvalidValue {
                    var: "JC_REQUIRE_MESH_IDENTITY",
                    reason: format!("{other:?} is neither `true` nor `false`"),
                })
            }
        };
        let portal_client_id = lookup("JC_PORTAL_CLIENT_ID")
            .filter(|v| !v.trim().is_empty())
            .unwrap_or_else(|| "portal-api".to_string());
        let functions_identity = lookup("JC_PROXY_FUNCTIONS_IDENTITY")
            .filter(|v| !v.trim().is_empty())
            .unwrap_or_else(|| "jc-functions".to_string());
        let portal_identity = lookup("JC_PROXY_PORTAL_IDENTITY")
            .filter(|v| !v.trim().is_empty())
            .unwrap_or_else(|| "portal".to_string());
        let assistant_client_id = lookup("JC_ASSISTANT_CLIENT_ID")
            .filter(|v| !v.trim().is_empty())
            .unwrap_or_else(|| "jc-assistant".to_string());

        let number = |var: &'static str, default: u64| -> Result<u64, ConfigError> {
            match lookup(var).filter(|v| !v.trim().is_empty()) {
                None => Ok(default),
                Some(v) => v.trim().parse().map_err(|e: std::num::ParseIntError| {
                    ConfigError::InvalidNumber {
                        var,
                        reason: e.to_string(),
                    }
                }),
            }
        };
        let daily_caps = DailyCaps {
            assistant: number("JC_DAILY_TOKENS_ASSISTANT", 0)?,
            app_builder: number("JC_DAILY_TOKENS_APP_BUILDER", 0)?,
            other: number("JC_DAILY_TOKENS_OTHER", 0)?,
            per_person: number("JC_DAILY_TOKENS_PER_PERSON", 0)?,
        };
        let model_probe_secs = number("JC_MODEL_PROBE_SECS", 900)?;

        Ok(Self {
            daily_caps,
            model_probe_secs,
            bind,
            portal_base,
            oidc_token_url,
            gateway_base,
            forge_base,
            forge_repo,
            oidc_issuer,
            oidc_client_id,
            oidc_client_secret,
            oidc_client_assertion_file,
            forge_token,
            model_base,
            model_key,
            model_provider,
            require_mesh_identity,
            portal_client_id,
            functions_identity,
            portal_identity,
            assistant_client_id,
        })
    }
}

impl Config {
    /// How this proxy proves its own client in a request to the realm: the projected token as
    /// `client_assertion` and no `client_id` (Keycloak finds a `federated-jwt` client by the
    /// token's subject and refuses one named beside it), else the id and the secret.
    pub fn client_auth(&self) -> Result<Vec<(&'static str, String)>, String> {
        match &self.oidc_client_assertion_file {
            Some(path) => jc_core::ClientCredential::Assertion(path.clone()),
            None => jc_core::ClientCredential::Secret(jc_core::Secret::new(
                self.oidc_client_secret.clone(),
            )),
        }
        .form(&self.oidc_client_id)
    }

    /// Whether the proxy holds any credential for its client.
    pub fn has_client_credential(&self) -> bool {
        !self.oidc_client_secret.is_empty() || self.oidc_client_assertion_file.is_some()
    }

    /// The realm's token endpoint: what the deployment named, else the issuer's own.
    pub fn token_url(&self) -> String {
        if let Some(url) = &self.oidc_token_url {
            return url.clone();
        }
        let mut url = self.oidc_issuer.clone();
        url.set_path(&format!(
            "{}/protocol/openid-connect/token",
            url.path().trim_end_matches('/')
        ));
        url.to_string()
    }

    /// The realm's introspection endpoint (RFC 7662), beside its token endpoint.
    pub fn introspection_url(&self) -> String {
        format!("{}/introspect", self.token_url())
    }

    /// The realm's revocation endpoint (RFC 7009): Keycloak serves it as a sibling of the token
    /// endpoint, `…/protocol/openid-connect/revoke`.
    pub fn revocation_url(&self) -> String {
        let token = self.token_url();
        match token.strip_suffix("/token") {
            Some(base) => format!("{base}/revoke"),
            None => format!("{token}/revoke"),
        }
    }
}

fn read_secret(
    lookup: &impl Fn(&str) -> Option<String>,
    file_var: &'static str,
    direct_var: &'static str,
) -> Result<String, ConfigError> {
    if let Some(path_str) = lookup(file_var) {
        let path = PathBuf::from(&path_str);
        return std::fs::read_to_string(&path)
            .map(|s| s.trim().to_string())
            .map_err(|e| ConfigError::SecretFile { path, source: e });
    }
    if let Some(direct) = lookup(direct_var) {
        return Ok(direct.trim().to_string());
    }
    Ok(String::new())
}
