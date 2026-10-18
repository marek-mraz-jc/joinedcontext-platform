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
//! `JC_ASSISTANT_CLIENT_ASSERTION_FILE` (the pod's projected token, PF-47) or
//! `JC_ASSISTANT_CLIENT_SECRET_FILE` (the service's own Keycloak client), `JC_ASSISTANT_LLM` (the
//! model the completions name) and `JC_ASSISTANT_FUNCTIONS_URL` (`jc-functions`, for the scripts
//! of a deployment with `sandbox: true`; none without it) and `JC_ASSISTANT_PORTAL_CLIENT` (the
//! Portal's client, the one caller of the administration paths; default `portal-api`) and
//! `JC_ASSISTANT_PUBLIC_ORIGIN` (`https://assistant.{domain}`, whose widget may ask the chat) and
//! `JC_ASSISTANT_ENDPOINT_URL` (the public origin `/api/endpoint/{slug}` is served under, which a
//! `catalogue` source's pages cite when the Endpoint publishes no CKAN dataset, AG-116) and
//! `JC_ASSISTANT_PORTAL_URL` (`https://portal.{domain}`, which a `guide` source's sections link
//! into, AG-118).

use std::collections::BTreeMap;
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use assistant::chat::model::{Model, ModelConfig};
use assistant::chat::{self, ChatState};
use assistant::crawl::Crawler;
use assistant::embed::{embed_missing, Embedder};
use assistant::worker::{self, checkout_from_env};
use jc_core::ClientCredential;
use sqlx::PgPool;

#[tokio::main]
async fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();
    match run().await {
        Ok(()) => {
            tracing::info!("stopping");
            ExitCode::SUCCESS
        }
        Err(Startup { what, why }) => {
            tracing::error!(%why, "{what}");
            ExitCode::FAILURE
        }
    }
}

/// Why the service did not start: what failed, and the error underneath.
#[derive(Debug)]
struct Startup {
    what: &'static str,
    why: String,
}

/// Turns an error into the [`Startup`] failure `what` names.
fn fail<E: std::fmt::Display>(what: &'static str) -> impl FnOnce(E) -> Startup {
    move |err| Startup {
        what,
        why: err.to_string(),
    }
}

/// Starts the chat and the probe, then works the queue once a minute until SIGTERM or Ctrl-C.
async fn run() -> Result<(), Startup> {
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
        return Err(Startup {
            what: "JC_ASSISTANT_DATABASE_URL, JC_ASSISTANT_REPO_DIR, JC_ASSISTANT_ORG_DOMAIN and JC_ASSISTANT_MODEL_DIR are required",
            why: "one of them is unset or empty".into(),
        });
    };
    let threads = embed_threads(&env).map_err(fail("the embedding model is not configured"))?;
    let embedder = Embedder::load(std::path::Path::new(&model_dir), threads)
        .map_err(fail("the embedding model is not usable"))?;
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(4)
        .connect(&url)
        .await
        .map_err(fail("the assistant's database is not reachable"))?;
    // As the `assistant` role, not a superuser, so the forced row-level security holds (T-3050).
    assistant::MIGRATOR
        .run(&pool)
        .await
        .map_err(fail("the assistant's migrations did not run"))?;
    let crawler = Crawler::public().map_err(fail("the crawler could not be built"))?;
    let (chat_state, reader) = chat_state(&env, &pool, &embedder, &organization)?;

    // Ready once the first look at the manifests went through; alive as long as the loop turns.
    let ready = Arc::new(AtomicBool::new(false));
    let bind = env("JC_ASSISTANT_BIND").unwrap_or_else(|| "0.0.0.0:8080".into());
    let listener = tokio::net::TcpListener::bind(&bind)
        .await
        .map_err(|err| Startup {
            what: "the chat and the health probe cannot listen",
            why: format!("{bind}: {err}"),
        })?;
    let probe = probe(Arc::clone(&chat_state), Arc::clone(&ready));
    tokio::spawn(async move {
        if let Err(err) = axum::serve(listener, probe).await {
            tracing::error!(%err, "the chat and the health probe stopped");
        }
    });

    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .map_err(fail("SIGTERM cannot be watched"))?;
    let mut minute = Minute {
        pool,
        embedder,
        crawler,
        reader,
        organization,
        name: env("HOSTNAME").unwrap_or_else(|| "jc-assistant".into()),
        checkout,
        chat_state,
        ready,
        catalogue_digests: BTreeMap::new(),
    };
    let mut tick = tokio::time::interval(Duration::from_secs(60));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => return Ok(()),
            _ = terminate.recv() => return Ok(()),
            _ = tick.tick() => {}
        }
        minute.work().await;
    }
}

/// The chat's shared state and the catalogue reader the crawl worker shares its client with.
fn chat_state(
    env: &impl Fn(&str) -> Option<String>,
    pool: &PgPool,
    embedder: &Embedder,
    organization: &str,
) -> Result<(Arc<ChatState>, assistant::catalogue::Reader), Startup> {
    let (gateway, model_config) = chat_config(env).map_err(fail("the chat is not configured"))?;
    // No redirect of its own: a token or a question is never carried to a host nobody named.
    let http = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(120))
        .build()
        .map_err(fail("the HTTP client could not be built"))?;
    let reader = assistant::catalogue::Reader {
        http: http.clone(),
        gateway: url::Url::parse(&format!("{gateway}/")).ok(),
        endpoint_base: endpoint_base(env)
            .map_err(fail("the public Endpoint address is not usable"))?,
        portal: optional_base(env, "JC_ASSISTANT_PORTAL_URL")
            .map_err(fail("the Portal address is not usable"))?,
    };
    let chat_state = Arc::new(ChatState {
        pool: pool.clone(),
        embedder: embedder.clone(),
        model: Model::new(model_config, http.clone()),
        http,
        gateway,
        functions: optional_base(env, "JC_ASSISTANT_FUNCTIONS_URL")
            .map_err(fail("the sandbox address is not usable"))?,
        portal_client: env("JC_ASSISTANT_PORTAL_CLIENT").unwrap_or_else(|| "portal-api".into()),
        public_origin: env("JC_ASSISTANT_PUBLIC_ORIGIN")
            .map(|o| o.trim_end_matches('/').to_owned()),
        snapshot: RwLock::new(Arc::default()),
        limits: tokio::sync::Mutex::default(),
        questions: std::sync::Arc::new(tokio::sync::Semaphore::new(assistant::chat::MAX_QUESTIONS)),
        answers: Default::default(),
        endpoint_base: reader.endpoint_base.clone(),
        organization: organization.to_owned(),
    });
    Ok((chat_state, reader))
}

/// What the minute's work reads and keeps between minutes.
struct Minute {
    pool: PgPool,
    embedder: Embedder,
    crawler: Crawler,
    reader: assistant::catalogue::Reader,
    organization: String,
    name: String,
    checkout: worker::Checkout,
    chat_state: Arc<ChatState>,
    ready: Arc<AtomicBool>,
    catalogue_digests: BTreeMap<(String, String), String>,
}

impl Minute {
    /// Reads the manifests, queues what is due or changed, works the queue and embeds.
    async fn work(&mut self) {
        let snapshot = match worker::snapshot(&self.checkout) {
            Ok(snapshot) => Arc::new(snapshot),
            Err(err) => {
                // A checkout mid-update is read again next minute; the queue keeps its jobs.
                tracing::warn!(%err, "manifests not read this minute");
                return;
            }
        };
        if let Ok(mut held) = self.chat_state.snapshot.write() {
            *held = Arc::clone(&snapshot);
        }
        // Routing's type lists, ahead of the first question (T-4611).
        tokio::spawn(chat::warm_types(Arc::clone(&self.chat_state)));
        let sources = &snapshot.sources;
        self.ready.store(true, Ordering::Relaxed);
        let pool = &self.pool;
        let now = time::OffsetDateTime::now_utc()
            .replace_second(0)
            .unwrap_or_else(|_| time::OffsetDateTime::now_utc());
        if let Err(err) = chat::forget_expired(pool).await {
            tracing::warn!(%err, "expired conversations not deleted this minute");
        }
        match worker::enqueue_due(pool, sources, now).await {
            Ok(0) => {}
            Ok(queued) => tracing::info!(queued, "sources due"),
            Err(err) => tracing::warn!(%err, "sources not queued this minute"),
        }
        match worker::enqueue_changed(pool, sources, &mut self.catalogue_digests).await {
            Ok(0) => {}
            Ok(queued) => tracing::info!(queued, "catalogues changed"),
            Err(err) => tracing::warn!(%err, "changed catalogues not queued this minute"),
        }
        // Work what is ready, one job at a time, then wait for the next minute.
        loop {
            let worked = worker::work_one(
                pool,
                &self.crawler,
                &self.reader,
                &self.organization,
                &self.name,
                sources,
            )
            .await;
            match worked {
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
            match embed_missing(pool, &self.embedder, project, EMBED_PER_MINUTE).await {
                Ok(0) => {}
                Ok(embedded) => tracing::info!(project, embedded, "passages embedded"),
                Err(err) => tracing::warn!(project, %err, "passages not embedded this minute"),
            }
        }
    }
}

/// Passages embedded per project and minute: a few seconds of the model's time.
const EMBED_PER_MINUTE: i64 = 256;

/// The chat route beside `/healthz` and `/readyz`, which answers 503 until `ready` is set.
fn probe(chat_state: Arc<ChatState>, ready: Arc<AtomicBool>) -> axum::Router {
    chat::router(chat_state)
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
}

/// `JC_ASSISTANT_EMBED_THREADS`: 2 when unset, else a number from 1 to 16.
fn embed_threads(env: &impl Fn(&str) -> Option<String>) -> Result<usize, String> {
    match env("JC_ASSISTANT_EMBED_THREADS").map(|value| value.parse::<usize>()) {
        None => Ok(2),
        Some(Ok(threads)) if (1..=16).contains(&threads) => Ok(threads),
        Some(_) => Err("JC_ASSISTANT_EMBED_THREADS is a number from 1 to 16".into()),
    }
}

/// A service address from `key`: scheme and authority only, returned without its last `/`.
fn base(env: &impl Fn(&str) -> Option<String>, key: &str) -> Result<String, String> {
    let value = env(key).ok_or_else(|| format!("{key} is required"))?;
    let parsed = url::Url::parse(&value).map_err(|err| format!("{key}: {err}"))?;
    if parsed.path() != "/" || parsed.query().is_some() {
        return Err(format!("{key} is scheme and authority only"));
    }
    Ok(value.trim_end_matches('/').to_owned())
}

/// [`base`] for an address the deployment may leave out.
fn optional_base(
    env: &impl Fn(&str) -> Option<String>,
    key: &str,
) -> Result<Option<String>, String> {
    match env(key) {
        None => Ok(None),
        Some(_) => base(env, key).map(Some),
    }
}

/// `JC_ASSISTANT_ENDPOINT_URL` as the base a `catalogue` page's Endpoint links join onto.
fn endpoint_base(env: &impl Fn(&str) -> Option<String>) -> Result<Option<url::Url>, String> {
    optional_base(env, "JC_ASSISTANT_ENDPOINT_URL")?
        .map(|value| url::Url::parse(&format!("{value}/")).map_err(|e| e.to_string()))
        .transpose()
}

/// The chat's Keycloak credential: the pod's projected token or the client secret, never both.
fn credential(env: &impl Fn(&str) -> Option<String>) -> Result<ClientCredential, String> {
    match (
        env("JC_ASSISTANT_CLIENT_ASSERTION_FILE"),
        env("JC_ASSISTANT_CLIENT_SECRET_FILE"),
    ) {
        (Some(_), Some(_)) => Err(
            "JC_ASSISTANT_CLIENT_ASSERTION_FILE and JC_ASSISTANT_CLIENT_SECRET_FILE \
             are both set; a federated client holds no secret beside it"
                .into(),
        ),
        (Some(file), None) => Ok(ClientCredential::Assertion(file.into())),
        (None, Some(secret_file)) => {
            let client_secret = std::fs::read_to_string(&secret_file)
                .map_err(|err| format!("JC_ASSISTANT_CLIENT_SECRET_FILE could not be read: {err}"))?
                .trim()
                .to_owned();
            if client_secret.is_empty() {
                return Err("JC_ASSISTANT_CLIENT_SECRET_FILE is empty".into());
            }
            Ok(ClientCredential::Secret(jc_core::Secret::new(
                client_secret,
            )))
        }
        (None, None) => Err("JC_ASSISTANT_CLIENT_ASSERTION_FILE or \
                             JC_ASSISTANT_CLIENT_SECRET_FILE is required"
            .into()),
    }
}

/// The connectors' gateway and the model the chat asks through the agent proxy.
fn chat_config(env: &impl Fn(&str) -> Option<String>) -> Result<(String, ModelConfig), String> {
    let credential = credential(env)?;
    Ok((
        base(env, "JC_ASSISTANT_GATEWAY_URL")?,
        ModelConfig {
            proxy: base(env, "JC_ASSISTANT_PROXY_URL")?,
            token_url: env("JC_ASSISTANT_TOKEN_URL").ok_or("JC_ASSISTANT_TOKEN_URL is required")?,
            client_id: env("JC_ASSISTANT_CLIENT_ID").unwrap_or_else(|| "jc-assistant".into()),
            credential,
            model: env("JC_ASSISTANT_LLM").ok_or("JC_ASSISTANT_LLM is required")?,
        },
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        move |key| map.get(key).cloned()
    }

    fn chat_env(extra: &[(&'static str, &'static str)]) -> Vec<(&'static str, &'static str)> {
        let mut pairs = vec![
            ("JC_ASSISTANT_CLIENT_ASSERTION_FILE", "/var/run/token"),
            ("JC_ASSISTANT_GATEWAY_URL", "http://gateway:8080/"),
            ("JC_ASSISTANT_PROXY_URL", "http://proxy:8080"),
            ("JC_ASSISTANT_TOKEN_URL", "https://idm.test/token"),
            ("JC_ASSISTANT_LLM", "a-model"),
        ];
        pairs.extend_from_slice(extra);
        pairs
    }

    #[test]
    fn embed_threads_default_to_two_and_stay_between_one_and_sixteen() {
        assert_eq!(embed_threads(&env(&[])), Ok(2));
        assert_eq!(
            embed_threads(&env(&[("JC_ASSISTANT_EMBED_THREADS", "16")])),
            Ok(16)
        );
        for wrong in ["0", "17", "two", "-1"] {
            assert!(embed_threads(&env(&[("JC_ASSISTANT_EMBED_THREADS", wrong)])).is_err());
        }
    }

    #[test]
    fn a_base_is_scheme_and_authority_only() {
        let key = "JC_ASSISTANT_GATEWAY_URL";
        assert_eq!(
            base(&env(&[(key, "http://gw:8080/")]), key),
            Ok("http://gw:8080".into())
        );
        assert_eq!(base(&env(&[]), key), Err(format!("{key} is required")));
        for wrong in ["http://gw/mcp", "http://gw/?a=1"] {
            assert_eq!(
                base(&env(&[(key, wrong)]), key),
                Err(format!("{key} is scheme and authority only"))
            );
        }
        assert!(base(&env(&[(key, "not a url")]), key).is_err_and(|e| e.starts_with(key)));
        assert_eq!(optional_base(&env(&[]), key), Ok(None));
        assert!(optional_base(&env(&[(key, "http://gw/x")]), key).is_err());
    }

    #[test]
    fn the_endpoint_base_ends_with_a_slash_for_joins() {
        let set = env(&[("JC_ASSISTANT_ENDPOINT_URL", "https://data.city.test")]);
        let endpoint = endpoint_base(&set).expect("usable").expect("set");
        assert_eq!(
            endpoint.join("api/endpoint/air").expect("joins").as_str(),
            "https://data.city.test/api/endpoint/air"
        );
        assert_eq!(endpoint_base(&env(&[])), Ok(None));
    }

    #[test]
    fn the_credential_is_one_of_the_token_or_a_non_empty_secret() {
        let both = chat_env(&[("JC_ASSISTANT_CLIENT_SECRET_FILE", "/x")]);
        assert!(credential(&env(&both)).is_err_and(|e| e.contains("both set")));
        assert!(credential(&env(&[])).is_err_and(|e| e.contains("is required")));
        assert!(matches!(
            credential(&env(&chat_env(&[]))),
            Ok(ClientCredential::Assertion(path)) if path == std::path::Path::new("/var/run/token")
        ));

        let dir = std::env::temp_dir().join(format!("jc-assistant-secret-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("a temp dir");
        let file = dir.join("secret");
        let path = file.to_str().expect("utf-8 temp dir");
        std::fs::write(&file, " s3cret\n").expect("written");
        assert!(matches!(
            credential(&env(&[("JC_ASSISTANT_CLIENT_SECRET_FILE", path)])),
            Ok(ClientCredential::Secret(secret)) if secret.expose() == "s3cret"
        ));
        std::fs::write(&file, "  \n").expect("written");
        assert_eq!(
            credential(&env(&[("JC_ASSISTANT_CLIENT_SECRET_FILE", path)])).err(),
            Some("JC_ASSISTANT_CLIENT_SECRET_FILE is empty".into())
        );
        let missing = dir.join("missing");
        let missing = missing.to_str().expect("utf-8 temp dir");
        assert!(
            credential(&env(&[("JC_ASSISTANT_CLIENT_SECRET_FILE", missing)]))
                .is_err_and(|e| e.contains("could not be read"))
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_chat_config_names_what_is_missing() {
        let (gateway, model) = chat_config(&env(&chat_env(&[]))).expect("configured");
        assert_eq!(gateway, "http://gateway:8080");
        assert_eq!(model.proxy, "http://proxy:8080");
        assert_eq!(model.client_id, "jc-assistant");
        assert_eq!(model.model, "a-model");
        for key in [
            "JC_ASSISTANT_TOKEN_URL",
            "JC_ASSISTANT_LLM",
            "JC_ASSISTANT_PROXY_URL",
        ] {
            let pairs: Vec<_> = chat_env(&[])
                .into_iter()
                .filter(|(k, _)| *k != key)
                .collect();
            assert_eq!(
                chat_config(&env(&pairs)).err(),
                Some(format!("{key} is required"))
            );
        }
    }
}
