//! `jc-assistant`: the knowledge assistant's crawl worker and chat route (Architecture/22 §1,
//! T-3052, T-3055).
//!
//! Environment: `JC_ASSISTANT_DATABASE_URL` (the `assistant` role's own database),
//! `JC_ASSISTANT_REPO_DIR` (the organization's checkout), `JC_ASSISTANT_PROJECTS_DIR` and
//! `JC_ASSISTANT_ASSEMBLY_DIR` (layout 2), `JC_ASSISTANT_ORG_DOMAIN`, `JC_ASSISTANT_MODEL_DIR`
//! (the embedding model's files, `scripts/ci/e5-small.sh`), `JC_ASSISTANT_EMBED_THREADS`
//! (default 2), `JC_ASSISTANT_BIND` (the chat route and the health probe, default
//! `0.0.0.0:8080`), `HOSTNAME` (the worker's name in the queue), and for the chat:
//! `JC_ASSISTANT_PROXY_URL` (`jc-agent-proxy`), `JC_ASSISTANT_GATEWAY_URL` (the connectors' MCP
//! surfaces), `JC_ASSISTANT_TOKEN_URL`, `JC_ASSISTANT_CLIENT_ID` (default `jc-assistant`) and
//! `JC_ASSISTANT_CLIENT_SECRET_FILE` (the service's own Keycloak client), `JC_ASSISTANT_LLM` (the
//! model the completions name) and `JC_ASSISTANT_FUNCTIONS_URL` (`jc-functions`, for the scripts
//! of a deployment with `sandbox: true`; none without it).

use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use assistant::chat::model::{Model, ModelConfig};
use assistant::chat::{self, ChatState};
use assistant::crawl::Crawler;
use assistant::embed::{embed_missing, Embedder};
use assistant::worker::{self, checkout_from_env};

#[tokio::main]
async fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();
    let env = |key: &str| {
        std::env::var(key)
            .ok()
            .filter(|value| !value.trim().is_empty())
    };

    let (Some(url), Some(checkout), Some(organization), Some(model_dir)) = (
        env("JC_ASSISTANT_DATABASE_URL"),
        checkout_from_env(env),
        env("JC_ASSISTANT_ORG_DOMAIN"),
        env("JC_ASSISTANT_MODEL_DIR"),
    ) else {
        tracing::error!("JC_ASSISTANT_DATABASE_URL, JC_ASSISTANT_REPO_DIR, JC_ASSISTANT_ORG_DOMAIN and JC_ASSISTANT_MODEL_DIR are required");
        return ExitCode::FAILURE;
    };
    let threads = match env("JC_ASSISTANT_EMBED_THREADS").map(|value| value.parse::<usize>()) {
        None => 2,
        Some(Ok(threads)) if (1..=16).contains(&threads) => threads,
        Some(_) => {
            tracing::error!("JC_ASSISTANT_EMBED_THREADS is a number from 1 to 16");
            return ExitCode::FAILURE;
        }
    };
    let embedder = match Embedder::load(std::path::Path::new(&model_dir), threads) {
        Ok(embedder) => embedder,
        Err(err) => {
            tracing::error!(%err, "the embedding model is not usable");
            return ExitCode::FAILURE;
        }
    };
    let pool = match sqlx::postgres::PgPoolOptions::new()
        .max_connections(4)
        .connect(&url)
        .await
    {
        Ok(pool) => pool,
        Err(err) => {
            tracing::error!(%err, "the assistant's database is not reachable");
            return ExitCode::FAILURE;
        }
    };
    // As the `assistant` role, not a superuser, so the forced row-level security holds (T-3050).
    if let Err(err) = assistant::MIGRATOR.run(&pool).await {
        tracing::error!(%err, "the assistant's migrations did not run");
        return ExitCode::FAILURE;
    }
    let crawler = match Crawler::public() {
        Ok(crawler) => crawler,
        Err(err) => {
            tracing::error!(%err, "the crawler could not be built");
            return ExitCode::FAILURE;
        }
    };
    let name = env("HOSTNAME").unwrap_or_else(|| "jc-assistant".into());

    let base = |key: &str| -> Result<String, String> {
        let value = env(key).ok_or_else(|| format!("{key} is required"))?;
        let parsed = url::Url::parse(&value).map_err(|err| format!("{key}: {err}"))?;
        if parsed.path() != "/" || parsed.query().is_some() {
            return Err(format!("{key} is scheme and authority only"));
        }
        Ok(value.trim_end_matches('/').to_owned())
    };
    let chat_config = (|| -> Result<(String, ModelConfig), String> {
        let secret_file = env("JC_ASSISTANT_CLIENT_SECRET_FILE")
            .ok_or("JC_ASSISTANT_CLIENT_SECRET_FILE is required")?;
        let client_secret = std::fs::read_to_string(&secret_file)
            .map_err(|err| format!("JC_ASSISTANT_CLIENT_SECRET_FILE could not be read: {err}"))?
            .trim()
            .to_owned();
        if client_secret.is_empty() {
            return Err("JC_ASSISTANT_CLIENT_SECRET_FILE is empty".into());
        }
        Ok((
            base("JC_ASSISTANT_GATEWAY_URL")?,
            ModelConfig {
                proxy: base("JC_ASSISTANT_PROXY_URL")?,
                token_url: env("JC_ASSISTANT_TOKEN_URL")
                    .ok_or("JC_ASSISTANT_TOKEN_URL is required")?,
                client_id: env("JC_ASSISTANT_CLIENT_ID").unwrap_or_else(|| "jc-assistant".into()),
                client_secret,
                model: env("JC_ASSISTANT_LLM").ok_or("JC_ASSISTANT_LLM is required")?,
            },
        ))
    })();
    let (gateway, model_config) = match chat_config {
        Ok(config) => config,
        Err(why) => {
            tracing::error!(%why, "the chat is not configured");
            return ExitCode::FAILURE;
        }
    };
    // No redirect of its own: a token or a question is never carried to a host nobody named.
    let http = match reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(120))
        .build()
    {
        Ok(http) => http,
        Err(err) => {
            tracing::error!(%err, "the HTTP client could not be built");
            return ExitCode::FAILURE;
        }
    };
    let chat_state = Arc::new(ChatState {
        pool: pool.clone(),
        embedder: embedder.clone(),
        model: Model::new(model_config, http.clone()),
        http,
        gateway,
        functions: match env("JC_ASSISTANT_FUNCTIONS_URL") {
            None => None,
            Some(_) => match base("JC_ASSISTANT_FUNCTIONS_URL") {
                Ok(functions) => Some(functions),
                Err(why) => {
                    tracing::error!(%why, "the sandbox address is not usable");
                    return ExitCode::FAILURE;
                }
            },
        },
        snapshot: RwLock::new(Arc::default()),
        limits: tokio::sync::Mutex::default(),
    });

    // Ready once the first look at the manifests went through; alive as long as the loop turns.
    let ready = Arc::new(AtomicBool::new(false));
    let bind = env("JC_ASSISTANT_BIND").unwrap_or_else(|| "0.0.0.0:8080".into());
    let probe = {
        let ready = Arc::clone(&ready);
        chat::router(Arc::clone(&chat_state))
            .route("/healthz", axum::routing::get(|| async { "ok" }))
            .route(
                "/readyz",
                axum::routing::get(move || {
                    let ready = ready.load(Ordering::Relaxed);
                    async move {
                        if ready {
                            (axum::http::StatusCode::OK, "ready")
                        } else {
                            (axum::http::StatusCode::SERVICE_UNAVAILABLE, "starting")
                        }
                    }
                }),
            )
    };
    let listener = match tokio::net::TcpListener::bind(&bind).await {
        Ok(listener) => listener,
        Err(err) => {
            tracing::error!(%err, %bind, "the chat and the health probe cannot listen");
            return ExitCode::FAILURE;
        }
    };
    tokio::spawn(async move {
        if let Err(err) = axum::serve(listener, probe).await {
            tracing::error!(%err, "the chat and the health probe stopped");
        }
    });

    let mut terminate =
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(signal) => signal,
            Err(err) => {
                tracing::error!(%err, "SIGTERM cannot be watched");
                return ExitCode::FAILURE;
            }
        };
    let mut tick = tokio::time::interval(Duration::from_secs(60));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                tracing::info!("stopping");
                return ExitCode::SUCCESS;
            }
            _ = terminate.recv() => {
                tracing::info!("stopping");
                return ExitCode::SUCCESS;
            }
            _ = tick.tick() => {}
        }
        let snapshot = match worker::snapshot(&checkout) {
            Ok(snapshot) => Arc::new(snapshot),
            Err(err) => {
                // A checkout mid-update is read again next minute; the queue keeps its jobs.
                tracing::warn!(%err, "manifests not read this minute");
                continue;
            }
        };
        if let Ok(mut held) = chat_state.snapshot.write() {
            *held = Arc::clone(&snapshot);
        }
        let sources = &snapshot.sources;
        ready.store(true, Ordering::Relaxed);
        let now = time::OffsetDateTime::now_utc()
            .replace_second(0)
            .unwrap_or_else(|_| time::OffsetDateTime::now_utc());
        match worker::enqueue_due(&pool, sources, now).await {
            Ok(0) => {}
            Ok(queued) => tracing::info!(queued, "sources due"),
            Err(err) => tracing::warn!(%err, "sources not queued this minute"),
        }
        // Work what is ready, one job at a time, then wait for the next minute.
        loop {
            match worker::work_one(&pool, &crawler, &organization, &name, sources).await {
                Ok(true) => continue,
                Ok(false) => break,
                Err(err) => {
                    tracing::warn!(%err, "the queue could not be worked");
                    break;
                }
            }
        }
        // Then the passages without an embedding: what this minute's crawls stored and what an
        // earlier failure left. A minute embeds at most EMBED_PER_MINUTE per project and the
        // rest waits for the next one, so a large first crawl never starves the queue.
        let projects: std::collections::BTreeSet<&str> = sources
            .iter()
            .map(|source| source.project.as_str())
            .collect();
        for project in projects {
            match embed_missing(&pool, &embedder, project, EMBED_PER_MINUTE).await {
                Ok(0) => {}
                Ok(embedded) => tracing::info!(project, embedded, "passages embedded"),
                Err(err) => tracing::warn!(project, %err, "passages not embedded this minute"),
            }
        }
    }
}

/// Passages embedded per project and minute: a few seconds of the model's time.
const EMBED_PER_MINUTE: i64 = 256;
