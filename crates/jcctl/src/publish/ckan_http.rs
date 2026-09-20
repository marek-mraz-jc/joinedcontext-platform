//! The CKAN Action API over HTTPS (T-0487, EP-62, EP-65, EP-67).
//!
//! [`super::ckan`] and [`super::ckan_datastore`] build payloads and decide which action to
//! call; this is the one place a socket is opened towards the catalogue. The API token is
//! held here and travels in the `Authorization` header of every request and nowhere else:
//! not in a URL, not in a payload, and never in an error, which repeats what CKAN said and
//! the status it said it with (EP-67).
//!
//! A `*_show` that CKAN answers `404` is `None`, so the callers' create-or-update logic
//! reads the same against the real catalogue as against the in-memory double (CC-18).

use super::ckan::{CkanApi, CkanError};
use crate::secrets::SecretValue;
use reqwest::blocking::{Client, RequestBuilder};
use reqwest::{StatusCode, Url};
use serde_json::{json, Value};
use std::time::Duration;

/// How long a connection may take to open.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// How long one action may take end to end. A `datastore_upsert` of a full table is the
/// slow one, and CKAN writes it in one transaction.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(120);

/// How much of a non-JSON answer an error repeats: enough to see a proxy's error page,
/// not enough to flood a log.
const SNIPPET: usize = 200;

/// The DataStore's own row number, which CKAN adds to every table and no mirror declares.
const ROW_NUMBER: &str = "_id";

/// One CKAN instance reached over HTTP(S), holding the token its `apiTokenRef` resolved to.
#[derive(Debug)]
pub struct HttpCkan {
    base: Url,
    token: SecretValue,
    client: Client,
}

impl HttpCkan {
    /// A client for the instance at `base_url`, authenticated with `token`.
    ///
    /// The URL is checked here rather than on the first call, so a malformed instance URL
    /// is one error at start-up and not one per endpoint. A URL carrying credentials is
    /// refused: the token is the only credential and it does not live in a URL (EP-67).
    pub fn new(base_url: &str, token: SecretValue) -> Result<Self, CkanError> {
        let base = Url::parse(base_url.trim_end_matches('/'))
            .map_err(|e| CkanError::Unavailable(format!("'{base_url}' is not a URL: {e}")))?;
        if !base.username().is_empty() || base.password().is_some() {
            return Err(CkanError::Unavailable(
                "the CKAN URL carries credentials; the API token is the only credential and \
                 it travels in the Authorization header (EP-67)"
                    .to_owned(),
            ));
        }
        if !matches!(base.scheme(), "http" | "https") || base.host_str().is_none() {
            return Err(CkanError::Unavailable(format!(
                "'{base_url}' is not an http(s) URL naming a host"
            )));
        }
        if token.is_empty() {
            return Err(CkanError::Unavailable(
                "the CKAN API token is empty".to_owned(),
            ));
        }
        let client = Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(REQUEST_TIMEOUT)
            .user_agent(super::ckan::GENERATOR)
            .build()
            .map_err(|e| CkanError::Unavailable(format!("HTTP client: {e}")))?;
        Ok(Self {
            base,
            token,
            client,
        })
    }

    /// The instance this client speaks to, without a trailing slash.
    pub fn base_url(&self) -> &str {
        self.base.as_str().trim_end_matches('/')
    }

    /// `{base}/api/3/action/{action}`.
    fn url(&self, action: &str) -> Result<Url, CkanError> {
        let path = format!(
            "{}/api/3/action/{action}",
            self.base.path().trim_end_matches('/')
        );
        self.base
            .join(&path)
            .map_err(|e| CkanError::Unavailable(format!("action URL for {action}: {e}")))
    }

    /// Sends one request and reads the envelope CKAN wraps every answer in.
    ///
    /// `Ok(None)` is a `404`: an object a `*_show` did not find. Every other refusal is an
    /// error carrying the status and CKAN's own message; a body that is not the envelope
    /// (a proxy's error page, an empty answer) is reported as unavailable with a snippet.
    fn send(&self, action: &str, request: RequestBuilder) -> Result<Option<Value>, CkanError> {
        let response = request
            .header("Authorization", self.token.expose())
            .header("Accept", "application/json")
            .send()
            .map_err(|e| CkanError::Unavailable(transport(action, &e)))?;
        let status = response.status();
        let text = response
            .text()
            .map_err(|e| CkanError::Unavailable(transport(action, &e)))?;
        let envelope: Value = match serde_json::from_str(&text) {
            Ok(envelope) => envelope,
            Err(_) if status == StatusCode::NOT_FOUND => return Ok(None),
            Err(_) => {
                let message = format!(
                    "{action} answered {} without a JSON body: {}",
                    status.as_u16(),
                    snippet(&text)
                );
                return Err(
                    if status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN {
                        CkanError::Rejected {
                            action: action.to_owned(),
                            message,
                        }
                    } else {
                        CkanError::Unavailable(message)
                    },
                );
            }
        };
        if envelope.get("success") == Some(&json!(true)) {
            return Ok(Some(envelope.get("result").cloned().unwrap_or(Value::Null)));
        }
        if status == StatusCode::NOT_FOUND {
            return Ok(None);
        }
        Err(CkanError::Rejected {
            action: action.to_owned(),
            message: format!("{} {}", status.as_u16(), refusal(&envelope)),
        })
    }

    /// A `GET` of one action with its query.
    fn get(&self, action: &str, query: &[(&str, &str)]) -> Result<Option<Value>, CkanError> {
        let request = self.client.get(self.url(action)?).query(query);
        self.send(action, request)
    }

    /// The DataStore fields of one resource, as the mirror declares them: every column but
    /// CKAN's own row number (EP-65).
    fn fields_of(&self, resource_id: &str) -> Result<Vec<Value>, CkanError> {
        let table = self
            .get(
                "datastore_search",
                &[("resource_id", resource_id), ("limit", "0")],
            )?
            .unwrap_or(Value::Null);
        Ok(table
            .get("fields")
            .and_then(Value::as_array)
            .map(|fields| {
                fields
                    .iter()
                    .filter(|field| field.get("id").and_then(Value::as_str) != Some(ROW_NUMBER))
                    .cloned()
                    .collect()
            })
            .unwrap_or_default())
    }
}

impl CkanApi for HttpCkan {
    /// `GET {action}?id={name}`.
    ///
    /// `resource_show` answers the resource together with the fields of its DataStore
    /// table, which is what [`super::ckan_datastore::ensure`] reads and what CKAN keeps
    /// behind a second action (`datastore_search`); a resource without a table has none.
    fn show(&self, action: &str, name: &str) -> Result<Option<Value>, CkanError> {
        let Some(mut object) = self.get(action, &[("id", name)])? else {
            return Ok(None);
        };
        if action == "resource_show" {
            let active = object.get("datastore_active") == Some(&json!(true));
            let id = object
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or(name)
                .to_owned();
            let fields = if active {
                self.fields_of(&id)?
            } else {
                Vec::new()
            };
            object["fields"] = Value::Array(fields);
        }
        Ok(Some(object))
    }

    /// `POST {action}` with the payload as its JSON body.
    fn action(&mut self, action: &str, payload: &Value) -> Result<Value, CkanError> {
        let request = self.client.post(self.url(action)?).json(payload);
        match self.send(action, request)? {
            Some(result) => Ok(result),
            // A writing action has nothing to be "not found" but the action itself.
            None => Err(CkanError::Rejected {
                action: action.to_owned(),
                message: "404 the action is not known to this CKAN".to_owned(),
            }),
        }
    }
}

/// A transport failure, worded without anything a request carried.
///
/// `reqwest` repeats the URL in its message and nothing else of the request; the URL
/// holds no token, so the message is safe to show. The body of a request is never part
/// of it.
fn transport(action: &str, error: &reqwest::Error) -> String {
    let kind = if error.is_timeout() {
        "timed out"
    } else if error.is_connect() {
        "could not connect"
    } else {
        "failed"
    };
    format!("{action} {kind}: {error}")
}

/// What CKAN said in a failed envelope: the error's type and message, and every field it
/// refused, so a validation error names the field rather than saying "validation error".
fn refusal(envelope: &Value) -> String {
    let Some(error) = envelope.get("error").and_then(Value::as_object) else {
        return "CKAN answered no error".to_owned();
    };
    let mut parts: Vec<String> = Vec::new();
    if let Some(kind) = error.get("__type").and_then(Value::as_str) {
        parts.push(kind.to_owned());
    }
    if let Some(message) = error.get("message").and_then(Value::as_str) {
        parts.push(message.to_owned());
    }
    for (field, problems) in error {
        if field == "__type" || field == "message" {
            continue;
        }
        let detail = match problems {
            Value::Array(items) => items
                .iter()
                .map(|item| match item {
                    Value::String(text) => text.clone(),
                    other => other.to_string(),
                })
                .collect::<Vec<_>>()
                .join("; "),
            Value::String(text) => text.clone(),
            other => other.to_string(),
        };
        parts.push(format!("{field}: {detail}"));
    }
    if parts.is_empty() {
        "CKAN refused without a message".to_owned()
    } else {
        parts.join(": ")
    }
}

/// The start of a body that was not JSON, on one line.
fn snippet(text: &str) -> String {
    let flat: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.is_empty() {
        return "(empty body)".to_owned();
    }
    let mut end = SNIPPET.min(flat.len());
    while !flat.is_char_boundary(end) {
        end -= 1;
    }
    if end < flat.len() {
        format!("{}…", &flat[..end])
    } else {
        flat
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_refusal_names_the_type_the_message_and_the_fields() {
        let envelope = json!({
            "success": false,
            "error": {
                "__type": "Validation Error",
                "name": ["That URL is already in use."],
                "owner_org": ["Organization does not exist"]
            }
        });
        assert_eq!(
            refusal(&envelope),
            "Validation Error: name: That URL is already in use.: owner_org: Organization does not exist"
        );
        assert_eq!(
            refusal(&json!({ "success": false })),
            "CKAN answered no error"
        );
    }

    #[test]
    fn a_snippet_is_one_short_line() {
        assert_eq!(
            snippet("  <html>\n  <body>oops</body>\n"),
            "<html> <body>oops</body>"
        );
        assert_eq!(snippet("   "), "(empty body)");
        let long = "x".repeat(SNIPPET + 50);
        let cut = snippet(&long);
        assert!(cut.ends_with('…'));
        assert_eq!(cut.chars().count(), SNIPPET + 1);
    }

    #[test]
    fn a_url_with_credentials_or_without_a_host_is_refused() {
        let token = || SecretValue::new("t".to_owned());
        assert!(HttpCkan::new("https://user:pw@data.example.org", token()).is_err());
        assert!(HttpCkan::new("data.example.org", token()).is_err());
        assert!(HttpCkan::new(
            "https://data.example.org/ckan/",
            SecretValue::new(String::new())
        )
        .is_err());
        let client = HttpCkan::new("https://data.example.org/ckan/", token()).expect("a client");
        assert_eq!(client.base_url(), "https://data.example.org/ckan");
        assert_eq!(
            client.url("package_show").expect("an action URL").as_str(),
            "https://data.example.org/ckan/api/3/action/package_show"
        );
    }
}
