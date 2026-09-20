//! The harness the per-route edge-case suites share (AG-22, AG-25, AG-64).
//!
//! One run, one proxy, one way to present the run's credentials. Every helper here is used by
//! every suite that includes the module, so there is nothing in it to forget to remove.

use agent_proxy::config::Config;
use agent_proxy::inject::CredentialManager;
use agent_proxy::limits::LimitManager;
use agent_proxy::runs::{RunContext, RunResolver};
use agent_proxy::{router, ProxyState};
use argon2::{password_hash::SaltString, Argon2, PasswordHasher};
use axum::body::Body;
use axum::http::{request, Request, Response};
use axum::Router;
use std::sync::Arc;

/// The run every suite authenticates as.
pub const RUN_ID: &str = "e3b0c442-98fc-1c14-9afb-4c7b2756a120";
/// Its ticket in the clear; the run record holds only the Argon2 hash of it.
pub const TICKET: &str = "secret-ticket-123";
/// The run's primary endpoint slug.
pub const SLUG: &str = "scsd2eehkx42n53z2zyd6vshfh7s7irf";

fn ticket_hash(ticket: &str) -> String {
    let salt = SaltString::generate(&mut argon2::password_hash::rand_core::OsRng);
    Argon2::default()
        .hash_password(ticket.as_bytes(), &salt)
        .expect("a ticket hashes")
        .to_string()
}

/// A live run of the `helsinki` project, read-only unless a suite says otherwise.
pub fn sample_run(allows_write: bool) -> RunContext {
    RunContext {
        id: RUN_ID.to_owned(),
        project: "helsinki".to_owned(),
        app_name: "bikes".to_owned(),
        endpoint_slug: SLUG.to_owned(),
        endpoint_slugs: vec![],
        allows_write,
        branch: format!("agent/app-bikes/{RUN_ID}"),
        path_prefix: "projects/helsinki/apps/bikes/".to_owned(),
        status: "building".to_owned(),
        ticket_hash: ticket_hash(TICKET),
        max_tokens: 1000,
        allowed_hosts: vec!["crates.io".to_owned()],
        requests_per_minute: 100,
        steps_per_run: 0,
        max_response_bytes: 1_048_576,
        max_egress_bytes_per_run: 0,
        created_by: "demo.steward@hel.fi".to_owned(),
        model_name: "claude-3-7".to_owned(),
        reasoning_effort: None,
    }
}

/// Where the proxy sends what it forwards. A suite overrides the one upstream it stubs and
/// leaves the rest pointing at names that resolve nowhere, so a test that forwards by mistake
/// fails instead of reaching a real service.
pub struct Bases {
    pub gateway: String,
    pub model: String,
    pub forge: String,
    pub portal: String,
}

impl Default for Bases {
    fn default() -> Self {
        Self {
            gateway: "http://context-gateway.invalid:8080".to_owned(),
            model: "http://model-provider.invalid".to_owned(),
            forge: "http://gitea-http.invalid:3000".to_owned(),
            portal: "http://portal.invalid:8080".to_owned(),
        }
    }
}

pub fn state(run: RunContext, bases: Bases) -> Arc<ProxyState> {
    let config = Config::from_lookup(|key| match key {
        "JC_PROXY_BIND" => Some("127.0.0.1:0".to_owned()),
        "JC_GATEWAY_BASE" => Some(bases.gateway.clone()),
        "JC_MODEL_BASE" => Some(bases.model.clone()),
        "JC_FORGE_BASE" => Some(bases.forge.clone()),
        "JC_PORTAL_BASE" => Some(bases.portal.clone()),
        "JC_MODEL_KEY" => Some("mock-model-key".to_owned()),
        "JC_FORGE_TOKEN" => Some("mock-forge-token".to_owned()),
        _ => None,
    })
    .expect("the test configuration is complete");

    let config = Arc::new(config);
    let portal_base = config.portal_base.clone();
    let credentials = CredentialManager::new(config.clone());
    Arc::new(ProxyState {
        config,
        runs: RunResolver::with_cached_at(portal_base, run),
        credentials,
        limits: LimitManager::default(),
        http: reqwest::Client::new(),
        // No redirect of its own: the fetch route checks every hop against the allow-list.
        egress: reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(std::time::Duration::from_secs(5))
            .build()
            .expect("a client builds"),
    })
}

/// The whole proxy, wired to `bases`.
pub fn app(run: RunContext, bases: Bases) -> Router {
    router(state(run, bases))
}

/// A request that carries the run's credentials in the header form.
pub fn authed(method: &str, uri: &str) -> request::Builder {
    Request::builder()
        .method(method)
        .uri(uri)
        .header("x-jc-run", RUN_ID)
        .header("x-jc-ticket", TICKET)
}

/// The answer's body as text, for asserting what a refusal says and what it does not.
pub async fn body_of(response: Response<Body>) -> String {
    let bytes = axum::body::to_bytes(response.into_body(), 8 * 1024 * 1024)
        .await
        .expect("the answer's body is readable");
    String::from_utf8_lossy(&bytes).into_owned()
}
