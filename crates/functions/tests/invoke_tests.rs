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

    /// A token with exactly these claims beside `iss`, `exp` and `iat`.
    fn token_with(&self, claims: Value) -> String {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        let mut header = Header::new(Algorithm::ES256);
        header.kid = Some("k1".to_owned());
        let mut all = json!({ "iss": ISSUER, "sub": "someone", "exp": now + 300, "iat": now - 10 });
        for (key, value) in claims.as_object().unwrap() {
            all[key] = value.clone();
        }
        encode(&header, &all, &self.signing).unwrap()
    }

    fn app(&self, slots: usize) -> axum::Router {
        self.app_with(slots, "http://127.0.0.1:9")
    }

    fn app_with(&self, slots: usize, gateway: &str) -> axum::Router {
        router(Arc::new(AppState {
            verifier: Arc::clone(&self.verifier),
            audience: "jc-functions".to_owned(),
            caller: "joinedcontext-portal".to_owned(),
            gateway: gateway.to_owned(),
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

// -------------------------------------------------------------------------------------------------
// T-2499: the edges of `invoke` the cases above leave (SDK-22, SDK-23)
// -------------------------------------------------------------------------------------------------
//
// Struck: "a sandbox panic or join error answers 500 generic" has no input that reaches it. The arm
// is `_ => problem(StatusCode::INTERNAL_SERVER_ERROR, "the invocation could not be run")` in
// `lib.rs`, a fixed sentence, and the sandbox turns everything a function does into an `Outcome`
// (T-2500's cases). `a_token_of_the_right_audience_but_wrong_azp_is_401` is
// `only_a_token_for_jc_functions_issued_to_the_portal_may_invoke` above; the one shape it leaves, a
// token with no `azp` at all, is below.

fn portal_token(realm: &Realm) -> String {
    realm.token("joinedcontext-portal", "jc-functions")
}

/// SDK-23: a token for the right audience that names no client is not the Portal's.
#[tokio::test(flavor = "multi_thread")]
async fn a_token_of_the_right_audience_but_wrong_azp_is_401() {
    let realm = Realm::new();
    let body = invocation(json!({})).to_string().into_bytes();
    for claims in [
        json!({ "aud": "jc-functions" }),
        json!({ "aud": "jc-functions", "azp": "" }),
        json!({ "aud": "jc-functions", "azp": "JOINEDCONTEXT-PORTAL" }),
        json!({ "aud": ["jc-functions", "context-gateway"], "azp": "joinedcontext-portal-2" }),
    ] {
        let token = realm.token_with(claims.clone());
        let (status, _) = send(realm.app(16), Some(&token), body.clone()).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{claims}");
    }
}

/// A function that calls its endpoint once and answers with what the gateway said.
fn calling(token: Value) -> Value {
    json!({
        "files": {
            "@app/functions/call.ts": format!("export default async (request, ctx) => ({{ body: await ctx.jc.get('/api/endpoint/{SLUG}/access') }});"),
            "@joinedcontext/sdk/server": "export const createClient = (config, transport) => ({ get: (path) => transport({ method: 'GET', path }) });",
        },
        "entry": "@app/functions/call.ts",
        "request": { "method": "GET" },
        "config": { "slug": SLUG },
        "token": token,
    })
}

/// GW10: an empty token is no token: the endpoint is called as the public, with no credential.
#[tokio::test(flavor = "multi_thread")]
async fn an_empty_bearer_token_field_calls_the_endpoint_anonymously_not_with_an_empty_credential() {
    use wiremock::{matchers::any, Mock, MockServer, ResponseTemplate};
    let gateway = MockServer::start().await;
    Mock::given(any())
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "ok": true })))
        .mount(&gateway)
        .await;
    let realm = Realm::new();
    for token in [json!(""), json!(null), json!("caller-token")] {
        let (status, answer) = send(
            realm.app_with(16, &gateway.uri()),
            Some(&portal_token(&realm)),
            calling(token.clone()).to_string().into_bytes(),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{token}: {answer}");
    }
    let sent = gateway.received_requests().await.unwrap_or_default();
    assert_eq!(sent.len(), 3);
    assert!(
        sent[0].headers.get("authorization").is_none(),
        "an empty token was sent"
    );
    assert!(sent[1].headers.get("authorization").is_none());
    assert_eq!(
        sent[2]
            .headers
            .get("authorization")
            .and_then(|v| v.to_str().ok()),
        Some("Bearer caller-token")
    );
}

/// SDK-23: a body that is not the invocation document is a 400, never a panic.
#[tokio::test(flavor = "multi_thread")]
async fn malformed_json_body_is_400_not_a_panic() {
    let realm = Realm::new();
    for body in [
        &b""[..],
        b"{",
        b"null",
        b"[]",
        b"\xff\xfe",
        b"{\"files\": 1}",
    ] {
        let (status, answer) =
            send(realm.app(16), Some(&portal_token(&realm)), body.to_vec()).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body:?}: {answer}");
    }
}

/// SDK-23: a member the invocation does not define is refused, at the top and in `request`.
#[tokio::test(flavor = "multi_thread")]
async fn an_unknown_field_in_the_body_is_refused_by_deny_unknown_fields() {
    let realm = Realm::new();
    let mut top = invocation(json!({}));
    top["secret"] = json!("x");
    let mut nested = invocation(json!({}));
    nested["request"]["headers"] = json!({ "authorization": "Bearer other" });
    for body in [top, nested] {
        let (status, answer) = send(
            realm.app(16),
            Some(&portal_token(&realm)),
            body.to_string().into_bytes(),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{answer}");
    }
}

/// SDK-22: a function is called with GET or POST, in that spelling.
#[tokio::test(flavor = "multi_thread")]
async fn a_request_method_outside_get_or_post_is_400() {
    let realm = Realm::new();
    for method in ["PUT", "DELETE", "PATCH", "get", "", "POST "] {
        let mut body = invocation(json!({}));
        body["request"]["method"] = json!(method);
        let (status, answer) = send(
            realm.app(16),
            Some(&portal_token(&realm)),
            body.to_string().into_bytes(),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{method:?}: {answer}");
    }
}

/// SDK-22: the slug a function's calls are confined to is a slug, or there is no call.
#[tokio::test(flavor = "multi_thread")]
async fn config_slug_missing_or_not_base32_is_400() {
    let realm = Realm::new();
    for slug in [
        None,
        Some(json!(null)),
        Some(json!(42)),
        Some(json!("")),
        Some(json!(SLUG.to_uppercase())),
        Some(json!("k7m2qz4tv6xh3n5jb2ryd3wcf1")),
        Some(json!(&SLUG[..25])),
        Some(json!("a".repeat(33))),
    ] {
        let mut body = invocation(json!({}));
        match &slug {
            None => {
                body["config"].as_object_mut().unwrap().remove("slug");
            }
            Some(value) => body["config"]["slug"] = value.clone(),
        }
        let (status, answer) = send(
            realm.app(16),
            Some(&portal_token(&realm)),
            body.to_string().into_bytes(),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{slug:?}: {answer}");
    }
}

/// SDK-22: the 256 KiB of a function's request body is measured as the JSON it is: exactly the
/// limit runs, one byte more is 413.
#[tokio::test(flavor = "multi_thread")]
async fn a_request_body_exactly_at_256_kib_passes_and_one_byte_over_is_413() {
    let realm = Realm::new();
    // A JSON string is its characters and two quotes.
    let at = json!("x".repeat(functions::REQUEST_BODY_LIMIT - 2));
    let over = json!("x".repeat(functions::REQUEST_BODY_LIMIT - 1));
    let (status, answer) = send(
        realm.app(16),
        Some(&portal_token(&realm)),
        invocation(at).to_string().into_bytes(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{answer}");
    let (status, _) = send(
        realm.app(16),
        Some(&portal_token(&realm)),
        invocation(over).to_string().into_bytes(),
    )
    .await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
}

/// SDK-23: `SLOTS` invocations run at once; the next one is refused while they do.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_17th_concurrent_invocation_is_429_while_16_are_in_flight() {
    assert_eq!(functions::SLOTS, 16);
    let realm = Realm::new();
    let app = realm.app(functions::SLOTS);
    let token = portal_token(&realm);
    // A function that never finishes on its own: the wall clock ends it after five seconds.
    let mut stuck = invocation(json!({}));
    stuck["files"]["@app/functions/echo.ts"] =
        json!("export default async () => { await new Promise(() => {}); };");
    let running: Vec<_> = (0..functions::SLOTS)
        .map(|_| {
            let (app, token, body) = (app.clone(), token.clone(), stuck.to_string().into_bytes());
            tokio::spawn(async move { send(app, Some(&token), body).await.0 })
        })
        .collect();
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    let (status, _) = send(
        app.clone(),
        Some(&token),
        invocation(json!({})).to_string().into_bytes(),
    )
    .await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    for call in running {
        assert_eq!(
            call.await.unwrap(),
            StatusCode::OK,
            "a stuck call ends as an Outcome"
        );
    }
    let (status, _) = send(
        app,
        Some(&token),
        invocation(json!({})).to_string().into_bytes(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "the slots are free again");
}

/// SDK-23: `files` is part of the invocation, so an invocation without it is a 400 that names it,
/// not a function run with no code.
#[tokio::test(flavor = "multi_thread")]
async fn files_map_absent_is_a_400_naming_files_not_a_function_with_no_code() {
    let realm = Realm::new();
    let mut body = invocation(json!({}));
    body.as_object_mut().unwrap().remove("files");
    let (status, answer) = send(
        realm.app(16),
        Some(&portal_token(&realm)),
        body.to_string().into_bytes(),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{answer}");
    assert!(answer.to_string().contains("files"), "{answer}");
}
