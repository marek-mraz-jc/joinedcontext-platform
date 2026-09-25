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
    /// The path, which must be the plain CIM 009 tree with no surface prefix left on it.
    pub path: String,
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
                    let path = request.uri().path().to_owned();
                    let query = request.uri().query().unwrap_or_default().to_owned();
                    let body = axum::body::to_bytes(request.into_body(), 4 * 1024 * 1024)
                        .await
                        .map(|bytes| bytes.to_vec())
                        .unwrap_or_default();
                    let mut hops = recorder.lock().expect("no poisoned lock");
                    hops.push(Hop {
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
                    });
                    let pages = served.lock().expect("no poisoned lock");

                    // `GET /types` is how a caller asks what a tenant holds; the answer is
                    // built from the entities this stub was given, so it never disagrees
                    // with them.
                    if path == "/ngsi-ld/v1/types" {
                        return entity_type_list(&pages).into_response();
                    }

                    // CIM 009 5.7.2: a query over the collection must select something.
                    if path == "/ngsi-ld/v1/entities" && !selects(&query) {
                        return unselected_query().into_response();
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

/// Whether a query selects at all (CIM 009 5.7.2).
fn selects(query: &str) -> bool {
    query
        .split('&')
        .filter_map(|pair| pair.split_once('='))
        .any(|(name, value)| !value.is_empty() && matches!(name, "type" | "attrs" | "q" | "georel"))
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

/// The delivery key of this run, generated like the realm's so none is ever committed (GW27).
fn delivery_secret() -> &'static [u8; 32] {
    static SECRET: std::sync::OnceLock<[u8; 32]> = std::sync::OnceLock::new();
    SECRET.get_or_init(|| {
        let mut bytes = [0u8; 32];
        ring::rand::SecureRandom::fill(&SystemRandom::new(), &mut bytes)
            .expect("the system has randomness");
        bytes
    })
}

/// The key a test gateway seals each subscription's subscriber with (GW27, T-2383).
pub fn delivery_key() -> context_gateway::egress::subject::DeliveryKey {
    context_gateway::egress::subject::DeliveryKey::new(delivery_secret())
        .expect("32 bytes is a long enough key")
}

/// A stored subscription's routed endpoint, sealed for `subject` the way the gateway seals it
/// when the subscription is written: over the endpoint, the target and the areas the URI names.
pub fn seal_stored(
    subscription: &mut Value,
    base_path: &str,
    subject: &context_gateway::pdp::evaluator::Subject,
) {
    use context_gateway::query::{encode, first, parse};
    let uri = subscription["notification"]["endpoint"]["uri"]
        .as_str()
        .expect("a stored endpoint")
        .to_owned();
    let (_, query) = uri.split_once('?').expect("a routed endpoint");
    let params = parse(query);
    let target = first(&params, "to").expect("a target");
    let areas: Vec<String> = params
        .iter()
        .filter(|(name, _)| name == "area")
        .map(|(_, area)| area.clone())
        .collect();
    let sealed = delivery_key().seal(base_path, target, &areas, subject);
    subscription["notification"]["endpoint"]["uri"] =
        Value::String(format!("{uri}&sub={}", encode(&sealed)));
}

/// A public grant to subscribe to one type's attributes in one space: what a stored subscription
/// of an attack case was written under, and what its delivery is decided against again (GW27).
pub fn public_subscribe_policy(
    space: &str,
    domain: &str,
    kind: &str,
    attributes: &[&str],
) -> jc_core::kinds::PolicySpec {
    serde_norway::from_str(&format!(
        "contextSpaceRef: {space}\n\
         assigner: did:web:{domain}\n\
         assignee: {{ kind: role, id: public }}\n\
         operations: [createSubscription, queryEntity]\n\
         information:\n\
         \x20 - entities:\n\
         \x20     - type: {kind}\n\
         \x20   propertyNames: [{}]\n",
        attributes.join(", ")
    ))
    .expect("the policy spec parses")
}
