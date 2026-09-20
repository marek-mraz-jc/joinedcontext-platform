//! `POST /invoke` through the router: who may call it, the size and concurrency limits, and one
//! function answering (SDK-22, SDK-23).

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use context_gateway::auth::token::Verifier;
use functions::{router, AppState};
use jsonwebtoken::{encode, Algorithm, EncodingKey, Header};
use ring::rand::SystemRandom;
use ring::signature::{EcdsaKeyPair, KeyPair, ECDSA_P256_SHA256_FIXED_SIGNING};
use serde_json::{json, Value};
use tokio::sync::Semaphore;
use tower::ServiceExt;

const ISSUER: &str = "https://portal.example/realms/joinedcontext";
const SLUG: &str = "k7m2qz4tv6xh3n5jb2ryd3wcfa";

/// One P-256 key per run, published the way Keycloak publishes one.
struct Realm {
    signing: EncodingKey,
    verifier: Arc<Verifier>,
}

impl Realm {
    fn new() -> Self {
        let rng = SystemRandom::new();
        let pkcs8 = EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, &rng).unwrap();
        let pair = EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, pkcs8.as_ref(), &rng)
            .unwrap();
        let point = pair.public_key().as_ref();
        let jwks = serde_json::from_value(json!({ "keys": [{
            "kty": "EC", "crv": "P-256", "alg": "ES256", "use": "sig", "kid": "k1",
            "x": URL_SAFE_NO_PAD.encode(&point[1..33]), "y": URL_SAFE_NO_PAD.encode(&point[33..]),
        }]}))
        .unwrap();
        let verifier = Verifier::new(ISSUER);
        assert_eq!(verifier.replace_keys(&jwks), 1);
        Self {
            signing: EncodingKey::from_ec_der(pkcs8.as_ref()),
            verifier: Arc::new(verifier),
        }
    }

    fn token(&self, azp: &str, aud: &str) -> String {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        let mut header = Header::new(Algorithm::ES256);
        header.kid = Some("k1".to_owned());
        let claims = json!({ "iss": ISSUER, "sub": format!("service-account-{azp}"), "aud": aud, "azp": azp, "exp": now + 300, "iat": now - 10 });
        encode(&header, &claims, &self.signing).unwrap()
    }

    fn app(&self, slots: usize) -> axum::Router {
        router(Arc::new(AppState {
            verifier: Arc::clone(&self.verifier),
            audience: "jc-functions".to_owned(),
            caller: "joinedcontext-portal".to_owned(),
            gateway: "http://127.0.0.1:9".to_owned(),
            http: reqwest::Client::new(),
            slots: Arc::new(Semaphore::new(slots)),
        }))
    }
}

fn invocation(body: Value) -> Value {
    json!({
        "files": {
            "@app/functions/echo.ts": "export default async (request, ctx) => { ctx.log('echo'); return { body: { got: request.body, method: request.method } }; };",
            "@joinedcontext/sdk/server": "export const createClient = (config) => ({ config });",
        },
        "entry": "@app/functions/echo.ts",
        "request": { "method": "POST", "query": {}, "body": body, "user": null },
        "config": { "slug": SLUG, "orgDomain": "hel.fi", "space": "mobility" },
        "token": "caller-token",
    })
}

async fn send(app: axum::Router, token: Option<&str>, body: Vec<u8>) -> (StatusCode, Value) {
    let mut request = Request::post("/invoke").header("content-type", "application/json");
    if let Some(token) = token {
        request = request.header("authorization", format!("Bearer {token}"));
    }
    let response = app
        .oneshot(request.body(Body::from(body)).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

#[tokio::test(flavor = "multi_thread")]
async fn the_portal_invokes_a_function_and_gets_its_answer() {
    let realm = Realm::new();
    let token = realm.token("joinedcontext-portal", "jc-functions");
    let (status, answer) = send(
        realm.app(16),
        Some(&token),
        invocation(json!({ "n": 1 })).to_string().into_bytes(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{answer}");
    assert_eq!(
        answer,
        json!({ "status": 200, "body": { "got": { "n": 1 }, "method": "POST" }, "logs": ["echo"] })
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn only_a_token_for_jc_functions_issued_to_the_portal_may_invoke() {
    let realm = Realm::new();
    let body = invocation(json!({})).to_string().into_bytes();
    for token in [
        None,
        Some(realm.token("joinedcontext-portal", "context-gateway")),
        Some(realm.token("some-department-app", "jc-functions")),
        Some("not-a-jwt".to_owned()),
    ] {
        let (status, _) = send(realm.app(16), token.as_deref(), body.clone()).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{token:?}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn the_size_and_concurrency_limits_answer_413_and_429() {
    let realm = Realm::new();
    let token = realm.token("joinedcontext-portal", "jc-functions");
    let big = invocation(json!({ "text": "x".repeat(256 * 1024) }))
        .to_string()
        .into_bytes();
    assert_eq!(
        send(realm.app(16), Some(&token), big).await.0,
        StatusCode::PAYLOAD_TOO_LARGE
    );
    let huge = vec![b' '; 9 * 1024 * 1024];
    assert_eq!(
        send(realm.app(16), Some(&token), huge).await.0,
        StatusCode::PAYLOAD_TOO_LARGE
    );
    let body = invocation(json!({})).to_string().into_bytes();
    assert_eq!(
        send(realm.app(0), Some(&token), body.clone()).await.0,
        StatusCode::TOO_MANY_REQUESTS
    );

    let mut other_slug = invocation(json!({}));
    other_slug["config"]["slug"] = json!("../../v1");
    assert_eq!(
        send(
            realm.app(16),
            Some(&token),
            other_slug.to_string().into_bytes()
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );
}
