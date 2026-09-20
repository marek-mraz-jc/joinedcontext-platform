//! OpenBao over HTTPS: the one place this crate opens a socket towards the secret store
//! (T-0423, CC-06, OPS-37, PL-17, ADR-N-012).
//!
//! [`super::openbao`] builds the paths and the payloads, reads the KV v2 envelope and decides
//! every refusal; this carries the request. The split is the one [`crate::publish::ckan_http`]
//! uses, and it is why the resolution path is testable without a server.
//!
//! Two credentials pass through here and neither is written down. The ServiceAccount JWT goes
//! into the login body, the session token into the `X-Vault-Token` header, and no error ever
//! repeats either: a refusal carries the status and what OpenBao put in its `errors` array,
//! which is a message about a policy and never a value (PL-17).

use super::openbao::{BaoApi, BaoError};
use reqwest::blocking::Client;
use reqwest::{StatusCode, Url};
use serde_json::Value;
use std::path::Path;
use std::time::Duration;

/// How long a connection may take to open.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// How long one call may take end to end. Both calls are small reads; an apply that waits
/// longer than this on a secret store is a store that is down.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// How much of a non-JSON answer an error repeats: enough to recognise a proxy's error page,
/// not enough to flood a log.
const SNIPPET: usize = 200;

/// One OpenBao instance reached over HTTP(S).
#[derive(Debug)]
pub struct HttpBao {
    base: Url,
    client: Client,
}

impl HttpBao {
    /// A client for the instance at `address`, trusting the platform's root store.
    ///
    /// The URL is checked here rather than on the first call, so a malformed address is one
    /// error at start-up instead of one per secret. A URL carrying credentials is refused:
    /// the ServiceAccount token is the only credential and it does not live in a URL.
    pub fn new(address: &str) -> Result<Self, BaoError> {
        Self::build(address, Client::builder())
    }

    /// The same, trusting one additional root from a PEM file.
    ///
    /// A cluster that issues its own certificates (`selfsigned-ca`) serves OpenBao with a root
    /// the platform's bundle does not carry, and the handshake fails before any policy is
    /// consulted. This adds that one root; it never replaces the bundle, so a public issuer
    /// keeps working with the same configuration.
    pub fn with_ca_file(address: &str, ca_pem: &Path) -> Result<Self, BaoError> {
        let pem = std::fs::read(ca_pem).map_err(|source| BaoError::Io {
            path: ca_pem.to_path_buf(),
            source,
        })?;
        // `Certificate::from_pem` hands the bytes to rustls, which parses them when the first
        // handshake happens — so a file that is not a certificate is accepted here and fails
        // much later, on a connection, looking like an unreachable store. One look at the
        // armour turns that into an error where the configuration is read.
        if !String::from_utf8_lossy(&pem).contains("-----BEGIN CERTIFICATE-----") {
            return Err(BaoError::Transport {
                path: ca_pem.display().to_string(),
                message: "not a PEM certificate: no BEGIN CERTIFICATE block".to_owned(),
            });
        }
        let certificate =
            reqwest::Certificate::from_pem(&pem).map_err(|e| BaoError::Transport {
                path: ca_pem.display().to_string(),
                message: format!("not a PEM certificate: {e}"),
            })?;
        Self::build(address, Client::builder().add_root_certificate(certificate))
    }

    fn build(address: &str, builder: reqwest::blocking::ClientBuilder) -> Result<Self, BaoError> {
        let base = Url::parse(address.trim_end_matches('/')).map_err(|e| BaoError::Transport {
            path: address.to_owned(),
            message: format!("'{address}' is not a URL: {e}"),
        })?;
        if !base.username().is_empty() || base.password().is_some() {
            return Err(BaoError::Transport {
                path: address.to_owned(),
                message: "the OpenBao address carries credentials; the ServiceAccount token is \
                          the only credential and it travels in the body or the header"
                    .to_owned(),
            });
        }
        if !matches!(base.scheme(), "http" | "https") || base.host_str().is_none() {
            return Err(BaoError::Transport {
                path: address.to_owned(),
                message: format!("'{address}' is not an http(s) URL naming a host"),
            });
        }
        let client = builder
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(REQUEST_TIMEOUT)
            .build()
            .map_err(|e| BaoError::Transport {
                path: address.to_owned(),
                message: format!("HTTP client: {e}"),
            })?;
        Ok(Self { base, client })
    }

    /// The instance this client speaks to, without a trailing slash.
    pub fn address(&self) -> &str {
        self.base.as_str().trim_end_matches('/')
    }

    /// `{base}/v1/{path}`, the API root OpenBao's own documentation writes paths against.
    fn url(&self, path: &str) -> Result<Url, BaoError> {
        let joined = format!(
            "{}/v1/{}",
            self.base.path().trim_end_matches('/'),
            path.trim_start_matches('/')
        );
        self.base.join(&joined).map_err(|e| BaoError::Transport {
            path: path.to_owned(),
            message: format!("not a usable API path: {e}"),
        })
    }
}

impl BaoApi for HttpBao {
    fn post(&mut self, path: &str, body: &Value) -> Result<Value, BaoError> {
        let response = self
            .client
            .post(self.url(path)?)
            .header("Accept", "application/json")
            .json(body)
            .send()
            .map_err(|e| transport(path, &e))?;
        match read(path, response)? {
            Some(value) => Ok(value),
            // A login OpenBao does not recognise answers 404 with an error body, which `read`
            // turns into an `Api` refusal. A 404 with no body at all means the auth mount is
            // not enabled, and saying so beats handing back an empty session.
            None => Err(BaoError::Api {
                status: StatusCode::NOT_FOUND.as_u16(),
                path: path.to_owned(),
                message: "no such path; is the Kubernetes auth method mounted there?".to_owned(),
            }),
        }
    }

    fn get(&self, path: &str, token: &str) -> Result<Option<Value>, BaoError> {
        let response = self
            .client
            .get(self.url(path)?)
            .header("Accept", "application/json")
            // The one place the session token is put on a wire.
            .header("X-Vault-Token", token)
            .send()
            .map_err(|e| transport(path, &e))?;
        read(path, response)
    }
}

/// The body of one answer, or `None` for a 404.
///
/// OpenBao answers 404 both for a path that holds nothing and for a path the token may not
/// read, so the caller cannot tell a missing secret from a missing grant — and neither can
/// this. Every other refusal carries the status and the `errors` array, which is OpenBao's
/// own message about a policy and never a value.
fn read(path: &str, response: reqwest::blocking::Response) -> Result<Option<Value>, BaoError> {
    let status = response.status();
    let text = response.text().map_err(|e| transport(path, &e))?;
    let parsed: Option<Value> = serde_json::from_str(&text).ok();

    if status.is_success() {
        return match parsed {
            Some(value) => Ok(Some(value)),
            None => Err(BaoError::Malformed {
                path: path.to_owned(),
                reason: format!(
                    "answered {} without a JSON body: {}",
                    status.as_u16(),
                    snippet(&text)
                ),
            }),
        };
    }
    if status == StatusCode::NOT_FOUND && errors(parsed.as_ref()).is_empty() {
        return Ok(None);
    }
    Err(BaoError::Api {
        status: status.as_u16(),
        path: path.to_owned(),
        message: match errors(parsed.as_ref()) {
            message if message.is_empty() => snippet(&text),
            message => message,
        },
    })
}

/// What OpenBao put in its `errors` array, joined into one line.
fn errors(body: Option<&Value>) -> String {
    body.and_then(|value| value.get("errors"))
        .and_then(Value::as_array)
        .map(|entries| {
            entries
                .iter()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join("; ")
        })
        .unwrap_or_default()
}

/// `reqwest` repeats the URL in its message and nothing else of the request, so the message is
/// safe to pass on; the path is named separately for a reader who sees only the log.
fn transport(path: &str, error: &reqwest::Error) -> BaoError {
    BaoError::Transport {
        path: path.to_owned(),
        message: error.to_string(),
    }
}

fn snippet(text: &str) -> String {
    let trimmed = text.trim();
    match trimmed.char_indices().nth(SNIPPET) {
        Some((cut, _)) => format!("{}…", &trimmed[..cut]),
        None => trimmed.to_owned(),
    }
}
