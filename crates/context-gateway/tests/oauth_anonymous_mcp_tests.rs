//! The stateless OAuth server for anonymous MCP, through the router (ADR-N-052 §2.2, EP-129,
//! T-4631): an assistant registers, authorizes and takes a token with nothing stored, and the
//! token it gets reads exactly what a call without one reads, in the same rate-limit bucket.

mod common;

use axum::body::Body;
use axum::http::{header, Method, Request, StatusCode};
use axum::Router;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use context_gateway::app::{router, Gateway};
use context_gateway::auth::accounts::ServiceAccounts;
use context_gateway::oauth::{Issuer, DEFAULT_REDIRECT_HOSTS};
use context_gateway::pdp::PolicyPdp;
use context_gateway::proxy::Broker;
use context_gateway::resolver::Endpoint;
use jc_core::kinds::{Audience, PolicySpec, RateLimits, Representation};
use serde_json::{json, Value};
use std::sync::Arc;
use tower::ServiceExt;

const HOST: &str = "https://api.city.example";
const OPEN: &str = "oooooooooooooooooooooooooo";
const INNER: &str = "iiiiiiiiiiiiiiiiiiiiiiiiii";
const GEMINI: &str = "https://oauth-redirect.googleusercontent.com/oauth2/callback";
const VERIFIER: &str = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";

fn endpoint(slug: &str, audience: Audience, role: &str) -> Endpoint {
    let policy: PolicySpec = serde_norway::from_str(&format!(
        "contextSpaceRef: mesto\nassigner: did:web:city.example\nassignee: {{ kind: role, id: {role} }}\n\
         operations: [queryEntity]\ninformation:\n  - entities:\n      - type: AirQualityObserved\n"
    ))
    .expect("the policy spec parses");
    Endpoint {
        declared_types: None,
        roles: Default::default(),
        slug: slug.to_owned(),
        title: [("en".to_owned(), slug.to_owned())].into_iter().collect(),
        description: Default::default(),
        space: "mesto".to_owned(),
        project: "mesto".to_owned(),
        audience,
        allowed_projects: Vec::new(),
        representations: vec![Representation::NgsiLd, Representation::Mcp],
        rate_limit: None,
        creates: None,
        file_limits: None,
        hidden_attributes: Default::default(),
        projection: None,
        view_mapping: None,
        catalog: None,
        deprecation: None,
        feeds: Vec::new(),
        calendars: Vec::new(),
        map_defaults: Default::default(),
        base_path: format!("/api/endpoint/{slug}"),
        models: Vec::new(),
        policy_names: Vec::new(),
        manifest_name: String::new(),
        policies: vec![policy],
    }
}

fn app(realm: &common::Realm, anonymous: RateLimits) -> Router {
    let hosts = DEFAULT_REDIRECT_HOSTS
        .iter()
        .map(|host| (*host).to_owned())
        .collect();
    let issuer = Issuer::new(HOST, &jc_core::Secret::new("t".repeat(40)), hosts).expect("a key");
    router(Arc::new(
        Gateway::new(
            Broker::new("http://127.0.0.1:1"),
            Box::new(PolicyPdp),
            "city.example",
        )
        .serve([
            endpoint(OPEN, Audience::Public, "public"),
            endpoint(INNER, Audience::Organization, "steward"),
        ])
        .authenticate(
            Arc::new(realm.verifier()),
            ServiceAccounts::new(),
            Some(HOST.to_owned()),
        )
        .issue_anonymous_tokens(issuer)
        .limit_anonymous_mcp_to(anonymous),
    ))
}

async fn send(app: &Router, request: Request<Body>) -> (StatusCode, axum::http::HeaderMap, String) {
    let response = app
        .clone()
        .oneshot(request)
        .await
        .expect("the gateway answers");
    let (status, headers) = (response.status(), response.headers().clone());
    let bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .expect("a body");
    (
        status,
        headers,
        String::from_utf8_lossy(&bytes).into_owned(),
    )
}

fn get(path: &str) -> Request<Body> {
    Request::get(path).body(Body::empty()).expect("a request")
}

fn json_post(path: &str, body: &Value, token: Option<&str>) -> Request<Body> {
    let mut builder = Request::builder()
        .method(Method::POST)
        .uri(path)
        .header(header::CONTENT_TYPE, "application/json")
        .header("x-forwarded-for", "203.0.113.7");
    if let Some(token) = token {
        builder = builder.header(header::AUTHORIZATION, format!("Bearer {token}"));
    }
    builder
        .body(Body::from(body.to_string()))
        .expect("a request")
}

fn form_post(path: &str, pairs: &[(&str, &str)]) -> Request<Body> {
    let body: String = pairs
        .iter()
        .map(|(k, v)| format!("{k}={}", urlencode(v)))
        .collect::<Vec<_>>()
        .join("&");
    Request::post(path)
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from(body))
        .expect("a request")
}

fn urlencode(text: &str) -> String {
    text.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

fn challenge() -> String {
    URL_SAFE_NO_PAD.encode(ring::digest::digest(
        &ring::digest::SHA256,
        VERIFIER.as_bytes(),
    ))
}

const TOOLS_LIST: &str = r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#;

/// Register, authorize and take a token for `resource`, as Gemini does.
async fn token_for(app: &Router, resource: &str) -> String {
    let (status, _, body) = send(
        app,
        json_post(
            "/oauth/register",
            &json!({ "redirect_uris": [GEMINI], "client_name": "Gemini" }),
            None,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let registered: Value = serde_json::from_str(&body).expect("JSON");
    let client_id = registered["client_id"]
        .as_str()
        .expect("a client_id")
        .to_owned();
    assert_eq!(registered["token_endpoint_auth_method"], "none");

    let authorize = format!(
        "/oauth/authorize?response_type=code&client_id={}&redirect_uri={}&code_challenge={}&code_challenge_method=S256&state=xyz%201&resource={}",
        urlencode(&client_id),
        urlencode(GEMINI),
        challenge(),
        urlencode(resource),
    );
    let (status, headers, _) = send(app, get(&authorize)).await;
    assert_eq!(status, StatusCode::FOUND);
    let location = headers[header::LOCATION].to_str().expect("a location");
    assert!(
        location.starts_with(&format!("{GEMINI}?code=")),
        "{location}"
    );
    assert!(
        location.contains("&state=xyz+1&") || location.contains("&state=xyz%201&"),
        "{location}"
    );
    let code = location
        .split(['?', '&'])
        .find_map(|pair| pair.strip_prefix("code="))
        .expect("a code");

    let (status, headers, body) = send(
        app,
        form_post(
            "/oauth/token",
            &[
                ("grant_type", "authorization_code"),
                ("code", code),
                ("client_id", &client_id),
                ("redirect_uri", GEMINI),
                ("code_verifier", VERIFIER),
            ],
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(headers[header::CACHE_CONTROL]
        .to_str()
        .is_ok_and(|value| value.contains("no-store")));
    let answer: Value = serde_json::from_str(&body).expect("JSON");
    assert_eq!(answer["token_type"], "Bearer");
    assert!(answer["expires_in"]
        .as_u64()
        .is_some_and(|seconds| seconds <= 3600));
    answer["access_token"]
        .as_str()
        .expect("an access token")
        .to_owned()
}

#[tokio::test]
async fn gemini_s_flow_ends_in_the_anonymous_answer_of_the_hub() {
    let realm = common::Realm::new();
    let app = app(&realm, context_gateway::app::ANONYMOUS_MCP_RATE_LIMIT);

    // Discovery: the hub names this server first and the realm second; the server its endpoints.
    for path in [
        "/api/mcp/.well-known/oauth-protected-resource",
        "/.well-known/oauth-protected-resource/api/mcp",
    ] {
        let (status, _, body) = send(&app, get(path)).await;
        assert_eq!(status, StatusCode::OK, "{path}: {body}");
        let metadata: Value = serde_json::from_str(&body).expect("JSON");
        assert_eq!(
            metadata["authorization_servers"],
            json!([HOST, common::ISSUER]),
            "{path}"
        );
    }
    let (_, _, body) = send(&app, get("/.well-known/oauth-authorization-server")).await;
    let server: Value = serde_json::from_str(&body).expect("JSON");
    assert_eq!(server["issuer"], HOST);
    assert_eq!(
        server["registration_endpoint"],
        format!("{HOST}/oauth/register")
    );
    assert_eq!(server["code_challenge_methods_supported"], json!(["S256"]));
    assert_eq!(
        server["token_endpoint_auth_methods_supported"],
        json!(["none"])
    );

    let token = token_for(&app, &format!("{HOST}/api/mcp")).await;
    let (status, _, anonymous) = send(
        &app,
        json_post(
            "/api/mcp",
            &serde_json::from_str(TOOLS_LIST).expect("JSON"),
            None,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{anonymous}");
    let (status, _, with_token) = send(
        &app,
        json_post(
            "/api/mcp",
            &serde_json::from_str(TOOLS_LIST).expect("JSON"),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{with_token}");
    assert_eq!(with_token, anonymous, "the token reads what no token reads");
}

#[tokio::test]
async fn a_private_endpoint_keeps_the_realm_alone_and_refuses_the_token() {
    let realm = common::Realm::new();
    let app = app(&realm, context_gateway::app::ANONYMOUS_MCP_RATE_LIMIT);

    let (_, _, body) = send(
        &app,
        get(&format!(
            "/api/endpoint/{INNER}/.well-known/oauth-protected-resource"
        )),
    )
    .await;
    let metadata: Value = serde_json::from_str(&body).expect("JSON");
    assert_eq!(metadata["authorization_servers"], json!([common::ISSUER]));
    let (_, _, body) = send(
        &app,
        get(&format!(
            "/.well-known/oauth-protected-resource/api/endpoint/{OPEN}/mcp"
        )),
    )
    .await;
    let metadata: Value = serde_json::from_str(&body).expect("JSON");
    assert_eq!(
        metadata["authorization_servers"],
        json!([HOST, common::ISSUER])
    );

    // A token for the whole host is still anonymous on the private Endpoint: its 401 names the
    // realm's metadata, as for a call with no token.
    let token = token_for(&app, HOST).await;
    let (status, headers, _) = send(
        &app,
        json_post(
            &format!("/api/endpoint/{INNER}/mcp"),
            &serde_json::from_str(TOOLS_LIST).expect("JSON"),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let challenge = headers[header::WWW_AUTHENTICATE]
        .to_str()
        .expect("a challenge");
    assert!(
        challenge.contains(&format!(
            "{HOST}/api/endpoint/{INNER}/.well-known/oauth-protected-resource"
        )),
        "{challenge}"
    );

    // A token for the hub is no token for an Endpoint, and a forged one is no token at all.
    let hub_only = token_for(&app, &format!("{HOST}/api/mcp")).await;
    let (status, _, _) = send(
        &app,
        json_post(
            &format!("/api/endpoint/{OPEN}/mcp"),
            &serde_json::from_str(TOOLS_LIST).expect("JSON"),
            Some(&hub_only),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let mut forged = hub_only.clone();
    forged.replace_range(forged.len() - 3.., "AAA");
    let (status, _, _) = send(
        &app,
        json_post(
            "/api/mcp",
            &serde_json::from_str(TOOLS_LIST).expect("JSON"),
            Some(&forged),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn a_minted_token_spends_the_anonymous_bucket_of_its_address() {
    let realm = common::Realm::new();
    let app = app(
        &realm,
        RateLimits {
            requests_per_minute: 2,
            burst: Some(2),
        },
    );
    let token = token_for(&app, &format!("{HOST}/api/mcp")).await;
    let list: Value = serde_json::from_str(TOOLS_LIST).expect("JSON");
    for _ in 0..2 {
        let (status, _, body) = send(&app, json_post("/api/mcp", &list, None)).await;
        assert_eq!(status, StatusCode::OK, "{body}");
    }
    let (status, _, _) = send(&app, json_post("/api/mcp", &list, Some(&token))).await;
    assert_eq!(
        status,
        StatusCode::TOO_MANY_REQUESTS,
        "a token is no bucket of its own"
    );
}

#[tokio::test]
async fn a_refused_authorize_never_redirects() {
    let realm = common::Realm::new();
    let app = app(&realm, context_gateway::app::ANONYMOUS_MCP_RATE_LIMIT);
    let (status, _, body) = send(
        &app,
        json_post(
            "/oauth/register",
            &json!({ "redirect_uris": ["https://evil.example/cb"] }),
            None,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(
        serde_json::from_str::<Value>(&body).expect("JSON")["error"],
        "invalid_redirect_uri"
    );

    let evil = format!(
        "/oauth/authorize?response_type=code&client_id=forged&redirect_uri={}&code_challenge={}&code_challenge_method=S256",
        urlencode("https://evil.example/cb"),
        challenge()
    );
    let (status, headers, _) = send(&app, get(&evil)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(headers.get(header::LOCATION).is_none());
}

/// EP-166, T-5847: the OAuth answers are never compressed: the registration answer carries a
/// fresh client next to the redirect URI the caller sent, and a compressed length beside
/// reflected text is the BREACH oracle. The data answers are (compression_tests.rs).
#[tokio::test]
async fn an_oauth_answer_is_never_compressed() {
    let realm = common::Realm::new();
    let app = app(&realm, context_gateway::app::ANONYMOUS_MCP_RATE_LIMIT);

    let mut register = json_post(
        "/oauth/register",
        &json!({ "redirect_uris": [GEMINI], "client_name": "Gemini" }),
        None,
    );
    register.headers_mut().insert(
        header::ACCEPT_ENCODING,
        "gzip, br".parse().expect("a value"),
    );
    let (status, headers, body) = send(&app, register).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert!(
        headers.get(header::CONTENT_ENCODING).is_none(),
        "{headers:?}"
    );
    let registered: Value = serde_json::from_str(&body).expect("plain JSON");
    assert!(registered["client_id"].is_string(), "{body}");

    let mut metadata = get("/.well-known/oauth-authorization-server");
    metadata
        .headers_mut()
        .insert(header::ACCEPT_ENCODING, "gzip".parse().expect("a value"));
    let (status, headers, body) = send(&app, metadata).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(
        headers.get(header::CONTENT_ENCODING).is_none(),
        "{headers:?}"
    );
}
