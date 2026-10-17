//! The caller's token, exchanged for the brokers a federated read reaches (T-3999,
//! ADR-N-047 §2.4, PF-113, PF-114).
//!
//! A registration asking for `identity: caller` is answered by the member's own Policy deciding
//! on the caller. The caller's original token never reaches a broker (K36), so the gateway asks
//! the realm, as its own client, for a token of the same person whose only audiences are the
//! member brokers' (RFC 8693, Keycloak standard token exchange V2): one optional client scope
//! `context-broker-{name}` per broker holds that audience, and only this client may request them.
//! A refused or failed exchange is an answer of its own; nothing falls back to the original token
//! or to an account of the gateway's. No token is ever part of an error or a log line.

use std::collections::BTreeSet;
use std::time::Duration;

use jc_core::{ClientCredential, ProblemDetails};
use serde::Deserialize;

const TOKEN_EXCHANGE: &str = "urn:ietf:params:oauth:grant-type:token-exchange";
const ACCESS_TOKEN_TYPE: &str = "urn:ietf:params:oauth:token-type:access_token";

/// The realm's token endpoint and the gateway's own client.
pub struct Exchanger {
    http: reqwest::Client,
    token_url: String,
    client_id: String,
    credential: ClientCredential,
}

/// Why a federated read carries no exchanged token. Never holds a token.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ExchangeError {
    /// The realm answered and refused: the caller's session ended, or the gateway may not
    /// exchange for that broker.
    #[error("the realm refused the token exchange ({0})")]
    Refused(u16),
    /// The realm did not answer, answered something unreadable, or the gateway could not prove
    /// its own client.
    #[error("the realm could not exchange the token: {0}")]
    Unavailable(String),
}

impl ExchangeError {
    /// The answer to the caller: `502` when the realm refused, `503` when it could not answer.
    pub fn problem(&self) -> ProblemDetails {
        match self {
            Self::Refused(_) => {
                ProblemDetails::new(502, "federation-exchange-failed", "Bad Gateway").with_detail(
                    "the identity provider refused to issue your token for the brokers this read \
                 federates to; sign in again, or ask the space's steward whether the member \
                 broker is registered for token exchange",
                )
            }
            Self::Unavailable(_) => {
                ProblemDetails::new(503, "federation-exchange-failed", "Service Unavailable")
                    .with_detail(
                        "the identity provider did not answer the token exchange this federated \
                     read needs; try again shortly",
                    )
            }
        }
    }
}

#[derive(Deserialize)]
struct Issued {
    access_token: String,
}

impl Exchanger {
    /// An exchanger asking `token_url` as `client_id`, proven by `credential` (PF-47).
    pub fn new(token_url: String, client_id: String, credential: ClientCredential) -> Self {
        // No redirect: the form carries the caller's token and this client's secret, and a 307
        // or 308 would send both on to whatever host the answer names (T-3591).
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(5))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap_or_default();
        Self {
            http,
            token_url,
            client_id,
            credential,
        }
    }

    /// The scope string that names `brokers`' audiences, one `context-broker-{name}` each.
    pub fn scope_of(brokers: &BTreeSet<String>) -> String {
        brokers
            .iter()
            .map(|name| format!("context-broker-{name}"))
            .collect::<Vec<_>>()
            .join(" ")
    }

    /// A token of the caller whose only audiences are `brokers`'.
    ///
    /// `subject` is the token the caller presented; its audience names this gateway's client,
    /// which V2 requires of a subject token. An empty `brokers` is a caller error.
    pub async fn exchange(
        &self,
        subject: &str,
        brokers: &BTreeSet<String>,
    ) -> Result<String, ExchangeError> {
        if brokers.is_empty() {
            return Err(ExchangeError::Unavailable(
                "no broker to exchange for".to_owned(),
            ));
        }
        let mut form = self
            .credential
            .form(&self.client_id)
            .map_err(ExchangeError::Unavailable)?;
        form.extend([
            ("grant_type", TOKEN_EXCHANGE.to_owned()),
            ("subject_token", subject.to_owned()),
            ("subject_token_type", ACCESS_TOKEN_TYPE.to_owned()),
            ("requested_token_type", ACCESS_TOKEN_TYPE.to_owned()),
            ("scope", Self::scope_of(brokers)),
        ]);
        let response = self
            .http
            .post(&self.token_url)
            .form(&form)
            .send()
            .await
            .map_err(|e| ExchangeError::Unavailable(e.without_url().to_string()))?;
        let status = response.status();
        if status.is_server_error() {
            return Err(ExchangeError::Unavailable(format!(
                "token endpoint answered {status}"
            )));
        }
        if !status.is_success() {
            return Err(ExchangeError::Refused(status.as_u16()));
        }
        let issued: Issued = response
            .json()
            .await
            .map_err(|_| ExchangeError::Unavailable("unreadable token answer".to_owned()))?;
        if issued.access_token.is_empty() {
            return Err(ExchangeError::Unavailable("empty access token".to_owned()));
        }
        Ok(issued.access_token)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::extract::Form;
    use axum::http::StatusCode;
    use axum::routing::post;
    use axum::{Json, Router};
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    type Seen = Arc<Mutex<Vec<HashMap<String, String>>>>;

    async fn realm(status: StatusCode, body: serde_json::Value) -> (String, Seen) {
        let seen: Seen = Arc::default();
        let kept = Arc::clone(&seen);
        let app = Router::new().route(
            "/token",
            post(move |Form(form): Form<HashMap<String, String>>| {
                let kept = Arc::clone(&kept);
                let body = body.clone();
                async move {
                    kept.lock().expect("lock").push(form);
                    (status, Json(body))
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let url = format!("http://{}/token", listener.local_addr().expect("addr"));
        tokio::spawn(async move { axum::serve(listener, app).await });
        (url, seen)
    }

    fn exchanger(url: String) -> Exchanger {
        Exchanger::new(
            url,
            "context-gateway".to_owned(),
            ClientCredential::Secret(jc_core::Secret::new("s3cret")),
        )
    }

    fn brokers(names: &[&str]) -> BTreeSet<String> {
        names.iter().map(|n| (*n).to_owned()).collect()
    }

    #[tokio::test]
    async fn the_caller_token_is_exchanged_for_the_member_brokers_scopes_only() {
        let (url, seen) = realm(
            StatusCode::OK,
            serde_json::json!({ "access_token": "exchanged", "token_type": "Bearer" }),
        )
        .await;
        let token = exchanger(url)
            .exchange("caller-token", &brokers(&["sensors", "archive"]))
            .await
            .expect("exchanged");
        assert_eq!(token, "exchanged");
        let form = seen.lock().expect("lock")[0].clone();
        assert_eq!(form["grant_type"], TOKEN_EXCHANGE);
        assert_eq!(form["subject_token"], "caller-token");
        assert_eq!(form["requested_token_type"], ACCESS_TOKEN_TYPE);
        assert_eq!(
            form["scope"],
            "context-broker-archive context-broker-sensors"
        );
        assert_eq!(form["client_id"], "context-gateway");
        assert!(!form.contains_key("audience"), "{form:?}");
    }

    #[tokio::test]
    async fn a_redirect_from_the_token_endpoint_is_refused_and_the_form_goes_nowhere_else() {
        let (elsewhere, seen) = realm(
            StatusCode::OK,
            serde_json::json!({ "access_token": "stolen" }),
        )
        .await;
        let app = Router::new().route(
            "/token",
            post(move || {
                let elsewhere = elsewhere.clone();
                async move {
                    (
                        StatusCode::TEMPORARY_REDIRECT,
                        [(axum::http::header::LOCATION, elsewhere)],
                    )
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let url = format!("http://{}/token", listener.local_addr().expect("addr"));
        tokio::spawn(async move { axum::serve(listener, app).await });
        let refused = exchanger(url)
            .exchange("caller-token", &brokers(&["archive"]))
            .await
            .expect_err("a redirect is no token");
        assert_eq!(refused, ExchangeError::Refused(307));
        assert!(seen.lock().expect("lock").is_empty(), "the form was resent");
    }

    #[tokio::test]
    async fn a_refusal_is_a_502_and_a_realm_failure_a_503_and_neither_carries_a_token() {
        let (url, _) = realm(
            StatusCode::BAD_REQUEST,
            serde_json::json!({ "error": "invalid_request" }),
        )
        .await;
        let refused = exchanger(url)
            .exchange("caller-token", &brokers(&["archive"]))
            .await
            .expect_err("refused");
        assert_eq!(refused, ExchangeError::Refused(400));
        assert_eq!(refused.problem().status, 502);

        let (url, _) = realm(StatusCode::SERVICE_UNAVAILABLE, serde_json::json!({})).await;
        let down = exchanger(url)
            .exchange("caller-token", &brokers(&["archive"]))
            .await
            .expect_err("down");
        assert_eq!(down.problem().status, 503);
        assert!(down
            .problem()
            .type_uri
            .ends_with("federation-exchange-failed"));

        // Nobody listens on port 9.
        let gone = exchanger("http://127.0.0.1:9/token".to_owned())
            .exchange("caller-token", &brokers(&["archive"]))
            .await
            .expect_err("unreachable");
        assert!(matches!(gone, ExchangeError::Unavailable(_)));
        for error in [refused, down, gone] {
            let said = format!("{error} {:?} {:?}", error, error.problem());
            assert!(
                !said.contains("caller-token") && !said.contains("s3cret"),
                "{said}"
            );
        }
    }

    #[tokio::test]
    async fn an_answer_without_a_token_or_no_broker_at_all_is_no_token() {
        let (url, _) = realm(StatusCode::OK, serde_json::json!({ "access_token": "" })).await;
        let empty = exchanger(url.clone())
            .exchange("caller-token", &brokers(&["archive"]))
            .await;
        assert!(matches!(empty, Err(ExchangeError::Unavailable(_))));
        let none = exchanger(url).exchange("caller-token", &brokers(&[])).await;
        assert!(matches!(none, Err(ExchangeError::Unavailable(_))));
    }
}
