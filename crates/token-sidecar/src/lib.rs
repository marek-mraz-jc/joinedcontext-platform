//! The pipeline runner's token service (PL-19, T-1508, Architecture/12 §3).
//!
//! Every stream of the one runner pod asks this service for its token, as Bento's OAuth 2
//! `token_url`, naming its pipeline as the client id `{project}/{pipeline}`. The sidecar
//! asks the Kubernetes API for a short token of that pipeline's own ServiceAccount (TokenRequest,
//! audience the realm issuer), presents it to Keycloak as the client assertion of a
//! `client_credentials` request, and hands back Keycloak's answer as it came. It holds no secret
//! and stores no token: a token is minted per request and Bento keeps the access token until it
//! expires.
//!
//! The service runs in a pod of its own, so the runner never reaches the Kubernetes API. The
//! ceiling is the shared runner: anything running in it can ask for any of its pipelines'
//! tokens. What it buys is a principal per pipeline (PL-19, PL-20).

use axum::extract::State;
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Form, Router};
use base64::Engine;
use jc_core::kinds::pipeline_identity::kubernetes_service_account;
use jc_core::names::validate_dns1123_label;
use std::collections::HashMap;
use std::sync::Arc;

/// How long a minted Kubernetes token is worth, the shortest the API server accepts.
pub const TOKEN_SECONDS: u64 = 600;

/// Where the sidecar asks and what it asks for.
#[derive(Debug, Clone)]
pub struct Config {
    /// The Kubernetes API, `https://kubernetes.default.svc` in a pod.
    pub kubernetes_api: String,
    /// The namespace that holds every pipeline's ServiceAccount and nothing else.
    pub namespace: String,
    /// The file holding this service's own projected token, which TokenRequest is authorized by.
    pub own_token_file: std::path::PathBuf,
    /// The realm's token endpoint.
    pub token_url: String,
    /// The audience Keycloak's federated client authentication expects: the realm issuer.
    pub audience: String,
}

/// Where the service listens: `JC_SIDECAR_LISTEN`, `0.0.0.0:4180` when unset or blank. It runs
/// in a pod of its own and the runner calls it across the pod network, so any address is taken;
/// who may reach it is the NetworkPolicy's and the mesh's to say.
pub fn listen_address(value: Option<&str>) -> Result<std::net::SocketAddr, String> {
    let listen = value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("0.0.0.0:4180");
    listen
        .parse()
        .map_err(|_| format!("JC_SIDECAR_LISTEN {listen} is no address"))
}

/// The sidecar's state: its configuration and its HTTP client.
#[derive(Clone)]
pub struct Sidecar {
    config: Arc<Config>,
    http: reqwest::Client,
}

impl Sidecar {
    /// A sidecar over `config`, its client trusting `http`'s roots.
    pub fn new(config: Config, http: reqwest::Client) -> Self {
        Self {
            config: Arc::new(config),
            http,
        }
    }
}

/// `POST /token`, the OAuth 2 token endpoint Bento calls, and `GET /healthz` for the probes.
pub fn router(sidecar: Sidecar) -> Router {
    Router::new()
        .route("/token", post(token))
        .route("/healthz", axum::routing::get(|| async { "ok" }))
        .with_state(sidecar)
}

/// Why a request got no token, said to the stream without anything secret in it.
#[derive(Debug, thiserror::Error)]
pub enum Refusal {
    /// The client id is not `{project}/{pipeline}` of two DNS labels.
    #[error("the client id must be `{{project}}/{{pipeline}}`, both DNS-1123 labels")]
    ClientId,
    /// The runner's own token could not be read.
    #[error("the runner's own ServiceAccount token could not be read")]
    OwnToken,
    /// The Kubernetes API refused or did not answer the TokenRequest.
    #[error("the Kubernetes API gave no token for {0}: {1}")]
    Kubernetes(String, String),
    /// Keycloak did not answer.
    #[error("the token endpoint did not answer: {0}")]
    Keycloak(String),
}

impl IntoResponse for Refusal {
    fn into_response(self) -> Response {
        let status = match self {
            Self::ClientId => StatusCode::BAD_REQUEST,
            _ => StatusCode::BAD_GATEWAY,
        };
        let body = serde_json::json!({ "error": "invalid_request", "error_description": self.to_string() });
        (status, axum::Json(body)).into_response()
    }
}

/// The pipeline a request names: `client_id` from HTTP Basic, which Bento's client sends, else
/// from the form.
pub fn pipeline_of(
    headers: &HeaderMap,
    form: &HashMap<String, String>,
) -> Result<(String, String), Refusal> {
    let basic = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Basic "))
        .and_then(|encoded| {
            base64::engine::general_purpose::STANDARD
                .decode(encoded.trim())
                .ok()
        })
        .and_then(|bytes| String::from_utf8(bytes).ok())
        .and_then(|pair| pair.split_once(':').map(|(id, _)| percent_decoded(id)));
    let client_id = basic
        .or_else(|| form.get("client_id").cloned())
        .ok_or(Refusal::ClientId)?;
    let (project, pipeline) = client_id.split_once('/').ok_or(Refusal::ClientId)?;
    if validate_dns1123_label(project).is_err() || validate_dns1123_label(pipeline).is_err() {
        return Err(Refusal::ClientId);
    }
    Ok((project.to_owned(), pipeline.to_owned()))
}

/// RFC 6749 §2.3.1 form-encodes the client id inside Basic, so `/` arrives as `%2F` (Go's
/// `oauth2` package, which Bento uses, does exactly that).
pub fn percent_decoded(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        let escaped = bytes
            .get(index + 1..index + 3)
            .filter(|pair| pair.iter().all(u8::is_ascii_hexdigit))
            .and_then(|pair| u8::from_str_radix(std::str::from_utf8(pair).ok()?, 16).ok());
        match (bytes[index], escaped) {
            (b'%', Some(byte)) => {
                out.push(byte);
                index += 3;
            }
            (b'+', _) => {
                out.push(b' ');
                index += 1;
            }
            (byte, _) => {
                out.push(byte);
                index += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

async fn token(
    State(sidecar): State<Sidecar>,
    headers: HeaderMap,
    Form(form): Form<HashMap<String, String>>,
) -> Response {
    match exchange(&sidecar, &headers, &form).await {
        Ok(response) => response,
        Err(refusal) => {
            tracing::warn!(reason = %refusal, "a stream got no token");
            refusal.into_response()
        }
    }
}

async fn exchange(
    sidecar: &Sidecar,
    headers: &HeaderMap,
    form: &HashMap<String, String>,
) -> Result<Response, Refusal> {
    let (project, pipeline) = pipeline_of(headers, form)?;
    let account = kubernetes_service_account(&project, &pipeline);
    let config = &sidecar.config;
    // Read on every request: the kubelet rewrites the projected file before it expires.
    let own = std::fs::read_to_string(&config.own_token_file).map_err(|_| Refusal::OwnToken)?;
    let request = serde_json::json!({
        "apiVersion": "authentication.k8s.io/v1",
        "kind": "TokenRequest",
        "spec": { "audiences": [config.audience], "expirationSeconds": TOKEN_SECONDS },
    });
    let url = format!(
        "{}/api/v1/namespaces/{}/serviceaccounts/{account}/token",
        config.kubernetes_api.trim_end_matches('/'),
        config.namespace
    );
    let answer = sidecar
        .http
        .post(url)
        .bearer_auth(own.trim())
        .json(&request)
        .send()
        .await
        .map_err(|error| Refusal::Kubernetes(account.clone(), error.without_url().to_string()))?;
    if !answer.status().is_success() {
        return Err(Refusal::Kubernetes(
            account,
            format!("HTTP {}", answer.status().as_u16()),
        ));
    }
    let minted: serde_json::Value = answer
        .json()
        .await
        .map_err(|error| Refusal::Kubernetes(account.clone(), error.without_url().to_string()))?;
    let assertion = minted["status"]["token"].as_str().ok_or_else(|| {
        Refusal::Kubernetes(account.clone(), "the answer carried no token".to_owned())
    })?;

    // Keycloak finds the federated client by the assertion's subject, so no client id is sent:
    // with one it answers `client_id parameter does not match sub claim` (Architecture/12 §3).
    let mut params = vec![
        ("grant_type", "client_credentials"),
        (
            "client_assertion_type",
            "urn:ietf:params:oauth:client-assertion-type:jwt-bearer",
        ),
        ("client_assertion", assertion),
    ];
    if let Some(scope) = form.get("scope").filter(|scope| !scope.is_empty()) {
        params.push(("scope", scope.as_str()));
    }
    let answer = sidecar
        .http
        .post(&config.token_url)
        .form(&params)
        .send()
        .await
        .map_err(|error| Refusal::Keycloak(error.without_url().to_string()))?;
    let status = StatusCode::from_u16(answer.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    let body = answer
        .bytes()
        .await
        .map_err(|error| Refusal::Keycloak(error.without_url().to_string()))?;
    if !status.is_success() {
        tracing::warn!(
            project,
            pipeline,
            status = status.as_u16(),
            "Keycloak refused the pipeline's assertion"
        );
    }
    Ok((status, [(header::CONTENT_TYPE, "application/json")], body).into_response())
}
