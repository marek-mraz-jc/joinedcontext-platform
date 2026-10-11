//! The Context Gateway binary: one listener, one broker, one endpoint table (T-0005).

use context_gateway::app::{router, Gateway};
use context_gateway::auth::jwks;
use context_gateway::auth::token::Verifier;
use context_gateway::config::Config;
use context_gateway::egress::migrate::{self, Args, Found, Surface};
use context_gateway::egress::subject::DeliveryKey;
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
    let checkouts = config
        .projects_dir
        .clone()
        .map(|projects| store::Checkouts {
            projects,
            assembly: config.assembly_dir.clone(),
        });
    let (endpoints, spaces, accounts, federations, agreements, limits, servers) = match &config
        .repo_dir
    {
        None => (
            Vec::new(),
            Vec::new(),
            context_gateway::auth::accounts::ServiceAccounts::new(),
            context_gateway::federation::Federations::new(),
            context_gateway::auth::dataspace_token::Agreements::new(),
            store::Limits::default(),
            Vec::new(),
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
    // T-3883: a one-time run, inside the gateway's own pod, of what only its key can read.
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("migrate-r12-subscriptions") {
        let mut surfaces: Vec<Surface> = endpoints.iter().map(Surface::of).collect();
        surfaces.extend(spaces.iter().map(|space| Surface::of(&space.endpoint)));
        return migrate_r12(&broker, &config, &surfaces, &args[1..]).await;
    }
    let gateway = Gateway::new(broker, Box::new(PolicyPdp), config.org_domain.clone());
    let gateway = gateway.serve(endpoints).serve_spaces(spaces);
    gateway.replace_federation(federations);
    gateway.replace_agreements(agreements);
    gateway.replace_limits(limits);
    gateway.replace_servers(servers);

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
    // Each subscription's subscriber is sealed with this key and every delivery decided again
    // for them (GW27, T-2383); without it a subscription that routes a delivery is refused.
    let gateway =
        match config.delivery_key.as_ref().and_then(|key| {
            context_gateway::egress::subject::DeliveryKey::new(key.expose().as_bytes())
        }) {
            Some(key) => gateway.seal_subscribers_with(key),
            None => {
                tracing::warn!(
                    "JC_GATEWAY_DELIVERY_KEY is not set: subscriptions that deliver are refused"
                );
                gateway
            }
        };
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
                         JC_OIDC_CLIENT_ID and JC_OIDC_CLIENT_ASSERTION_FILE (or \
                         JC_OIDC_CLIENT_SECRET)"
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

/// Moves the subscriptions stored with an R12 endpoint to their view (T-3883, O-7 (a)): counts
/// per space on `--dry-run`, otherwise the stored form into `--backup` (a new file, mode 600: it
/// holds what the broker holds, `receiverInfo` included) and then the move. Prints ids, spaces
/// and reasons, never a target, a seal or the key.
async fn migrate_r12(
    broker: &Broker,
    config: &Config,
    surfaces: &[Surface],
    args: &[String],
) -> ExitCode {
    let args = match Args::parse(args) {
        Ok(args) => args,
        Err(error) => {
            eprintln!("{error}");
            return ExitCode::FAILURE;
        }
    };
    let Some(key) = config
        .delivery_key
        .as_ref()
        .and_then(|key| DeliveryKey::new(key.expose().as_bytes()))
    else {
        eprintln!("JC_GATEWAY_DELIVERY_KEY is not set: no seal can be opened");
        return ExitCode::FAILURE;
    };
    let survey = match migrate::survey(
        broker,
        surfaces,
        &key,
        &args.portal_client,
        &config.egress_private_hosts,
    )
    .await
    {
        Ok(survey) => survey,
        Err(error) => {
            eprintln!("nothing changed: {error}");
            return ExitCode::FAILURE;
        }
    };
    for (space, counts) in survey.counts() {
        println!(
            "space={space} routed={} reconciler={} rewritable={} unreadable={} refused={}",
            counts.routed, counts.reconciler, counts.rewritable, counts.unreadable, counts.refused
        );
    }
    for (space, found) in &survey.spaces {
        for (stored, found) in found {
            let why = match found {
                Found::Rewritable { .. } => continue,
                Found::Reconciler => "written by the Portal, which writes it again",
                Found::Unreadable(why) => why,
                Found::Refused(why) => why.as_str(),
            };
            let id = stored
                .get("id")
                .and_then(|id| id.as_str())
                .unwrap_or_default();
            println!("left alone: {id} in {space}: {why}");
        }
    }
    let Some(backup) = args.backup else {
        return ExitCode::SUCCESS;
    };
    if let Err(error) = keep(&backup, &survey) {
        eprintln!("nothing changed: {}: {error}", backup.display());
        return ExitCode::FAILURE;
    }
    let mut failed = false;
    for (id, outcome) in migrate::apply(broker, &survey).await {
        match outcome {
            migrate::Outcome::Rewritten => println!("moved: {id}"),
            migrate::Outcome::Failed(why) => {
                failed = true;
                println!("not moved: {id}: {why}");
            }
        }
    }
    match failed {
        true => ExitCode::FAILURE,
        false => ExitCode::SUCCESS,
    }
}

/// Writes the stored form of every R12 subscription into a file that did not exist, readable
/// by its owner only.
fn keep(path: &std::path::Path, survey: &migrate::Survey) -> std::io::Result<()> {
    use std::io::Write as _;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let mut file = options.open(path)?;
    file.write_all(&serde_json::to_vec_pretty(&survey.stored())?)?;
    file.sync_all()
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
