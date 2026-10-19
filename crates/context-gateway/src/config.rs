//! Where the gateway listens, what it forwards to, and where it reads its endpoints from
//! (T-0005, OPS-12).
//!
//! Everything comes from the environment, because the deployment sets it and no request
//! can. The broker URL in particular is never derived from anything a client sends: the
//! gateway forwards to exactly one upstream, chosen before the first request arrives.

use std::net::SocketAddr;
use std::path::PathBuf;

/// How the gateway proves its own Keycloak client (`JC_OIDC_CLIENT_SECRET` or
/// `JC_OIDC_CLIENT_ASSERTION_FILE`, PF-47).
pub use jc_core::ClientCredential as Credential;

/// The gateway's runtime configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    /// The address to listen on (`JC_GATEWAY_BIND`, default `0.0.0.0:8080`).
    pub bind: SocketAddr,
    /// The internal listener, for the MQTT brokers' CONNECT checks only
    /// (`JC_GATEWAY_INTERNAL_BIND`, e.g. `0.0.0.0:8082`); absent serves none (ADR-N-048).
    pub internal_bind: Option<SocketAddr>,
    /// The pipeline runner's headless Service (`JC_GATEWAY_MQTT_RUNNER_HOST`, e.g.
    /// `pipeline-runner-pods.dev.svc.cluster.local`): a broker admits the runner's tokenless
    /// CONNECT only from an address this name resolves to; absent admits none (PL-76, T-4004).
    pub mqtt_runner_host: Option<String>,
    /// The broker to forward to, scheme and authority only (`JC_GATEWAY_BROKER_URL`): the
    /// implicit `main`, and every space when the organization declares no broker.
    pub broker_url: String,
    /// The address of every other `ContextBroker`, `{broker}` replaced by its name
    /// (`JC_GATEWAY_BROKER_URL_TEMPLATE`, ADR-N-047); absent refuses the spaces that live on
    /// one rather than sending them to `main`.
    pub broker_url_template: Option<String>,
    /// The manifest repository the endpoint table is built from
    /// (`JC_GATEWAY_REPO_DIR`); absent means an empty table until one is loaded.
    pub repo_dir: Option<PathBuf>,
    /// The project checkouts of a layout 2 organization, one directory per registry slug at
    /// its pinned ref (`JC_GATEWAY_PROJECTS_DIR`); a layout 1 repository needs none (CC-86).
    pub projects_dir: Option<PathBuf>,
    /// Where a layout 2 organization is assembled (`JC_GATEWAY_ASSEMBLY_DIR`, default
    /// `/tmp/jc-assembly`), a scratch directory the pod owns.
    pub assembly_dir: PathBuf,
    /// The Portal's list of running workspace previews (`JC_GATEWAY_PREVIEWS_URL`, its
    /// internal listener's `/internal/previews`); absent serves `main` alone (CC-78).
    pub previews_url: Option<String>,
    /// The Portal's read of a person's connectors (`JC_GATEWAY_CONNECTORS_URL`, its internal
    /// listener's `/internal/connectors`); absent serves no `/api/mcp/me/{name}` (EP-123).
    pub connectors_url: Option<String>,
    /// `jc-assistant`, scheme and authority, for `search_documents` (`JC_GATEWAY_ASSISTANT_URL`,
    /// e.g. `http://jc-assistant.assistant:8080`, EP-133); absent offers the tool nowhere.
    pub assistant_url: Option<String>,
    /// The knowledge sources the hub's `search_documents` searches
    /// (`JC_GATEWAY_HUB_KNOWLEDGE_SOURCES`, comma-separated `{project}/{source}`, MF-57).
    pub hub_knowledge_sources: String,
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
    /// The gateway's own Keycloak client and how it proves it (`JC_OIDC_CLIENT_ID` with
    /// `JC_OIDC_CLIENT_ASSERTION_FILE` or `JC_OIDC_CLIENT_SECRET`): the identity it presents when
    /// it calls the Portal's internal listener (PF-46, PF-47, AG-52). Absent means it presents
    /// none and that listener refuses it, which is what an instance without previews looks like.
    ///
    /// The secret is a [`jc_core::Secret`], so the derived `Debug` above cannot print it: this
    /// struct is the one a `tracing::debug!(?config)` during a startup problem would reach, and a
    /// client secret in the cluster's logs is forbidden outright (CC-06, OPS-12, T-2273). The
    /// client *id* is not a secret and stays readable, because a log that cannot say which client
    /// was refused is no use to an operator.
    pub oidc_client: Option<(String, Credential)>,
    /// The key the anonymous OAuth server signs with (`JC_GATEWAY_OAUTH_KEY`, from a `secretRef`,
    /// at least 32 bytes, ADR-N-052 §2.2.6); absent serves no `/oauth/*`. It needs `public_url`.
    pub oauth_key: Option<jc_core::Secret>,
    /// The hosts that server redirects to (`JC_GATEWAY_OAUTH_REDIRECT_HOSTS`, comma-separated,
    /// default [`crate::oauth::DEFAULT_REDIRECT_HOSTS`], ADR-N-052 §2.3).
    pub oauth_redirect_hosts: Vec<String>,
    /// The gateway's own public base URL (`JC_GATEWAY_PUBLIC_URL`), which makes the full
    /// RFC 8707 resource URI an acceptable token audience alongside the endpoint slug.
    pub public_url: Option<String>,
    /// A PEM file of extra trust anchors the outbound client trusts on top of the public roots
    /// (`JC_GATEWAY_EGRESS_CA_BUNDLE`), for a broker behind the installation's own CA (R46).
    pub egress_ca_bundle: Option<PathBuf>,
    /// Hosts inside the platform's own networks a subscription may still name as its target
    /// (`JC_GATEWAY_EGRESS_PRIVATE_HOSTS`, comma-separated); empty refuses them all (T-1302).
    pub egress_private_hosts: Vec<String>,
    /// The directory the reconciler mounts each space's `spec.personalData.keyRef` in, one file
    /// per space (`JC_GATEWAY_PERSONAL_KEYS`, DM-121); absent refuses every write a model hashes.
    pub personal_keys: Option<PathBuf>,
    /// `report` or `enforce` (`JC_GATEWAY_DOMAIN_VERIFICATION`, default `report`): whether a
    /// write waits for the Organization's verified domain (PF-41, Architecture/03 §3).
    pub domain_verification: crate::domain_gate::Mode,
    /// The Portal's list of domain states (`JC_GATEWAY_DOMAIN_VERIFICATIONS_URL`, its internal
    /// listener's `/internal/domain-verifications`); required under `enforce`.
    pub domain_verifications_url: Option<String>,
    /// How many MiB of write bodies may be in the gateway at once
    /// (`JC_GATEWAY_WRITE_BUDGET_MIB`, default `16`, [`crate::app::WRITE_BUDGET_MIB`]); a write past
    /// it is answered 503 with `Retry-After` (T-4077).
    pub write_budget_mib: usize,
    /// How many requests the gateway works on at once (`JC_GATEWAY_MAX_IN_FLIGHT`, default
    /// `128`, [`crate::middleware::in_flight::MAX_IN_FLIGHT`]); one past it is answered 503 with
    /// `Retry-After` (T-4782).
    pub max_in_flight: usize,
    /// How many of them one caller may hold (`JC_GATEWAY_MAX_IN_FLIGHT_PER_CALLER`, default
    /// `32`, [`crate::middleware::in_flight::MAX_IN_FLIGHT_PER_CALLER`]).
    pub max_in_flight_per_caller: usize,
    /// How many `file.csv` downloads are built at once (`JC_GATEWAY_MAX_DOWNLOADS`, default `2`,
    /// [`crate::middleware::in_flight::MAX_DOWNLOADS`]).
    pub max_downloads: usize,
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

        let internal_bind = std::env::var("JC_GATEWAY_INTERNAL_BIND")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .map(|value| value.parse())
            .transpose()
            .map_err(|e: std::net::AddrParseError| ConfigError::Invalid {
                name: "JC_GATEWAY_INTERNAL_BIND",
                reason: e.to_string(),
            })?;

        let broker_url = std::env::var("JC_GATEWAY_BROKER_URL")
            .map_err(|_| ConfigError::Missing("JC_GATEWAY_BROKER_URL"))?;
        let broker_url = normalize_broker_url(&broker_url)?;
        let broker_url_template = match std::env::var("JC_GATEWAY_BROKER_URL_TEMPLATE") {
            Ok(raw) if !raw.trim().is_empty() => Some(broker_url_template(&raw)?),
            _ => None,
        };

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

        let non_empty = |name| {
            std::env::var(name)
                .ok()
                .filter(|v: &String| !v.trim().is_empty())
        };
        let credential = match (
            non_empty("JC_OIDC_CLIENT_SECRET"),
            non_empty("JC_OIDC_CLIENT_ASSERTION_FILE"),
        ) {
            (Some(_), Some(_)) => {
                return Err(ConfigError::Invalid {
                    name: "JC_OIDC_CLIENT_ASSERTION_FILE",
                    reason: "a federated client holds no secret beside it; set JC_OIDC_CLIENT_SECRET or JC_OIDC_CLIENT_ASSERTION_FILE, not both".to_owned(),
                })
            }
            (Some(secret), None) => Some(Credential::Secret(jc_core::Secret::new(secret))),
            (None, Some(file)) => Some(Credential::Assertion(PathBuf::from(file))),
            (None, None) => None,
        };

        let domain_verification = crate::domain_gate::Mode::parse(
            std::env::var("JC_GATEWAY_DOMAIN_VERIFICATION")
                .ok()
                .as_deref(),
        )
        .map_err(|reason| ConfigError::Invalid {
            name: "JC_GATEWAY_DOMAIN_VERIFICATION",
            reason,
        })?;
        let domain_verifications_url = std::env::var("JC_GATEWAY_DOMAIN_VERIFICATIONS_URL")
            .ok()
            .filter(|url| !url.trim().is_empty());
        // `enforce` with nowhere to learn the state would refuse every write forever and look
        // like a broken platform; it is a configuration that does not run.
        if domain_verification == crate::domain_gate::Mode::Enforce
            && domain_verifications_url.is_none()
        {
            return Err(ConfigError::Missing("JC_GATEWAY_DOMAIN_VERIFICATIONS_URL"));
        }

        let write_budget_mib = positive(
            "JC_GATEWAY_WRITE_BUDGET_MIB",
            std::env::var("JC_GATEWAY_WRITE_BUDGET_MIB"),
            "MiB",
            crate::app::WRITE_BUDGET_MIB,
        )?;
        use crate::middleware::in_flight;
        let max_in_flight = positive(
            "JC_GATEWAY_MAX_IN_FLIGHT",
            std::env::var("JC_GATEWAY_MAX_IN_FLIGHT"),
            "requests",
            in_flight::MAX_IN_FLIGHT,
        )?;
        let max_in_flight_per_caller = positive(
            "JC_GATEWAY_MAX_IN_FLIGHT_PER_CALLER",
            std::env::var("JC_GATEWAY_MAX_IN_FLIGHT_PER_CALLER"),
            "requests",
            in_flight::MAX_IN_FLIGHT_PER_CALLER,
        )?;
        let max_downloads = positive(
            "JC_GATEWAY_MAX_DOWNLOADS",
            std::env::var("JC_GATEWAY_MAX_DOWNLOADS"),
            "downloads",
            in_flight::MAX_DOWNLOADS,
        )?;

        let public_url = std::env::var("JC_GATEWAY_PUBLIC_URL")
            .ok()
            .map(|url| url.trim_end_matches('/').to_owned())
            .filter(|url| !url.is_empty());
        let oauth_key = non_empty("JC_GATEWAY_OAUTH_KEY").map(jc_core::Secret::new);
        if let Some(key) = &oauth_key {
            if key.expose().len() < crate::oauth::MIN_KEY_BYTES {
                return Err(ConfigError::Invalid {
                    name: "JC_GATEWAY_OAUTH_KEY",
                    reason: format!("needs at least {} bytes", crate::oauth::MIN_KEY_BYTES),
                });
            }
            if public_url.is_none() {
                return Err(ConfigError::Missing("JC_GATEWAY_PUBLIC_URL"));
            }
        }
        let oauth_redirect_hosts = match non_empty("JC_GATEWAY_OAUTH_REDIRECT_HOSTS") {
            Some(hosts) => hosts
                .split(',')
                .map(|host| host.trim().to_ascii_lowercase())
                .filter(|host| !host.is_empty())
                .collect(),
            None => crate::oauth::DEFAULT_REDIRECT_HOSTS
                .iter()
                .map(|host| (*host).to_owned())
                .collect(),
        };

        Ok(Self {
            oauth_key,
            oauth_redirect_hosts,
            write_budget_mib,
            max_in_flight,
            max_in_flight_per_caller,
            max_downloads,
            domain_verification,
            domain_verifications_url,
            bind,
            internal_bind,
            mqtt_runner_host: std::env::var("JC_GATEWAY_MQTT_RUNNER_HOST")
                .ok()
                .map(|host| host.trim().to_owned())
                .filter(|host| !host.is_empty()),
            broker_url,
            broker_url_template,
            repo_dir: std::env::var("JC_GATEWAY_REPO_DIR").ok().map(PathBuf::from),
            projects_dir: std::env::var("JC_GATEWAY_PROJECTS_DIR")
                .ok()
                .filter(|dir| !dir.trim().is_empty())
                .map(PathBuf::from),
            assembly_dir: std::env::var("JC_GATEWAY_ASSEMBLY_DIR")
                .ok()
                .filter(|dir| !dir.trim().is_empty())
                .map_or_else(|| PathBuf::from("/tmp/jc-assembly"), PathBuf::from),
            previews_url: std::env::var("JC_GATEWAY_PREVIEWS_URL")
                .ok()
                .filter(|url| !url.trim().is_empty()),
            connectors_url: std::env::var("JC_GATEWAY_CONNECTORS_URL")
                .ok()
                .filter(|url| !url.trim().is_empty()),
            assistant_url: std::env::var("JC_GATEWAY_ASSISTANT_URL")
                .ok()
                .filter(|url| !url.trim().is_empty()),
            hub_knowledge_sources: std::env::var("JC_GATEWAY_HUB_KNOWLEDGE_SOURCES")
                .unwrap_or_default(),
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
            oidc_client: non_empty("JC_OIDC_CLIENT_ID").zip(credential),
            public_url,
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
            personal_keys: non_empty("JC_GATEWAY_PERSONAL_KEYS").map(PathBuf::from),
        })
    }
}

/// Reduces the configured broker URL to `scheme://authority`, with no trailing slash.
///
/// The forwarded path is always built by the gateway from the route it matched, so a base
/// that carried a path of its own would silently prefix every request; refusing it here is
/// cheaper than debugging it later.
/// `name`, as `read` from the environment, as a positive whole number of `unit`, `default` when it is unset or blank.
fn positive(
    name: &'static str,
    read: Result<String, std::env::VarError>,
    unit: &str,
    default: usize,
) -> Result<usize, ConfigError> {
    match read {
        Ok(value) if !value.trim().is_empty() => match value.trim().parse::<usize>() {
            Ok(number) if number > 0 => Ok(number),
            _ => Err(ConfigError::Invalid {
                name,
                reason: format!("{value:?} is not a positive number of {unit}"),
            }),
        },
        _ => Ok(default),
    }
}

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

/// A broker address template: `scheme://authority` once `{broker}` is put in, with `{broker}`
/// in the authority, so one broker's name can never point at another host's path.
fn broker_url_template(raw: &str) -> Result<String, ConfigError> {
    let invalid = |reason: &str| ConfigError::Invalid {
        name: "JC_GATEWAY_BROKER_URL_TEMPLATE",
        reason: reason.to_owned(),
    };
    if !raw.contains("{broker}") {
        return Err(invalid("must contain {broker}"));
    }
    normalize_broker_url(&raw.replace("{broker}", "b"))
        .map_err(|_| invalid("must be scheme and authority only, with no path"))?;
    Ok(raw.trim().trim_end_matches('/').to_owned())
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
            internal_bind: None,
            mqtt_runner_host: None,
            broker_url: "http://broker:1026".to_owned(),
            broker_url_template: None,
            repo_dir: None,
            projects_dir: None,
            assembly_dir: PathBuf::from("/tmp/jc-assembly"),
            previews_url: Some("http://portal:8081/internal/previews".to_owned()),
            connectors_url: None,
            assistant_url: None,
            hub_knowledge_sources: String::new(),
            previews_dir: PathBuf::from("/tmp/jc-previews"),
            org_domain: "hel.fi".to_owned(),
            oidc_issuer: Some("https://idm.example/realms/dev".to_owned()),
            oidc_jwks_url: Some(
                "http://keycloak/realms/dev/protocol/openid-connect/certs".to_owned(),
            ),
            oidc_token_url: None,
            oidc_client: Some((
                "context-gateway".to_owned(),
                Credential::Secret(jc_core::Secret::new("Kx7-the-realms-client-secret-9fQ")),
            )),
            public_url: Some("https://api.dev.example".to_owned()),
            oauth_key: Some(jc_core::Secret::new(
                "Zq4-the-anonymous-oauth-signing-key-7Wm",
            )),
            oauth_redirect_hosts: vec!["claude.ai".to_owned()],
            egress_ca_bundle: None,
            egress_private_hosts: Vec::new(),
            personal_keys: None,
            domain_verification: crate::domain_gate::Mode::Report,
            domain_verifications_url: None,
            write_budget_mib: crate::app::WRITE_BUDGET_MIB,
            max_in_flight: crate::middleware::in_flight::MAX_IN_FLIGHT,
            max_in_flight_per_caller: crate::middleware::in_flight::MAX_IN_FLIGHT_PER_CALLER,
            max_downloads: crate::middleware::in_flight::MAX_DOWNLOADS,
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
                !rendered.contains("Zq4") && !rendered.contains("oauth-signing-key"),
                "the OAuth signing key reached a formatted configuration (T-4631): {rendered}",
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
        let Some((id, Credential::Secret(secret))) = config.oidc_client.as_ref() else {
            panic!("a client with a secret is configured");
        };
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
    fn a_broker_template_names_the_broker_in_its_authority() {
        assert_eq!(
            broker_url_template("http://context-broker-{broker}.brokers.svc:9090/").unwrap(),
            "http://context-broker-{broker}.brokers.svc:9090"
        );
        for bad in [
            "http://context-broker.brokers.svc:9090",
            "http://broker:9090/{broker}",
            "context-broker-{broker}:9090",
        ] {
            assert!(broker_url_template(bad).is_err(), "{bad:?} was accepted");
        }
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

    /// The docs' config reference reads a default as "default `N`" (T-4121): the number
    /// written beside the write budget is the constant's.
    #[test]
    fn the_write_budget_documents_its_default_as_a_number() {
        let documented = format!(
            "`JC_GATEWAY_WRITE_BUDGET_MIB`, default `{}`",
            crate::app::WRITE_BUDGET_MIB
        );
        assert!(
            include_str!("config.rs").contains(&documented),
            "{documented}"
        );
    }
}
