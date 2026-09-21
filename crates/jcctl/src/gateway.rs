//! The live half of the reconciler: one space surface, reached through the Context Gateway
//! (CC-04, CC-72, T-0421).
//!
//! Configuration is read from the repository by whoever serves it, so the only thing the
//! reconciler writes to a running platform is seed entities, and the only address it takes is
//! a gateway. Every call carries the reconciler's own ServiceAccount token in the
//! `Authorization` header, so the write is decided by the same Policy layer as any other
//! client's: a reconciler that could address a broker directly would be a policy bypass with
//! a command-line flag in front of it.

use crate::secrets::SecretValue;
use reqwest::blocking::Client;
use reqwest::{StatusCode, Url};
use serde_json::Value;
use std::path::Path;
use std::time::Duration;

/// How long a connection may take to open.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// How long one call may take end to end. An upsert of a space's whole seed is one request.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(120);
/// How much of an unusable answer an error repeats: enough to see a proxy's error page.
const SNIPPET: usize = 200;

/// Why the platform could not answer, or refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum BrokerError {
    /// The gateway could not be reached, or the address is not usable.
    #[error("the platform at {url} could not be reached: {message}")]
    Unavailable {
        /// The address that was tried, never the credential.
        url: String,
        /// What went wrong.
        message: String,
    },
    /// The gateway answered, and refused.
    #[error("{space}: the platform refused the write ({status}): {message}")]
    Refused {
        /// The space the call was for.
        space: String,
        /// The status it refused with.
        status: u16,
        /// What it said.
        message: String,
    },
}

/// The space surface of a live platform, as the reconciler uses it.
pub trait Broker {
    /// The entity the broker holds under this id, or `None` when it holds none.
    fn entity(&self, space: &str, id: &str) -> Result<Option<Value>, BrokerError>;

    /// Creates or replaces these entities in one space (CIM 009 `entityOperations/upsert`).
    fn upsert(&self, space: &str, entities: &[Value]) -> Result<(), BrokerError>;
}

/// A Context Gateway reached over HTTP, holding the token every call is made with.
#[derive(Debug)]
pub struct Gateway {
    base: Url,
    token: SecretValue,
    client: Client,
}

impl Gateway {
    /// A client for the gateway at `base_url`, authenticated with `token`.
    ///
    /// The address is checked here rather than on the first call, so a malformed URL is one
    /// error before anything is read. A URL carrying credentials is refused: the token is the
    /// only credential, and it does not live in a URL.
    pub fn new(base_url: &str, token: SecretValue) -> Result<Self, BrokerError> {
        let unusable = |message: &str| BrokerError::Unavailable {
            url: base_url.to_owned(),
            message: message.to_owned(),
        };
        let base = Url::parse(base_url.trim_end_matches('/'))
            .map_err(|e| unusable(&format!("it is not a URL: {e}")))?;
        if !base.username().is_empty() || base.password().is_some() {
            return Err(unusable(
                "it carries credentials; the ServiceAccount token is the only credential and it \
                 travels in the Authorization header",
            ));
        }
        if !matches!(base.scheme(), "http" | "https") || base.host_str().is_none() {
            return Err(unusable("it is not an http(s) URL naming a host"));
        }
        if token.is_empty() {
            return Err(unusable("the reconciler's token is empty"));
        }
        let client = Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(REQUEST_TIMEOUT)
            .user_agent("jcctl")
            .build()
            .map_err(|e| unusable(&e.to_string()))?;
        Ok(Self {
            base,
            token,
            client,
        })
    }

    /// The token as it was read from the file `--token-file` names.
    ///
    /// The file is the projected ServiceAccount token in the cluster, so it is re-read per
    /// run and never held anywhere else; trailing whitespace is a text editor's, not the
    /// token's.
    pub fn token_from(path: &Path) -> Result<SecretValue, BrokerError> {
        let text = std::fs::read_to_string(path).map_err(|e| BrokerError::Unavailable {
            url: path.display().to_string(),
            message: format!("the token file could not be read: {e}"),
        })?;
        Ok(SecretValue::new(text.trim().to_owned()))
    }

    /// `{base}/cs/{space}/ngsi-ld/v1/{tail}`, the space surface the gateway serves byte for
    /// byte (SP-03, CC-16).
    fn space_url(&self, space: &str, tail: &str) -> Result<Url, BrokerError> {
        self.base
            .join(&format!("/cs/{space}/ngsi-ld/v1/{tail}"))
            .map_err(|e| BrokerError::Unavailable {
                url: self.base.to_string(),
                message: format!("{space} is not a usable space name: {e}"),
            })
    }

    /// What the gateway answered, as an error repeats it: the token redacted first, so an
    /// answer that echoes the Authorization header (a proxy's error page) never carries it
    /// into a log, whole or cut at the snippet's edge (CC-06).
    fn said(&self, body: &str) -> String {
        snippet(&body.replace(self.token.expose(), "[redacted]"))
    }

    fn unavailable(&self, error: &reqwest::Error) -> BrokerError {
        BrokerError::Unavailable {
            url: self.base.to_string(),
            // `reqwest` repeats the URL and nothing else of the request, so no header of ours
            // reaches a log through this.
            message: error.to_string(),
        }
    }
}

/// The first `SNIPPET` characters of an answer that is not what was expected.
fn snippet(body: &str) -> String {
    let trimmed = body.trim();
    match trimmed.char_indices().nth(SNIPPET) {
        Some((at, _)) => format!("{}…", &trimmed[..at]),
        None => trimmed.to_owned(),
    }
}

impl Broker for Gateway {
    fn entity(&self, space: &str, id: &str) -> Result<Option<Value>, BrokerError> {
        // One retrieval per id rather than one `?id=a,b,c` query: a retrieval by id is the
        // one read every Policy allows a reader of the space, while a query carries type and
        // attribute narrowing this comparison has no business depending on.
        // ponytail: a space seeded with thousands of entities wants the batch query instead.
        let url = self.space_url(space, &format!("entities/{id}"))?;
        let response = self
            .client
            .get(url)
            .bearer_auth(self.token.expose())
            // Normalized JSON without the `@context` document, which is what the repository
            // declares and what makes the two comparable (CIM 009 6.3.5).
            .header("Accept", "application/json")
            .send()
            .map_err(|e| self.unavailable(&e))?;

        let status = response.status();
        if status == StatusCode::NOT_FOUND {
            return Ok(None);
        }
        let body = response.text().map_err(|e| self.unavailable(&e))?;
        if !status.is_success() {
            return Err(BrokerError::Refused {
                space: space.to_owned(),
                status: status.as_u16(),
                message: self.said(&body),
            });
        }
        serde_json::from_str(&body)
            .map(Some)
            .map_err(|e| BrokerError::Refused {
                space: space.to_owned(),
                status: status.as_u16(),
                message: format!("the answer is not an entity ({e}): {}", self.said(&body)),
            })
    }

    fn upsert(&self, space: &str, entities: &[Value]) -> Result<(), BrokerError> {
        if entities.is_empty() {
            return Ok(());
        }
        let url = self.space_url(space, "entityOperations/upsert")?;
        let response = self
            .client
            .post(url)
            .bearer_auth(self.token.expose())
            .header("Content-Type", "application/json")
            .json(&entities)
            .send()
            .map_err(|e| self.unavailable(&e))?;

        let status = response.status();
        let body = response.text().unwrap_or_default();
        // 201 and 204 are "all of them"; 207 is per-entity, and an entity the broker refused
        // is a failure of this run even though the others landed (CIM 009 6.17).
        if status == StatusCode::MULTI_STATUS {
            let refused = serde_json::from_str::<Value>(&body)
                .ok()
                .and_then(|report| report.get("errors").cloned())
                .filter(|errors| errors.as_array().is_some_and(|list| !list.is_empty()));
            return match refused {
                None => Ok(()),
                Some(errors) => Err(BrokerError::Refused {
                    space: space.to_owned(),
                    status: status.as_u16(),
                    message: self.said(&errors.to_string()),
                }),
            };
        }
        if status.is_success() {
            return Ok(());
        }
        Err(BrokerError::Refused {
            space: space.to_owned(),
            status: status.as_u16(),
            message: self.said(&body),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// CC-06 (T-2549): the token leaves an echoed answer before the answer is cut, so not even
    /// the part of it that would fall inside the snippet survives.
    #[test]
    fn an_echoed_token_is_redacted_before_the_snippet_is_cut() {
        let token = "eyJhbGciOiJSUzI1NiJ9.t2549-token";
        let gateway =
            Gateway::new("http://gw:9090", SecretValue::new(token.to_owned())).expect("a client");
        for pad in [0, SNIPPET - 20, SNIPPET - 1, SNIPPET] {
            let body = format!("{}Bearer {token} and again {token}", "x".repeat(pad));
            let said = gateway.said(&body);
            assert!(!said.contains("eyJhbGciOi"), "{pad}: {said}");
            assert!(said.chars().count() <= SNIPPET + 1, "{pad}: {said}");
        }
        assert_eq!(gateway.said("  no such space  "), "no such space");
    }
}
