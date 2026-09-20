//! Unit and integration tests for `jc-agent-proxy` (refusal matrix and credential isolation).

use agent_proxy::config::Config;
use agent_proxy::inject::CredentialManager;
use agent_proxy::limits::LimitManager;
use agent_proxy::runs::{RunContext, RunResolver};
use agent_proxy::{router, ProxyState};
use argon2::{password_hash::SaltString, Argon2, PasswordHasher};
use axum::body::Body;
use axum::http::{Request, StatusCode};
use std::sync::Arc;
use tower::ServiceExt;

fn test_hash(ticket: &str) -> String {
    let salt = SaltString::generate(&mut argon2::password_hash::rand_core::OsRng);
    Argon2::default()
        .hash_password(ticket.as_bytes(), &salt)
        .unwrap()
        .to_string()
}

fn sample_run(allows_write: bool, status: &str) -> RunContext {
    RunContext {
        id: "e3b0c442-98fc-1c14-9afb-4c7b2756a120".to_string(),
        project: "helsinki".to_string(),
        app_name: "bikes".to_string(),
        endpoint_slug: "scsd2eehkx42n53z2zyd6vshfh7s7irf".to_string(),
        endpoint_slugs: vec![],
        allows_write,
        branch: "agent/app-bikes/e3b0c442-98fc-1c14-9afb-4c7b2756a120".to_string(),
        path_prefix: "projects/helsinki/apps/bikes/".to_string(),
        status: status.to_string(),
        ticket_hash: test_hash("secret-ticket-123"),
        max_tokens: 1000,
        allowed_hosts: vec!["crates.io".to_string()],
        requests_per_minute: 100,
        steps_per_run: 0,
        max_response_bytes: 1048576,
        max_egress_bytes_per_run: 0,
        created_by: "demo.steward@hel.fi".to_string(),
        model_name: "claude-3-7".to_string(),
        reasoning_effort: None,
    }
}

fn test_state(run: RunContext) -> Arc<ProxyState> {
    test_state_with_gateway(run, "http://context-gateway:8080")
}

fn test_state_with_gateway(run: RunContext, gateway: &str) -> Arc<ProxyState> {
    test_state_with_upstreams(run, gateway, "https://api.anthropic.com")
}

fn test_state_with_upstreams(run: RunContext, gateway: &str, model: &str) -> Arc<ProxyState> {
    test_state_with(run, gateway, model, "http://gitea-http:3000")
}

fn test_state_with_forge(run: RunContext, forge: &str) -> Arc<ProxyState> {
    test_state_with(
        run,
        "http://context-gateway:8080",
        "https://api.anthropic.com",
        forge,
    )
}

/// A proxy whose Portal is a stub, so a test can see what reached it and what never did.
fn test_state_with_portal(run: RunContext, portal: &str) -> Arc<ProxyState> {
    test_state_with_all(
        run,
        "http://context-gateway:8080",
        "https://api.anthropic.com",
        "http://gitea-http:3000",
        Some(portal),
    )
}

fn test_state_with(run: RunContext, gateway: &str, model: &str, forge: &str) -> Arc<ProxyState> {
    test_state_with_all(run, gateway, model, forge, None)
}

fn test_state_with_all(
    run: RunContext,
    gateway: &str,
    model: &str,
    forge: &str,
    portal: Option<&str>,
) -> Arc<ProxyState> {
    let portal = portal.map(str::to_string);
    let gateway = gateway.to_string();
    let model = model.to_string();
    let forge = forge.to_string();
    let config = Config::from_lookup(|k| match k {
        "JC_PROXY_BIND" => Some("127.0.0.1:0".to_string()),
        "JC_GATEWAY_BASE" => Some(gateway.clone()),
        "JC_MODEL_BASE" => Some(model.clone()),
        "JC_FORGE_BASE" => Some(forge.clone()),
        "JC_MODEL_KEY" => Some("mock-model-key".to_string()),
        "JC_FORGE_TOKEN" => Some("mock-forge-token".to_string()),
        "JC_PORTAL_BASE" => portal.clone(),
        _ => None,
    })
    .unwrap();

    let config_arc = Arc::new(config);
    let credentials = CredentialManager::new(config_arc.clone());
    // A test that names a Portal gets a resolver that asks it: an id this run does not hold is
    // then "no such run" (404 → 401) rather than "the Portal could not be reached" (503, T-2418).
    let runs = match portal.as_deref() {
        Some(base) => {
            RunResolver::with_cached_at(base.parse().expect("the portal base is a URL"), run)
        }
        None => RunResolver::with_cached(run),
    };
    let limits = LimitManager::default();
    let http = reqwest::Client::new();
    // No redirect of its own: the fetch route checks every hop against the run's allow-list.
    let egress = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .unwrap_or_default();

    Arc::new(ProxyState {
        config: config_arc,
        runs,
        credentials,
        limits,
        http,
        egress,
    })
}

#[tokio::test]
async fn missing_ticket_returns_401() {
    let app = router(test_state(sample_run(false, "building")));
    let req = Request::builder()
        .uri("/v1/data/ngsi-ld/v1/entities")
        .body(Body::empty())
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn invalid_ticket_returns_401() {
    let app = router(test_state(sample_run(false, "building")));
    let req = Request::builder()
        .uri("/v1/data/ngsi-ld/v1/entities")
        .header("x-jc-run", "e3b0c442-98fc-1c14-9afb-4c7b2756a120")
        .header("x-jc-ticket", "wrong-ticket")
        .body(Body::empty())
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn path_traversal_on_data_route_returns_403() {
    let app = router(test_state(sample_run(false, "building")));
    let req = Request::builder()
        .uri("/v1/data/../../admin")
        .header("x-jc-run", "e3b0c442-98fc-1c14-9afb-4c7b2756a120")
        .header("x-jc-ticket", "secret-ticket-123")
        .body(Body::empty())
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn write_on_readonly_run_returns_403() {
    let app = router(test_state(sample_run(false, "building")));
    let req = Request::builder()
        .method("PATCH")
        .uri("/v1/data/ngsi-ld/v1/entities/some-id/attrs")
        .header("x-jc-run", "e3b0c442-98fc-1c14-9afb-4c7b2756a120")
        .header("x-jc-ticket", "secret-ticket-123")
        .body(Body::from("{}"))
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn forge_write_outside_prefix_returns_403() {
    let app = router(test_state(sample_run(false, "building")));
    let req = Request::builder()
        .method("PUT")
        .uri("/v1/forge/contents/projects/other/secret.yaml")
        .header("x-jc-run", "e3b0c442-98fc-1c14-9afb-4c7b2756a120")
        .header("x-jc-ticket", "secret-ticket-123")
        .body(Body::from("{}"))
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn package_host_outside_allowlist_returns_403() {
    let app = router(test_state(sample_run(false, "building")));
    let req = Request::builder()
        .uri("/v1/packages/evil.example.com/package.tar.gz")
        .header("x-jc-run", "e3b0c442-98fc-1c14-9afb-4c7b2756a120")
        .header("x-jc-ticket", "secret-ticket-123")
        .body(Body::empty())
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn token_budget_exhaustion_returns_429() {
    let state = test_state(sample_run(false, "building"));
    state
        .limits
        .record_tokens("e3b0c442-98fc-1c14-9afb-4c7b2756a120", 2000)
        .await;

    let app = router(state);
    let req = Request::builder()
        .method("POST")
        .uri("/v1/llm/v1/chat/completions")
        .header("x-jc-run", "e3b0c442-98fc-1c14-9afb-4c7b2756a120")
        .header("x-jc-ticket", "secret-ticket-123")
        .body(Body::from("{}"))
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::TOO_MANY_REQUESTS);
}

#[tokio::test]
async fn the_model_call_past_the_profiles_step_limit_returns_429() {
    let mut run = sample_run(false, "building");
    run.steps_per_run = 1;
    let state = test_state(run);
    let app = router(state.clone());
    let call = || {
        Request::builder()
            .method("POST")
            .uri("/v1/llm/v1/chat/completions")
            .header("x-jc-run", "e3b0c442-98fc-1c14-9afb-4c7b2756a120")
            .header("x-jc-ticket", "secret-ticket-123")
            .body(Body::from("{}"))
            .unwrap()
    };
    // The first call spends the run's one step; it fails on the provider, not on the limit.
    let first = app.clone().oneshot(call()).await.unwrap();
    assert_ne!(first.status(), StatusCode::TOO_MANY_REQUESTS);
    let second = app.oneshot(call()).await.unwrap();
    assert_eq!(second.status(), StatusCode::TOO_MANY_REQUESTS);
}

#[tokio::test]
async fn terminal_run_returns_409() {
    let app = router(test_state(sample_run(false, "failed")));
    let req = Request::builder()
        .uri("/v1/data/ngsi-ld/v1/entities")
        .header("x-jc-run", "e3b0c442-98fc-1c14-9afb-4c7b2756a120")
        .header("x-jc-ticket", "secret-ticket-123")
        .body(Body::empty())
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[test]
fn config_debug_redacts_credentials() {
    let config = Config::from_lookup(|k| match k {
        "JC_MODEL_KEY" => Some("secret-model-key".to_string()),
        "JC_FORGE_TOKEN" => Some("secret-forge-token".to_string()),
        "JC_OIDC_CLIENT_SECRET" => Some("secret-oidc".to_string()),
        _ => None,
    })
    .unwrap();

    let debug_str = format!("{config:?}");
    assert!(!debug_str.contains("secret-model-key"));
    assert!(!debug_str.contains("secret-forge-token"));
    assert!(!debug_str.contains("secret-oidc"));
    assert!(debug_str.contains("[redacted]"));
}

#[tokio::test]
async fn the_ticket_is_accepted_as_a_bearer_token() {
    // An OpenAI-compatible model client sends nothing but `Authorization: Bearer <key>`, so the
    // ticket travels there as `jcr_<run>.<ticket>`. Anything past authentication is proof it was
    // read: this run is read-only, so a write is refused with 403 rather than 401.
    let app = router(test_state(sample_run(false, "building")));
    let req = Request::builder()
        .method("PATCH")
        .uri("/v1/data/ngsi-ld/v1/entities/some-id/attrs")
        .header(
            "authorization",
            "Bearer jcr_e3b0c442-98fc-1c14-9afb-4c7b2756a120.secret-ticket-123",
        )
        .body(Body::from("{}"))
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn a_bearer_with_the_wrong_ticket_is_refused() {
    let app = router(test_state(sample_run(false, "building")));
    for token in [
        "Bearer jcr_e3b0c442-98fc-1c14-9afb-4c7b2756a120.wrong-ticket",
        // No prefix, no separator, an empty half: none of these is a credential.
        "Bearer e3b0c442-98fc-1c14-9afb-4c7b2756a120.secret-ticket-123",
        "Bearer jcr_e3b0c442-98fc-1c14-9afb-4c7b2756a120",
        "Bearer jcr_.secret-ticket-123",
        "Bearer sk-or-v1-a-model-provider-key",
    ] {
        let req = Request::builder()
            .uri("/v1/data/ngsi-ld/v1/entities")
            .header("authorization", token)
            .body(Body::empty())
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::UNAUTHORIZED,
            "accepted a bad bearer: {token}"
        );
    }
}

#[tokio::test]
async fn the_inbox_needs_a_ticket_like_every_other_route() {
    let app = router(test_state(sample_run(false, "interviewing")));
    let req = Request::builder()
        .uri("/v1/runs/inbox?after=0")
        .body(Body::empty())
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

/// A data read reaches the gateway with its query string: the type, the attributes, the page
/// and `options=keyValues` are the read itself, and a proxy that dropped them would hand every
/// caller the first page of everything, normalized.
#[tokio::test]
async fn a_data_read_carries_its_query_string_to_the_gateway() {
    use wiremock::matchers::{method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let gateway = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(
            "/api/endpoint/scsd2eehkx42n53z2zyd6vshfh7s7irf/ngsi-ld/v1/entities",
        ))
        .and(query_param("type", "BikeHireDockingStation"))
        .and(query_param("options", "keyValues"))
        .and(query_param("offset", "500"))
        .and(query_param("attrs", "name,location"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([])))
        .expect(1)
        .mount(&gateway)
        .await;

    let app = router(test_state_with_gateway(
        sample_run(false, "building"),
        &gateway.uri(),
    ));
    let req = Request::builder()
        .uri("/v1/data/ngsi-ld/v1/entities?type=BikeHireDockingStation&options=keyValues&limit=500&offset=500&attrs=name,location")
        .header("x-jc-run", "e3b0c442-98fc-1c14-9afb-4c7b2756a120")
        .header("x-jc-ticket", "secret-ticket-123")
        .body(Body::empty())
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    gateway.verify().await;
}

/// The second endpoint of a run (AP-44, AG-75): its slug in the run context.
const KPIS: &str = "q3mzkq2v7w5ayxcbn4ltdj6hof2repgu";

fn two_endpoint_run(allows_write: bool) -> RunContext {
    let mut run = sample_run(allows_write, "building");
    run.endpoint_slugs = vec![run.endpoint_slug.clone(), KPIS.to_string()];
    run
}

fn ticketed(method: &str, uri: &str, body: Body) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(uri)
        .header("x-jc-run", "e3b0c442-98fc-1c14-9afb-4c7b2756a120")
        .header("x-jc-ticket", "secret-ticket-123")
        .header("content-type", "application/json")
        .body(body)
        .unwrap()
}

/// A read and an MCP call under the run's second endpoint reach that endpoint on the gateway,
/// each with a token minted for that endpoint's audience and not the primary's.
#[tokio::test]
async fn a_second_endpoint_of_the_run_is_reached_with_its_own_token() {
    use wiremock::matchers::{body_partial_json, header, method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let gateway = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(format!("/api/endpoint/{KPIS}/ngsi-ld/v1/entities")))
        .and(query_param("type", "KeyPerformanceIndicator"))
        .and(header(
            "authorization",
            format!("Bearer mock-token-for-{KPIS}").as_str(),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([])))
        .expect(1)
        .mount(&gateway)
        .await;
    Mock::given(method("POST"))
        .and(path(format!("/api/endpoint/{KPIS}/mcp")))
        .and(header(
            "authorization",
            format!("Bearer mock-token-for-{KPIS}").as_str(),
        ))
        .and(body_partial_json(
            serde_json::json!({ "method": "tools/call" }),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(
            serde_json::json!({ "jsonrpc": "2.0", "id": 1, "result": { "content": [] } }),
        ))
        .expect(1)
        .mount(&gateway)
        .await;

    let state = test_state_with_gateway(two_endpoint_run(false), &gateway.uri());
    let read = router(state.clone())
        .oneshot(ticketed(
            "GET",
            &format!("/v1/data/endpoints/{KPIS}/ngsi-ld/v1/entities?type=KeyPerformanceIndicator"),
            Body::empty(),
        ))
        .await
        .unwrap();
    assert_eq!(read.status(), StatusCode::OK);

    let call = serde_json::json!({
        "jsonrpc": "2.0", "id": 1, "method": "tools/call",
        "params": { "name": "query_entities", "arguments": { "type": "KeyPerformanceIndicator" } }
    });
    let mcp = router(state)
        .oneshot(ticketed(
            "POST",
            &format!("/v1/data/endpoints/{KPIS}/mcp"),
            Body::from(call.to_string()),
        ))
        .await
        .unwrap();
    assert_eq!(mcp.status(), StatusCode::OK);
    gateway.verify().await;
}

/// A slug the run does not name is refused, and nothing reaches the gateway.
#[tokio::test]
async fn an_endpoint_outside_the_run_is_refused() {
    use wiremock::MockServer;

    let gateway = MockServer::start().await;
    let app = router(test_state_with_gateway(
        two_endpoint_run(false),
        &gateway.uri(),
    ));
    let resp = app
        .oneshot(ticketed(
            "GET",
            "/v1/data/endpoints/zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz/ngsi-ld/v1/entities",
            Body::empty(),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    assert!(gateway
        .received_requests()
        .await
        .unwrap_or_default()
        .is_empty());

    // A run whose Portal sends no slug list may address its primary endpoint alone.
    let app = router(test_state_with_gateway(
        sample_run(false, "building"),
        &gateway.uri(),
    ));
    let resp = app
        .oneshot(ticketed(
            "GET",
            &format!("/v1/data/endpoints/{KPIS}/ngsi-ld/v1/entities"),
            Body::empty(),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}

/// A write tool of the MCP façade stays refused on a read-only run under a second endpoint.
#[tokio::test]
async fn a_write_tool_under_a_second_endpoint_is_refused_on_a_read_only_run() {
    use wiremock::MockServer;

    let gateway = MockServer::start().await;
    let app = router(test_state_with_gateway(
        two_endpoint_run(false),
        &gateway.uri(),
    ));
    let call = serde_json::json!({
        "jsonrpc": "2.0", "id": 1, "method": "tools/call",
        "params": { "name": "upsert_entity", "arguments": {} }
    });
    let resp = app
        .oneshot(ticketed(
            "POST",
            &format!("/v1/data/endpoints/{KPIS}/mcp"),
            Body::from(call.to_string()),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    assert!(gateway
        .received_requests()
        .await
        .unwrap_or_default()
        .is_empty());
}

/// AG-72: the profile's reasoning effort reaches the model provider on a call whose body names
/// none, beside everything the caller sent.
#[tokio::test]
async fn a_model_call_carries_the_profiles_reasoning_effort_upstream() {
    use wiremock::matchers::{body_json, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let provider = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .and(body_json(serde_json::json!({
            "model": "google/gemini-3.8-flash",
            "messages": [],
            "reasoning": { "effort": "medium" },
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
        .expect(1)
        .mount(&provider)
        .await;

    let mut run = sample_run(false, "building");
    run.reasoning_effort = Some("medium".to_string());
    let app = router(test_state_with_upstreams(
        run,
        "http://context-gateway:8080",
        &provider.uri(),
    ));
    let req = Request::builder()
        .method("POST")
        .uri("/v1/llm/v1/chat/completions")
        .header("x-jc-run", "e3b0c442-98fc-1c14-9afb-4c7b2756a120")
        .header("x-jc-ticket", "secret-ticket-123")
        .body(Body::from(
            r#"{"model":"google/gemini-3.8-flash","messages":[]}"#,
        ))
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    provider.verify().await;
}

#[tokio::test]
async fn diagnostics_refuses_an_unknown_component_before_asking_anyone() {
    let app = router(test_state(sample_run(false, "building")));
    let req = Request::builder()
        .uri("/v1/diagnostics/database/main")
        .header("x-jc-run", "e3b0c442-98fc-1c14-9afb-4c7b2756a120")
        .header("x-jc-ticket", "secret-ticket-123")
        .body(Body::empty())
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn diagnostics_refuses_an_id_that_is_not_a_name() {
    let app = router(test_state(sample_run(false, "building")));
    let req = Request::builder()
        .uri("/v1/diagnostics/pipeline/Hsl%20Bikes")
        .header("x-jc-run", "e3b0c442-98fc-1c14-9afb-4c7b2756a120")
        .header("x-jc-ticket", "secret-ticket-123")
        .body(Body::empty())
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn diagnostics_needs_the_run_ticket_like_every_door() {
    let app = router(test_state(sample_run(false, "building")));
    let req = Request::builder()
        .uri("/v1/diagnostics/pipeline/hsl-bikes")
        .body(Body::empty())
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

/// An endpoint the assistant added to the conversation after the proxy cached the run is reached
/// on the first call: the proxy asks the Portal again before it refuses (AG-75).
#[tokio::test]
async fn an_endpoint_added_since_the_run_was_cached_is_reached_at_once() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let gateway = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(format!("/api/endpoint/{KPIS}/ngsi-ld/v1/entities")))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([])))
        .expect(1)
        .mount(&gateway)
        .await;
    let cached = sample_run(false, "interviewing");
    let portal = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(format!("/internal/agent-runs/{}", cached.id)))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": cached.id,
            "project": cached.project,
            "appName": cached.app_name,
            "endpointSlug": cached.endpoint_slug,
            "endpointSlugs": [cached.endpoint_slug, KPIS],
            "allowsWrite": false,
            "branch": cached.branch,
            "pathPrefix": cached.path_prefix,
            "status": "interviewing",
            "ticketHash": cached.ticket_hash,
            "maxTokens": 1000,
            "allowedHosts": [],
            "requestsPerMinute": 100,
            "maxResponseBytes": 1048576,
            "createdBy": cached.created_by,
            "modelName": cached.model_name,
        })))
        .expect(1)
        .mount(&portal)
        .await;

    let state = test_state_with_gateway(cached.clone(), &gateway.uri());
    let state = Arc::new(ProxyState {
        runs: RunResolver::with_cached_at(portal.uri().parse().unwrap(), cached),
        ..(*state).clone()
    });
    let resp = router(state)
        .oneshot(ticketed(
            "GET",
            &format!("/v1/data/endpoints/{KPIS}/ngsi-ld/v1/entities?type=KeyPerformanceIndicator"),
            Body::empty(),
        ))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    gateway.verify().await;
    portal.verify().await;
}

/// T-0817: axum decodes the path once, so a double-encoded dot segment reaches the guard as
/// `%2e%2e`, which the URL parser on the way out would fold into `..`. Nothing encoded, no
/// dot segment and no empty segment gets past the application directory, and the forge is
/// never called for it.
#[tokio::test]
async fn a_double_encoded_dot_segment_never_leaves_the_application_directory() {
    let forge = wiremock::MockServer::start().await;
    wiremock::Mock::given(wiremock::matchers::method("PUT"))
        .respond_with(wiremock::ResponseTemplate::new(201).set_body_json(serde_json::json!({})))
        .mount(&forge)
        .await;
    let state = test_state_with_forge(sample_run(false, "building"), &forge.uri());
    for path in [
        "projects/helsinki/apps/bikes/%252e%252e/other/x",
        "projects/helsinki/apps/bikes/%2e%2e/other/x",
        "projects/helsinki/apps/bikes/./x",
        "projects/helsinki/apps/bikes//x",
        "projects/helsinki/apps/bikes/%2Fsecret",
    ] {
        let req = Request::builder()
            .method("PUT")
            .uri(format!("/v1/forge/contents/{path}"))
            .header("x-jc-run", "e3b0c442-98fc-1c14-9afb-4c7b2756a120")
            .header("x-jc-ticket", "secret-ticket-123")
            .body(Body::from("{}"))
            .unwrap();
        let resp = router(state.clone()).oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN, "{path}");
    }
    assert!(
        forge
            .received_requests()
            .await
            .unwrap_or_default()
            .is_empty(),
        "the forge was called"
    );

    // A file inside the directory still reaches the forge, on the run's branch.
    let req = Request::builder()
        .method("PUT")
        .uri("/v1/forge/contents/projects/helsinki/apps/bikes/src/App.tsx")
        .header("x-jc-run", "e3b0c442-98fc-1c14-9afb-4c7b2756a120")
        .header("x-jc-ticket", "secret-ticket-123")
        .body(Body::from(r#"{"content":"aGk=","message":"add app"}"#))
        .unwrap();
    let resp = router(state).oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let calls = forge.received_requests().await.unwrap_or_default();
    assert_eq!(calls.len(), 1);
    assert!(calls[0]
        .url
        .path()
        .ends_with("/contents/projects/helsinki/apps/bikes/src/App.tsx"));
}

/// Edge cases of the application-directory guard: a directory listing with its trailing slash
/// still reaches the forge, and a literal `..` is refused like an encoded one.
#[tokio::test]
async fn a_directory_listing_inside_the_application_passes_and_a_literal_dot_dot_does_not() {
    let forge = wiremock::MockServer::start().await;
    wiremock::Mock::given(wiremock::matchers::method("GET"))
        .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!([])))
        .mount(&forge)
        .await;
    let state = test_state_with_forge(sample_run(false, "building"), &forge.uri());

    let req = Request::builder()
        .method("GET")
        .uri("/v1/forge/contents/projects/helsinki/apps/bikes/")
        .header("x-jc-run", "e3b0c442-98fc-1c14-9afb-4c7b2756a120")
        .header("x-jc-ticket", "secret-ticket-123")
        .body(Body::empty())
        .unwrap();
    let resp = router(state.clone()).oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let req = Request::builder()
        .method("GET")
        .uri("/v1/forge/contents/projects/helsinki/apps/bikes/../other/x")
        .header("x-jc-run", "e3b0c442-98fc-1c14-9afb-4c7b2756a120")
        .header("x-jc-ticket", "secret-ticket-123")
        .body(Body::empty())
        .unwrap();
    let resp = router(state).oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    assert_eq!(forge.received_requests().await.unwrap_or_default().len(), 1);
}

/// The egress allow-list of T-0557: what the builder may read, how much of it, and what the
/// upstream is never told (AG-50, AG-65).
mod fetch {
    use super::*;

    fn run_with_egress(hosts: &[&str], budget: u64) -> RunContext {
        RunContext {
            allowed_hosts: hosts.iter().map(|h| (*h).to_string()).collect(),
            max_egress_bytes_per_run: budget,
            ..sample_run(false, "running")
        }
    }

    async fn fetch(state: Arc<ProxyState>, url: &str) -> axum::http::Response<Body> {
        let uri = format!(
            "/v1/fetch?url={}",
            url::form_urlencoded::byte_serialize(url.as_bytes()).collect::<String>()
        );
        router(state)
            .oneshot(ticketed("GET", &uri, Body::empty()))
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn a_profile_that_names_no_host_reaches_nothing() {
        // The default, and the one the kit builder keeps: no allow-list, no budget, no door.
        let state = test_state(run_with_egress(&[], 0));
        let resp = fetch(state, "https://docs.maplibre.org/api/").await;
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn a_host_the_profile_did_not_name_is_refused_with_the_rule_that_refused_it() {
        let state = test_state(run_with_egress(&["docs.maplibre.org"], 1024));
        let resp = fetch(state, "https://cdn.evil.test/payload").await;
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
        let body = axum::body::to_bytes(resp.into_body(), 64 * 1024)
            .await
            .unwrap();
        let said = String::from_utf8_lossy(&body);
        assert!(said.contains("egress allow-list"), "{said}");
        assert!(said.contains("AG-50"), "{said}");
    }

    #[tokio::test]
    async fn a_run_whose_budget_is_spent_is_refused_before_anything_leaves() {
        // Nothing listens on the host below, so a 429 here is proof the budget is checked
        // before the request rather than after it.
        let state = test_state(run_with_egress(&["docs.maplibre.org"], 512));
        state
            .limits
            .record_egress(&sample_run(false, "running").id, 512, 512)
            .await;
        let resp = fetch(state, "https://docs.maplibre.org/api/").await;
        assert_eq!(resp.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(
            resp.headers()
                .get("X-JC-Egress-Remaining")
                .and_then(|v| v.to_str().ok()),
            Some("0")
        );
    }

    #[tokio::test]
    async fn the_budget_counts_down_across_fetches_of_one_run_and_not_between_runs() {
        let limits = LimitManager::default();
        assert_eq!(limits.egress_remaining("run-1", 1000).await, 1000);
        assert_eq!(limits.record_egress("run-1", 600, 1000).await, 400);
        assert_eq!(limits.record_egress("run-1", 400, 1000).await, 0);
        assert_eq!(limits.egress_remaining("run-1", 1000).await, 0);
        assert_eq!(limits.egress_remaining("run-2", 1000).await, 1000);
        // One answer may cross the budget; the arithmetic must not wrap when it does.
        assert_eq!(limits.record_egress("run-1", 5_000, 1000).await, 0);
    }

    #[tokio::test]
    async fn a_url_with_userinfo_or_a_token_parameter_is_refused_and_so_is_a_missing_one() {
        let state = test_state(run_with_egress(&["docs.maplibre.org"], 1024));
        for url in [
            "https://user:pass@docs.maplibre.org/api/",
            "https://docs.maplibre.org/api/?access_token=abcdef",
        ] {
            let resp = fetch(state.clone(), url).await;
            assert_eq!(resp.status(), StatusCode::BAD_REQUEST, "{url}");
        }
        let resp = router(state)
            .oneshot(ticketed("GET", "/v1/fetch", Body::empty()))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn a_fetch_without_a_ticket_is_refused_like_every_other_route() {
        let state = test_state(run_with_egress(&["docs.maplibre.org"], 1024));
        let request = Request::builder()
            .method("GET")
            .uri("/v1/fetch?url=https%3A%2F%2Fdocs.maplibre.org%2Fapi%2F")
            .body(Body::empty())
            .unwrap();
        let resp = router(state).oneshot(request).await.unwrap();
        assert!(
            resp.status() == StatusCode::UNAUTHORIZED || resp.status() == StatusCode::FORBIDDEN,
            "an unauthenticated fetch answered {}",
            resp.status()
        );
    }
}

/// One ceiling on every door (AG-41, T-0811): the proxy is shared by every run in the
/// organization, so a body one run sends is memory taken from all of them.
mod request_bodies {
    use super::*;
    use agent_proxy::routes::body::MAX_REQUEST_BYTES;
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn oversized() -> Body {
        Body::from(vec![b'x'; MAX_REQUEST_BYTES + 1])
    }

    #[tokio::test]
    async fn a_body_past_the_ceiling_is_refused_on_the_data_route_and_never_forwarded() {
        let gateway = MockServer::start().await;
        Mock::given(wiremock::matchers::any())
            .respond_with(ResponseTemplate::new(200))
            .mount(&gateway)
            .await;
        let state = test_state_with_gateway(sample_run(true, "running"), &gateway.uri());

        let resp = router(state)
            .oneshot(ticketed(
                "POST",
                "/v1/data/ngsi-ld/v1/entities",
                oversized(),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::PAYLOAD_TOO_LARGE);
        assert!(
            gateway
                .received_requests()
                .await
                .unwrap_or_default()
                .is_empty(),
            "the gateway was asked to read a body the proxy refused"
        );
    }

    #[tokio::test]
    async fn a_body_past_the_ceiling_is_refused_on_the_forge_route_and_never_forwarded() {
        let forge = MockServer::start().await;
        Mock::given(wiremock::matchers::any())
            .respond_with(ResponseTemplate::new(200))
            .mount(&forge)
            .await;
        let state = test_state_with_forge(sample_run(true, "running"), &forge.uri());

        let resp = router(state)
            .oneshot(ticketed(
                "POST",
                "/v1/forge/contents/projects/helsinki/apps/bikes/src/app.tsx",
                oversized(),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::PAYLOAD_TOO_LARGE);
        assert!(
            forge
                .received_requests()
                .await
                .unwrap_or_default()
                .is_empty(),
            "the forge was asked to read a body the proxy refused"
        );
    }

    #[tokio::test]
    async fn a_body_at_the_ceiling_still_goes_through() {
        // The ceiling is a ceiling, not a margin: the largest legitimate body is a model call
        // carrying a long conversation, and refusing it would break the run it belongs to.
        let gateway = MockServer::start().await;
        Mock::given(wiremock::matchers::any())
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
            .mount(&gateway)
            .await;
        let state = test_state_with_gateway(sample_run(true, "running"), &gateway.uri());

        let body = serde_json::json!({ "filler": "x".repeat(MAX_REQUEST_BYTES - 64) }).to_string();
        assert!(body.len() <= MAX_REQUEST_BYTES);
        let resp = router(state)
            .oneshot(ticketed(
                "POST",
                "/v1/data/ngsi-ld/v1/entities",
                Body::from(body),
            ))
            .await
            .unwrap();
        assert_ne!(resp.status(), StatusCode::PAYLOAD_TOO_LARGE);
        assert_eq!(
            gateway.received_requests().await.unwrap_or_default().len(),
            1
        );
    }
}

/// T-1074, AG-40/AG-52: a header the workspace sets does not reach the gateway. The run's own
/// ticket is the only identity on that hop, so a workspace that writes `authorization`,
/// `ngsild-tenant` or the edge's own `x-userinfo` must have them dropped, not forwarded.
#[tokio::test]
async fn a_header_the_workspace_wrote_never_reaches_the_gateway() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let gateway = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(
            "/api/endpoint/scsd2eehkx42n53z2zyd6vshfh7s7irf/ngsi-ld/v1/entities",
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([])))
        .expect(1)
        .mount(&gateway)
        .await;

    let app = router(test_state_with_gateway(
        sample_run(false, "building"),
        &gateway.uri(),
    ));
    let response = app
        .oneshot(
            Request::builder()
                .uri("/v1/data/ngsi-ld/v1/entities?type=Vehicle")
                .header("x-jc-run", "e3b0c442-98fc-1c14-9afb-4c7b2756a120")
                .header("x-jc-ticket", "secret-ticket-123")
                // What a workspace might try: somebody else's session, another tenant, the
                // edge's own identity headers, and a scope it was never granted.
                .header("authorization", "Bearer somebody-elses-token")
                .header("cookie", "jc_session=someone")
                .header("ngsild-tenant", "another-space")
                .header("x-userinfo", "eyJzdWIiOiJtYWxsb3J5In0=")
                .header("x-access-token", "another-token")
                .header("x-allowed-scope-ids", "*")
                .header("x-endpoint-slug", "some-other-endpoint")
                .header("x-consumer-identity", "mallory")
                .header("accept", "application/json")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let seen = gateway.received_requests().await.unwrap_or_default();
    assert_eq!(seen.len(), 1);
    let sent = &seen[0].headers;
    // The proxy's own bearer is there, and it is the only authorization.
    assert_eq!(
        sent.get("authorization")
            .and_then(|value| value.to_str().ok()),
        Some("Bearer mock-token-for-scsd2eehkx42n53z2zyd6vshfh7s7irf")
    );
    for dropped in [
        "cookie",
        "ngsild-tenant",
        "x-userinfo",
        "x-access-token",
        "x-allowed-scope-ids",
        "x-endpoint-slug",
        "x-consumer-identity",
    ] {
        assert!(
            sent.get(dropped).is_none(),
            "the workspace's `{dropped}` reached the gateway"
        );
    }
    // A header that is nobody's identity travels, so this is a list and not a wall.
    assert!(sent.get("accept").is_some());
}

/// The forge hop carries no client header at all: the request is built from the method, the URL
/// and the proxy's own forge token, so there is nothing of the workspace's on it to strip.
#[tokio::test]
async fn the_forge_request_is_the_proxys_own_and_carries_nothing_of_the_workspaces() {
    use wiremock::matchers::method;
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let forge = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(serde_json::json!({ "content": "" })),
        )
        .expect(1)
        .mount(&forge)
        .await;

    let app = router(test_state_with_forge(
        sample_run(false, "building"),
        &forge.uri(),
    ));
    let response = app
        .oneshot(
            Request::builder()
                .uri("/v1/forge/contents/projects/helsinki/apps/bikes/README.md")
                .header("x-jc-run", "e3b0c442-98fc-1c14-9afb-4c7b2756a120")
                .header("x-jc-ticket", "secret-ticket-123")
                .header("authorization", "Bearer somebody-elses-token")
                .header("cookie", "jc_session=someone")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_ne!(response.status(), StatusCode::UNAUTHORIZED);

    let seen = forge.received_requests().await.unwrap_or_default();
    assert_eq!(seen.len(), 1);
    let sent = &seen[0].headers;
    assert!(
        sent.get("authorization")
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| value.starts_with("token ")),
        "the forge sees the proxy's own token and not a bearer somebody wrote"
    );
    assert!(sent.get("cookie").is_none());
}

/// T-0957, AG-40/AG-56: an event is the model's own words, and a credential that slips into
/// them is stored, streamed to every reader of the run and put back in front of the model.
/// The proxy redacts before the Portal ever sees it, as it does on the diagnostics door.
#[tokio::test]
async fn a_credential_in_an_event_is_redacted_before_the_portal_stores_it() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    // Assembled, so this source holds no string a secret scanner would take for a real one.
    let github = format!("ghp_{}", "abc123def456ghi789jkl012");
    let portal = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/internal/agent-runs/events"))
        .respond_with(ResponseTemplate::new(202).set_body_json(serde_json::json!({ "seq": 1 })))
        .mount(&portal)
        .await;

    let app = router(test_state_with_portal(
        sample_run(false, "building"),
        &portal.uri(),
    ));
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/runs/events")
                .header("x-jc-run", "e3b0c442-98fc-1c14-9afb-4c7b2756a120")
                .header("x-jc-ticket", "secret-ticket-123")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::json!({
                        "kind": "tool",
                        "payload": {
                            "name": "fetch",
                            "output": format!("git push failed: {github} is not authorized"),
                            "status": "failed",
                        },
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::ACCEPTED);

    let seen = portal.received_requests().await.unwrap_or_default();
    assert_eq!(seen.len(), 1);
    let sent = String::from_utf8_lossy(&seen[0].body);
    assert!(
        !sent.contains(&github),
        "the token reached the Portal: {sent}"
    );
    assert!(sent.contains("[REDACTED]"), "{sent}");
    // The event is still an event: the shape the Portal stores is the one the run sent.
    let sent: serde_json::Value = serde_json::from_str(&sent).expect("the body is still JSON");
    assert_eq!(sent["kind"], serde_json::json!("tool"));
    assert_eq!(sent["payload"]["name"], serde_json::json!("fetch"));
    assert_eq!(sent["payload"]["status"], serde_json::json!("failed"));
    assert_eq!(
        sent["runId"],
        serde_json::json!("e3b0c442-98fc-1c14-9afb-4c7b2756a120")
    );
}

/// T-0981, AG-46/AG-52: the inbox a workspace reads is its own run's, because the run comes off
/// the ticket. Nothing in the request names a run, and a request that tries to name one is
/// answered with the caller's own inbox all the same.
#[tokio::test]
async fn the_inbox_is_the_ticket_s_run_and_never_one_the_request_names() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let other = "11111111-2222-3333-4444-555555555555";
    let portal = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(
            "/internal/agent-runs/e3b0c442-98fc-1c14-9afb-4c7b2756a120/inbox",
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({ "items": [] })))
        .mount(&portal)
        .await;

    let app = router(test_state_with_portal(
        sample_run(false, "building"),
        &portal.uri(),
    ));
    let resp = app
        .oneshot(
            Request::builder()
                .uri(format!("/v1/runs/inbox?after=0&run={other}&id={other}"))
                .header("x-jc-run", "e3b0c442-98fc-1c14-9afb-4c7b2756a120")
                .header("x-jc-ticket", "secret-ticket-123")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let asked = portal.received_requests().await.unwrap_or_default();
    assert_eq!(asked.len(), 1);
    assert!(
        !asked[0].url.as_str().contains(other),
        "a run the request named reached the Portal: {}",
        asked[0].url
    );
}

/// T-0850, AG-45/AG-46: an event is one line of a conversation. The route's own 64 KiB ceiling
/// (Architecture/19 §4) refuses a larger one before the Portal ever sees it, so a workspace
/// cannot decide how much of the run store and of every reader's stream one event takes.
#[tokio::test]
async fn an_event_over_the_cap_is_refused_and_never_reaches_the_portal() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let portal = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/internal/agent-runs/events"))
        .respond_with(ResponseTemplate::new(202).set_body_json(serde_json::json!({ "seq": 1 })))
        .mount(&portal)
        .await;

    let event = |text: &str| {
        Body::from(
            serde_json::to_vec(
                &serde_json::json!({ "kind": "message", "payload": { "text": text } }),
            )
            .unwrap(),
        )
    };
    let send = |body: Body| {
        let app = router(test_state_with_portal(
            sample_run(false, "building"),
            &portal.uri(),
        ));
        app.oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/runs/events")
                .header("x-jc-run", "e3b0c442-98fc-1c14-9afb-4c7b2756a120")
                .header("x-jc-ticket", "secret-ticket-123")
                .header("content-type", "application/json")
                .body(body)
                .unwrap(),
        )
    };

    // One byte over, counted on what arrived rather than on what it parses into.
    let resp = send(event(&"x".repeat(65 * 1024))).await.unwrap();
    assert_eq!(resp.status(), StatusCode::PAYLOAD_TOO_LARGE);
    assert!(
        portal
            .received_requests()
            .await
            .unwrap_or_default()
            .is_empty(),
        "the over-size event was forwarded"
    );

    // An event that fits is forwarded, so the cap refuses size and nothing else.
    let resp = send(event("the pipeline is green")).await.unwrap();
    assert_eq!(resp.status(), StatusCode::ACCEPTED);
    assert_eq!(
        portal.received_requests().await.unwrap_or_default().len(),
        1
    );

    // A small body that is not an event is still the old refusal, not a 413.
    let resp = send(Body::from("{\"kind\":")).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        portal.received_requests().await.unwrap_or_default().len(),
        1
    );
}

/// T-0969, AG-46: the run a call is made under comes from the ticket, never from the body, so a
/// workspace naming another run or another project in its JSON-RPC reaches its own run's door
/// and nothing else. The Portal then applies the starting person's own permissions to whatever
/// project the body names, so the proxy does not need to know them.
#[tokio::test]
async fn the_body_cannot_name_another_run_or_project() {
    let app = router(test_state(sample_run(false, "running")));
    let req = Request::builder()
        .method("POST")
        .uri("/v1/mcp")
        .header("x-jc-run", "e3b0c442-98fc-1c14-9afb-4c7b2756a120")
        .header("x-jc-ticket", "secret-ticket-123")
        .header("content-type", "application/json")
        .body(Body::from(
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "tools/call",
                "params": {
                    "name": "jc_resource_list",
                    "arguments": { "project": "another-department", "kind": "Role" },
                    "runId": "00000000-0000-0000-0000-000000000000"
                }
            })
            .to_string(),
        ))
        .unwrap();

    // The Portal is not reachable in this test, so the answer is the upstream failure rather
    // than a success: what matters is that the proxy did not accept the body's own run id as
    // the one to call under, which would be a 200 from another run's door.
    let resp = app.oneshot(req).await.unwrap();
    assert_ne!(
        resp.status(),
        StatusCode::OK,
        "a body naming another run is never answered from that run"
    );
}

/// T-1300, T-1301: a path that would leave its base once the outbound URL is parsed — a
/// double-encoded `..`, a backslash, a dot segment — is refused on the data route before the
/// gateway is asked, and on the packages route before anything leaves the proxy; an ordinary
/// read under the endpoint still reaches the gateway.
#[tokio::test]
async fn a_path_that_would_leave_its_base_is_refused_on_the_data_and_packages_routes() {
    let gateway = wiremock::MockServer::start().await;
    wiremock::Mock::given(wiremock::matchers::method("GET"))
        .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!([])))
        .mount(&gateway)
        .await;
    let state = test_state_with_gateway(sample_run(false, "building"), &gateway.uri());
    for uri in [
        "/v1/data/%252e%252e/%252e%252e/other-endpoint/ngsi-ld/v1/entities",
        "/v1/data/ngsi-ld/v1/%252e%252e/entities",
        "/v1/data/ngsi-ld%5C..%5Cadmin",
        "/v1/data/./ngsi-ld/v1/entities",
        "/v1/packages/crates.io/%252e%252e/%252e%252e/admin",
        "/v1/packages/crates.io/../../etc/passwd",
        "/v1/packages/crates.io/api%5C..%5Cadmin",
    ] {
        let resp = router(state.clone())
            .oneshot(ticketed("GET", uri, Body::empty()))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN, "{uri}");
    }
    assert!(
        gateway
            .received_requests()
            .await
            .unwrap_or_default()
            .is_empty(),
        "the gateway was called"
    );

    let resp = router(state)
        .oneshot(ticketed(
            "GET",
            "/v1/data/ngsi-ld/v1/entities?type=Bike",
            Body::empty(),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        gateway.received_requests().await.unwrap_or_default().len(),
        1
    );
}

/// AG-52: the mesh identity is the second factor, so only the run's own workload passes it.
///
/// `l5d-client-id` was matched with `contains`, which admitted every identity that merely held the
/// string: a longer run id, and any ServiceAccount or namespace named around it (T-1477).
mod mesh_identity {
    use super::*;

    const RUN: &str = "e3b0c442-98fc-1c14-9afb-4c7b2756a120";
    const TICKET: &str = "secret-ticket-123";

    /// A proxy that demands the mesh identity, with a stub gateway to answer what passes.
    async fn proxy_and_gateway() -> (Arc<ProxyState>, wiremock::MockServer) {
        use wiremock::matchers::method;
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let gateway = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([])))
            .mount(&gateway)
            .await;
        let uri = gateway.uri();
        let config = Config::from_lookup(|key| match key {
            "JC_PROXY_BIND" => Some("127.0.0.1:0".to_string()),
            "JC_GATEWAY_BASE" => Some(uri.clone()),
            "JC_MODEL_BASE" => Some("https://api.anthropic.com".to_string()),
            "JC_FORGE_BASE" => Some("http://gitea-http:3000".to_string()),
            "JC_MODEL_KEY" => Some("mock-model-key".to_string()),
            "JC_FORGE_TOKEN" => Some("mock-forge-token".to_string()),
            "JC_REQUIRE_MESH_IDENTITY" => Some("true".to_string()),
            _ => None,
        })
        .expect("the test configuration parses");
        assert!(
            config.require_mesh_identity,
            "this suite is about the check being on"
        );
        let config = Arc::new(config);
        let state = Arc::new(ProxyState {
            credentials: CredentialManager::new(config.clone()),
            runs: RunResolver::with_cached(sample_run(false, "building")),
            limits: LimitManager::default(),
            http: reqwest::Client::new(),
            egress: reqwest::Client::new(),
            config,
        });
        (state, gateway)
    }

    async fn read_as(state: Arc<ProxyState>, identity: Option<&str>) -> StatusCode {
        let mut request = Request::builder()
            .uri("/v1/data/ngsi-ld/v1/entities?type=Bike")
            .header("x-jc-run", RUN)
            .header("x-jc-ticket", TICKET);
        if let Some(identity) = identity {
            request = request.header("l5d-client-id", identity);
        }
        router(state)
            .oneshot(request.body(Body::empty()).unwrap())
            .await
            .unwrap()
            .status()
    }

    #[tokio::test]
    async fn a_mesh_identity_that_only_contains_the_run_is_refused() {
        let (state, _gateway) = proxy_and_gateway().await;
        for identity in [
            // The run id with one more character: another run's workload.
            &format!("agent-run-{RUN}1.jc-agents.serviceaccount.identity.linkerd.cluster.local"),
            // An account named around this run's name.
            &format!("x-agent-run-{RUN}.jc-agents.serviceaccount.identity.linkerd.cluster.local"),
            // The name in a later label rather than in the ServiceAccount.
            &format!(
                "something-else.agent-run-{RUN}.serviceaccount.identity.linkerd.cluster.local"
            ),
        ] {
            assert_eq!(
                read_as(state.clone(), Some(identity)).await,
                StatusCode::FORBIDDEN,
                "{identity} is not this run's workload"
            );
        }
    }

    #[tokio::test]
    async fn the_runs_own_mesh_identity_passes() {
        let (state, gateway) = proxy_and_gateway().await;
        let identity =
            format!("agent-run-{RUN}.jc-agents.serviceaccount.identity.linkerd.cluster.local");
        assert_eq!(
            read_as(state, Some(&identity)).await,
            StatusCode::OK,
            "the run's own workload reads"
        );
        assert_eq!(
            gateway.received_requests().await.unwrap_or_default().len(),
            1
        );
    }

    #[tokio::test]
    async fn a_missing_or_empty_mesh_identity_is_refused() {
        let (state, gateway) = proxy_and_gateway().await;
        assert_eq!(read_as(state.clone(), None).await, StatusCode::FORBIDDEN);
        assert_eq!(read_as(state, Some("")).await, StatusCode::FORBIDDEN);
        assert!(
            gateway
                .received_requests()
                .await
                .unwrap_or_default()
                .is_empty(),
            "a refused request never reaches the gateway"
        );
    }
}

/// AG-52, T-2271: the proxy names itself to the Portal with a token of its own client, minted from
/// the realm and audience-bound to the internal listener. It presented `JC_PROXY_TOKEN` — one string
/// the Portal held as well, which never rotates and which either side can leak.
#[tokio::test]
async fn a_callback_presents_a_minted_token_and_never_a_configured_string() {
    use wiremock::matchers::{body_string_contains, header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let realm = MockServer::start().await;
    // The grant this proxy asks for: its own client, for the Portal's internal listener.
    Mock::given(method("POST"))
        .and(path("/realms/dev/protocol/openid-connect/token"))
        .and(body_string_contains("grant_type=client_credentials"))
        .and(body_string_contains("audience=portal-internal"))
        .and(body_string_contains("client_id=helsinki-agent-proxy"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "access_token": "minted-for-the-listener",
            "expires_in": 300,
        })))
        .expect(1..)
        .mount(&realm)
        .await;

    let portal = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/internal/agent-runs/events"))
        .and(header("authorization", "Bearer minted-for-the-listener"))
        .respond_with(ResponseTemplate::new(204))
        .expect(1)
        .mount(&portal)
        .await;

    let portal_uri = portal.uri();
    let realm_uri = realm.uri();
    let config = Config::from_lookup(|k| match k {
        "JC_PROXY_BIND" => Some("127.0.0.1:0".to_string()),
        "JC_GATEWAY_BASE" => Some("http://context-gateway:8080".to_string()),
        "JC_MODEL_BASE" => Some("https://api.anthropic.com".to_string()),
        "JC_FORGE_BASE" => Some("http://gitea-http:3000".to_string()),
        "JC_MODEL_KEY" => Some("mock-model-key".to_string()),
        "JC_FORGE_TOKEN" => Some("mock-forge-token".to_string()),
        "JC_PORTAL_BASE" => Some(portal_uri.clone()),
        "JC_OIDC_ISSUER" => Some(format!("{realm_uri}/realms/dev")),
        "JC_OIDC_CLIENT_ID" => Some("helsinki-agent-proxy".to_string()),
        // A secret is what makes the manager mint rather than answer a stub, and the token URL is
        // the one the deployment names: a pod cannot dial its own cluster's public hostname.
        "JC_OIDC_CLIENT_SECRET" => Some("the-proxys-own-secret".to_string()),
        "JC_OIDC_TOKEN_URL" => Some(format!(
            "{realm_uri}/realms/dev/protocol/openid-connect/token"
        )),
        _ => None,
    })
    .unwrap();

    let config_arc = Arc::new(config);
    let credentials = CredentialManager::new(config_arc.clone());
    let state = Arc::new(ProxyState {
        config: config_arc,
        runs: RunResolver::with_cached(sample_run(false, "building")),
        credentials,
        limits: LimitManager::default(),
        http: reqwest::Client::new(),
        egress: reqwest::Client::new(),
    });

    let resp = router(state)
        .oneshot(ticketed(
            "POST",
            "/v1/runs/events",
            Body::from(
                serde_json::json!({ "kind": "thought", "payload": { "text": "hm" } }).to_string(),
            ),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    realm.verify().await;
    portal.verify().await;
}

/// A realm that cannot be reached costs the callback a 503, and the reason — which names the realm
/// and the client — never reaches the run.
#[tokio::test]
async fn a_callback_with_no_token_answers_503_and_says_nothing_about_the_realm() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let realm = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/realms/dev/protocol/openid-connect/token"))
        .respond_with(ResponseTemplate::new(401).set_body_json(serde_json::json!({
            "error": "invalid_client",
            "error_description": "Invalid client or Invalid client credentials",
        })))
        .mount(&realm)
        .await;

    let realm_uri = realm.uri();
    let config = Config::from_lookup(|k| match k {
        "JC_PROXY_BIND" => Some("127.0.0.1:0".to_string()),
        "JC_GATEWAY_BASE" => Some("http://context-gateway:8080".to_string()),
        "JC_MODEL_BASE" => Some("https://api.anthropic.com".to_string()),
        "JC_FORGE_BASE" => Some("http://gitea-http:3000".to_string()),
        "JC_MODEL_KEY" => Some("mock-model-key".to_string()),
        "JC_FORGE_TOKEN" => Some("mock-forge-token".to_string()),
        "JC_PORTAL_BASE" => Some("http://portal.invalid".to_string()),
        "JC_OIDC_ISSUER" => Some(format!("{realm_uri}/realms/dev")),
        "JC_OIDC_CLIENT_ID" => Some("helsinki-agent-proxy".to_string()),
        "JC_OIDC_CLIENT_SECRET" => Some("the-wrong-secret".to_string()),
        _ => None,
    })
    .unwrap();

    let config_arc = Arc::new(config);
    let credentials = CredentialManager::new(config_arc.clone());
    let state = Arc::new(ProxyState {
        config: config_arc,
        runs: RunResolver::with_cached(sample_run(false, "building")),
        credentials,
        limits: LimitManager::default(),
        http: reqwest::Client::new(),
        egress: reqwest::Client::new(),
    });

    let resp = router(state)
        .oneshot(ticketed(
            "POST",
            "/v1/runs/events",
            Body::from(
                serde_json::json!({ "kind": "thought", "payload": { "text": "hm" } }).to_string(),
            ),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    let body = axum::body::to_bytes(resp.into_body(), 64 * 1024)
        .await
        .unwrap();
    let text = String::from_utf8_lossy(&body);
    assert!(!text.contains("invalid_client"), "{text}");
    assert!(!text.contains("the-wrong-secret"), "{text}");
    assert!(!text.contains("realms/dev"), "{text}");
}

// ---------------------------------------------------------------------------------------------
// T-1872, EP-26, MP-02: the edge cases of `auth::authenticate`.
//
// The contract, in one sentence: a request is a run's own only when it presents that run's id with
// a ticket that verifies against the stored Argon2id hash — in either of the two forms, the two
// headers or `Authorization: Bearer jcr_<run>.<ticket>` — and, where the mesh identity is required,
// from that run's own workload; every failure is the same refusal.
//
// `missing_ticket_returns_401`, `invalid_ticket_returns_401` and `the_ticket_is_accepted_as_a_bearer_token`
// above cover the happy path and the two obvious refusals.
// ---------------------------------------------------------------------------------------------

const RUN: &str = "e3b0c442-98fc-1c14-9afb-4c7b2756a120";
const TICKET: &str = "secret-ticket-123";

/// One request through the router with the headers a test chose.
async fn with_headers(headers: &[(&str, &str)]) -> StatusCode {
    // A Portal that holds no run and answers 404 to every lookup: these cases are about the
    // credential presented, and a Portal that cannot be reached is a different answer (T-2418).
    let portal = wiremock::MockServer::start().await;
    let app = router(test_state_with_portal(
        sample_run(false, "building"),
        &portal.uri(),
    ));
    let mut req = Request::builder().uri("/v1/data/ngsi-ld/v1/entities");
    for (name, value) in headers {
        req = req.header(*name, *value);
    }
    app.oneshot(req.body(Body::empty()).unwrap())
        .await
        .unwrap()
        .status()
}

#[tokio::test]
async fn no_shape_of_a_half_presented_credential_authenticates() {
    for headers in [
        vec![],
        vec![("x-jc-run", RUN)],       // the run without its ticket
        vec![("x-jc-ticket", TICKET)], // the ticket without its run
        vec![("x-jc-run", ""), ("x-jc-ticket", TICKET)], // an empty run id
        vec![("x-jc-run", RUN), ("x-jc-ticket", "")], // an empty ticket
        vec![("x-jc-run", " "), ("x-jc-ticket", TICKET)], // whitespace is not a run id
        vec![("authorization", "")],
        vec![("authorization", "Bearer")],
        vec![("authorization", "Bearer ")],
        vec![("authorization", "Bearer jcr_")],
        vec![("authorization", "Bearer jcr_.")], // both halves empty
        vec![("authorization", "Bearer jcr_.ticket")], // no run id
        vec![("authorization", "Bearer jcr_run.")], // no ticket
        vec![("authorization", "Bearer jcr_runticket")], // no separator
        vec![("authorization", "Basic anNyXw==")], // another scheme
        vec![("authorization", "bearer jcr_run.ticket")], // the scheme as this proxy writes it
        vec![("authorization", "Bearer JCR_run.ticket")], // the prefix is lower case
        vec![("authorization", "Bearer run.ticket")], // no prefix at all
    ] {
        assert_eq!(
            with_headers(&headers).await,
            StatusCode::UNAUTHORIZED,
            "{headers:?} must not authenticate",
        );
    }
}

#[tokio::test]
async fn a_ticket_that_is_not_this_runs_never_verifies_however_close_it_is() {
    for ticket in [
        "secret-ticket-12",
        "secret-ticket-1234",
        "SECRET-TICKET-123",
        "secret-ticket-123 x",
        // A NUL inside the ticket is struck as impossible: `HeaderValue::from_str` refuses it
        // (`http::Error(InvalidHeaderValue)`), so no such request exists to answer.
    ] {
        assert_eq!(
            with_headers(&[("x-jc-run", RUN), ("x-jc-ticket", ticket)]).await,
            StatusCode::UNAUTHORIZED,
            "{ticket:?} is not the ticket",
        );
    }
    // The right one does, in both forms, so the refusals above are about the ticket and not the shape.
    assert_ne!(
        with_headers(&[("x-jc-run", RUN), ("x-jc-ticket", TICKET)]).await,
        StatusCode::UNAUTHORIZED,
    );
    assert_ne!(
        with_headers(&[("authorization", &format!("Bearer jcr_{RUN}.{TICKET}"))]).await,
        StatusCode::UNAUTHORIZED,
    );
}

#[tokio::test]
async fn the_ticket_may_hold_the_separator_and_is_taken_whole() {
    // `split_once` keeps everything after the first dot, so a ticket containing dots survives; a run
    // id may not, which is what makes the split unambiguous.
    let app = router(test_state({
        let mut run = sample_run(false, "building");
        run.ticket_hash = test_hash("a.b.c");
        run
    }));
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/v1/data/ngsi-ld/v1/entities")
                .header("authorization", format!("Bearer jcr_{RUN}.a.b.c"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_ne!(
        resp.status(),
        StatusCode::UNAUTHORIZED,
        "a ticket with dots is one ticket"
    );
}

#[tokio::test]
async fn surrounding_whitespace_is_not_part_of_the_credential() {
    assert_ne!(
        with_headers(&[("x-jc-run", RUN), ("x-jc-ticket", &format!(" {TICKET} "))]).await,
        StatusCode::UNAUTHORIZED,
        "a client that pads its header still authenticates",
    );
    assert_ne!(
        with_headers(&[("authorization", &format!("Bearer  jcr_{RUN}.{TICKET}  "))]).await,
        StatusCode::UNAUTHORIZED,
    );
}

#[tokio::test]
async fn the_headers_are_read_before_the_bearer_and_a_wrong_pair_is_not_rescued_by_a_right_bearer()
{
    // A request that presents both must not pass on the weaker of the two: the headers are the
    // credential when they are there, so a wrong pair is a wrong credential even beside a good
    // bearer. Otherwise a caller who stole one form could launder it through the other.
    assert_eq!(
        with_headers(&[
            ("x-jc-run", RUN),
            ("x-jc-ticket", "wrong-ticket"),
            ("authorization", &format!("Bearer jcr_{RUN}.{TICKET}")),
        ])
        .await,
        StatusCode::UNAUTHORIZED,
    );
}

#[tokio::test]
async fn a_run_that_is_not_the_stored_one_is_refused_whatever_the_ticket() {
    for run in [
        "e3b0c442-98fc-1c14-9afb-4c7b2756a121",
        "E3B0C442-98FC-1C14-9AFB-4C7B2756A120",
        &format!("{RUN}x"),
        &RUN[..RUN.len() - 1],
        "../../etc/passwd",
        "%65%33%62%30",
    ] {
        assert_eq!(
            with_headers(&[("x-jc-run", run), ("x-jc-ticket", TICKET)]).await,
            StatusCode::UNAUTHORIZED,
            "{run:?} is not the run",
        );
    }
}
#[tokio::test]
async fn a_refusal_says_the_same_thing_whether_the_run_or_the_ticket_was_wrong() {
    // Otherwise the proxy is an oracle for run ids: a prober learns which ids exist by reading the
    // difference between the two answers (EP-26, R20).
    let unknown = body_of(&[("x-jc-run", "no-such-run"), ("x-jc-ticket", TICKET)]).await;
    let wrong_ticket = body_of(&[("x-jc-run", RUN), ("x-jc-ticket", "wrong-ticket")]).await;
    assert_eq!(
        unknown, wrong_ticket,
        "an unknown run and a wrong ticket answer differently:\n  {unknown}\n  {wrong_ticket}",
    );
}

/// The body of one refusal, for comparing two of them.
async fn body_of(headers: &[(&str, &str)]) -> String {
    let portal = wiremock::MockServer::start().await;
    let app = router(test_state_with_portal(
        sample_run(false, "building"),
        &portal.uri(),
    ));
    let mut req = Request::builder().uri("/v1/data/ngsi-ld/v1/entities");
    for (name, value) in headers {
        req = req.header(*name, *value);
    }
    let resp = app.oneshot(req.body(Body::empty()).unwrap()).await.unwrap();
    let bytes = axum::body::to_bytes(resp.into_body(), 64 * 1024)
        .await
        .unwrap();
    String::from_utf8_lossy(&bytes).into_owned()
}

// ---------------------------------------------------------------------------------------------
// T-1873, AG-22, AG-25, AG-64: the edge cases of `router`.
//
// The contract, in one sentence: the proxy answers exactly the paths it declares, every one of them
// only to a run that presented its ticket, `/healthz` alone without one, and anything else is one
// constant refusal that says nothing about what does exist.
// ---------------------------------------------------------------------------------------------

/// Every route of the proxy, with the method it takes.
const ROUTES: &[(&str, &str)] = &[
    ("GET", "/v1/data/ngsi-ld/v1/entities"),
    ("GET", "/v1/data/endpoints/some-slug/ngsi-ld/v1/entities"),
    ("GET", "/v1/forge/repos/org/repo/contents/file.yaml"),
    ("GET", "/v1/fetch?url=https://crates.io/api/v1/crates/serde"),
    ("POST", "/v1/llm/v1/messages"),
    ("GET", "/v1/packages/crates.io/api/v1/crates/serde"),
    ("GET", "/v1/diagnostics/pipeline/some-id"),
    ("POST", "/v1/mcp"),
    ("POST", "/v1/runs/events"),
    ("GET", "/v1/runs/inbox"),
];

async fn answer(method: &str, uri: &str, ticket: bool) -> StatusCode {
    let app = router(test_state(sample_run(false, "building")));
    let mut req = Request::builder().method(method).uri(uri);
    if ticket {
        req = req.header("x-jc-run", RUN).header("x-jc-ticket", TICKET);
    }
    app.oneshot(req.body(Body::empty()).unwrap())
        .await
        .unwrap()
        .status()
}

#[tokio::test]
async fn every_route_but_the_health_check_needs_a_ticket() {
    for (method, uri) in ROUTES {
        assert_eq!(
            answer(method, uri, false).await,
            StatusCode::UNAUTHORIZED,
            "{method} {uri} answered without a ticket",
        );
    }
    // The health check is the one door with nothing behind it: no ticket, and nothing about the run.
    let app = router(test_state(sample_run(false, "building")));
    let health = app
        .oneshot(
            Request::builder()
                .uri("/healthz")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(health.status(), StatusCode::OK);
    let body = axum::body::to_bytes(health.into_body(), 1024)
        .await
        .unwrap();
    assert_eq!(
        &body[..],
        b"ok",
        "the health check says nothing but that it is up"
    );
}

#[tokio::test]
async fn a_path_the_proxy_does_not_declare_is_one_constant_refusal() {
    let app = router(test_state(sample_run(false, "building")));
    let refusal = app
        .oneshot(
            Request::builder()
                .uri("/v1/not-a-route")
                .header("x-jc-run", RUN)
                .header("x-jc-ticket", TICKET)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(refusal.status(), StatusCode::FORBIDDEN);
    let expected = axum::body::to_bytes(refusal.into_body(), 4096)
        .await
        .unwrap();

    // The same answer for a bare prefix, a trailing slash, another case, an encoded separator, a
    // traversal and the paths of the platform's other components: none of them says whether
    // something is there.
    for uri in [
        "/",
        "/v1",
        "/v1/",
        "/v1/data",
        "/v1/fetch/",
        "/V1/FETCH",
        "/v1/%6c%6c%6d/v1/messages",
        "/api/v1/projects/helsinki/spaces",
        "/ngsi-ld/v1/entities",
        "/metrics",
        "/.env",
        "/v1/runs/inbox/extra",
    ] {
        let app = router(test_state(sample_run(false, "building")));
        let response = app
            .oneshot(
                Request::builder()
                    .uri(uri)
                    .header("x-jc-run", RUN)
                    .header("x-jc-ticket", TICKET)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        assert!(
            status == StatusCode::FORBIDDEN || status == StatusCode::NOT_FOUND,
            "{uri} answered {status}",
        );
        if status == StatusCode::FORBIDDEN {
            let body = axum::body::to_bytes(response.into_body(), 4096)
                .await
                .unwrap();
            assert_eq!(body, expected, "{uri} answered a different refusal");
        }
    }
}

#[tokio::test]
async fn climbing_out_of_a_wildcard_route_is_refused_by_name() {
    // `/v1/data/{*rest}` and its siblings take the rest of the path whole, so `..` inside it is the
    // one way to aim at a path the proxy never declared. The handler's own guard answers it, and it
    // says traversal rather than the fallback's sentence — which is the answer worth keeping: a
    // refusal by name is what an operator reads in the log, and a caller learns only that it was
    // refused. What must never happen is the climb reaching another route or an upstream.
    for uri in [
        "/v1/data/../healthz",
        "/v1/data/..%2fhealthz",
        "/v1/data/ngsi-ld/../../../etc/passwd",
        "/v1/forge/../../healthz",
        "/v1/packages/crates.io/../../healthz",
    ] {
        let app = router(test_state(sample_run(false, "building")));
        let response = app
            .oneshot(
                Request::builder()
                    .uri(uri)
                    .header("x-jc-run", RUN)
                    .header("x-jc-ticket", TICKET)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            StatusCode::FORBIDDEN,
            "{uri} was not refused"
        );
        let body = axum::body::to_bytes(response.into_body(), 4096)
            .await
            .unwrap();
        let body = String::from_utf8(body.to_vec()).unwrap();
        assert!(
            !body.contains("ok"),
            "{uri} reached the health check: {body}",
        );
    }
}

#[tokio::test]
async fn a_route_takes_the_method_it_declares_and_no_other() {
    // A method nobody declared is not a way in: axum answers 405 and no handler runs, so no ticket
    // check is skipped and no upstream is called.
    for (declared, uri) in ROUTES {
        let other = if *declared == "GET" { "POST" } else { "GET" };
        let status = answer(other, uri, true).await;
        assert!(
            status == StatusCode::METHOD_NOT_ALLOWED
                || status == StatusCode::FORBIDDEN
                || status == StatusCode::NOT_FOUND
                // `/v1/data` and `/v1/forge` take any method by design (AG-22): the run's own write
                // permission decides, which `write_on_readonly_run_returns_403` covers.
                || uri.starts_with("/v1/data")
                || uri.starts_with("/v1/forge"),
            "{other} {uri} answered {status}",
        );
    }
}

// ---------------------------------------------------------------------------------------------
// T-2362, T-2363, T-2364: what a body the proxy cannot read, an accounting kind and an upstream
// outage are allowed to do.
// ---------------------------------------------------------------------------------------------

/// The whole body of a response as text, for the assertions that are about what is *not* in it.
async fn text_of(response: axum::response::Response) -> String {
    let bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap_or_default();
    String::from_utf8_lossy(&bytes).into_owned()
}

/// AG-64: a body the proxy cannot read as an object is a body it cannot pin to the run's branch,
/// so it is refused rather than forwarded with the platform's forge token and no branch at all.
#[tokio::test]
async fn a_forge_body_that_is_not_an_object_is_refused_rather_than_forwarded_unpinned() {
    let forge = wiremock::MockServer::start().await;
    wiremock::Mock::given(wiremock::matchers::method("PUT"))
        .respond_with(wiremock::ResponseTemplate::new(201).set_body_json(serde_json::json!({})))
        .mount(&forge)
        .await;
    wiremock::Mock::given(wiremock::matchers::method("POST"))
        .respond_with(wiremock::ResponseTemplate::new(201).set_body_json(serde_json::json!({})))
        .mount(&forge)
        .await;
    wiremock::Mock::given(wiremock::matchers::method("DELETE"))
        .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
        .mount(&forge)
        .await;
    let state = test_state_with_forge(sample_run(true, "building"), &forge.uri());

    let inside = "contents/projects/helsinki/apps/bikes/src/main.rs";
    for (verb, rest) in [
        ("PUT", inside),
        ("POST", inside),
        ("DELETE", inside),
        ("POST", "branches"),
        ("POST", "pulls"),
    ] {
        for body in ["not json", "[1,2,3]", "\"a string\"", "42"] {
            let response = router(state.clone())
                .oneshot(ticketed(
                    verb,
                    &format!("/v1/forge/{rest}"),
                    Body::from(body),
                ))
                .await
                .unwrap();
            assert_eq!(
                response.status(),
                StatusCode::BAD_REQUEST,
                "{verb} {rest} with {body}"
            );
        }
    }
    assert!(
        forge
            .received_requests()
            .await
            .unwrap_or_default()
            .is_empty(),
        "the forge was reached with a body the proxy could not pin"
    );
}

/// AG-22, AG-25: every `contents/` write carries the run's branch, its authorship and the
/// trailer naming the person who proposed the run. Gitea creates with POST as well as updating
/// with PUT, so all three verbs are pinned, not only the two.
#[tokio::test]
async fn every_contents_write_carries_the_runs_branch_and_its_author() {
    for verb in ["POST", "PUT", "DELETE"] {
        let forge = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method(verb))
            .respond_with(wiremock::ResponseTemplate::new(201).set_body_json(serde_json::json!({})))
            .mount(&forge)
            .await;
        let state = test_state_with_forge(sample_run(true, "building"), &forge.uri());

        let response = router(state)
            .oneshot(ticketed(
                verb,
                "/v1/forge/contents/projects/helsinki/apps/bikes/src/main.rs",
                Body::from(r#"{"content":"aGk=","message":"add main"}"#),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::CREATED, "{verb}");

        let calls = forge.received_requests().await.unwrap_or_default();
        assert_eq!(calls.len(), 1, "{verb}");
        let sent: serde_json::Value = serde_json::from_slice(&calls[0].body).expect("a JSON body");
        assert_eq!(
            sent.get("branch").and_then(serde_json::Value::as_str),
            Some("agent/app-bikes/e3b0c442-98fc-1c14-9afb-4c7b2756a120"),
            "{verb} was not pinned to the run's branch"
        );
        assert_eq!(
            sent.pointer("/author/name")
                .and_then(serde_json::Value::as_str),
            Some("agent:app-builder@helsinki"),
            "{verb} carried no author"
        );
        assert!(
            sent.get("message")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|message| message.contains("Co-Proposed-By: demo.steward@hel.fi")),
            "{verb} carried no proposer trailer: {sent}"
        );
    }
}

/// AG-64: a `DELETE` that carries everything in its query has no body to insert into, and the
/// branch is still what the run is allowed to write on.
#[tokio::test]
async fn a_contents_write_with_an_empty_body_is_still_pinned_to_the_runs_branch() {
    let forge = wiremock::MockServer::start().await;
    wiremock::Mock::given(wiremock::matchers::method("DELETE"))
        .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
        .mount(&forge)
        .await;
    let state = test_state_with_forge(sample_run(true, "building"), &forge.uri());

    let response = router(state)
        .oneshot(ticketed(
            "DELETE",
            "/v1/forge/contents/projects/helsinki/apps/bikes/src/main.rs",
            Body::empty(),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let calls = forge.received_requests().await.unwrap_or_default();
    assert_eq!(calls.len(), 1);
    let sent: serde_json::Value = serde_json::from_slice(&calls[0].body).expect("a JSON body");
    assert_eq!(
        sent.get("branch").and_then(serde_json::Value::as_str),
        Some("agent/app-bikes/e3b0c442-98fc-1c14-9afb-4c7b2756a120")
    );
}

/// AG-64: the branch a run may create is its own, and an unreadable body does not excuse the
/// check. A pull request is opened from the run's branch whatever the body asked for.
#[tokio::test]
async fn a_run_creates_its_own_branch_and_opens_a_pull_request_from_it() {
    let forge = wiremock::MockServer::start().await;
    wiremock::Mock::given(wiremock::matchers::method("POST"))
        .respond_with(wiremock::ResponseTemplate::new(201).set_body_json(serde_json::json!({})))
        .mount(&forge)
        .await;
    let state = test_state_with_forge(sample_run(true, "building"), &forge.uri());

    let refused = router(state.clone())
        .oneshot(ticketed(
            "POST",
            "/v1/forge/branches",
            Body::from(r#"{"new_branch_name":"main"}"#),
        ))
        .await
        .unwrap();
    assert_eq!(refused.status(), StatusCode::FORBIDDEN);
    assert!(forge
        .received_requests()
        .await
        .unwrap_or_default()
        .is_empty());

    let opened = router(state)
        .oneshot(ticketed(
            "POST",
            "/v1/forge/pulls",
            Body::from(r#"{"head":"main","base":"main","title":"take everything"}"#),
        ))
        .await
        .unwrap();
    assert_eq!(opened.status(), StatusCode::CREATED);
    let calls = forge.received_requests().await.unwrap_or_default();
    assert_eq!(calls.len(), 1);
    let sent: serde_json::Value = serde_json::from_slice(&calls[0].body).expect("a JSON body");
    assert_eq!(
        sent.get("head").and_then(serde_json::Value::as_str),
        Some("agent/app-bikes/e3b0c442-98fc-1c14-9afb-4c7b2756a120"),
        "the head was taken from the body instead of the run"
    );
}

/// AG-25, AG-41: `usage` is the proxy's own accounting channel, posted by `routes::llm` after
/// every model call and applied by the Portal to the run's tokens and step count. A workspace
/// that could post it would be writing the record its own budget is read from.
#[tokio::test]
async fn a_workspace_cannot_write_the_proxys_own_accounting_kinds() {
    let portal = wiremock::MockServer::start().await;
    wiremock::Mock::given(wiremock::matchers::method("POST"))
        .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
        .mount(&portal)
        .await;
    let state = test_state_with_portal(sample_run(true, "building"), &portal.uri());

    for kind in ["usage", "USAGE", "Usage"] {
        let sent = format!(r#"{{"kind":"{kind}","payload":{{"tokensThisStep":-999999}}}}"#);
        let response = router(state.clone())
            .oneshot(ticketed("POST", "/v1/runs/events", Body::from(sent)))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN, "kind '{kind}'");
    }
    assert!(
        portal
            .received_requests()
            .await
            .unwrap_or_default()
            .is_empty(),
        "the Portal was asked to record usage a workspace typed"
    );
}

/// The kinds the run owns still go through: its output and its navigation are its own to send.
#[tokio::test]
async fn the_kinds_a_run_owns_still_reach_the_portal() {
    let portal = wiremock::MockServer::start().await;
    wiremock::Mock::given(wiremock::matchers::method("POST"))
        .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
        .mount(&portal)
        .await;
    let state = test_state_with_portal(sample_run(true, "building"), &portal.uri());

    for kind in ["preview", "navigate", "message"] {
        let sent = format!(r#"{{"kind":"{kind}","payload":{{"text":"hello"}}}}"#);
        let response = router(state.clone())
            .oneshot(ticketed("POST", "/v1/runs/events", Body::from(sent)))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK, "kind '{kind}'");
    }
    assert_eq!(
        portal.received_requests().await.unwrap_or_default().len(),
        3
    );
}

/// AG-35, AG-40, AG-64: an upstream that cannot be reached is an outage the run is told about,
/// not a map of the cluster it is told. The workspace's NetworkPolicy lets it reach kube-dns and
/// this proxy and nothing else, precisely so it cannot learn where the Portal, the gateway, the
/// forge and the model provider live; a transport error string hands it all four on every outage.
#[tokio::test]
async fn an_unreachable_upstream_names_no_internal_address() {
    const GATEWAY: &str = "http://context-gateway.invalid:8080";
    const MODEL: &str = "http://anthropic.invalid:8443";
    const FORGE: &str = "http://gitea-http.invalid:3000";
    const PORTAL: &str = "http://portal-internal.invalid:8080";
    const SLUG: &str = "scsd2eehkx42n53z2zyd6vshfh7s7irf";

    let state = test_state_with_all(
        sample_run(true, "building"),
        GATEWAY,
        MODEL,
        FORGE,
        Some(PORTAL),
    );

    for (verb, uri, body) in [
        ("GET", "/v1/data/ngsi-ld/v1/entities", ""),
        ("GET", "/v1/runs/inbox", ""),
        (
            "POST",
            "/v1/runs/events",
            r#"{"kind":"message","payload":{}}"#,
        ),
        (
            "POST",
            "/v1/mcp",
            r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#,
        ),
        ("GET", "/v1/diagnostics/pipeline/bikes", ""),
        (
            "PUT",
            "/v1/forge/contents/projects/helsinki/apps/bikes/src/main.rs",
            r#"{"content":"aGk=","message":"add main"}"#,
        ),
        (
            "POST",
            "/v1/llm/v1/chat/completions",
            r#"{"model":"claude-3-7","messages":[]}"#,
        ),
    ] {
        let response = router(state.clone())
            .oneshot(ticketed(verb, uri, Body::from(body)))
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            StatusCode::BAD_GATEWAY,
            "{verb} {uri} did not answer 502"
        );
        let seen = text_of(response).await;
        for hidden in [
            "context-gateway.invalid",
            "anthropic.invalid",
            "gitea-http.invalid",
            "portal-internal.invalid",
            ":8080",
            ":8443",
            ":3000",
            "/api/endpoint/",
            "/internal/agent-runs",
            "/api/v1/repos/",
            SLUG,
        ] {
            assert!(
                !seen.contains(hidden),
                "{verb} {uri} handed the workspace '{hidden}': {seen}"
            );
        }
        assert!(
            seen.contains("upstream-unavailable") && seen.contains("requestId"),
            "{verb} {uri} gave the run no correlation id to quote: {seen}"
        );
    }
}

/// The package registry is the host the run itself named, so the refusal is the same shape as
/// every other one rather than a raw `reqwest` string.
#[tokio::test]
async fn an_unreachable_package_registry_answers_the_same_problem_document() {
    let state = test_state(sample_run(false, "building"));
    let response = router(state)
        .oneshot(ticketed(
            "GET",
            "/v1/packages/crates.io/api/v1/crates/serde/1.0.0/download",
            Body::empty(),
        ))
        .await
        .unwrap();
    // crates.io is reachable from a build machine and not from CI; either way the answer is the
    // registry's own or the proxy's problem document, never a transport error string.
    if response.status() == StatusCode::BAD_GATEWAY {
        let seen = text_of(response).await;
        assert!(seen.contains("upstream-unavailable"), "{seen}");
        assert!(!seen.contains("error sending request"), "{seen}");
    }
}

/// A correlation id is minted per refusal, so two outages are two lines in the log.
#[tokio::test]
async fn two_outages_carry_two_correlation_ids() {
    let state = test_state_with_gateway(sample_run(false, "building"), "http://nowhere.invalid:1");
    let mut ids = Vec::new();
    for _ in 0..2 {
        let response = router(state.clone())
            .oneshot(ticketed(
                "GET",
                "/v1/data/ngsi-ld/v1/entities",
                Body::empty(),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
        let body: serde_json::Value =
            serde_json::from_str(&text_of(response).await).expect("a problem document");
        ids.push(
            body.get("requestId")
                .and_then(serde_json::Value::as_str)
                .expect("a correlation id")
                .to_owned(),
        );
    }
    assert_ne!(ids[0], ids[1], "both outages were logged under one id");
}
