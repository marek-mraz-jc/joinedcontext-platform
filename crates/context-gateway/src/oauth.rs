//! The stateless authorization server for anonymous MCP (ADR-N-052 §2.2, EP-129, T-4631).
//!
//! An assistant that connects only through OAuth 2.1 (Gemini's custom apps) registers, authorizes
//! and takes a token here, and the token it gets is worth exactly a call without one. Nothing is
//! stored and nobody is identified: the `client_id`, the code and both tokens are HS256 JWTs this
//! gateway signs with one key from a `secretRef`, so each request proves itself by what it
//! carries, and the gateway's own tokens are taken off a request in `admit` before anything
//! else reads it, so every handler, the rate limiter included, sees an anonymous caller.

use crate::app::Gateway;
use axum::body::Body;
use axum::extract::rejection::{FormRejection, JsonRejection, QueryRejection};
use axum::extract::{Form, Json, Query, Request, State};
use axum::http::header::{AUTHORIZATION, CACHE_CONTROL, CONTENT_TYPE, LOCATION};
use axum::http::{HeaderValue, Response, StatusCode};
use axum::middleware::Next;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::Router;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use jc_core::{ProblemDetails, Secret};
use jsonwebtoken::{Algorithm, DecodingKey, EncodingKey, Header, Validation};
use reqwest::Url;
use ring::rand::{SecureRandom, SystemRandom};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Arc;

/// How long a registration holds (ADR-N-052 §2.2.2).
const CLIENT_SECONDS: u64 = 365 * 24 * 3600;
/// How long a code holds (§2.2.3).
const CODE_SECONDS: u64 = 60;
/// How long an access token holds (§2.2.4).
const ACCESS_SECONDS: u64 = 3600;
/// How long a refresh token holds (§2.2.4).
const REFRESH_SECONDS: u64 = 30 * 24 * 3600;
/// The fewest bytes of key HMAC-SHA-256 is given (§2.2.6).
pub const MIN_KEY_BYTES: usize = 32;
/// The most redirect URIs one registration names, and the longest one, so a `client_id` stays a
/// size a query string carries.
const MAX_REDIRECT_URIS: usize = 8;
const MAX_REDIRECT_URI_LEN: usize = 512;

/// The allowlist when the deployment names none (ADR-N-052 §2.3).
pub const DEFAULT_REDIRECT_HOSTS: [&str; 7] = [
    "oauth-redirect.googleusercontent.com",
    "claude.ai",
    "claude.com",
    "chatgpt.com",
    "localhost",
    "127.0.0.1",
    "[::1]",
];

/// The hosts a command-line client listens on, the only ones reached over `http` and on any
/// port (RFC 8252 §7.3).
const LOOPBACK: [&str; 3] = ["localhost", "127.0.0.1", "[::1]"];

/// What each signed value is, so none passes for another (a code is no access token).
const CLIENT: &str = "client";
const CODE: &str = "code";
const ACCESS: &str = "access";
const REFRESH: &str = "refresh";

/// The authorization server: its key, its issuer and the hosts it redirects to.
pub struct Issuer {
    issuer: String,
    encoding: EncodingKey,
    decoding: DecodingKey,
    redirect_hosts: Vec<String>,
    random: SystemRandom,
}

impl std::fmt::Debug for Issuer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Issuer")
            .field("issuer", &self.issuer)
            .field("key", &"<redacted>")
            .field("redirect_hosts", &self.redirect_hosts)
            .finish()
    }
}

/// Why an `Issuer` was not made.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum KeyError {
    /// Shorter than [`MIN_KEY_BYTES`].
    #[error("the OAuth signing key needs at least {MIN_KEY_BYTES} bytes")]
    Short,
}

/// A signed value's claims; each kind fills the fields it needs.
#[derive(Debug, Default, Serialize, Deserialize)]
struct Claims {
    #[serde(rename = "use")]
    kind: String,
    exp: u64,
    iat: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    iss: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    aud: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    jti: Option<String>,
    /// A registration's redirect URIs.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    redirect_uris: Vec<String>,
    /// The registration a code or refresh token belongs to: the SHA-256 of its `client_id`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    cid: Option<String>,
    /// A code's redirect URI and PKCE challenge.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    redirect_uri: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    challenge: Option<String>,
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

fn sha256(text: &str) -> String {
    URL_SAFE_NO_PAD.encode(ring::digest::digest(&ring::digest::SHA256, text.as_bytes()))
}

/// RFC 7636 §4.1: 43 to 128 unreserved characters, a verifier's and an S256 challenge's shape.
fn pkce_shaped(text: &str) -> bool {
    (43..=128).contains(&text.len())
        && text
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~'))
}

impl Issuer {
    /// An authorization server named `issuer` (the gateway's public URL), signing with `key`.
    pub fn new(issuer: &str, key: &Secret, redirect_hosts: Vec<String>) -> Result<Self, KeyError> {
        let bytes = key.expose().as_bytes();
        if bytes.len() < MIN_KEY_BYTES {
            return Err(KeyError::Short);
        }
        Ok(Self {
            issuer: issuer.trim_end_matches('/').to_owned(),
            encoding: EncodingKey::from_secret(bytes),
            decoding: DecodingKey::from_secret(bytes),
            redirect_hosts: redirect_hosts
                .into_iter()
                .map(|host| host.trim().to_ascii_lowercase())
                .filter(|host| !host.is_empty())
                .collect(),
            random: SystemRandom::new(),
        })
    }

    /// Its issuer URL.
    pub fn issuer(&self) -> &str {
        &self.issuer
    }

    fn sign(&self, claims: &Claims) -> Option<String> {
        jsonwebtoken::encode(&Header::new(Algorithm::HS256), claims, &self.encoding).ok()
    }

    /// The claims of `token` when this key signed it as a `kind` and it has not expired.
    fn open(&self, token: &str, kind: &str) -> Option<Claims> {
        let mut validation = Validation::new(Algorithm::HS256);
        validation.leeway = 0;
        validation.validate_aud = false;
        validation.set_required_spec_claims(&["exp"]);
        let claims = jsonwebtoken::decode::<Claims>(token, &self.decoding, &validation)
            .ok()?
            .claims;
        (claims.kind == kind).then_some(claims)
    }

    fn jti(&self) -> Option<String> {
        let mut bytes = [0u8; 16];
        self.random.fill(&mut bytes).ok()?;
        Some(URL_SAFE_NO_PAD.encode(bytes))
    }

    /// Whether a redirect URI is on the allowlist: `https` on a listed host and its default port,
    /// or `http` on a listed loopback host and any port; never a userinfo part or a fragment
    /// (ADR-N-052 §2.3).
    pub fn redirect_allowed(&self, uri: &str) -> bool {
        if uri.len() > MAX_REDIRECT_URI_LEN {
            return false;
        }
        let Ok(url) = Url::parse(uri) else {
            return false;
        };
        let Some(host) = url.host_str().map(str::to_ascii_lowercase) else {
            return false;
        };
        if !url.username().is_empty() || url.password().is_some() || url.fragment().is_some() {
            return false;
        }
        if !self.redirect_hosts.contains(&host) {
            return false;
        }
        if LOOPBACK.contains(&host.as_str()) {
            url.scheme() == "http"
        } else {
            url.scheme() == "https" && url.port().is_none()
        }
    }

    /// A `client_id` for `redirect_uris`, or the reason none is issued.
    fn register(&self, redirect_uris: Vec<String>) -> Result<(String, u64), &'static str> {
        if redirect_uris.is_empty() || redirect_uris.len() > MAX_REDIRECT_URIS {
            return Err("name between one and eight redirect_uris");
        }
        if !redirect_uris.iter().all(|uri| self.redirect_allowed(uri)) {
            return Err("a redirect URI is not on this server's allowlist");
        }
        let iat = now();
        let claims = Claims {
            kind: CLIENT.into(),
            iat,
            exp: iat + CLIENT_SECONDS,
            redirect_uris,
            ..Claims::default()
        };
        self.sign(&claims).map(|id| (id, iat)).ok_or("cannot sign")
    }

    /// A code for an authorize request, or the sentence the error page says.
    fn authorize(&self, request: &HashMap<String, String>) -> Result<String, String> {
        let field = |name: &str| {
            request
                .get(name)
                .map(String::as_str)
                .filter(|v| !v.is_empty())
        };
        let client_id = field("client_id").ok_or("client_id is missing")?;
        let client = self
            .open(client_id, CLIENT)
            .ok_or("the client_id is not one this server issued, or it expired; register again")?;
        let redirect_uri = match (field("redirect_uri"), client.redirect_uris.as_slice()) {
            (Some(uri), _) => uri.to_owned(),
            // RFC 6749 §3.1.2.3: one registered URI may be left out.
            (None, [only]) => only.clone(),
            (None, _) => return Err("redirect_uri is missing".into()),
        };
        if !client.redirect_uris.contains(&redirect_uri) {
            return Err("the redirect_uri is not one this client registered".into());
        }
        if field("response_type") != Some("code") {
            return Err("response_type must be code".into());
        }
        if field("code_challenge_method") != Some("S256") {
            return Err("PKCE with code_challenge_method=S256 is required".into());
        }
        let challenge = field("code_challenge")
            .filter(|challenge| challenge.len() == 43 && pkce_shaped(challenge))
            .ok_or("code_challenge must be an S256 challenge")?;
        let resource = self.resource(field("resource"))?;
        let iat = now();
        let claims = Claims {
            kind: CODE.into(),
            iat,
            exp: iat + CODE_SECONDS,
            aud: Some(resource),
            cid: Some(sha256(client_id)),
            redirect_uri: Some(redirect_uri),
            challenge: Some(challenge.to_owned()),
            ..Claims::default()
        };
        self.sign(&claims).ok_or_else(|| "cannot sign".into())
    }

    /// The resource a token is for: one on this host, or the whole host when none is named
    /// (RFC 8707).
    fn resource(&self, named: Option<&str>) -> Result<String, String> {
        match named.map(|resource| resource.trim_end_matches('/')) {
            None => Ok(self.issuer.clone()),
            Some(resource)
                if resource == self.issuer
                    || resource.starts_with(&format!("{}/", self.issuer)) =>
            {
                Ok(resource.to_owned())
            }
            Some(_) => Err("the resource is not on this server".into()),
        }
    }

    /// The token answer for a token request, or its RFC 6749 §5.2 error.
    fn token(
        &self,
        request: &HashMap<String, String>,
    ) -> Result<Value, (&'static str, &'static str)> {
        let field = |name: &str| {
            request
                .get(name)
                .map(String::as_str)
                .filter(|v| !v.is_empty())
        };
        let client_id = field("client_id").ok_or(("invalid_request", "client_id is missing"))?;
        if self.open(client_id, CLIENT).is_none() {
            return Err((
                "invalid_client",
                "the client_id is not one this server issued, or it expired",
            ));
        }
        let cid = sha256(client_id);
        let audience = match field("grant_type") {
            Some("authorization_code") => {
                let code = field("code")
                    .and_then(|code| self.open(code, CODE))
                    .ok_or(("invalid_grant", "the code is not valid, or it expired"))?;
                if code.cid.as_deref() != Some(&cid) {
                    return Err(("invalid_grant", "the code was issued to another client"));
                }
                if code.redirect_uri.as_deref() != field("redirect_uri") {
                    return Err(("invalid_grant", "the redirect_uri is not the code's"));
                }
                let verifier = field("code_verifier")
                    .filter(|verifier| pkce_shaped(verifier))
                    .ok_or(("invalid_grant", "code_verifier is missing or malformed"))?;
                // The challenge is public, it travelled in the authorize URL; the signatures, the
                // secret-keyed comparisons, are jsonwebtoken's constant-time ones.
                if code.challenge.as_deref() != Some(sha256(verifier).as_str()) {
                    return Err((
                        "invalid_grant",
                        "the code_verifier does not match the challenge",
                    ));
                }
                if let Some(resource) = field("resource") {
                    if code.aud.as_deref() != Some(resource.trim_end_matches('/')) {
                        return Err(("invalid_target", "the resource is not the code's"));
                    }
                }
                code.aud
            }
            Some("refresh_token") => {
                let refresh = field("refresh_token")
                    .and_then(|token| self.open(token, REFRESH))
                    .filter(|refresh| refresh.iss.as_deref() == Some(self.issuer.as_str()))
                    .ok_or((
                        "invalid_grant",
                        "the refresh token is not valid, or it expired",
                    ))?;
                if refresh.cid.as_deref() != Some(&cid) {
                    return Err((
                        "invalid_grant",
                        "the refresh token was issued to another client",
                    ));
                }
                refresh.aud
            }
            _ => {
                return Err((
                    "unsupported_grant_type",
                    "grant_type must be authorization_code or refresh_token",
                ))
            }
        };
        let audience = audience.ok_or(("invalid_grant", "the grant names no resource"))?;
        let iat = now();
        let token = |kind: &str, seconds: u64, cid: Option<String>| {
            self.sign(&Claims {
                kind: kind.into(),
                iat,
                exp: iat + seconds,
                iss: Some(self.issuer.clone()),
                aud: Some(audience.clone()),
                jti: self.jti(),
                cid,
                ..Claims::default()
            })
        };
        let (Some(access), Some(refresh)) = (
            token(ACCESS, ACCESS_SECONDS, None),
            token(REFRESH, REFRESH_SECONDS, Some(cid)),
        ) else {
            return Err(("server_error", "cannot sign"));
        };
        Ok(json!({
            "access_token": access,
            "token_type": "Bearer",
            "expires_in": ACCESS_SECONDS,
            "refresh_token": refresh,
        }))
    }

    /// Whether `token` is an access token of this server for `url`, the resource it was issued
    /// for or one under it.
    fn admits(&self, token: &str, url: &str) -> bool {
        self.open(token, ACCESS)
            .filter(|claims| claims.iss.as_deref() == Some(self.issuer.as_str()))
            .and_then(|claims| claims.aud)
            .is_some_and(|aud| url == aud || url.starts_with(&format!("{aud}/")))
    }
}

/// Whether a bearer token is one an `Issuer` signs rather than the realm's: HS256, which no realm
/// key is.
fn is_ours(token: &str) -> bool {
    jsonwebtoken::decode_header(token).is_ok_and(|header| header.alg == Algorithm::HS256)
}

/// Takes this server's access token off a request, so the request is anonymous from here on,
/// the rate limiter's bucket included (ADR-N-052 §2.2.5). A token of this server that is
/// expired, forged or for another resource is `401`; a realm token passes untouched.
pub(crate) async fn admit(
    State(gateway): State<Arc<Gateway>>,
    mut request: Request,
    next: Next,
) -> Response<Body> {
    if let Some(issuer) = &gateway.oauth {
        let presented = request
            .headers()
            .get(AUTHORIZATION)
            .and_then(|value| value.to_str().ok());
        if let Ok(raw) = crate::auth::token::bearer(presented) {
            if is_ours(raw) {
                let url = format!("{}{}", issuer.issuer(), request.uri().path());
                if !issuer.admits(raw, &url) {
                    return ProblemDetails::unauthorized().into_response();
                }
                request.headers_mut().remove(AUTHORIZATION);
            }
        }
    }
    next.run(request).await
}

/// The authorization servers a resource's metadata names (RFC 9728): this gateway's first when an
/// anonymous caller reads the resource and the deployment serves one, then the realm's (ADR-N-052
/// §2.2.1). A resource no anonymous caller reads names the realm alone, as before.
pub(crate) fn authorization_servers(gateway: &Gateway, anonymous_readable: bool) -> Vec<String> {
    gateway
        .oauth
        .as_ref()
        .filter(|_| anonymous_readable)
        .map(|issuer| issuer.issuer().to_owned())
        .into_iter()
        .chain(
            gateway
                .verifier
                .as_ref()
                .map(|verifier| verifier.issuer().to_owned()),
        )
        .collect()
}

/// Its routes, merged by `app::router` when the deployment configures a key.
pub(crate) fn routes() -> Router<Arc<Gateway>> {
    Router::new()
        .route("/.well-known/oauth-authorization-server", get(metadata))
        .route("/oauth/register", post(register))
        .route("/oauth/authorize", get(authorize))
        .route("/oauth/token", post(token))
}

fn issuer_of(gateway: &Gateway) -> Option<&Issuer> {
    gateway.oauth.as_deref()
}

fn no_store(status: StatusCode, body: &Value) -> Response<Body> {
    let mut response = (status, Json(body)).into_response();
    response
        .headers_mut()
        .insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

fn error(status: StatusCode, code: &str, description: &str) -> Response<Body> {
    no_store(
        status,
        &json!({ "error": code, "error_description": description }),
    )
}

/// RFC 8414 server metadata.
async fn metadata(State(gateway): State<Arc<Gateway>>) -> Response<Body> {
    let Some(issuer) = issuer_of(&gateway) else {
        return ProblemDetails::not_found().into_response();
    };
    let base = issuer.issuer();
    Json(json!({
        "issuer": base,
        "authorization_endpoint": format!("{base}/oauth/authorize"),
        "token_endpoint": format!("{base}/oauth/token"),
        "registration_endpoint": format!("{base}/oauth/register"),
        "response_types_supported": ["code"],
        "grant_types_supported": ["authorization_code", "refresh_token"],
        "code_challenge_methods_supported": ["S256"],
        "token_endpoint_auth_methods_supported": ["none"],
    }))
    .into_response()
}

/// RFC 7591 registration. The client's other metadata is read past, as §2 allows; only its
/// redirect URIs are kept, inside the `client_id`.
async fn register(
    State(gateway): State<Arc<Gateway>>,
    body: Result<Json<Value>, JsonRejection>,
) -> Response<Body> {
    let Some(issuer) = issuer_of(&gateway) else {
        return ProblemDetails::not_found().into_response();
    };
    let Ok(Json(body)) = body else {
        return error(
            StatusCode::BAD_REQUEST,
            "invalid_client_metadata",
            "the body is not a JSON object",
        );
    };
    let uris: Option<Vec<String>> = body
        .get("redirect_uris")
        .and_then(Value::as_array)
        .and_then(|uris| {
            uris.iter()
                .map(|uri| uri.as_str().map(str::to_owned))
                .collect()
        });
    match issuer.register(uris.unwrap_or_default()) {
        Ok((client_id, issued)) => {
            let mut answer = json!({
                "client_id": client_id,
                "client_id_issued_at": issued,
                "redirect_uris": body["redirect_uris"],
                "token_endpoint_auth_method": "none",
                "grant_types": ["authorization_code", "refresh_token"],
                "response_types": ["code"],
            });
            if let Some(name) = body.get("client_name").and_then(Value::as_str) {
                answer["client_name"] = json!(name.chars().take(200).collect::<String>());
            }
            no_store(StatusCode::CREATED, &answer)
        }
        Err(reason) => error(StatusCode::BAD_REQUEST, "invalid_redirect_uri", reason),
    }
}

/// The page a failed authorize step answers on this host: it never redirects, so it cannot be an
/// open redirect (ADR-N-052 §2.2.3).
fn refusal_page(reason: &str) -> Response<Body> {
    let mut response = Response::new(Body::from(format!("The request was refused: {reason}.\n")));
    *response.status_mut() = StatusCode::BAD_REQUEST;
    response.headers_mut().insert(
        CONTENT_TYPE,
        HeaderValue::from_static("text/plain; charset=utf-8"),
    );
    response
        .headers_mut()
        .insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

/// The authorize step: no login and no consent, a code at once.
async fn authorize(
    State(gateway): State<Arc<Gateway>>,
    query: Result<Query<HashMap<String, String>>, QueryRejection>,
) -> Response<Body> {
    let Some(issuer) = issuer_of(&gateway) else {
        return ProblemDetails::not_found().into_response();
    };
    let Ok(Query(query)) = query else {
        return refusal_page("the query is malformed");
    };
    let code = match issuer.authorize(&query) {
        Ok(code) => code,
        Err(reason) => return refusal_page(&reason),
    };
    // `authorize` checked the URI against the registration, which the allowlist admitted.
    let Some(mut target) = query
        .get("redirect_uri")
        .filter(|uri| !uri.is_empty())
        .and_then(|uri| Url::parse(uri).ok())
        .or_else(|| {
            let client = issuer.open(query.get("client_id")?, CLIENT)?;
            Url::parse(client.redirect_uris.first()?).ok()
        })
    else {
        return refusal_page("the redirect_uri is malformed");
    };
    {
        let mut pairs = target.query_pairs_mut();
        pairs.append_pair("code", &code);
        if let Some(state) = query.get("state") {
            pairs.append_pair("state", state);
        }
        pairs.append_pair("iss", issuer.issuer());
    }
    let Ok(location) = HeaderValue::from_str(target.as_str()) else {
        return refusal_page("the redirect_uri is malformed");
    };
    let mut response = Response::new(Body::empty());
    *response.status_mut() = StatusCode::FOUND;
    response.headers_mut().insert(LOCATION, location);
    response
        .headers_mut()
        .insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

/// The token endpoint, for a public client: no secret, PKCE instead.
async fn token(
    State(gateway): State<Arc<Gateway>>,
    form: Result<Form<HashMap<String, String>>, FormRejection>,
) -> Response<Body> {
    let Some(issuer) = issuer_of(&gateway) else {
        return ProblemDetails::not_found().into_response();
    };
    let Ok(Form(form)) = form else {
        return error(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "the body is not a form",
        );
    };
    match issuer.token(&form) {
        Ok(answer) => no_store(StatusCode::OK, &answer),
        Err((code, description)) => {
            let status = if code == "invalid_client" {
                StatusCode::UNAUTHORIZED
            } else {
                StatusCode::BAD_REQUEST
            };
            error(status, code, description)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASE: &str = "https://api.dev.example";
    const GEMINI: &str = "https://oauth-redirect.googleusercontent.com/oauth2/callback";
    const VERIFIER: &str = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";

    fn issuer() -> Issuer {
        let hosts = DEFAULT_REDIRECT_HOSTS
            .iter()
            .map(|host| (*host).to_owned())
            .collect();
        Issuer::new(BASE, &Secret::new("k".repeat(32)), hosts).expect("a long enough key")
    }

    fn form(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    fn client(issuer: &Issuer) -> String {
        issuer
            .register(vec![GEMINI.into()])
            .expect("an allowed redirect")
            .0
    }

    fn code(issuer: &Issuer, client_id: &str) -> String {
        issuer
            .authorize(&form(&[
                ("client_id", client_id),
                ("redirect_uri", GEMINI),
                ("response_type", "code"),
                ("code_challenge", &sha256(VERIFIER)),
                ("code_challenge_method", "S256"),
                ("resource", &format!("{BASE}/api/mcp")),
            ]))
            .expect("a valid authorize request")
    }

    fn exchange(
        issuer: &Issuer,
        client_id: &str,
        code: &str,
        verifier: &str,
    ) -> Result<Value, (&'static str, &'static str)> {
        issuer.token(&form(&[
            ("grant_type", "authorization_code"),
            ("client_id", client_id),
            ("code", code),
            ("redirect_uri", GEMINI),
            ("code_verifier", verifier),
        ]))
    }

    #[test]
    fn a_short_key_makes_no_server() {
        let short = Issuer::new(BASE, &Secret::new("k".repeat(31)), Vec::new());
        assert_eq!(short.err(), Some(KeyError::Short));
    }

    #[test]
    fn the_allowlist_takes_listed_hosts_and_loopback_over_http_only() {
        let issuer = issuer();
        for allowed in [
            GEMINI,
            "https://claude.ai/api/mcp/auth_callback",
            "http://localhost:8976/cb",
            "http://127.0.0.1:1/x",
            "http://[::1]:9/x",
        ] {
            assert!(issuer.redirect_allowed(allowed), "{allowed}");
        }
        for refused in [
            "https://evil.example/cb",
            "https://claude.ai.evil.example/cb",
            "http://claude.ai/cb",
            "https://claude.ai:8443/cb",
            "https://localhost/cb",
            "https://user@claude.ai/cb",
            "https://claude.ai/cb#frag",
            "javascript:alert(1)",
            "claude.ai/cb",
            "",
        ] {
            assert!(!issuer.redirect_allowed(refused), "{refused}");
        }
        assert_eq!(
            issuer
                .register(vec!["https://evil.example/cb".into()])
                .err(),
            Some("a redirect URI is not on this server's allowlist")
        );
        assert!(issuer.register(Vec::new()).is_err());
    }

    #[test]
    fn the_whole_flow_issues_an_anonymous_token_for_the_resource() {
        let issuer = issuer();
        let client_id = client(&issuer);
        let answer =
            exchange(&issuer, &client_id, &code(&issuer, &client_id), VERIFIER).expect("a token");
        let access = answer["access_token"].as_str().expect("an access token");
        assert!(issuer.admits(access, &format!("{BASE}/api/mcp")));
        assert!(
            !issuer.admits(access, &format!("{BASE}/api/endpoint/x/mcp")),
            "another resource"
        );
        assert!(!issuer.admits(access, &format!("{BASE}/api/mcpx")));
        let claims = issuer.open(access, ACCESS).expect("its own token");
        assert!(claims.exp - claims.iat <= 3600 && claims.jti.is_some());
        let raw = access.split('.').nth(1).expect("a JWT");
        let payload: Value =
            serde_json::from_slice(&URL_SAFE_NO_PAD.decode(raw).expect("base64url")).expect("JSON");
        for named in [
            "sub",
            "name",
            "email",
            "groups",
            "preferred_username",
            "azp",
        ] {
            assert!(payload.get(named).is_none(), "{named}");
        }
        // A refresh answers a new pair for the same resource.
        let refreshed = issuer
            .token(&form(&[
                ("grant_type", "refresh_token"),
                ("client_id", &client_id),
                (
                    "refresh_token",
                    answer["refresh_token"].as_str().expect("a refresh token"),
                ),
            ]))
            .expect("a new pair");
        assert!(issuer.admits(
            refreshed["access_token"].as_str().expect("an access token"),
            &format!("{BASE}/api/mcp")
        ));
        // A code or refresh token is not an access token.
        assert!(!issuer.admits(
            answer["refresh_token"].as_str().expect("refresh"),
            &format!("{BASE}/api/mcp")
        ));
    }

    #[test]
    fn each_authorize_refusal_names_its_reason() {
        let issuer = issuer();
        let client_id = client(&issuer);
        let challenge = sha256(VERIFIER);
        let good = [
            ("client_id", client_id.as_str()),
            ("redirect_uri", GEMINI),
            ("response_type", "code"),
            ("code_challenge", challenge.as_str()),
            ("code_challenge_method", "S256"),
        ];
        let with = |name: &str, value: &str| {
            let mut request = form(&good);
            request.insert(name.into(), value.into());
            issuer.authorize(&request)
        };
        let without = |name: &str| {
            let mut request = form(&good);
            request.remove(name);
            issuer.authorize(&request)
        };
        assert!(issuer.authorize(&form(&good)).is_ok());
        assert!(
            with("code_challenge_method", "plain").is_err(),
            "plain PKCE"
        );
        assert!(without("code_challenge").is_err(), "no PKCE");
        assert!(without("code_challenge_method").is_err(), "no PKCE method");
        assert!(with("code_challenge", "short").is_err());
        assert!(
            with("redirect_uri", "https://claude.ai/cb").is_err(),
            "a URI the client did not register"
        );
        assert!(with("response_type", "token").is_err());
        assert!(with("resource", "https://elsewhere.example/api/mcp").is_err());
        let mut tampered = client_id.clone();
        tampered.replace_range(tampered.len() - 2.., "AA");
        assert!(
            with("client_id", &tampered).is_err(),
            "a tampered client_id"
        );
        // A client_id signed with another key is not this server's.
        let other = Issuer::new(
            BASE,
            &Secret::new("o".repeat(32)),
            vec!["oauth-redirect.googleusercontent.com".into()],
        )
        .expect("a key");
        assert!(with("client_id", &client(&other)).is_err());
        // One registered URI may be left out.
        assert!(without("redirect_uri").is_ok());
    }

    #[test]
    fn each_token_refusal_is_an_oauth_error() {
        let issuer = issuer();
        let client_id = client(&issuer);
        let code = code(&issuer, &client_id);
        assert_eq!(
            exchange(&issuer, &client_id, &code, &"x".repeat(43))
                .err()
                .map(|e| e.0),
            Some("invalid_grant"),
            "a wrong verifier"
        );
        assert_eq!(
            exchange(&issuer, &client_id, &code, "short")
                .err()
                .map(|e| e.0),
            Some("invalid_grant")
        );
        // The same URIs in the same second are the same registration; another client names others.
        let other_client = issuer
            .register(vec!["https://claude.ai/cb".into()])
            .expect("allowed")
            .0;
        assert_eq!(
            exchange(&issuer, &other_client, &code, VERIFIER)
                .err()
                .map(|e| e.0),
            Some("invalid_grant"),
            "another client's code"
        );
        assert_eq!(
            exchange(&issuer, "nobody", &code, VERIFIER)
                .err()
                .map(|e| e.0),
            Some("invalid_client")
        );
        let mut wrong_uri = form(&[
            ("grant_type", "authorization_code"),
            ("client_id", &client_id),
            ("code", &code),
            ("code_verifier", VERIFIER),
        ]);
        wrong_uri.insert("redirect_uri".into(), "https://claude.ai/cb".into());
        assert_eq!(
            issuer.token(&wrong_uri).err().map(|e| e.0),
            Some("invalid_grant")
        );
        assert_eq!(
            issuer
                .token(&form(&[
                    ("grant_type", "password"),
                    ("client_id", &client_id)
                ]))
                .err()
                .map(|e| e.0),
            Some("unsupported_grant_type")
        );
        // A code is spent by nobody: with nothing stored it works again inside its minute (§2.2.4).
        assert!(exchange(&issuer, &client_id, &code, VERIFIER).is_ok());
        assert!(exchange(&issuer, &client_id, &code, VERIFIER).is_ok());
    }

    #[test]
    fn an_expired_code_or_token_is_refused() {
        let issuer = issuer();
        let client_id = client(&issuer);
        let past = now() - 10;
        let expired = |kind: &str, aud: &str| {
            issuer
                .sign(&Claims {
                    kind: kind.into(),
                    iat: past - CODE_SECONDS,
                    exp: past,
                    iss: Some(BASE.into()),
                    aud: Some(aud.into()),
                    cid: Some(sha256(&client_id)),
                    redirect_uri: Some(GEMINI.into()),
                    challenge: Some(sha256(VERIFIER)),
                    ..Claims::default()
                })
                .expect("signed")
        };
        let code = expired(CODE, &format!("{BASE}/api/mcp"));
        assert_eq!(
            exchange(&issuer, &client_id, &code, VERIFIER)
                .err()
                .map(|e| e.0),
            Some("invalid_grant")
        );
        assert!(!issuer.admits(&expired(ACCESS, BASE), &format!("{BASE}/api/mcp")));
    }

    #[test]
    fn only_this_servers_tokens_are_taken_off_a_request() {
        let issuer = issuer();
        let client_id = client(&issuer);
        let answer =
            exchange(&issuer, &client_id, &code(&issuer, &client_id), VERIFIER).expect("a token");
        assert!(is_ours(answer["access_token"].as_str().expect("token")));
        // A realm token is RS256 and passes to the realm's verifier untouched.
        let rs256 = format!(
            "{}.e30.sig",
            URL_SAFE_NO_PAD.encode(r#"{"alg":"RS256","typ":"JWT"}"#)
        );
        assert!(!is_ours(&rs256) && !is_ours("not a jwt"));
    }
}
