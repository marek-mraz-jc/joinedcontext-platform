//! `jcctl login`: a person's token for the Portal, by RFC 8628's device authorization grant on
//! the public Keycloak client `jcctl` (PF-45, MF-14, API/03 §2a).
//!
//! A terminal has no browser to come back to, so the person opens the address it prints on any
//! device, signs in there and confirms the code; `jcctl` polls the token endpoint meanwhile and
//! writes the access token to a file only its owner may read. The client verbs read that file
//! when neither `--token-file` nor `JC_TOKEN_FILE` names another. The refresh token is not kept:
//! a session ends with its access token, and the next one is another `jcctl login`.

use crate::secrets::SecretValue;
use reqwest::blocking::Client;
use reqwest::Url;
use serde::Deserialize;
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

/// Runs the device flow against `issuer` and writes the token to `path`.
///
/// `tell` receives the one sentence the person acts on (the address and the code), and `sleep`
/// waits between polls, so a test drives the flow without waiting for real.
pub fn login(
    issuer: &Url,
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

    let response = client
        .post(endpoint("auth/device"))
        .form(&[("client_id", CLIENT_ID), ("scope", "openid")])
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
    let device: DeviceAuthorization = serde_json::from_str(&text).map_err(|e| {
        unavailable(
            issuer,
            &format!("the device authorization is unreadable: {e}"),
        )
    })?;
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
