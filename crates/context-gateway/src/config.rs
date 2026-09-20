//! Where the gateway listens, what it forwards to, and where it reads its endpoints from
//! (T-0005, OPS-12).
//!
//! Everything comes from the environment, because the deployment sets it and no request
//! can. The broker URL in particular is never derived from anything a client sends: the
//! gateway forwards to exactly one upstream, chosen before the first request arrives.

use std::net::SocketAddr;
use std::path::PathBuf;

/// The gateway's runtime configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    /// The address to listen on (`JC_GATEWAY_BIND`, default `0.0.0.0:8080`).
    pub bind: SocketAddr,
    /// The broker to forward to, scheme and authority only (`JC_GATEWAY_BROKER_URL`).
    pub broker_url: String,
    /// The manifest repository the endpoint table is built from
    /// (`JC_GATEWAY_REPO_DIR`); absent means an empty table until one is loaded.
    pub repo_dir: Option<PathBuf>,
    /// The Portal's list of running workspace previews (`JC_GATEWAY_PREVIEWS_URL`, its
    /// internal listener's `/internal/previews`); absent serves `main` alone (CC-78).
    pub previews_url: Option<String>,
    /// Where the previews are written (`JC_GATEWAY_PREVIEWS_DIR`, default
    /// `/tmp/jc-previews`), a scratch directory the pod owns.
    pub previews_dir: PathBuf,
    /// The organization's verified domain, the middle segment of every entity URN
    /// (`JC_GATEWAY_ORG_DOMAIN`).
    pub org_domain: String,
    /// The realm every token must be issued by (`JC_OIDC_ISSUER`); absent means the
    /// gateway serves public endpoints only and refuses every presented token.
    pub oidc_issuer: Option<String>,
    /// The realm's JWKS, fetched in the background (`JC_OIDC_JWKS_URL`).
    pub oidc_jwks_url: Option<String>,
    /// Where the gateway asks for its own token (`JC_OIDC_TOKEN_URL`), for the same reason
    /// `JC_OIDC_JWKS_URL` exists: the issuer is the address a *browser* uses, and a pod that dials
    /// its own cluster's public hostname leaves through the ingress or not at all. The realm's
    /// `openid-connect/token` under the issuer when the deployment names none.
    pub oidc_token_url: Option<String>,
    /// The gateway's own Keycloak client and its secret (`JC_OIDC_CLIENT_ID`,
    /// `JC_OIDC_CLIENT_SECRET`): the identity it presents when it calls the Portal's internal
    /// listener (PF-46, AG-52). Absent means it presents none and that listener refuses it, which
    /// is what an instance without previews looks like.
    ///
    /// The secret is a [`jc_core::Secret`], so the derived `Debug` above cannot print it: this
    /// struct is the one a `tracing::debug!(?config)` during a startup problem would reach, and a
    /// client secret in the cluster's logs is forbidden outright (CC-06, OPS-12, T-2273). The
    /// client *id* is not a secret and stays readable, because a log that cannot say which client
    /// was refused is no use to an operator.
    pub oidc_client: Option<(String, jc_core::Secret)>,
    /// The gateway's own public base URL (`JC_GATEWAY_PUBLIC_URL`), which makes the full
    /// RFC 8707 resource URI an acceptable token audience alongside the endpoint slug.
    pub public_url: Option<String>,
    /// The base a rewritten `notification.endpoint.uri` carries
    /// (`JC_GATEWAY_EGRESS_URL`), which is the address the broker dials to deliver; the
    /// public URL when the deployment names none. An in-cluster Service URL keeps the
    /// delivery hop inside the cluster, where a NetworkPolicy governs it (R46).
    pub egress_url: Option<String>,
    /// A PEM file of extra trust anchors the notification egress trusts on top of the
    /// public roots (`JC_GATEWAY_EGRESS_CA_BUNDLE`), for subscribers behind the
    /// installation's own CA (R46).
    pub egress_ca_bundle: Option<PathBuf>,
    /// Hosts inside the platform's own networks a notification may still be delivered to
    /// (`JC_GATEWAY_EGRESS_PRIVATE_HOSTS`, comma-separated); empty refuses them all (T-1302).
    pub egress_private_hosts: Vec<String>,
}

/// Why the environment does not describe a runnable gateway.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ConfigError {
    /// A variable the gateway cannot invent a default for is missing.
    #[error("{0} is not set")]
    Missing(&'static str),
    /// A variable is set to something the gateway cannot use.
    #[error("{name} is not usable: {reason}")]
    Invalid {
        /// The variable.
        name: &'static str,
        /// What is wrong with it.
        reason: String,
    },
}

impl Config {
    /// Where the gateway asks for its own token: what the deployment named, else the realm's own
    /// endpoint under the issuer. `None` when there is no realm at all, which is an instance that
    /// presents no identity anywhere.
    pub fn token_url(&self) -> Option<String> {
        token_endpoint(self.oidc_token_url.as_deref(), self.oidc_issuer.as_deref())
    }

    /// Reads the configuration from the process environment.
    pub fn from_env() -> Result<Self, ConfigError> {
        let bind = match std::env::var("JC_GATEWAY_BIND") {
            Err(_) => "0.0.0.0:8080".to_owned(),
            Ok(value) => value,
        };
        let bind = bind
            .parse()
            .map_err(|e: std::net::AddrParseError| ConfigError::Invalid {
                name: "JC_GATEWAY_BIND",
                reason: e.to_string(),
            })?;

        let broker_url = std::env::var("JC_GATEWAY_BROKER_URL")
            .map_err(|_| ConfigError::Missing("JC_GATEWAY_BROKER_URL"))?;
        let broker_url = normalize_broker_url(&broker_url)?;

        let org_domain = std::env::var("JC_GATEWAY_ORG_DOMAIN")
            .map_err(|_| ConfigError::Missing("JC_GATEWAY_ORG_DOMAIN"))?;

        let oidc_issuer = std::env::var("JC_OIDC_ISSUER").ok();
        let oidc_jwks_url = std::env::var("JC_OIDC_JWKS_URL").ok();
        if oidc_issuer.is_some() != oidc_jwks_url.is_some() {
            return Err(ConfigError::Invalid {
                name: "JC_OIDC_ISSUER",
                reason: "an issuer without a JWKS URL verifies nothing, and a JWKS URL without an issuer accepts any realm; set both or neither".to_owned(),
            });
        }

        Ok(Self {
            bind,
            broker_url,
            repo_dir: std::env::var("JC_GATEWAY_REPO_DIR").ok().map(PathBuf::from),
            previews_url: std::env::var("JC_GATEWAY_PREVIEWS_URL")
                .ok()
                .filter(|url| !url.trim().is_empty()),
            previews_dir: std::env::var("JC_GATEWAY_PREVIEWS_DIR")
                .ok()
                .filter(|dir| !dir.trim().is_empty())
                .map_or_else(|| PathBuf::from("/tmp/jc-previews"), PathBuf::from),
            org_domain,
            oidc_issuer,
            oidc_jwks_url,
            oidc_token_url: std::env::var("JC_OIDC_TOKEN_URL")
                .ok()
                .filter(|url| !url.trim().is_empty()),
            oidc_client: match (
                std::env::var("JC_OIDC_CLIENT_ID")
                    .ok()
                    .filter(|value| !value.trim().is_empty()),
                std::env::var("JC_OIDC_CLIENT_SECRET")
                    .ok()
                    .filter(|value| !value.trim().is_empty()),
            ) {
                (Some(id), Some(secret)) => Some((id, jc_core::Secret::new(secret))),
                _ => None,
            },
            public_url: std::env::var("JC_GATEWAY_PUBLIC_URL")
                .ok()
                .map(|url| url.trim_end_matches('/').to_owned()),
            egress_url: std::env::var("JC_GATEWAY_EGRESS_URL")
                .ok()
                .map(|url| url.trim_end_matches('/').to_owned()),
            egress_ca_bundle: std::env::var("JC_GATEWAY_EGRESS_CA_BUNDLE")
                .ok()
                .filter(|path| !path.trim().is_empty())
                .map(PathBuf::from),
            egress_private_hosts: std::env::var("JC_GATEWAY_EGRESS_PRIVATE_HOSTS")
                .map(|hosts| {
                    hosts
                        .split(',')
                        .map(|host| host.trim().to_ascii_lowercase())
                        .filter(|host| !host.is_empty())
                        .collect()
                })
                .unwrap_or_default(),
        })
    }
}

/// Reduces the configured broker URL to `scheme://authority`, with no trailing slash.
///
/// The forwarded path is always built by the gateway from the route it matched, so a base
/// that carried a path of its own would silently prefix every request; refusing it here is
/// cheaper than debugging it later.
fn normalize_broker_url(raw: &str) -> Result<String, ConfigError> {
    let invalid = |reason: &str| ConfigError::Invalid {
        name: "JC_GATEWAY_BROKER_URL",
        reason: reason.to_owned(),
    };
    let trimmed = raw.trim().trim_end_matches('/');
    let rest = trimmed
        .strip_prefix("http://")
        .or_else(|| trimmed.strip_prefix("https://"))
        .ok_or_else(|| invalid("must start with http:// or https://"))?;

    if rest.is_empty() {
        return Err(invalid("has no host"));
    }
    if rest.contains('/') {
        return Err(invalid("must be scheme and authority only, with no path"));
    }
    Ok(trimmed.to_owned())
}

/// The token endpoint of `Config::token_url`, apart from the struct so both branches are readable:
/// what the deployment named wins, and the realm's own is derived from the issuer only when it did
/// not name one.
fn token_endpoint(named: Option<&str>, issuer: Option<&str>) -> Option<String> {
    if let Some(url) = named {
        return Some(url.to_owned());
    }
    issuer.map(|issuer| {
        format!(
            "{}/protocol/openid-connect/token",
            issuer.trim_end_matches('/')
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A configuration carrying the gateway's own client and its secret.
    fn config_with_a_client_secret() -> Config {
        Config {
            bind: "0.0.0.0:8080".parse().expect("a socket address"),
            broker_url: "http://broker:1026".to_owned(),
            repo_dir: None,
            previews_url: Some("http://portal:8081/internal/previews".to_owned()),
            previews_dir: PathBuf::from("/tmp/jc-previews"),
            org_domain: "hel.fi".to_owned(),
            oidc_issuer: Some("https://idm.example/realms/dev".to_owned()),
            oidc_jwks_url: Some(
                "http://keycloak/realms/dev/protocol/openid-connect/certs".to_owned(),
            ),
            oidc_token_url: None,
            oidc_client: Some((
                "context-gateway".to_owned(),
                jc_core::Secret::new("Kx7-the-realms-client-secret-9fQ"),
            )),
            public_url: None,
            egress_url: None,
            egress_ca_bundle: None,
            egress_private_hosts: Vec::new(),
        }
    }

    /// T-2273, SEC-GAP-03, OPS-12: the gateway's configuration is the struct a
    /// `tracing::debug!(?config)` while chasing a startup problem would reach, and it holds the
    /// realm client secret. The property is the absence of the value, not the presence of the word
    /// "redacted": this test fails on a derived `Debug` over a plain `String`.
    #[test]
    fn debugging_the_configuration_prints_the_client_id_and_never_the_secret() {
        let config = config_with_a_client_secret();
        for rendered in [format!("{config:?}"), format!("{config:#?}")] {
            assert!(
                !rendered.contains("Kx7"),
                "the client secret reached a formatted configuration: {rendered}",
            );
            assert!(
                !rendered.contains("realms-client-secret"),
                "part of the client secret reached a formatted configuration: {rendered}",
            );
            assert!(
                rendered.contains("context-gateway"),
                "the client id belongs in the log, so an operator can see which client was \
                 refused: {rendered}",
            );
        }
    }

    /// And the value is still there for the one caller that needs it (`main.rs`, the previews
    /// poller's workload token), so the redaction costs nothing but the explicit ask.
    #[test]
    fn the_secret_is_still_readable_by_asking_for_it() {
        let config = config_with_a_client_secret();
        let (id, secret) = config.oidc_client.as_ref().expect("a client is configured");
        assert_eq!(id, "context-gateway");
        assert_eq!(secret.expose(), "Kx7-the-realms-client-secret-9fQ");
    }

    /// On `dev` the issuer is `https://idm.<node>.sslip.io/realms/dev`, the address a browser uses.
    /// The gateway pod cannot dial its own cluster's ingress hostname, so the poller asked and got
    /// nothing for as long as it ran (T-1500); `JC_OIDC_TOKEN_URL` is the in-cluster Service, the
    /// way `JC_OIDC_JWKS_URL` already is.
    #[test]
    fn the_token_endpoint_the_deployment_names_wins_over_the_issuers_own() {
        assert_eq!(
            token_endpoint(
                Some("http://keycloak.dev.svc.cluster.local:80/realms/dev/protocol/openid-connect/token"),
                Some("https://idm.example/realms/dev"),
            ),
            Some(
                "http://keycloak.dev.svc.cluster.local:80/realms/dev/protocol/openid-connect/token"
                    .to_owned()
            )
        );
    }

    #[test]
    fn without_one_named_the_endpoint_is_the_issuers_own_whether_or_not_it_ends_in_a_slash() {
        let expected =
            Some("https://idm.example/realms/dev/protocol/openid-connect/token".to_owned());
        assert_eq!(
            token_endpoint(None, Some("https://idm.example/realms/dev")),
            expected
        );
        assert_eq!(
            token_endpoint(None, Some("https://idm.example/realms/dev/")),
            expected
        );
    }

    #[test]
    fn no_realm_is_no_endpoint_and_no_identity_presented_anywhere() {
        assert_eq!(token_endpoint(None, None), None);
    }

    #[test]
    fn broker_url_is_reduced_to_scheme_and_authority() {
        assert_eq!(
            normalize_broker_url("http://antares:9090/"),
            Ok("http://antares:9090".to_owned())
        );
        assert_eq!(
            normalize_broker_url("  https://broker.example  "),
            Ok("https://broker.example".to_owned())
        );
        for bad in [
            "antares:9090",
            "http://",
            "http://antares:9090/ngsi-ld/v1",
            "",
        ] {
            assert!(normalize_broker_url(bad).is_err(), "{bad:?} was accepted");
        }
    }
}
