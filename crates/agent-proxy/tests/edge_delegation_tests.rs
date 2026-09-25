//! A run reads as the person who started it (T-2865, ADR-N-038, AG-94).
//!
//! A stub realm answers introspection, the token exchange, refresh and revocation; a stub Portal
//! holds the run; a stub gateway records what a data call carried. The proxy holds a real client
//! secret here, so no stub token is minted anywhere.

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::{
    app, authed, body_of, sample_run, state, Bases, PROXY_CLIENT, PROXY_SECRET, RUN_ID, SLUG,
};
use serde_json::json;
use std::io::Write;
use std::sync::{Arc, Mutex};
use tower::ServiceExt;
use wiremock::matchers::{body_string_contains, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const PERSON: &str = "demo.steward@hel.fi";
const PORTAL_TOKEN: &str = "portal-service-account-token";
const PERSON_TOKEN: &str = "person-access-token";
const DELEGATED: &str = "delegated-access-token";
const REFRESH: &str = "delegated-refresh-token";
const TOKEN_PATH: &str = "/realms/jc/protocol/openid-connect/token";
const INTROSPECT_PATH: &str = "/realms/jc/protocol/openid-connect/token/introspect";
const REVOKE_PATH: &str = "/realms/jc/protocol/openid-connect/revoke";

/// A realm that knows the Portal's service token and the person's token, and answers the
/// exchange with a grant that lives `expires_in` seconds.
async fn realm(expires_in: u64) -> MockServer {
    let realm = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(INTROSPECT_PATH))
        .and(body_string_contains(format!("token={PORTAL_TOKEN}")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "active": true,
            "azp": "portal-api",
            "username": "service-account-portal-api",
            "aud": [PROXY_CLIENT, "jc-functions"],
        })))
        .mount(&realm)
        .await;
    Mock::given(method("POST"))
        .and(path(INTROSPECT_PATH))
        .and(body_string_contains(format!("token={PERSON_TOKEN}")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "active": true, "azp": "edge", "username": PERSON, "aud": [PROXY_CLIENT],
        })))
        .mount(&realm)
        .await;
    Mock::given(method("POST"))
        .and(path(TOKEN_PATH))
        .and(body_string_contains("token-exchange"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "access_token": DELEGATED, "expires_in": expires_in, "refresh_token": REFRESH,
            "token_type": "Bearer",
        })))
        .mount(&realm)
        .await;
    // The proxy's own token, for its lookups on the Portal's internal listener.
    Mock::given(method("POST"))
        .and(path(TOKEN_PATH))
        .and(body_string_contains("grant_type=client_credentials"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "access_token": "proxy-own-token", "expires_in": 300,
        })))
        .mount(&realm)
        .await;
    Mock::given(method("POST"))
        .and(path(REVOKE_PATH))
        .respond_with(ResponseTemplate::new(200))
        .mount(&realm)
        .await;
    realm
}

/// A Portal that holds the sample run with `status`.
async fn portal(status: &str) -> MockServer {
    let portal = MockServer::start().await;
    let run = sample_run(false);
    Mock::given(method("GET"))
        .and(path(format!("/internal/agent-runs/{RUN_ID}")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": run.id, "project": run.project, "appName": run.app_name,
            "endpointSlug": run.endpoint_slug, "allowsWrite": run.allows_write,
            "branch": run.branch, "pathPrefix": run.path_prefix, "status": status,
            "ticketHash": run.ticket_hash, "maxTokens": run.max_tokens,
            "allowedHosts": run.allowed_hosts, "requestsPerMinute": run.requests_per_minute,
            "maxResponseBytes": run.max_response_bytes, "createdBy": run.created_by,
            "modelName": run.model_name,
        })))
        .mount(&portal)
        .await;
    portal
}

/// A gateway that answers every read with an empty list.
async fn gateway() -> MockServer {
    let gateway = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
        .mount(&gateway)
        .await;
    gateway
}

fn proxy(realm: &MockServer, portal: &MockServer, gateway: &MockServer) -> axum::Router {
    app(
        sample_run(false),
        Bases {
            realm: Some(format!("{}/realms/jc", realm.uri())),
            portal: portal.uri(),
            gateway: gateway.uri(),
            ..Bases::default()
        },
    )
}

fn hand_over(bearer: &str, subject_token: &str) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(format!("/internal/runs/{RUN_ID}/identity"))
        .header("authorization", format!("Bearer {bearer}"))
        .header("content-type", "application/json")
        .body(Body::from(
            json!({ "subjectToken": subject_token }).to_string(),
        ))
        .expect("a request")
}

fn read() -> Request<Body> {
    authed("GET", "/v1/data/ngsi-ld/v1/entities")
        .body(Body::empty())
        .expect("a request")
}

async fn bodies(server: &MockServer, at: &str) -> Vec<String> {
    server
        .received_requests()
        .await
        .unwrap_or_default()
        .into_iter()
        .filter(|r| r.url.path() == at)
        .map(|r| String::from_utf8_lossy(&r.body).into_owned())
        .collect()
}

async fn bearers_at_gateway(gateway: &MockServer) -> Vec<String> {
    gateway
        .received_requests()
        .await
        .unwrap_or_default()
        .into_iter()
        .filter_map(|r| {
            r.headers
                .get("authorization")
                .and_then(|v| v.to_str().ok())
                .map(str::to_owned)
        })
        .collect()
}

/// ADR-N-038 §3.1–3: the Portal hands over, the proxy exchanges with its own client's
/// credentials, and the run's data call carries the person's delegated token.
#[tokio::test]
async fn a_handed_over_identity_is_exchanged_and_carried_on_every_data_call() {
    let (realm, portal, gateway) = (realm(300).await, portal("building").await, gateway().await);
    let proxy = proxy(&realm, &portal, &gateway);

    let answer = proxy
        .clone()
        .oneshot(hand_over(PORTAL_TOKEN, PERSON_TOKEN))
        .await
        .expect("an answer");
    assert_eq!(answer.status(), StatusCode::NO_CONTENT);

    let exchange = bodies(&realm, TOKEN_PATH)
        .await
        .into_iter()
        .find(|b| b.contains("token-exchange"))
        .expect("the proxy asked for an exchange");
    for field in [
        "grant_type=urn%3Aietf%3Aparams%3Aoauth%3Agrant-type%3Atoken-exchange".to_owned(),
        format!("client_id={PROXY_CLIENT}"),
        format!("client_secret={PROXY_SECRET}"),
        format!("subject_token={PERSON_TOKEN}"),
        "subject_token_type=urn%3Aietf%3Aparams%3Aoauth%3Atoken-type%3Aaccess_token".to_owned(),
        "requested_token_type=urn%3Aietf%3Aparams%3Aoauth%3Atoken-type%3Arefresh_token".to_owned(),
        "audience=context-gateway".to_owned(),
    ] {
        assert!(exchange.contains(&field), "{field} missing from {exchange}");
    }

    for _ in 0..2 {
        let answer = proxy.clone().oneshot(read()).await.expect("an answer");
        assert_eq!(answer.status(), StatusCode::OK);
    }
    assert_eq!(
        bearers_at_gateway(&gateway).await,
        vec![format!("Bearer {DELEGATED}"); 2]
    );
    assert_eq!(
        bodies(&realm, TOKEN_PATH)
            .await
            .iter()
            .filter(|b| b.contains("token-exchange") || b.contains("refresh_token="))
            .count(),
        1,
        "a live token is reused, never exchanged or refreshed again"
    );
}

/// ADR-N-038 §3.3: no grant, no data, and no fallback to the proxy's own token.
#[tokio::test]
async fn a_run_without_its_persons_grant_reads_nothing() {
    let (realm, portal, gateway) = (realm(300).await, portal("building").await, gateway().await);
    let answer = proxy(&realm, &portal, &gateway)
        .oneshot(read())
        .await
        .expect("an answer");

    assert_eq!(answer.status(), StatusCode::UNAUTHORIZED);
    assert!(body_of(answer).await.contains("ADR-N-038"));
    assert!(bearers_at_gateway(&gateway).await.is_empty());
    assert!(
        !bodies(&realm, TOKEN_PATH)
            .await
            .iter()
            .any(|b| b.contains(&format!("audience={SLUG}"))),
        "no endpoint token of the proxy's own is minted in its place"
    );
}

/// A token that expires inside the leeway is refreshed before it is sent.
#[tokio::test]
async fn a_grant_about_to_expire_is_refreshed_before_the_call() {
    let (realm, portal, gateway) = (realm(10).await, portal("building").await, gateway().await);
    Mock::given(method("POST"))
        .and(path(TOKEN_PATH))
        .and(body_string_contains(format!("refresh_token={REFRESH}")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "access_token": "refreshed-access-token", "expires_in": 300,
            "refresh_token": "rotated-refresh-token",
        })))
        .mount(&realm)
        .await;
    let proxy = proxy(&realm, &portal, &gateway);
    proxy
        .clone()
        .oneshot(hand_over(PORTAL_TOKEN, PERSON_TOKEN))
        .await
        .expect("an answer");

    let answer = proxy.oneshot(read()).await.expect("an answer");

    assert_eq!(answer.status(), StatusCode::OK);
    assert_eq!(
        bearers_at_gateway(&gateway).await,
        vec!["Bearer refreshed-access-token".to_owned()]
    );
    let refresh = bodies(&realm, TOKEN_PATH)
        .await
        .into_iter()
        .find(|b| b.contains("grant_type=refresh_token"))
        .expect("a refresh");
    assert!(refresh.contains(&format!("client_secret={PROXY_SECRET}")));
}

/// ADR-N-038 §3.5: the person's session ended, the realm refuses the refresh, and the run's
/// grant is gone: this read and every later one are a 401 without asking the realm again.
#[tokio::test]
async fn a_refused_refresh_ends_the_grant() {
    let (realm, portal, gateway) = (realm(10).await, portal("building").await, gateway().await);
    Mock::given(method("POST"))
        .and(path(TOKEN_PATH))
        .and(body_string_contains("grant_type=refresh_token"))
        .respond_with(ResponseTemplate::new(400).set_body_json(json!({ "error": "invalid_grant" })))
        .mount(&realm)
        .await;
    let proxy = proxy(&realm, &portal, &gateway);
    proxy
        .clone()
        .oneshot(hand_over(PORTAL_TOKEN, PERSON_TOKEN))
        .await
        .expect("an answer");

    for _ in 0..2 {
        let answer = proxy.clone().oneshot(read()).await.expect("an answer");
        assert_eq!(answer.status(), StatusCode::UNAUTHORIZED);
    }
    assert!(bearers_at_gateway(&gateway).await.is_empty());
    assert_eq!(
        bodies(&realm, TOKEN_PATH)
            .await
            .iter()
            .filter(|b| b.contains("grant_type=refresh_token"))
            .count(),
        1
    );
}

/// ADR-N-038 §3.5: a run the Portal holds as finished has its grant revoked and dropped.
#[tokio::test]
async fn a_finished_runs_grant_is_revoked() {
    let (realm, gateway) = (realm(300).await, gateway().await);
    let live = portal("building").await;
    let state = state(
        sample_run(false),
        Bases {
            realm: Some(format!("{}/realms/jc", realm.uri())),
            portal: live.uri(),
            gateway: gateway.uri(),
            ..Bases::default()
        },
    );
    let proxy = agent_proxy::router(Arc::clone(&state));
    proxy
        .clone()
        .oneshot(hand_over(PORTAL_TOKEN, PERSON_TOKEN))
        .await
        .expect("an answer");

    // A live run keeps its grant.
    state.credentials.grants().end_finished(&state.runs).await;
    assert!(bodies(&realm, REVOKE_PATH).await.is_empty());

    live.reset().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&live)
        .await;
    state.credentials.grants().end_finished(&state.runs).await;

    let revoked = bodies(&realm, REVOKE_PATH).await;
    assert_eq!(revoked.len(), 1);
    assert!(revoked[0].contains(&format!("token={REFRESH}")));
    assert!(revoked[0].contains("token_type_hint=refresh_token"));
    assert!(revoked[0].contains(&format!("client_secret={PROXY_SECRET}")));
    assert!(matches!(
        state.credentials.get_run_token(RUN_ID).await,
        Err(agent_proxy::delegation::GrantError::Missing)
    ));
}

/// AG-52: a workspace, a person's own Portal login and a stranger cannot hand over an identity;
/// the realm is asked about the caller before anything about the run.
#[tokio::test]
async fn only_the_portals_service_account_may_hand_over() {
    let (realm, portal, gateway) = (realm(300).await, portal("building").await, gateway().await);
    Mock::given(method("POST"))
        .and(path(INTROSPECT_PATH))
        .and(body_string_contains("token=persons-portal-login"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "active": true, "azp": "portal-api", "username": PERSON, "aud": [PROXY_CLIENT],
        })))
        .mount(&realm)
        .await;
    Mock::given(method("POST"))
        .and(path(INTROSPECT_PATH))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "active": false })))
        .mount(&realm)
        .await;
    let proxy = proxy(&realm, &portal, &gateway);

    let no_bearer = Request::builder()
        .method("POST")
        .uri(format!("/internal/runs/{RUN_ID}/identity"))
        .header("content-type", "application/json")
        .body(Body::from(
            json!({ "subjectToken": PERSON_TOKEN }).to_string(),
        ))
        .expect("a request");
    for request in [
        no_bearer,
        hand_over("persons-portal-login", PERSON_TOKEN),
        hand_over("forged-token", PERSON_TOKEN),
    ] {
        let answer = proxy.clone().oneshot(request).await.expect("an answer");
        assert_eq!(answer.status(), StatusCode::UNAUTHORIZED);
    }
    assert!(portal
        .received_requests()
        .await
        .unwrap_or_default()
        .is_empty());
    assert!(!bodies(&realm, TOKEN_PATH)
        .await
        .iter()
        .any(|b| b.contains("token-exchange")));
}

/// The token must be an active token of the run's own person; another person's is refused
/// before the exchange, and a refused exchange leaves the run without a grant.
#[tokio::test]
async fn another_persons_token_or_a_refused_exchange_binds_nothing() {
    let (realm, portal, gateway) = (realm(300).await, portal("building").await, gateway().await);
    Mock::given(method("POST"))
        .and(path(INTROSPECT_PATH))
        .and(body_string_contains("token=someone-elses-token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "active": true, "azp": "edge", "username": "someone.else@hel.fi",
        })))
        .mount(&realm)
        .await;
    let proxy = proxy(&realm, &portal, &gateway);

    let answer = proxy
        .clone()
        .oneshot(hand_over(PORTAL_TOKEN, "someone-elses-token"))
        .await
        .expect("an answer");
    assert_eq!(answer.status(), StatusCode::FORBIDDEN);
    assert!(!bodies(&realm, TOKEN_PATH)
        .await
        .iter()
        .any(|b| b.contains("token-exchange")));

    // The realm refuses the exchange itself.
    let refusing = realm_refusing_exchange().await;
    let proxy = self::proxy(&refusing, &portal, &gateway);
    let answer = proxy
        .clone()
        .oneshot(hand_over(PORTAL_TOKEN, PERSON_TOKEN))
        .await
        .expect("an answer");
    assert_eq!(answer.status(), StatusCode::FORBIDDEN);
    let answer = proxy.oneshot(read()).await.expect("an answer");
    assert_eq!(answer.status(), StatusCode::UNAUTHORIZED);
}

async fn realm_refusing_exchange() -> MockServer {
    let realm = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(TOKEN_PATH))
        .and(body_string_contains("token-exchange"))
        .respond_with(ResponseTemplate::new(403).set_body_json(json!({ "error": "access_denied" })))
        .mount(&realm)
        .await;
    for (token, body) in [
        (
            PORTAL_TOKEN,
            json!({ "active": true, "azp": "portal-api", "username": "service-account-portal-api", "aud": PROXY_CLIENT }),
        ),
        (
            PERSON_TOKEN,
            json!({ "active": true, "azp": "edge", "username": PERSON }),
        ),
    ] {
        Mock::given(method("POST"))
            .and(path(INTROSPECT_PATH))
            .and(body_string_contains(format!("token={token}")))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .mount(&realm)
            .await;
    }
    Mock::given(method("POST"))
        .and(path(TOKEN_PATH))
        .and(body_string_contains("grant_type=client_credentials"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "access_token": "proxy-own-token", "expires_in": 300,
        })))
        .mount(&realm)
        .await;
    realm
}

/// A hand-over for a run the Portal holds as finished takes no identity.
#[tokio::test]
async fn a_finished_run_takes_no_identity() {
    let (realm, portal, gateway) = (realm(300).await, portal("published").await, gateway().await);
    let answer = proxy(&realm, &portal, &gateway)
        .oneshot(hand_over(PORTAL_TOKEN, PERSON_TOKEN))
        .await
        .expect("an answer");
    assert_eq!(answer.status(), StatusCode::CONFLICT);
    assert!(!bodies(&realm, TOKEN_PATH)
        .await
        .iter()
        .any(|b| b.contains("token-exchange")));
}

/// Collects every log line of this suite's binary.
#[derive(Clone, Default)]
struct Lines(Arc<Mutex<Vec<u8>>>);

/// The one subscriber of this binary, installed on first use. A per-test default subscriber
/// misses lines: the suites run in parallel, and a callsite first hit on a thread with no
/// subscriber is cached as uninteresting for all of them.
fn captured() -> Lines {
    static LINES: std::sync::OnceLock<Lines> = std::sync::OnceLock::new();
    LINES
        .get_or_init(|| {
            let lines = Lines::default();
            let writer = lines.clone();
            tracing::subscriber::set_global_default(
                tracing_subscriber::fmt()
                    .with_max_level(tracing::Level::TRACE)
                    .with_writer(move || writer.clone())
                    .finish(),
            )
            .expect("the suite's subscriber is the only one");
            lines
        })
        .clone()
}

impl Write for Lines {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0
            .lock()
            .map_err(|_| std::io::Error::other("poisoned"))?
            .extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Security: the person's token, the delegated token and its refresh token reach no log line,
/// through a hand-over, a read, a refused refresh and a revocation.
#[tokio::test]
async fn no_token_reaches_a_log_line() {
    let lines = captured();

    let (realm, portal, gateway) = (realm(10).await, portal("building").await, gateway().await);
    Mock::given(method("POST"))
        .and(path(TOKEN_PATH))
        .and(body_string_contains("grant_type=refresh_token"))
        .respond_with(ResponseTemplate::new(400))
        .mount(&realm)
        .await;
    let state = state(
        sample_run(false),
        Bases {
            realm: Some(format!("{}/realms/jc", realm.uri())),
            portal: portal.uri(),
            gateway: gateway.uri(),
            ..Bases::default()
        },
    );
    let proxy = agent_proxy::router(Arc::clone(&state));
    proxy
        .clone()
        .oneshot(hand_over(PORTAL_TOKEN, PERSON_TOKEN))
        .await
        .expect("an answer");
    proxy
        .clone()
        .oneshot(hand_over(PORTAL_TOKEN, PERSON_TOKEN))
        .await
        .expect("an answer");
    proxy.oneshot(read()).await.expect("an answer");

    let logged = String::from_utf8_lossy(&lines.0.lock().expect("the lines")).into_owned();
    assert!(
        logged.contains("the run holds its person's grant"),
        "{logged}"
    );
    for token in [PORTAL_TOKEN, PERSON_TOKEN, DELEGATED, REFRESH, PROXY_SECRET] {
        assert!(!logged.contains(token), "{token} was logged: {logged}");
    }
}
