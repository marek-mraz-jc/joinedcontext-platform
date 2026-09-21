//! `jc-agent-proxy`: Credential-free proxy mediating all workspace communication for autonomous agents.

pub mod audit;
pub mod auth;
pub mod config;
pub mod inject;
pub mod limits;
pub mod public_dns;
pub mod routes;
pub mod runs;

use axum::routing::{get, post};
use axum::Router;
use std::sync::Arc;

#[derive(Clone)]
pub struct ProxyState {
    pub config: Arc<config::Config>,
    pub runs: runs::RunResolver,
    pub credentials: inject::CredentialManager,
    pub limits: limits::LimitManager,
    /// The client every credentialed upstream is called with: the gateway with a minted endpoint
    /// token, the forge with the forge token, the model with the model key, the Portal with this
    /// proxy's own. Built by [`no_redirect_client`], so none of those credentials can be carried
    /// to a path or a host the proxy never addressed (AG-52, T-1695).
    pub http: reqwest::Client,
    /// The client the fetch and packages routes use. It follows no redirect of its own: every hop
    /// is checked against the run's allow-list first, which `reqwest`'s policy cannot do (AG-65).
    pub egress: reqwest::Client,
}

impl ProxyState {
    /// The proxy's state with the clients it is entitled to, which is the only way one is built.
    ///
    /// The choice of client is a security property (see [`no_redirect_client`]), so it is made
    /// here and not at each call site: a deployment and a test then run the same wiring, and a
    /// suite cannot prove a redirect refused against a client a deployment does not use (T-1695).
    pub fn new(
        config: Arc<config::Config>,
        runs: runs::RunResolver,
        credentials: inject::CredentialManager,
        limits: limits::LimitManager,
    ) -> Self {
        Self {
            config,
            runs,
            credentials,
            limits,
            // No deadline: a long generation is not a hung model provider.
            http: no_redirect_client(None),
            egress: egress_client(),
        }
    }
}

/// A client that follows no redirect, which is the only kind this proxy builds.
///
/// `reqwest`'s default policy follows up to ten hops and decides for itself what to carry along.
/// Neither half is acceptable here. A request this proxy makes carries a credential the workspace
/// must never hold — a minted endpoint token, the forge token, the model key, the OIDC client
/// secret in a form body — or is the run's egress, whose every hop belongs to the profile's
/// allow-list. A redirect is how an upstream would move either one somewhere nobody reviewed, so
/// the hop is answered by this proxy, never by the HTTP client (AG-52, AG-65, T-1695).
/// `None` is a client with no deadline of its own, which a model call needs: a long generation is
/// not a hung upstream.
pub fn no_redirect_client(timeout: Option<std::time::Duration>) -> reqwest::Client {
    let builder = reqwest::Client::builder().redirect(reqwest::redirect::Policy::none());
    match timeout {
        Some(timeout) => builder.timeout(timeout),
        None => builder,
    }
    .build()
    .unwrap_or_default()
}

/// The client of the fetch and packages routes: no redirect of its own, a 30 s deadline, and a
/// resolver that returns public addresses only, so a listed host whose DNS answers with a
/// private, loopback or link-local address reaches nothing (AG-65, T-1304).
pub fn egress_client() -> reqwest::Client {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(std::time::Duration::from_secs(30))
        .dns_resolver(Arc::new(public_dns::PublicOnly))
        .build()
        .unwrap_or_default()
}

pub fn router(state: Arc<ProxyState>) -> Router {
    Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .route(
            "/v1/data/endpoints/{slug}/{*rest}",
            axum::routing::any(routes::data::endpoint_handler),
        )
        .route(
            "/v1/data/{*rest}",
            axum::routing::any(routes::data::handler),
        )
        .route(
            "/v1/forge/{*rest}",
            axum::routing::any(routes::forge::handler),
        )
        .route("/v1/fetch", get(routes::fetch::handler))
        .route("/v1/llm/{*rest}", post(routes::llm::handler))
        .route(
            "/v1/packages/{host}/{*rest}",
            get(routes::packages::handler),
        )
        .route(
            "/v1/diagnostics/{component}/{id}",
            get(routes::diagnostics::handler),
        )
        .route("/v1/mcp", post(routes::mcp::handler))
        .route("/v1/runs/events", post(routes::events::handler))
        .route("/v1/runs/inbox", get(routes::inbox::handler))
        .fallback(|| async {
            jc_core::ProblemDetails::forbidden().with_detail("endpoint not recognized by proxy")
        })
        .with_state((*state).clone())
}
