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

/// jemalloc, not glibc's malloc (T-4782): after a 100 req/s load glibc kept 928 Mi of the 1 Gi
/// limit, the freed blocks of the broker answers spread over its heap. The image's
/// `_RJEM_MALLOC_CONF` starts jemalloc's background thread, which gives them back. Behind the
/// `jemalloc` feature, which the image build turns on (T-5017).
#[cfg(feature = "jemalloc")]
#[global_allocator]
static ALLOCATOR: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

/// The one outbound client: the broker hop (R46).
fn broker_of(config: &Config) -> Result<Broker, String> {
    let Some(path) = config.egress_ca_bundle.as_ref() else {
        return Broker::verified(config.broker_url.clone())
            .map(|broker| broker.with_template(config.broker_url_template.clone()))
            .map_err(|error| error.to_string());
    };
    let bundle = std::fs::read(path).map_err(|error| format!("{}: {error}", path.display()))?;
    Broker::trusting(config.broker_url.clone(), &bundle)
        .map(|broker| broker.with_template(config.broker_url_template.clone()))
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
            // talk to a broker nobody verified (R46).
            tracing::error!(%error, "cannot build the outbound client");
            return ExitCode::FAILURE;
        }
    };
    let checkouts = config
        .projects_dir
        .clone()
        .map(|projects| store::Checkouts {
            projects,
            assembly: config.assembly_dir.clone(),
            renders: Default::default(),
        });
    let (
        endpoints,
        spaces,
        accounts,
        federations,
        agreements,
        limits,
        servers,
        homes,
        brokers,
        mappings,
        publishers,
        provenance,
    ) = match &config.repo_dir {
        None => (
            Vec::new(),
            Vec::new(),
            context_gateway::auth::accounts::ServiceAccounts::new(),
            context_gateway::federation::Federations::new(),
            context_gateway::auth::dataspace_token::Agreements::new(),
            store::Limits::default(),
            Vec::new(),
            store::SpaceBrokers::default(),
            context_gateway::mqtt::Brokers::new(),
            Default::default(),
            Default::default(),
            Default::default(),
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
    // The hub's `limit` and `lastN` defaults and maxima are the broker's own (T-4804).
    tokio::spawn(context_gateway::mcp::limits::follow(broker.clone()));
    let gateway = Gateway::new(broker, Box::new(PolicyPdp), config.org_domain.clone())
        .budget_writes(config.write_budget_mib)
        .limit_in_flight(
            config.max_in_flight,
            config.max_in_flight_per_caller,
            config.max_downloads,
        );
    let gateway = gateway.serve(endpoints).serve_spaces(spaces);
    gateway.replace_federation(federations);
    gateway.replace_agreements(agreements);
    gateway.replace_limits(limits);
    gateway.replace_servers(servers);
    gateway.replace_mappings(mappings);
    gateway.replace_publishers(publishers);
    gateway.replace_provenance(provenance);
    gateway.broker.route(&homes);
    gateway.replace_brokers(brokers);

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

    // The anonymous OAuth server (ADR-N-052 §2.2): config checked the key and the public URL.
    let gateway = match (&config.oauth_key, &config.public_url) {
        (Some(key), Some(public_url)) => {
            match context_gateway::oauth::Issuer::new(
                public_url,
                key,
                config.oauth_redirect_hosts.clone(),
            ) {
                Ok(issuer) => gateway.issue_anonymous_tokens(issuer),
                Err(error) => {
                    tracing::error!(%error, "cannot start the anonymous OAuth server");
                    return ExitCode::FAILURE;
                }
            }
        }
        _ => gateway,
    };

    let gateway = match &config.personal_keys {
        Some(dir) => gateway.personal_keys_in(dir.clone()),
        None => gateway,
    };

    // A read over a space with `identity: caller` registrations exchanges the caller's token,
    // as the gateway's own client, for the member brokers' audiences (T-3999, ADR-N-047 §2.4).
    // Without a client such a read answers 503; it is never forwarded as anyone else.
    let gateway = match (config.token_url(), config.oidc_client.as_ref()) {
        (Some(token_url), Some((id, credential))) => {
            gateway.exchange_with(context_gateway::federation::exchange::Exchanger::new(
                token_url,
                id.clone(),
                credential.clone(),
            ))
        }
        _ => gateway,
    };

    // The Portal's internal listener answers a workload it can name (PF-46, AG-52): the
    // gateway's own client, audience-bound to that listener. Without a client configured the
    // pollers send no token and the Portal refuses them, which says so in the log rather
    // than relying on the port's NetworkPolicy.
    let token = match (config.token_url(), config.oidc_client.as_ref()) {
        (Some(token_url), Some((id, credential))) => Some(std::sync::Arc::new(
            context_gateway::previews::WorkloadToken::with_credential(
                token_url,
                id.clone(),
                credential.clone(),
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
    let gateway = gateway
        .deliver_privately_to(config.egress_private_hosts.clone())
        .gate_writes_on(domain_gate);
    // The publishers' pages, searched by the assistant for the hub and the servers (EP-133).
    let gateway = match config.assistant_url.as_deref() {
        Some(url) => {
            gateway.search_documents_with(context_gateway::mcp::documents::Documents::new(
                url,
                context_gateway::mcp::documents::hub_sources(&config.hub_knowledge_sources),
            ))
        }
        None => gateway,
    };
    // The publishers' Usage tab: counts per Endpoint and surface, nothing personal (T-5118).
    if let Some(url) = config.usage_url.clone() {
        tokio::spawn(context_gateway::usage::follow(url, token.clone()));
    }
    // A person's own connectors are the Portal's to keep and the gateway's to serve (EP-123).
    let gateway = Arc::new(match config.connectors_url.clone() {
        Some(url) => {
            gateway.read_personal_connectors(context_gateway::mcp::personal::Personal::new(
                url,
                token.clone(),
                context_gateway::mcp::personal::KEEP,
            ))
        }
        None => gateway,
    });
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
                         JC_OIDC_CLIENT_ID and JC_OIDC_CLIENT_ASSERTION_FILE (or \
                         JC_OIDC_CLIENT_SECRET)"
                    );
                }
                tokio::spawn(mirror.follow(url, token.clone()));
                reaper.with_previews(config.previews_dir.clone())
            }
            None => reaper,
        };
        tokio::spawn(async move {
            let error = reaper.run().await;
            tracing::error!(%error, "the repository reaper stopped; exiting so a revoked grant never stays");
            std::process::exit(1);
        });
    }

    // The MQTT brokers' CONNECT checks answer on a listener of their own, which APISIX does not
    // route to and the NetworkPolicy opens to broker pods only (ADR-N-048, PL-75).
    if let Some(internal) = config.internal_bind {
        let listener = match tokio::net::TcpListener::bind(internal).await {
            Ok(listener) => listener,
            Err(error) => {
                tracing::error!(%error, bind = %internal, "cannot listen internally");
                return ExitCode::FAILURE;
            }
        };
        let app = context_gateway::mqtt::internal_router(
            Arc::clone(&gateway),
            config.mqtt_runner_host.clone(),
            config.mqtt_portal_host.clone(),
        );
        tracing::info!(bind = %internal, "internal listener for the MQTT brokers");
        tokio::spawn(async move {
            if let Err(error) = axum::serve(listener, app).await {
                tracing::error!(%error, "the internal listener stopped");
            }
        });
    }

    let app = router(gateway).layer(tower_http::trace::TraceLayer::new_for_http());
    let listener = match tokio::net::TcpListener::bind(config.bind).await {
        Ok(listener) => listener,
        Err(error) => {
            tracing::error!(%error, bind = %config.bind, "cannot listen");
            return ExitCode::FAILURE;
        }
    };
    tracing::info!(
        bind = %config.bind,
        broker = %config.broker_url,
        allocator = if cfg!(feature = "jemalloc") { "jemalloc" } else { "system" },
        "context-gateway listening"
    );

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
