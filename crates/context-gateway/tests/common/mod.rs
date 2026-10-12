//! A throwaway realm, so no signing key is ever written into the repository.
//!
//! One P-256 key is generated per test run and published as a JWKS exactly the way
//! Keycloak publishes one, which means the verifier is exercised through the path it uses
//! in production rather than through a back door built for the test.

#![allow(dead_code)]

use axum::response::IntoResponse;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use context_gateway::auth::token::Verifier;
use jsonwebtoken::jwk::JwkSet;
use jsonwebtoken::{encode, Algorithm, EncodingKey, Header};
use ring::rand::SystemRandom;
use ring::signature::{EcdsaKeyPair, KeyPair, ECDSA_P256_SHA256_FIXED_SIGNING};
use serde_json::{json, Value};

/// The realm the gateway is configured with in every test.
pub const ISSUER: &str = "https://2.28.67.127.sslip.io/realms/joinedcontext";
/// The key id the realm publishes.
pub const KID: &str = "realm-key-1";

/// A realm: one signing key, its JWKS, and the ability to mint a token.
pub struct Realm {
    signing: EncodingKey,
    jwks: JwkSet,
}

impl Realm {
    pub fn new() -> Self {
        let rng = SystemRandom::new();
        let pkcs8 = EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, &rng)
            .expect("a key pair");
        let pair = EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, pkcs8.as_ref(), &rng)
            .expect("the pair round-trips");

        // An uncompressed P-256 point: 0x04 || x(32) || y(32), which is exactly what a
        // JWK's `x` and `y` carry, base64url without padding.
        let point = pair.public_key().as_ref();
        assert_eq!(point.len(), 65, "an uncompressed P-256 point");
        let jwks = serde_json::from_value(json!({
            "keys": [{
                "kty": "EC",
                "crv": "P-256",
                "alg": "ES256",
                "use": "sig",
                "kid": KID,
                "x": URL_SAFE_NO_PAD.encode(&point[1..33]),
                "y": URL_SAFE_NO_PAD.encode(&point[33..]),
            }]
        }))
        .expect("a JWKS");

        Self {
            signing: EncodingKey::from_ec_der(pkcs8.as_ref()),
            jwks,
        }
    }

    /// The key material this realm publishes, which is exactly what an attacker holds: the
    /// JWKS is world readable, so every forgery below is built from it and from nothing else.
    pub fn published_jwks(&self) -> &JwkSet {
        &self.jwks
    }

    /// A verifier that trusts this realm and nothing else.
    pub fn verifier(&self) -> Verifier {
        let verifier = Verifier::new(ISSUER);
        assert_eq!(verifier.replace_keys(&self.jwks), 1, "one usable key");
        verifier
    }

    /// Signs any claim set, so a test can mint the malformed ones too.
    pub fn mint(&self, claims: &Value) -> String {
        let mut header = Header::new(Algorithm::ES256);
        header.kid = Some(KID.to_owned());
        encode(&header, claims, &self.signing).expect("the realm signs")
    }

    /// Signs with a key id the realm does not publish.
    pub fn mint_with_kid(&self, kid: &str, claims: &Value) -> String {
        let mut header = Header::new(Algorithm::ES256);
        header.kid = Some(kid.to_owned());
        encode(&header, claims, &self.signing).expect("the realm signs")
    }

    /// Signs without naming a key at all.
    pub fn mint_without_kid(&self, claims: &Value) -> String {
        encode(&Header::new(Algorithm::ES256), claims, &self.signing).expect("the realm signs")
    }

    /// The `client_credentials` token of one workload, bound to one resource.
    pub fn workload_token(&self, azp: &str, audience: Value) -> String {
        self.mint(&json!({
            "iss": ISSUER,
            "sub": format!("service-account-{azp}"),
            "aud": audience,
            "azp": azp,
            "exp": in_seconds(300),
            "iat": in_seconds(-10),
        }))
    }
}

/// A Unix timestamp `offset` seconds from now.
pub fn in_seconds(offset: i64) -> i64 {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("after 1970")
        .as_secs() as i64;
    now + offset
}

/// Base64url without padding, for the tokens a test forges by hand.
pub fn b64(raw: &str) -> String {
    URL_SAFE_NO_PAD.encode(raw)
}

/// What the broker was asked for on one hop.
#[derive(Debug, Clone, Default)]
pub struct Hop {
    /// The path, which must be the plain CIM 009 tree with no surface prefix left on it; a hop
    /// to a view root (`/x/views/{view}/ngsi-ld/v1/...`) is recorded under the plain tree.
    pub path: String,
    /// The view a hop went through, empty for the broker's plain root (T-3643).
    pub view: String,
    /// The query string as the gateway rewrote it.
    pub query: String,
    /// The tenant the gateway pinned (GW25).
    pub tenant: String,
    /// Whether any value the client forged survived the hop.
    pub forged: bool,
    /// The `Accept` the gateway asked with.
    pub accept: String,
    /// The request body, for the POST forms of the two query operations: CIM 009 puts their
    /// selector in the body, so a test that cannot read it cannot see what was asked (AG-84).
    pub body: Vec<u8>,
    /// The W3C Trace Context the gateway relayed: `traceparent` and `tracestate` (OPS-17).
    pub trace: (String, String),
    /// Every header the hop carried, for the tests of what the gateway lets through (T-3641).
    pub headers: axum::http::HeaderMap,
}

/// A broker on a real socket, because the gateway forwards over HTTP and what these tests
/// assert is what comes out of the other end.
///
/// `pages` are answered in order and the last one repeats, so a test can hand back one
/// full page followed by a short one and watch the gateway page through them (EP-44).
///
/// It refuses an unselected entity query the way CIM 009 5.7.2 requires, with the same
/// problem document a real broker sends. A stub that answered one would be more permissive
/// than the thing it stands in for, and a gateway bug that only a real broker catches is a
/// bug that reaches the cluster (T-0379).
pub struct BrokerStub {
    /// The base URL to build a [`context_gateway::proxy::Broker`] from.
    pub url: String,
    /// Every hop the gateway made, in order.
    pub hops: std::sync::Arc<std::sync::Mutex<Vec<Hop>>>,
}

impl BrokerStub {
    /// Starts the stub and answers `pages` in order.
    pub async fn start(pages: Vec<Value>) -> Self {
        Self::serve(pages, None).await
    }

    /// Starts the stub as the broker's narrowing view `view` (T-3651): a hop to that view is
    /// answered with `pages` narrowed by the rules of the roles the gateway pinned, as the
    /// engine narrows them; see [`View`].
    pub async fn narrowing(pages: Vec<Value>, view: View) -> Self {
        Self::serve(pages, Some(view)).await
    }

    async fn serve(pages: Vec<Value>, view: Option<View>) -> Self {
        let hops = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let served = std::sync::Arc::new(std::sync::Mutex::new(pages));
        let recorder = std::sync::Arc::clone(&hops);

        let app = axum::Router::new().fallback(axum::routing::any(
            move |request: axum::extract::Request| {
                let (recorder, served) = (
                    std::sync::Arc::clone(&recorder),
                    std::sync::Arc::clone(&served),
                );
                async move {
                    let headers = request.headers().clone();
                    let method = request.method().clone();
                    let (view, path) = in_view(request.uri().path());
                    let query = request.uri().query().unwrap_or_default().to_owned();
                    let body = axum::body::to_bytes(request.into_body(), 4 * 1024 * 1024)
                        .await
                        .map(|bytes| bytes.to_vec())
                        .unwrap_or_default();
                    let mut hops = recorder.lock().expect("no poisoned lock");
                    hops.push(Hop {
                        view,
                        path: path.clone(),
                        query: query.clone(),
                        tenant: headers
                            .get("NGSILD-Tenant")
                            .and_then(|value| value.to_str().ok())
                            .unwrap_or_default()
                            .to_owned(),
                        forged: headers
                            .get_all("NGSILD-Tenant")
                            .iter()
                            .any(|value| value.as_bytes() == b"somebody-elses-space"),
                        accept: headers
                            .get("Accept")
                            .and_then(|value| value.to_str().ok())
                            .unwrap_or_default()
                            .to_owned(),
                        body,
                        trace: (
                            headers
                                .get("traceparent")
                                .and_then(|value| value.to_str().ok())
                                .unwrap_or_default()
                                .to_owned(),
                            headers
                                .get("tracestate")
                                .and_then(|value| value.to_str().ok())
                                .unwrap_or_default()
                                .to_owned(),
                        ),
                        headers,
                    });
                    let pages = served.lock().expect("no poisoned lock");

                    // A view's MCP surface (T-3647, T-3808): a tool call answered with the
                    // next page as its structured content, the way the broker's tools answer.
                    if path == "/mcp" {
                        let asked = hops.iter().filter(|hop| hop.path == "/mcp").count();
                        let index = asked.saturating_sub(1).min(pages.len().saturating_sub(1));
                        return axum::Json(mcp_answer(
                            &hops[hops.len() - 1].body,
                            pages.get(index).cloned().unwrap_or_else(|| json!([])),
                        ))
                        .into_response();
                    }

                    // `GET /types` is how a caller asks what a tenant holds; the answer is
                    // built from the entities this stub was given, so it never disagrees
                    // with them.
                    if path == "/ngsi-ld/v1/types" {
                        return entity_type_list(&pages).into_response();
                    }

                    // CIM 009 5.7.2: a query over the collection must select something; a
                    // create at the same path is not a query.
                    if method == axum::http::Method::GET
                        && path == "/ngsi-ld/v1/entities"
                        && !selects(&query)
                    {
                        return unselected_query().into_response();
                    }

                    // AntaresBroker refuses an entity's identity named as an attribute, and
                    // a stub that took it hid T-2963 until dev answered every App read 400.
                    if let Some(name) = identity_in_attrs(&query) {
                        return identity_attribute(&name).into_response();
                    }

                    // Only a page request consumes a page, so a `/types` hop on the way in
                    // does not shift what the next entity query gets back.
                    let served_pages = hops
                        .iter()
                        .filter(|hop| hop.path == "/ngsi-ld/v1/entities")
                        .count();
                    let index = served_pages
                        .saturating_sub(1)
                        .min(pages.len().saturating_sub(1));
                    axum::Json(pages.get(index).cloned().unwrap_or_else(|| json!([])))
                        .into_response()
                }
            },
        ));
        let app = match view {
            Some(view) => app.layer(axum::middleware::from_fn_with_state(view, narrowed)),
            None => app,
        };

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("a free port");
        let port = listener.local_addr().expect("an address").port();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        Self {
            url: format!("http://127.0.0.1:{port}"),
            hops,
        }
    }

    /// The hops the gateway made, cloned out of the recorder.
    pub fn hops(&self) -> Vec<Hop> {
        self.hops.lock().expect("no poisoned lock").clone()
    }
}

/// What one role may read through a [`View`]: entities of `types`, with `attrs` alone when
/// named (`None` is every attribute).
#[derive(Debug, Clone)]
pub struct ViewRule {
    pub role: String,
    pub types: Vec<String>,
    pub attrs: Option<Vec<String>>,
    /// Whether the rule covers the temporal reads too (CIM 009 5.7.3, 5.7.4).
    pub history: bool,
}

/// The broker's view `name` and the rules its engine holds (T-3651), the stand-in for
/// `ANTARES_POLICY=joinedcontext` in the translator tests: a hop to another view is a `404`, a
/// caller whose pinned `x-jc-roles` match no rule is the engine's `403 AccessDenied`, and the
/// answer keeps only the entities and attributes the matching rules show, marked
/// `antares-results-restricted` when the caller asked (`x-jc-restricted`) and something was left
/// out. A hop that carries no roles matches no rule, so a gateway that forgets to pin the caller
/// is refused rather than served.
#[derive(Debug, Clone)]
pub struct View {
    pub name: String,
    pub rules: Vec<ViewRule>,
}

impl View {
    /// One view whose `role` reads `types`, every attribute or `attrs` alone.
    pub fn of(name: &str, role: &str, types: &[&str], attrs: Option<&[&str]>) -> Self {
        Self {
            name: name.to_owned(),
            rules: vec![ViewRule {
                role: role.to_owned(),
                types: types.iter().map(|t| (*t).to_owned()).collect(),
                attrs: attrs.map(|attrs| attrs.iter().map(|a| (*a).to_owned()).collect()),
                history: true,
            }],
        }
    }
}

async fn narrowed(
    axum::extract::State(view): axum::extract::State<View>,
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    let (hop_view, plain) = in_view(request.uri().path());
    let temporal = plain.starts_with("/ngsi-ld/v1/temporal/");
    if hop_view.is_empty() {
        return next.run(request).await;
    }
    if hop_view != view.name {
        return (
            axum::http::StatusCode::NOT_FOUND,
            axum::Json(
                json!({"type": "https://uri.etsi.org/ngsi-ld/errors/ResourceNotFound",
                "title": "no such view", "status": 404}),
            ),
        )
            .into_response();
    }
    let (roles, asked, wanted) = {
        let header = |name: &str| {
            request
                .headers()
                .get(name)
                .and_then(|value| value.to_str().ok())
                .unwrap_or_default()
                .to_owned()
        };
        let roles: Vec<String> = header("x-jc-roles")
            .split(',')
            .filter(|role| !role.is_empty())
            .map(str::to_owned)
            .collect();
        let asked = header("x-jc-restricted") == "true";
        // The broker's own selection by `type`, which the stub's pages do not know about.
        let wanted: Vec<String> = request
            .uri()
            .query()
            .unwrap_or_default()
            .split('&')
            .filter_map(|pair| pair.strip_prefix("type="))
            .flat_map(|value| value.split("%2C").flat_map(|part| part.split(',')))
            .map(str::to_owned)
            .collect();
        (roles, asked, wanted)
    };
    let rules: Vec<ViewRule> = view
        .rules
        .into_iter()
        .filter(|rule| roles.contains(&rule.role) && (rule.history || !temporal))
        .collect();
    if rules.is_empty() {
        return (
            axum::http::StatusCode::FORBIDDEN,
            axum::Json(json!({"type": "urn:antares:error:AccessDenied", "title": "Forbidden",
                "status": 403, "detail": "no grant of the joinedcontext policy covers this request"})),
        )
            .into_response();
    }
    let answer = next.run(request).await;
    if !answer.status().is_success() {
        return answer;
    }
    let (mut parts, body) = answer.into_parts();
    let bytes = axum::body::to_bytes(body, 64 * 1024 * 1024)
        .await
        .unwrap_or_default();
    let Ok(mut value) = serde_json::from_slice::<Value>(&bytes) else {
        return axum::response::Response::from_parts(parts, axum::body::Body::from(bytes));
    };
    let mut dropped = false;
    let mut keep = |entity: &mut Value| -> bool {
        if !wanted.is_empty()
            && !entity["type"]
                .as_str()
                .is_some_and(|t| wanted.iter().any(|w| w == t))
        {
            return false;
        }
        let Some(rule) = rules.iter().find(|rule| {
            entity["type"]
                .as_str()
                .is_some_and(|t| rule.types.iter().any(|r| r == t))
        }) else {
            dropped = true;
            return false;
        };
        if let (Some(attrs), Some(members)) = (&rule.attrs, entity.as_object_mut()) {
            let before = members.len();
            members.retain(|name, _| {
                matches!(name.as_str(), "id" | "type" | "@context") || attrs.contains(name)
            });
            dropped |= members.len() != before;
        }
        true
    };
    match &mut value {
        Value::Array(entities) => entities.retain_mut(|entity| keep(entity)),
        entity if entity.is_object() => {
            if !keep(entity) {
                return (
                    axum::http::StatusCode::NOT_FOUND,
                    axum::Json(
                        json!({"type": "https://uri.etsi.org/ngsi-ld/errors/ResourceNotFound",
                        "title": "Not Found", "status": 404}),
                    ),
                )
                    .into_response();
            }
        }
        _ => {}
    }
    parts.headers.remove(axum::http::header::CONTENT_LENGTH);
    if asked && dropped {
        parts.headers.insert(
            "antares-results-restricted",
            axum::http::HeaderValue::from_static("true"),
        );
    }
    axum::response::Response::from_parts(parts, axum::body::Body::from(value.to_string()))
}

/// A fake broker that answers the broker's view roots as its plain tree (T-3643): the gateway
/// sends `/x/views/{view}/ngsi-ld/v1/...`, and a fake written for `/ngsi-ld/v1/...` keeps
/// answering it, its view dropped.
pub fn plain_tree(app: axum::Router) -> axum::Router {
    axum::Router::new()
        .fallback_service(app)
        .layer(axum::middleware::map_request(
            |mut request: axum::extract::Request| async move {
                let (view, path) = in_view(request.uri().path());
                if !view.is_empty() {
                    let rebuilt = match request.uri().query() {
                        Some(query) => format!("{path}?{query}"),
                        None => path,
                    };
                    if let Ok(uri) = rebuilt.parse() {
                        *request.uri_mut() = uri;
                    }
                }
                request
            },
        ))
}

/// The view a path goes through and the plain CIM 009 path it stands for: the broker's view
/// root serves the same tree as its plain root (ADR-0023), so the stub answers both alike.
fn in_view(path: &str) -> (String, String) {
    match path
        .strip_prefix("/x/views/")
        .and_then(|rest| rest.split_once('/'))
    {
        Some((view, rest)) => (view.to_owned(), format!("/{rest}")),
        None => (String::new(), path.to_owned()),
    }
}

/// The broker's answer to one MCP tool call: a list is the `entities` of a page, anything else
/// the tool's own structured content.
fn mcp_answer(sent: &[u8], page: Value) -> Value {
    let id =
        serde_json::from_slice::<Value>(sent).map_or(Value::Null, |message| message["id"].clone());
    let structured = if page.is_array() {
        json!({ "entities": page })
    } else {
        page
    };
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": {
            "isError": false,
            "content": [{ "type": "text", "text": structured.to_string() }],
            "structuredContent": structured,
        },
    })
}

/// Whether a query selects at all (CIM 009 5.7.2).
fn selects(query: &str) -> bool {
    query
        .split('&')
        .filter_map(|pair| pair.split_once('='))
        .any(|(name, value)| !value.is_empty() && matches!(name, "type" | "attrs" | "q" | "georel"))
}

/// `id` or `type` in the `attrs` of a query: not attributes, so a broker refuses them.
fn identity_in_attrs(query: &str) -> Option<String> {
    query
        .split('&')
        .filter_map(|pair| pair.split_once('='))
        .filter(|(name, _)| *name == "attrs")
        .flat_map(|(_, value)| value.split("%2C").flat_map(|part| part.split(',')))
        .find(|attr| matches!(*attr, "id" | "type"))
        .map(str::to_owned)
}

/// AntaresBroker's refusal of an identity member named as an attribute, as dev answered it.
fn identity_attribute(name: &str) -> (axum::http::StatusCode, axum::Json<Value>) {
    (
        axum::http::StatusCode::BAD_REQUEST,
        axum::Json(json!({
            "type": "https://uri.etsi.org/ngsi-ld/errors/BadRequestData",
            "title": "BadRequestData",
            "status": 400,
            "detail": format!("invalid attribute name \"{name}\" in attrs"),
        })),
    )
}

/// The refusal a conformant broker sends for an unselected query, word for word.
fn unselected_query() -> (axum::http::StatusCode, axum::Json<Value>) {
    (
        axum::http::StatusCode::BAD_REQUEST,
        axum::Json(json!({
            "type": "https://uri.etsi.org/ngsi-ld/errors/BadRequestData",
            "title": "BadRequestData",
            "status": 400,
            "detail": "query needs at least one of type, attrs, q, georel (5.7.2)",
        })),
    )
}

/// The `EntityTypeList` of everything the stub was handed, deduplicated and ordered.
fn entity_type_list(pages: &[Value]) -> axum::Json<Value> {
    let types: std::collections::BTreeSet<&str> = pages
        .iter()
        .filter_map(Value::as_array)
        .flatten()
        .filter_map(|entity| entity["type"].as_str())
        .collect();
    axum::Json(json!({
        "id": "urn:ngsi-ld:EntityTypeList:stub",
        "type": "EntityTypeList",
        "typeList": types.into_iter().collect::<Vec<_>>(),
    }))
}

/// A broker whose MCP surface answers every request with `tools` as its tool list and no
/// resource: the hop an Endpoint's or a space's MCP message takes since T-3647. Returns its URL.
pub async fn mcp_broker(tools: Value) -> String {
    let stub = axum::Router::new().fallback(move || {
        let tools = tools.clone();
        async move {
            axum::Json(
                json!({ "jsonrpc": "2.0", "id": 1, "result": { "tools": tools, "resources": [] } }),
            )
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("a free port");
    let url = format!("http://{}", listener.local_addr().expect("an address"));
    tokio::spawn(async move {
        let _ = axum::serve(listener, stub).await;
    });
    url
}
