//! T-1695, AG-52, AG-38 — the attack: *the credential proxy hands a token to the wrong host, path
//! or run*.
//!
//! The proxy is the only thing between a workspace that holds no credential and four that matter:
//! the endpoint tokens it mints, the forge token, the model key, and its own token on the Portal's
//! internal listener. Every step below is one way a workspace, or an upstream a workspace can
//! provoke, would have one of those spent somewhere the platform never addressed.
//!
//! What the older suites already play, and is therefore not repeated here, by test name in
//! `proxy_tests.rs`: a stolen or half-presented ticket
//! (`no_shape_of_a_half_presented_credential_authenticates`,
//! `a_ticket_that_is_not_this_runs_never_verifies_however_close_it_is`,
//! `the_headers_are_read_before_the_bearer_and_a_wrong_pair_is_not_rescued_by_a_right_bearer`),
//! a run id that is not the stored one (`a_run_that_is_not_the_stored_one_is_refused_whatever_the_ticket`),
//! the identical wording of the two refusals (`a_refusal_says_the_same_thing_whether_the_run_or_the_ticket_was_wrong`),
//! a path that would leave the upstream base
//! (`a_path_that_would_leave_its_base_is_refused_on_the_data_and_packages_routes`,
//! `a_double_encoded_dot_segment_never_leaves_the_application_directory`,
//! `climbing_out_of_a_wildcard_route_is_refused_by_name`), and an endpoint slug outside the run
//! (`an_endpoint_outside_the_run_is_refused`). What was left, and is here, is the run id as a path
//! on the Portal and the redirect as a way to move a credentialed request.

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
use wiremock::matchers::any;
use wiremock::{Mock, MockServer, ResponseTemplate};

const RUN: &str = "e3b0c442-98fc-1c14-9afb-4c7b2756a120";
const TICKET: &str = "secret-ticket-123";
const SLUG: &str = "scsd2eehkx42n53z2zyd6vshfh7s7irf";

fn test_hash(ticket: &str) -> String {
    let salt = SaltString::generate(&mut argon2::password_hash::rand_core::OsRng);
    Argon2::default()
        .hash_password(ticket.as_bytes(), &salt)
        .expect("the ticket hashes")
        .to_string()
}

fn sample_run(status: &str) -> RunContext {
    RunContext {
        id: RUN.to_owned(),
        project: "helsinki".to_owned(),
        app_name: "bikes".to_owned(),
        endpoint_slug: SLUG.to_owned(),
        endpoint_slugs: vec![],
        allows_write: true,
        branch: format!("agent/app-bikes/{RUN}"),
        path_prefix: "projects/helsinki/apps/bikes/".to_owned(),
        repository: None,
        status: status.to_owned(),
        ticket_hash: test_hash(TICKET),
        max_tokens: 100_000,
        allowed_hosts: vec!["crates.io".to_owned()],
        requests_per_minute: 1000,
        steps_per_run: 0,
        max_response_bytes: 1_048_576,
        max_egress_bytes_per_run: 0,
        created_by: "demo.steward@hel.fi".to_owned(),
        model_name: "claude-3-7".to_owned(),
        reasoning_effort: None,
    }
}

/// Every upstream of the proxy, each one a URL a test can point at a stub.
struct Upstreams<'a> {
    gateway: &'a str,
    model: &'a str,
    forge: &'a str,
    portal: &'a str,
}

fn config_for(upstreams: &Upstreams<'_>) -> Arc<Config> {
    let (gateway, model, forge, portal) = (
        upstreams.gateway.to_owned(),
        upstreams.model.to_owned(),
        upstreams.forge.to_owned(),
        upstreams.portal.to_owned(),
    );
    Arc::new(
        Config::from_lookup(|key| match key {
            "JC_PROXY_BIND" => Some("127.0.0.1:0".to_owned()),
            "JC_GATEWAY_BASE" => Some(gateway.clone()),
            "JC_MODEL_BASE" => Some(model.clone()),
            "JC_FORGE_BASE" => Some(forge.clone()),
            "JC_PORTAL_BASE" => Some(portal.clone()),
            "JC_MODEL_KEY" => Some("mock-model-key".to_owned()),
            "JC_FORGE_TOKEN" => Some("mock-forge-token".to_owned()),
            _ => None,
        })
        .expect("the test configuration parses"),
    )
}

/// A proxy that holds `run` and asks nobody about it, with the given upstreams.
fn proxy_holding(run: RunContext, upstreams: &Upstreams<'_>) -> Arc<ProxyState> {
    let config = config_for(upstreams);
    let portal: url::Url = upstreams.portal.parse().expect("the portal base is a URL");
    Arc::new(ProxyState::new(
        config.clone(),
        RunResolver::with_cached_at(portal, run),
        CredentialManager::new(config),
        LimitManager::default(),
    ))
}

/// A proxy that holds no run at all, so every credential it is shown is looked up on the Portal
/// with this proxy's own token — which is what the first two cases are about.
fn proxy_asking_the_portal(portal: &str) -> Arc<ProxyState> {
    let upstreams = Upstreams {
        gateway: "http://context-gateway:8080",
        model: "https://api.anthropic.com",
        forge: "http://gitea-http:3000",
        portal,
    };
    let config = config_for(&upstreams);
    let credentials = CredentialManager::new(config.clone());
    let runs = RunResolver::new(
        portal.parse().expect("the portal base is a URL"),
        credentials.clone(),
    );
    Arc::new(ProxyState::new(
        config,
        runs,
        credentials,
        LimitManager::default(),
    ))
}

fn request(uri: &str, run: &str) -> Request<Body> {
    Request::builder()
        .uri(uri)
        .header("x-jc-run", run)
        .header("x-jc-ticket", TICKET)
        .body(Body::empty())
        .expect("the request builds")
}

// ---------------------------------------------------------------------------------------------
// Step 1 — a run id that is a path. The lookup that resolves it carries this proxy's own token on
// the Portal's internal listener, so an id read as a path spends that token on another route.
// ---------------------------------------------------------------------------------------------

/// T-1695, AG-52: no run id a workspace writes becomes a path on the Portal.
#[tokio::test]
async fn a_run_id_shaped_like_a_path_never_steers_the_portal_lookup() {
    let portal = MockServer::start().await;
    // Anything at all that reaches the Portal is recorded, so the assertion can be about the
    // whole traffic and not only about the answer.
    Mock::given(any())
        .respond_with(ResponseTemplate::new(404))
        .mount(&portal)
        .await;

    for run_id in [
        "../../internal/agent-runs",
        "..%2f..%2fadmin",
        "%2e%2e/%2e%2e/admin",
        &format!("{RUN}/../../admin"),
        &format!("{RUN}?audience=portal-internal"),
        &format!("{RUN}#fragment"),
        &format!("{RUN} extra"),
        "..",
        &"a".repeat(65),
    ] {
        let state = proxy_asking_the_portal(&portal.uri());
        let response = router(state)
            .oneshot(request("/v1/data/ngsi-ld/v1/entities", run_id))
            .await
            .expect("the router answers");
        assert_eq!(
            response.status(),
            StatusCode::UNAUTHORIZED,
            "{run_id:?} was not refused",
        );
    }

    let seen = portal.received_requests().await.unwrap_or_default();
    assert!(
        seen.is_empty(),
        "the Portal was asked about a string that cannot be a run: {:?}",
        seen.iter().map(|r| r.url.to_string()).collect::<Vec<_>>(),
    );
}

/// The counterpart, so the refusals above are about the shape and not about the lookup being
/// broken: a well-formed id the Portal does not know is looked up, at that one path, with this
/// proxy's own bearer, and is then refused with the same sentence as every other bad credential.
#[tokio::test]
async fn a_well_formed_run_id_is_looked_up_at_its_own_path_and_nowhere_else() {
    let portal = MockServer::start().await;
    Mock::given(any())
        .respond_with(ResponseTemplate::new(404))
        .mount(&portal)
        .await;

    let unknown = "f1f2f3f4-98fc-1c14-9afb-4c7b2756a120";
    let response = router(proxy_asking_the_portal(&portal.uri()))
        .oneshot(request("/v1/data/ngsi-ld/v1/entities", unknown))
        .await
        .expect("the router answers");
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

    let seen = portal.received_requests().await.unwrap_or_default();
    assert_eq!(seen.len(), 1, "the Portal was asked once");
    assert_eq!(
        seen[0].url.path(),
        format!("/internal/agent-runs/{unknown}"),
        "the lookup left its own route",
    );
    assert!(
        seen[0].url.query().is_none(),
        "the lookup carried a query the proxy never wrote",
    );
    assert!(
        seen[0]
            .headers
            .get("authorization")
            .is_some_and(|value| value.to_str().unwrap_or_default().starts_with("Bearer ")),
        "the lookup is the credentialed request this attack is about",
    );
}

// ---------------------------------------------------------------------------------------------
// Step 2 — a token for an endpoint of another run. The slug decides which audience is minted, so
// a slug the run does not name must stop before the credential manager is asked.
// ---------------------------------------------------------------------------------------------

/// T-1695, AG-38: no token is minted for an endpoint outside the run, and the gateway is not called.
#[tokio::test]
async fn no_token_is_minted_for_an_endpoint_the_run_does_not_name() {
    let gateway = MockServer::start().await;
    Mock::given(any())
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([])))
        .mount(&gateway)
        .await;
    let portal = MockServer::start().await;
    Mock::given(any())
        .respond_with(ResponseTemplate::new(404))
        .mount(&portal)
        .await;
    let gateway_uri = gateway.uri();
    let portal_uri = portal.uri();
    let upstreams = Upstreams {
        gateway: &gateway_uri,
        model: "https://api.anthropic.com",
        forge: "http://gitea-http:3000",
        portal: &portal_uri,
    };

    for slug in [
        "another-runs-endpoint",
        "SCSD2EEHKX42N53Z2ZYD6VSHFH7S7IRF",
        &format!("{SLUG}x"),
        &SLUG[..SLUG.len() - 1],
    ] {
        let state = proxy_holding(sample_run("building"), &upstreams);
        let response = router(state)
            .oneshot(request(
                &format!("/v1/data/endpoints/{slug}/ngsi-ld/v1/entities"),
                RUN,
            ))
            .await
            .expect("the router answers");
        assert_eq!(
            response.status(),
            StatusCode::FORBIDDEN,
            "{slug} was served"
        );
    }

    assert!(
        gateway
            .received_requests()
            .await
            .unwrap_or_default()
            .is_empty(),
        "the gateway was called for an endpoint outside the run",
    );

    // The run's own endpoint is served, so the refusals above are about the slug.
    let state = proxy_holding(sample_run("building"), &upstreams);
    let response = router(state)
        .oneshot(request(
            &format!("/v1/data/endpoints/{SLUG}/ngsi-ld/v1/entities"),
            RUN,
        ))
        .await
        .expect("the router answers");
    assert_eq!(response.status(), StatusCode::OK);
}

/// A run the Portal has finished with gets no token at all: the refusal comes before the mint and
/// before the gateway.
#[tokio::test]
async fn a_terminal_run_is_given_no_token_and_reaches_no_upstream() {
    let gateway = MockServer::start().await;
    Mock::given(any())
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([])))
        .mount(&gateway)
        .await;
    let gateway_uri = gateway.uri();
    let upstreams = Upstreams {
        gateway: &gateway_uri,
        model: "https://api.anthropic.com",
        forge: "http://gitea-http:3000",
        portal: "http://portal:8080",
    };

    for status in ["failed", "cancelled", "expired", "published"] {
        let state = proxy_holding(sample_run(status), &upstreams);
        let response = router(state)
            .oneshot(request("/v1/data/ngsi-ld/v1/entities", RUN))
            .await
            .expect("the router answers");
        assert_eq!(
            response.status(),
            StatusCode::UNAUTHORIZED,
            "a {status} run was served",
        );
    }
    assert!(
        gateway
            .received_requests()
            .await
            .unwrap_or_default()
            .is_empty(),
        "the gateway was called for a run that is over",
    );
}

// ---------------------------------------------------------------------------------------------
// Step 3 — another host, or another path, through a redirect. Every upstream call of this proxy
// carries a credential, so a `3xx` is an instruction to spend it elsewhere.
// ---------------------------------------------------------------------------------------------

/// One upstream that answers everything with `302 Location: <elsewhere>`, and the stub at
/// `elsewhere` that must never be reached.
async fn redirecting_pair() -> (MockServer, MockServer) {
    let elsewhere = MockServer::start().await;
    Mock::given(any())
        .respond_with(ResponseTemplate::new(200).set_body_string("{\"stolen\":true}"))
        .mount(&elsewhere)
        .await;
    let redirector = MockServer::start().await;
    Mock::given(any())
        .respond_with(
            ResponseTemplate::new(302)
                .insert_header("location", format!("{}/collect", elsewhere.uri()).as_str()),
        )
        .mount(&redirector)
        .await;
    (redirector, elsewhere)
}

/// T-1695, AG-52: a credentialed request is made to the configured address and to no other. The
/// gateway, the forge and the model provider each get one chance to move it, and none of them can.
#[tokio::test]
async fn a_credentialed_upstream_cannot_redirect_this_proxy_anywhere() {
    for (upstream, uri, method_name, body) in [
        ("gateway", "/v1/data/ngsi-ld/v1/entities", "GET", ""),
        (
            "forge",
            "/v1/forge/contents/projects/helsinki/apps/bikes/src/App.tsx",
            "PUT",
            r#"{"content":"aGk=","message":"add app"}"#,
        ),
        ("model", "/v1/llm/v1/messages", "POST", r#"{"model":"x"}"#),
    ] {
        let (redirector, elsewhere) = redirecting_pair().await;
        let redirector_uri = redirector.uri();
        let other = "http://unused.invalid";
        let upstreams = match upstream {
            "gateway" => Upstreams {
                gateway: &redirector_uri,
                model: other,
                forge: other,
                portal: other,
            },
            "forge" => Upstreams {
                gateway: other,
                model: other,
                forge: &redirector_uri,
                portal: other,
            },
            _ => Upstreams {
                gateway: other,
                model: &redirector_uri,
                forge: other,
                portal: other,
            },
        };
        let state = proxy_holding(sample_run("building"), &upstreams);
        let response = router(state)
            .oneshot(
                Request::builder()
                    .method(method_name)
                    .uri(uri)
                    .header("x-jc-run", RUN)
                    .header("x-jc-ticket", TICKET)
                    .body(Body::from(body))
                    .expect("the request builds"),
            )
            .await
            .expect("the router answers");

        assert_eq!(
            response.status(),
            StatusCode::BAD_GATEWAY,
            "the {upstream} moved a credentialed request",
        );
        assert!(
            response.headers().get("location").is_none(),
            "the {upstream}'s Location reached the workspace",
        );
        let body = axum::body::to_bytes(response.into_body(), 64 * 1024)
            .await
            .expect("the body reads");
        let body = String::from_utf8_lossy(&body);
        assert!(
            body.contains("upstream-redirect"),
            "the {upstream} redirect was not named in the answer: {body}",
        );
        assert!(
            !body.contains(&elsewhere.uri()),
            "the answer named an address inside the cluster: {body}",
        );
        assert!(
            elsewhere
                .received_requests()
                .await
                .unwrap_or_default()
                .is_empty(),
            "the {upstream}'s redirect target was called",
        );
    }
}

/// The one request that carries the OIDC client secret is the grant itself, in a form body that
/// `reqwest` would resend on a `307`. The credential manager's client follows nothing, so the
/// realm cannot name a second recipient for it.
#[tokio::test]
async fn the_token_grant_is_never_resent_to_a_second_address() {
    let (realm, elsewhere) = redirecting_pair().await;
    let realm_uri = realm.uri();
    let config = Arc::new(
        Config::from_lookup(|key| match key {
            "JC_PROXY_BIND" => Some("127.0.0.1:0".to_owned()),
            "JC_GATEWAY_BASE" => Some("http://context-gateway:8080".to_owned()),
            "JC_MODEL_BASE" => Some("https://api.anthropic.com".to_owned()),
            "JC_FORGE_BASE" => Some("http://gitea-http:3000".to_owned()),
            "JC_MODEL_KEY" => Some("mock-model-key".to_owned()),
            "JC_FORGE_TOKEN" => Some("mock-forge-token".to_owned()),
            "JC_OIDC_ISSUER" => Some(format!("{realm_uri}/realms/jc")),
            "JC_OIDC_CLIENT_ID" => Some("agent-proxy".to_owned()),
            "JC_OIDC_CLIENT_SECRET" => Some("the-client-secret".to_owned()),
            _ => None,
        })
        .expect("the test configuration parses"),
    );

    let error = CredentialManager::new(config)
        .get_endpoint_token("some-endpoint")
        .await
        .expect_err("a realm that only redirects mints nothing");
    assert!(
        !error.contains("the-client-secret"),
        "the refusal quoted the client secret: {error}",
    );
    assert!(
        elsewhere
            .received_requests()
            .await
            .unwrap_or_default()
            .is_empty(),
        "the grant was resent to the address the realm named",
    );
}
