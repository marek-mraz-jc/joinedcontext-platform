//! `jcctl login`: a person's token for the Portal, by RFC 8628's device authorization grant on
//! the public Keycloak client `jcctl` (PF-45, MF-14, API/03 §2a).
//!
//! A terminal has no browser to come back to, so the person opens the address it prints on any
//! device, signs in there and confirms the code; `jcctl` polls the token endpoint meanwhile and
//! writes the access token to a file only its owner may read. The client verbs read that file
//! when neither `--token-file` nor `JC_TOKEN_FILE` names another. The refresh token is not kept:
//! a session ends with its access token, and the next one is another `jcctl login`.
//!
//! A public client has no secret, so every sign-in carries RFC 7636's proof key (S256): a fresh
//! verifier per sign-in, its hash on the device authorization and the verifier itself on the
//! token request, so a device code read off the screen is worth nothing to anyone else. The realm
//! refuses a device authorization without it (T-5614).
//!
//! `jcctl token --resource <endpoint url>` runs the same flow for a person's script: it asks for
//! the `endpoint:{slug}` scope of that one Endpoint, and the gateway admits the token there alone,
//! decided by the person's Policy (T-5614, PF-46). Keycloak's token exchange takes no public
//! client, so the script's token is a sign-in of its own, which the browser session the person
//! already has turns into one confirmation.

use crate::secrets::SecretValue;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use reqwest::blocking::Client;
use reqwest::Url;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// The Keycloak client the device flow runs on (deployment `components/portal/keycloak-clients.yaml`).
pub const CLIENT_ID: &str = "jcctl";
const GRANT: &str = "urn:ietf:params:oauth:grant-type:device_code";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// RFC 8628 §3.2: the interval a server that names none expects.
const DEFAULT_INTERVAL: u64 = 5;
/// RFC 8628 §3.5: what `slow_down` adds to the interval.
const SLOW_DOWN: u64 = 5;
/// The scope of a sign-in for the Portal.
pub const PORTAL_SCOPE: &str = "openid";
/// The path an Endpoint's NGSI-LD surface answers under, `{public url}/api/endpoint/{slug}/…`.
const ENDPOINT_PATH: &str = "/api/endpoint/";

/// Why no token was written.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LoginError {
    /// The identity provider's address is not usable.
    #[error("{0}")]
    Address(String),
    /// The identity provider could not be reached or answered something unreadable.
    #[error("the identity provider at {url} could not be reached: {message}")]
    Unavailable {
        /// The address that was tried.
        url: String,
        /// What went wrong.
        message: String,
    },
    /// The sign-in ended without a token: refused, expired, or the client is not allowed to.
    #[error("sign-in did not complete: {0}")]
    Refused(String),
    /// The token could not be written.
    #[error("the token could not be written to {path}: {message}")]
    Write {
        /// Where it was going.
        path: String,
        /// What went wrong.
        message: String,
    },
}

/// Where the token goes when nothing else is named: `$XDG_CONFIG_HOME/jcctl/token`, else
/// `$HOME/.config/jcctl/token`.
pub fn default_token_file() -> Option<PathBuf> {
    let from = |name: &str| {
        std::env::var_os(name)
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
    };
    from("XDG_CONFIG_HOME")
        .or_else(|| from("HOME").map(|home| home.join(".config")))
        .map(|config| config.join("jcctl").join("token"))
}

/// Where `jcctl token` puts the token of one Endpoint: `endpoints/{slug}` beside the default
/// token file.
pub fn default_endpoint_token_file(slug: &str) -> Option<PathBuf> {
    default_token_file().and_then(|file| file.parent().map(|dir| dir.join("endpoints").join(slug)))
}

/// The Endpoint slug an address names, e.g. `https://api.example.org/api/endpoint/{slug}/ngsi-ld/v1`:
/// the segment after `/api/endpoint/`, lowercase letters and digits only, so it names exactly one
/// scope.
pub fn endpoint_slug(resource: &str) -> Result<String, LoginError> {
    let bad = |why: &str| {
        LoginError::Address(format!(
            "--resource {resource}: {why}; pass the Endpoint's address as its page shows it, \
             e.g. https://api.example.org/api/endpoint/<slug>"
        ))
    };
    let parsed = Url::parse(resource).map_err(|e| bad(&format!("not a URL: {e}")))?;
    if !matches!(parsed.scheme(), "https" | "http") {
        return Err(bad("not an http address"));
    }
    let slug = parsed
        .path()
        .strip_prefix(ENDPOINT_PATH)
        .map(|rest| rest.split('/').next().unwrap_or_default())
        .ok_or_else(|| bad("its path does not start with /api/endpoint/"))?;
    if slug.is_empty()
        || slug.len() > 64
        || !slug
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
    {
        return Err(bad("it names no Endpoint slug"));
    }
    Ok(slug.to_owned())
}

/// The scope that asks for a token of the one Endpoint `slug` (the gateway's `endpoint:{slug}`).
pub fn endpoint_scope(slug: &str) -> String {
    format!("{PORTAL_SCOPE} endpoint:{slug}")
}

/// RFC 7636 §4.1–4.2: a verifier of 32 random bytes (43 characters) and its S256 challenge.
fn proof_key() -> Result<(SecretValue, String), String> {
    let mut bytes = [0_u8; 32];
    getrandom::fill(&mut bytes).map_err(|e| format!("no random source for the proof key: {e}"))?;
    let verifier = URL_SAFE_NO_PAD.encode(bytes);
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    Ok((SecretValue::new(verifier), challenge))
}

/// Whether a token may travel to `url`: over https, or over plain http to a host that cannot be
/// on the internet — loopback, a single-label service name or a `.svc` name inside the cluster,
/// where the mesh carries it (F-R2PLT4-012). An http URL to a public name sends the token in
/// the clear to whoever answers on the way.
pub fn token_may_travel(url: &Url) -> bool {
    let Some(host) = url.host_str() else {
        return false;
    };
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    let inside = host == "localhost"
        || !host.contains(['.', ':', '['])
        || host.ends_with(".svc")
        || host.ends_with(".svc.cluster.local")
        || host
            .trim_matches(['[', ']'])
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback());
    match url.scheme() {
        "https" => true,
        "http" => inside,
        _ => false,
    }
}

/// The realm's issuer, e.g. `https://idm.example.org/realms/city`. A token travels over it, so it
/// must be `https`, except on the loopback interface; a URL carrying a user name or password is
/// refused.
pub fn issuer(url: &str) -> Result<Url, LoginError> {
    let bad = |why: &str| LoginError::Address(format!("--idm {url}: {why}"));
    let parsed =
        Url::parse(url.trim_end_matches('/')).map_err(|e| bad(&format!("not a URL: {e}")))?;
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err(bad("it carries credentials"));
    }
    let host = parsed.host_str().ok_or_else(|| bad("it names no host"))?;
    let loopback = host == "localhost"
        || host
            .trim_matches(['[', ']'])
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback());
    match parsed.scheme() {
        "https" => Ok(parsed),
        "http" if loopback => Ok(parsed),
        _ => Err(bad("a token is only fetched over https")),
    }
}

#[derive(Deserialize)]
struct DeviceAuthorization {
    device_code: String,
    user_code: String,
    verification_uri: String,
    #[serde(default)]
    verification_uri_complete: Option<String>,
    expires_in: u64,
    #[serde(default)]
    interval: Option<u64>,
}

#[derive(Deserialize)]
struct TokenAnswer {
    #[serde(default)]
    access_token: Option<String>,
    #[serde(default)]
    expires_in: Option<u64>,
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    error_description: Option<String>,
}

/// What a completed sign-in wrote.
#[derive(Debug, PartialEq, Eq)]
pub struct LoggedIn {
    /// The file holding the access token.
    pub path: PathBuf,
    /// How long the token is valid, when the provider said.
    pub expires_in: Option<u64>,
}

/// Runs the device flow against `issuer` for `scope` and writes the token to `path`.
///
/// `tell` receives the one sentence the person acts on (the address and the code), and `sleep`
/// waits between polls, so a test drives the flow without waiting for real.
pub fn login(
    issuer: &Url,
    scope: &str,
    path: &Path,
    mut tell: impl FnMut(&str),
    mut sleep: impl FnMut(Duration),
) -> Result<LoggedIn, LoginError> {
    let client = Client::builder()
        .timeout(REQUEST_TIMEOUT)
        .user_agent("jcctl")
        .build()
        .map_err(|e| unavailable(issuer, &e.to_string()))?;
    let endpoint = |tail: &str| {
        format!(
            "{}/protocol/openid-connect/{tail}",
            issuer.as_str().trim_end_matches('/')
        )
    };

    let (verifier, challenge) = proof_key().map_err(|e| unavailable(issuer, &e))?;
    let device = authorize_device(&client, issuer, &endpoint("auth/device"), scope, &challenge)?;
    tell(&match &device.verification_uri_complete {
        Some(complete) => format!(
            "To sign in, open {complete} and confirm the code {}",
            device.user_code
        ),
        None => format!(
            "To sign in, open {} and enter the code {}",
            device.verification_uri, device.user_code
        ),
    });

    let deadline = Instant::now() + Duration::from_secs(device.expires_in);
    let mut interval = device.interval.unwrap_or(DEFAULT_INTERVAL);
    loop {
        sleep(Duration::from_secs(interval));
        if Instant::now() >= deadline {
            return Err(LoginError::Refused(
                "the code expired before it was confirmed".to_owned(),
            ));
        }
        let response = client
            .post(endpoint("token"))
            .form(&[
                ("grant_type", GRANT),
                ("device_code", device.device_code.as_str()),
                ("client_id", CLIENT_ID),
                ("code_verifier", verifier.expose()),
            ])
            .send()
            .map_err(|e| unavailable(issuer, &e.without_url().to_string()))?;
        let text = response
            .text()
            .map_err(|e| unavailable(issuer, &e.to_string()))?;
        let answer: TokenAnswer = serde_json::from_str(&text).map_err(|_| {
            unavailable(
                issuer,
                "the token endpoint answered something that is not JSON",
            )
        })?;
        match (answer.access_token, answer.error.as_deref()) {
            (Some(token), _) if !token.is_empty() => {
                write_token(path, &SecretValue::new(token))?;
                return Ok(LoggedIn {
                    path: path.to_owned(),
                    expires_in: answer.expires_in,
                });
            }
            (_, Some("authorization_pending")) => {}
            (_, Some("slow_down")) => interval += SLOW_DOWN,
            (_, Some("access_denied")) => {
                return Err(LoginError::Refused("the sign-in was declined".to_owned()))
            }
            (_, Some("expired_token")) => {
                return Err(LoginError::Refused(
                    "the code expired before it was confirmed".to_owned(),
                ))
            }
            (_, error) => {
                return Err(LoginError::Refused(
                    answer
                        .error_description
                        .or(error.map(str::to_owned))
                        .unwrap_or_else(|| {
                            "the token endpoint answered without a token".to_owned()
                        }),
                ))
            }
        }
    }
}

/// RFC 8628 §3.1–3.2: asks `url` for a device code of `scope`, with the proof key's `challenge`.
fn authorize_device(
    client: &Client,
    issuer: &Url,
    url: &str,
    scope: &str,
    challenge: &str,
) -> Result<DeviceAuthorization, LoginError> {
    let response = client
        .post(url)
        .form(&[
            ("client_id", CLIENT_ID),
            ("scope", scope),
            ("code_challenge", challenge),
            ("code_challenge_method", "S256"),
        ])
        .send()
        .map_err(|e| unavailable(issuer, &e.without_url().to_string()))?;
    let status = response.status();
    let text = response
        .text()
        .map_err(|e| unavailable(issuer, &e.to_string()))?;
    if !status.is_success() {
        return Err(LoginError::Refused(oauth_error(&text).unwrap_or_else(
            || format!("the provider refused to start a device sign-in ({status})"),
        )));
    }
    serde_json::from_str(&text).map_err(|e| {
        unavailable(
            issuer,
            &format!("the device authorization is unreadable: {e}"),
        )
    })
}

fn unavailable(issuer: &Url, message: &str) -> LoginError {
    LoginError::Unavailable {
        url: issuer.to_string(),
        message: message.to_owned(),
    }
}

/// RFC 6749 §5.2's `error_description`, else its `error`.
fn oauth_error(body: &str) -> Option<String> {
    let answer: TokenAnswer = serde_json::from_str(body).ok()?;
    answer.error_description.or(answer.error)
}

/// Writes the token to `path`, readable by its owner alone: to a new file beside it created with
/// mode 600 and renamed over the old one, so no reader ever sees half a token or a wider mode.
fn write_token(path: &Path, token: &SecretValue) -> Result<(), LoginError> {
    let failed = |e: std::io::Error| LoginError::Write {
        path: path.display().to_string(),
        message: e.to_string(),
    };
    let dir = path
        .parent()
        .filter(|d| !d.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    create_private_dir(dir).map_err(failed)?;
    let staging = dir.join(format!(
        ".{}.{}",
        path.file_name().and_then(|n| n.to_str()).unwrap_or("token"),
        std::process::id()
    ));
    let written = open_private(&staging)
        .and_then(|mut file| {
            file.write_all(token.expose().as_bytes())
                .and_then(|()| file.sync_all())
        })
        .and_then(|()| std::fs::rename(&staging, path));
    if let Err(e) = written {
        let _ = std::fs::remove_file(&staging);
        return Err(failed(e));
    }
    Ok(())
}

#[cfg(unix)]
fn create_private_dir(dir: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
}

#[cfg(not(unix))]
fn create_private_dir(dir: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)
}

#[cfg(unix)]
fn open_private(path: &Path) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
}

#[cfg(not(unix))]
fn open_private(path: &Path) -> std::io::Result<std::fs::File> {
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
}
