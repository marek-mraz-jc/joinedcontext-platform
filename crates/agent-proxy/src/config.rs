//! Configuration parameters for `jc-agent-proxy`.
//!
//! Credentials and secrets are loaded from file paths or environment variables,
//! and are strictly redacted in `Debug` implementations to prevent log leakage.

use std::net::SocketAddr;
use std::path::PathBuf;
use url::Url;

#[derive(Clone)]
pub struct Config {
    pub bind: SocketAddr,
    pub portal_base: Url,
    pub gateway_base: Url,
    pub forge_base: Url,
    pub forge_repo: String,
    pub oidc_issuer: Url,
    pub oidc_client_id: String,
    pub oidc_client_secret: String,
    pub forge_token: String,
    pub model_base: Url,
    pub model_key: String,
    pub model_provider: String,
    /// Where the realm's token endpoint is, for the same reason the gateway needs one
    /// (`JC_OIDC_TOKEN_URL`, T-2272): the issuer is the address a browser uses, and a pod cannot
    /// dial its own cluster's ingress hostname. The issuer's own endpoint when none is named.
    pub oidc_token_url: Option<String>,
    pub require_mesh_identity: bool,
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
            .field("forge_token", &"[redacted]")
            .field("model_base", &self.model_base.as_str())
            .field("model_key", &"[redacted]")
            .field("model_provider", &self.model_provider)
            .field("oidc_token_url", &self.oidc_token_url)
            .field("require_mesh_identity", &self.require_mesh_identity)
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
        for (value, var) in [
            (&self.oidc_client_secret, "JC_OIDC_CLIENT_SECRET_FILE"),
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
        let require_mesh_identity = lookup("JC_REQUIRE_MESH_IDENTITY").is_some_and(|v| v == "true");

        Ok(Self {
            bind,
            portal_base,
            oidc_token_url,
            gateway_base,
            forge_base,
            forge_repo,
            oidc_issuer,
            oidc_client_id,
            oidc_client_secret,
            forge_token,
            model_base,
            model_key,
            model_provider,
            require_mesh_identity,
        })
    }
}

impl Config {
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
