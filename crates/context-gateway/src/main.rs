//! The Context Gateway binary: one listener, one broker, one endpoint table (T-0005).

use context_gateway::app::{router, Gateway};
use context_gateway::auth::jwks;
use context_gateway::auth::token::Verifier;
use context_gateway::config::Config;
use context_gateway::pdp::PolicyPdp;
use context_gateway::proxy::Broker;
use context_gateway::store;
use std::process::ExitCode;
use std::sync::Arc;

/// The one outbound client: the broker hop and every notification delivery (R46).
fn broker_of(config: &Config) -> Result<Broker, String> {
    let Some(path) = config.egress_ca_bundle.as_ref() else {
        return Broker::verified(config.broker_url.clone()).map_err(|error| error.to_string());
    };
    let bundle = std::fs::read(path).map_err(|error| format!("{}: {error}", path.display()))?;
    Broker::trusting(config.broker_url.clone(), &bundle)
        .map_err(|error| format!("{}: {error}", path.display()))
}

#[tokio::main]
async fn main() -> ExitCode {
    // OPS-15: JSON lines on stdout, nothing on disk.
    if let Err(error) = jc_core::logs::init("context_gateway=info,tower_http=info") {
        eprintln!("logging is not set up: {error}");
        return ExitCode::FAILURE;
    }

    let config = match Config::from_env() {
        Ok(config) => config,
        Err(error) => {
            tracing::error!(%error, "the gateway is not configured");
            return ExitCode::FAILURE;
        }
    };

    let broker = match broker_of(&config) {
        Ok(broker) => broker,
        Err(error) => {
            // An unusable trust bundle is a deployment fault, and starting without it would
            // deliver notifications to a peer nobody verified (R46).
            tracing::error!(%error, "cannot build the outbound client");
            return ExitCode::FAILURE;
        }
    };
    let gateway = Gateway::new(broker, Box::new(PolicyPdp), config.org_domain.clone());
    let checkouts = config
        .projects_dir
        .clone()
        .map(|projects| store::Checkouts {
            projects,
            assembly: config.assembly_dir.clone(),
        });
    let (endpoints, spaces, accounts, federations, agreements) = match &config.repo_dir {
        None => (
            Vec::new(),
            Vec::new(),
            context_gateway::auth::accounts::ServiceAccounts::new(),
            context_gateway::federation::Federations::new(),
            context_gateway::auth::dataspace_token::Agreements::new(),
        ),
        Some(dir) => match store::load_from(dir, checkouts.as_ref()) {
            Ok(loaded) => {
                tracing::info!(
                    endpoints = loaded.0.len(),
                    spaces = loaded.1.len(),
                    accounts = loaded.2.len(),
                    dir = %dir.display(),
                    "repository loaded"
                );
                loaded
            }
            Err(error) => {
                // An unreadable repository is a deployment fault, not a request fault:
                // serving an empty table would answer 404 for endpoints that exist.
                tracing::error!(%error, dir = %dir.display(), "cannot read the manifest repository");
                return ExitCode::FAILURE;
            }
        },
    };
    let gateway = gateway.serve(endpoints).serve_spaces(spaces);
    gateway.replace_federation(federations);
    gateway.replace_agreements(agreements);

    // The realm's keys are refreshed in the background; a request never fetches (PF-46).
    let gateway = match (&config.oidc_issuer, &config.oidc_jwks_url) {
        (Some(issuer), Some(jwks_url)) => {
            let verifier = Arc::new(Verifier::new(issuer.clone()));
            if let Err(error) = jwks::refresh(&verifier, jwks_url).await {
                tracing::error!(%error, "cannot load the realm signing keys");
                return ExitCode::FAILURE;
            }
            tokio::spawn(jwks::keep_current(Arc::clone(&verifier), jwks_url.clone()));
            gateway.authenticate(verifier, accounts, config.public_url.clone())
        }
        _ => {
            tracing::warn!(
                "no realm configured: only endpoints with audience `public` will answer"
            );
            gateway
        }
    };

    // Where the broker delivers a notification, which is not where a caller's token is
    // audience-bound: the public URL when the deployment names no egress URL (R46).
    // The Portal's internal listener answers a workload it can name (PF-46, AG-52): the
    // gateway's own client, audience-bound to that listener. Without a client configured the
    // pollers send no token and the Portal refuses them, which says so in the log rather
    // than relying on the port's NetworkPolicy.
    let token = match (config.token_url(), config.oidc_client.as_ref()) {
        (Some(token_url), Some((id, secret))) => Some(std::sync::Arc::new(
            context_gateway::previews::WorkloadToken::new(
                token_url,
                id.clone(),
                secret.expose().to_owned(),
            ),
        )),
        _ => None,
    };
    // PF-41, T-2572: under `enforce` a write waits for the Organization's verified domain, which
    // the Portal keeps; `report` asks nobody and refuses nothing.
    let domain_gate = Arc::new(context_gateway::domain_gate::DomainGate::new(
        config.domain_verification,
    ));
    if let (context_gateway::domain_gate::Mode::Enforce, Some(url)) = (
        config.domain_verification,
        config.domain_verifications_url.clone(),
    ) {
        tokio::spawn(Arc::clone(&domain_gate).follow(url, token.clone()));
    }
    let gateway = Arc::new(
        gateway
            .deliver_through(config.egress_url.clone())
            .deliver_privately_to(config.egress_private_hosts.clone())
            .gate_writes_on(domain_gate),
    );
    // The repository is a cache of the enforcement point's decisions, so it is followed
    // rather than read once: a revoked Policy or ServiceAccount stops granting within a
    // second, without a restart (R48, EP-19, OPS-45).
    if let Some(dir) = config.repo_dir.clone() {
        let reaper = context_gateway::pdp::reaper::Reaper::new(Arc::clone(&gateway), dir);
        let reaper = match checkouts {
            Some(checkouts) => reaper.with_checkouts(checkouts),
            None => reaper,
        };
        // Workspace previews ride the same reload: the poller writes, the reaper loads
        // (Architecture/06 §7.2).
        let reaper = match config.previews_url.clone() {
            Some(url) => {
                let mirror = context_gateway::previews::Mirror::new(config.previews_dir.clone());
                if token.is_none() {
                    tracing::warn!(
                        "previews are listed without a workload token: set JC_OIDC_ISSUER, \
                         JC_OIDC_CLIENT_ID and JC_OIDC_CLIENT_SECRET"
                    );
                }
                tokio::spawn(mirror.follow(url, token.clone()));
                reaper.with_previews(config.previews_dir.clone())
            }
            None => reaper,
        };
        tokio::spawn(reaper.run());
    }

    let app = router(gateway).layer(tower_http::trace::TraceLayer::new_for_http());
    let listener = match tokio::net::TcpListener::bind(config.bind).await {
        Ok(listener) => listener,
        Err(error) => {
            tracing::error!(%error, bind = %config.bind, "cannot listen");
            return ExitCode::FAILURE;
        }
    };
    tracing::info!(bind = %config.bind, broker = %config.broker_url, "context-gateway listening");

    if let Err(error) = axum::serve(listener, app)
        .with_graceful_shutdown(shutdown())
        .await
    {
        tracing::error!(%error, "the server stopped");
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}

/// Stops on SIGTERM, which is how Kubernetes asks (OPS-12).
async fn shutdown() {
    let interrupt = async {
        tokio::signal::ctrl_c()
            .await
            .expect("a process can always listen for an interrupt");
    };
    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("a process can always listen for SIGTERM")
            .recv()
            .await;
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        () = interrupt => {},
        () = terminate => {},
    }
    tracing::info!("shutting down");
}
