//! The runtime of one shard (ADR-N-044 §2.1, §2.6, AP-143, AP-146, AP-147): one wasmtime engine
//! with the pooling allocator, a fresh instance per request under the request's limits, components
//! compiled once per digest and kept in an LRU, and outgoing HTTP to the Context Gateway alone.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use sha2::{Digest, Sha256};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use wasmtime::component::{Component, Linker, ResourceTable};
use wasmtime::{
    Config, Engine, InstanceAllocationStrategy, PoolingAllocationConfig, ResourceLimiter, Store,
    StoreLimits, StoreLimitsBuilder, Trap, UpdateDeadline,
};
use wasmtime_wasi::{WasiCtx, WasiCtxView, WasiView};
use wasmtime_wasi_http::p2::bindings::http::types::Scheme;
use wasmtime_wasi_http::p2::bindings::ProxyPre;
use wasmtime_wasi_http::p2::body::HyperOutgoingBody;
use wasmtime_wasi_http::{
    RequestOptions, WasiBody, WasiHttpCtx, WasiHttpCtxView, WasiHttpHooks, WasiHttpView,
};

use crate::limits::{Limits, EPOCH_TICK};
use crate::metrics::CacheStats;
use crate::placement::Placed;
use crate::source::Source;
use crate::storage::Storage;

/// What one request's store holds: the WASI context (no environment, no files, no sockets), the
/// HTTP context, the limits, and the App it runs for.
pub struct State {
    wasi: WasiCtx,
    http: WasiHttpCtx,
    table: ResourceTable,
    limits: Budget,
    hooks: GatewayOnly,
    /// The App this instance runs for: the storage interfaces answer as it and no other.
    pub app: Placed,
    pub storage: Arc<dyn Storage>,
    /// The caller and the platform services, asked of the Portal as the request's person.
    pub services: crate::services::Services,
}

/// One request's memory, counted across every linear memory of its instance: the per-memory limit
/// alone lets a component of many core modules hold many times `memory_bytes` (T-4777). A growth
/// past the budget traps, as one past the per-memory limit does.
struct Budget {
    each: StoreLimits,
    memory_bytes: usize,
    used: usize,
}

impl Budget {
    fn new(memory_bytes: usize) -> Self {
        Self {
            each: StoreLimitsBuilder::new()
                .memory_size(memory_bytes)
                .instances(64)
                .trap_on_grow_failure(true)
                .build(),
            memory_bytes,
            used: 0,
        }
    }
}

impl ResourceLimiter for Budget {
    fn memory_growing(
        &mut self,
        current: usize,
        desired: usize,
        maximum: Option<usize>,
    ) -> wasmtime::Result<bool> {
        let total = self.used.saturating_sub(current).saturating_add(desired);
        if total > self.memory_bytes {
            return Err(wasmtime::format_err!(
                "the application's memories would pass {} bytes",
                self.memory_bytes
            ));
        }
        let allowed = self.each.memory_growing(current, desired, maximum)?;
        if allowed {
            self.used = total;
        }
        Ok(allowed)
    }

    fn memory_grow_failed(&mut self, error: wasmtime::Error) -> wasmtime::Result<()> {
        self.each.memory_grow_failed(error)
    }

    fn table_growing(
        &mut self,
        current: usize,
        desired: usize,
        maximum: Option<usize>,
    ) -> wasmtime::Result<bool> {
        self.each.table_growing(current, desired, maximum)
    }

    fn table_grow_failed(&mut self, error: wasmtime::Error) -> wasmtime::Result<()> {
        self.each.table_grow_failed(error)
    }

    fn instances(&self) -> usize {
        self.each.instances()
    }

    fn tables(&self) -> usize {
        self.each.tables()
    }

    fn memories(&self) -> usize {
        self.each.memories()
    }
}

impl WasiView for State {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.wasi,
            table: &mut self.table,
        }
    }
}

impl WasiHttpView for State {
    fn http(&mut self) -> WasiHttpCtxView<'_> {
        WasiHttpCtxView {
            ctx: &mut self.http,
            table: &mut self.table,
            hooks: &mut self.hooks,
        }
    }
}

/// The origin a component asks for when it calls its own Endpoint (AP-147): the host, never the
/// component, knows where the gateway is and which Endpoint is the App's.
pub const GATEWAY_ALIAS: &str = "gateway";

/// Outgoing HTTP to the App's own Endpoint on the Context Gateway alone, with the caller's token
/// and never the App's own `Authorization` (AP-147). Anything else is refused before a connection
/// is made.
struct GatewayOnly {
    /// The gateway's origin and the App's Endpoint slug, when the host knows its gateway and the
    /// App's placement names its Endpoint.
    endpoint: Option<(String, String)>,
    token: Option<String>,
    client: reqwest::Client,
    /// The body an App may send the gateway, the cap of the request it was handed (T-4777).
    request_bytes: usize,
    response_bytes: usize,
}

/// Where a call of a component goes (AP-147): `http://gateway/api/endpoint/<slug>/…` with the
/// App's own slug, or for short `http://gateway/ngsi-ld/v1/…`, the Endpoint's OGC API - Features
/// surface `http://gateway/ogc/features/…` (EP-29) and its schema `http://gateway/schema/…`, all
/// to `<gateway>/api/endpoint/<slug>/…`. Any other origin, path or
/// Endpoint, and any path with a dot segment, an encoded dot or slash, or a backslash (which a URL
/// parser could fold into a step out of the Endpoint), is refused.
pub fn endpoint_target(gateway: &str, slug: &str, uri: &http::Uri) -> Option<String> {
    if uri.scheme_str() != Some("http")
        || !uri
            .authority()
            .is_some_and(|a| a.as_str().eq_ignore_ascii_case(GATEWAY_ALIAS))
    {
        return None;
    }
    let target = uri.path_and_query()?.as_str();
    let own = format!("/api/endpoint/{slug}");
    let below = target
        .strip_prefix(own.as_str())
        .filter(|rest| rest.starts_with('/'))
        .unwrap_or(target);
    let path = below.split('?').next().unwrap_or_default();
    let served = ["/ngsi-ld/v1", "/ogc/features"].iter().any(|root| {
        path.strip_prefix(root)
            .is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
    }) || path.starts_with("/schema/");
    if !served {
        return None;
    }
    let lower = path.to_ascii_lowercase();
    if path
        .split('/')
        .any(|segment| segment == "." || segment == "..")
        || lower.contains("%2e")
        || lower.contains("%2f")
        || lower.contains("%5c")
        || path.contains('\\')
    {
        return None;
    }
    Some(format!("{gateway}{own}{below}"))
}

impl WasiHttpHooks for GatewayOnly {
    fn send_request(
        &mut self,
        request: http::Request<WasiBody>,
        options: Option<RequestOptions>,
        fut: Box<dyn std::future::Future<Output = Result<(), wasmtime_wasi_http::Error>> + Send>,
    ) -> Box<
        dyn std::future::Future<
                Output = Result<
                    (
                        http::Response<WasiBody>,
                        Box<
                            dyn std::future::Future<Output = Result<(), wasmtime_wasi_http::Error>>
                                + Send,
                        >,
                    ),
                    wasmtime_wasi_http::Error,
                >,
            > + Send,
    > {
        _ = fut;
        let Some(url) = self
            .endpoint
            .as_ref()
            .and_then(|(gateway, slug)| endpoint_target(gateway, slug, request.uri()))
        else {
            tracing::warn!(to = %request.uri().authority().map(|a| a.as_str()).unwrap_or(""), "outgoing request refused: only the App's own Endpoint is reachable, as http://gateway");
            return Box::new(async { Err(wasmtime_wasi_http::Error::HttpRequestDenied) });
        };
        let client = self.client.clone();
        let token = self.token.clone();
        let cap = self.response_bytes;
        let sent_cap = self.request_bytes;
        Box::new(async move {
            let (parts, body) = request.into_parts();
            // Read to the request cap, never further: a component streaming without end would
            // otherwise fill the shard's memory (T-4777).
            let body = http_body_util::Limited::new(body, sent_cap)
                .collect()
                .await
                .map_err(|_| wasmtime_wasi_http::Error::HttpRequestBodySize(Some(sent_cap as u64)))?
                .to_bytes();
            let mut headers = parts.headers;
            headers.remove(http::header::AUTHORIZATION);
            headers.remove(http::header::COOKIE);
            headers.remove(http::header::HOST);
            let mut call = client
                .request(parts.method, url)
                .headers(headers)
                .body(body);
            if let Some(token) = token {
                call = call.bearer_auth(token);
            }
            if let Some(timeout) = options.and_then(|o| o.first_byte_timeout) {
                call = call.timeout(timeout);
            }
            let response = call.send().await.map_err(|err| {
                if err.is_timeout() {
                    wasmtime_wasi_http::Error::ConnectionReadTimeout
                } else {
                    wasmtime_wasi_http::Error::DestinationUnavailable
                }
            })?;
            let mut answer = http::Response::builder().status(response.status());
            for (name, value) in response.headers() {
                answer = answer.header(name, value);
            }
            // The gateway's answer read whole, to the response cap: an App holds no stream open.
            let mut bytes = Vec::new();
            let mut response = response;
            while let Some(chunk) = response
                .chunk()
                .await
                .map_err(|_| wasmtime_wasi_http::Error::HttpResponseIncomplete)?
            {
                if bytes.len() + chunk.len() > cap {
                    return Err(wasmtime_wasi_http::Error::HttpResponseBodySize(Some(
                        cap as u64,
                    )));
                }
                bytes.extend_from_slice(&chunk);
            }
            let body: WasiBody = Full::new(Bytes::from(bytes))
                .map_err(|never: std::convert::Infallible| match never {})
                .boxed_unsync();
            let answer = answer
                .body(body)
                .map_err(|_| wasmtime_wasi_http::Error::HttpProtocolError)?;
            Ok((
                answer,
                Box::new(async { Ok(()) }) as Box<dyn std::future::Future<Output = _> + Send>,
            ))
        })
    }
}

/// A request's place among those its App and its tenant may run at once; released when dropped.
pub struct Admission {
    _tenant: OwnedSemaphorePermit,
    _app: OwnedSemaphorePermit,
}

/// Why a request got no answer from its App, as the edge reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Failure {
    /// The App, or its tenant, has as many requests running as it may.
    Busy,
    /// The request body is larger than the cap.
    TooLarge,
    /// The component could not be fetched, did not match its digest, or did not compile.
    Unavailable(String),
    /// The App ran past its wall time.
    Timeout,
    /// The App trapped: out of memory, out of fuel, or its own fault.
    Failed(String),
}

impl Failure {
    pub fn status(&self) -> u16 {
        match self {
            Self::Busy => 429,
            Self::TooLarge => 413,
            Self::Unavailable(_) => 503,
            Self::Timeout => 504,
            Self::Failed(_) => 502,
        }
    }

    /// What the caller reads; never a path, a key or another App's name.
    pub fn detail(&self) -> &str {
        match self {
            Self::Busy => {
                "The application has as many requests running as it may; try again shortly."
            }
            Self::TooLarge => "The request body is larger than an application may receive.",
            Self::Unavailable(_) => "The application cannot be started right now.",
            Self::Timeout => "The application took longer than it may and was stopped.",
            Self::Failed(_) => "The application failed while answering.",
        }
    }
}

/// Components compiled once per digest, the least recently used dropped past the cap.
struct Cache {
    capacity: usize,
    order: VecDeque<String>,
    entries: HashMap<String, ProxyPre<State>>,
    /// Lookups answered from the cache, and lookups that had to fetch and compile.
    hits: u64,
    misses: u64,
}

impl Cache {
    fn get(&mut self, digest: &str) -> Option<ProxyPre<State>> {
        let pre = self.entries.get(digest)?.clone();
        self.order.retain(|d| d != digest);
        self.order.push_back(digest.to_owned());
        Some(pre)
    }

    fn put(&mut self, digest: &str, pre: ProxyPre<State>) {
        if self.entries.insert(digest.to_owned(), pre).is_none() {
            self.order.push_back(digest.to_owned());
        }
        while self.order.len() > self.capacity {
            if let Some(old) = self.order.pop_front() {
                self.entries.remove(&old);
            }
        }
    }
}

/// One shard's runtime.
pub struct Host {
    engine: Engine,
    linker: Linker<State>,
    limits: Limits,
    source: Source,
    storage: Arc<dyn Storage>,
    /// The gateway's origin, `scheme://host[:port]`, without a trailing slash.
    gateway: Option<String>,
    /// The Portal's origin inside the cluster, which `identity`, `email` and `ai` ask.
    portal: Option<String>,
    /// The client an App's calls to the gateway go through: no redirects, so a gateway answer
    /// can never send one elsewhere.
    client: reqwest::Client,
    cache: Mutex<Cache>,
    /// One compile per digest in flight: a burst of first requests to a cold App waits on the
    /// one fetch and compile instead of each repeating it (F-PLT3-022).
    loading: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    apps: Mutex<HashMap<String, Arc<Semaphore>>>,
    tenants: Mutex<HashMap<String, Arc<Semaphore>>>,
}

impl Host {
    /// The engine with its limits, and a linker offering WASI (without its sockets' reach),
    /// `wasi:http` and `jc:app`. `gateway` is the one origin an App may call, `https://host[:port]`;
    /// `portal` the Portal the host asks for an App's caller and services.
    pub fn new(
        limits: Limits,
        source: Source,
        storage: Arc<dyn Storage>,
        gateway: Option<&str>,
        portal: Option<&str>,
    ) -> wasmtime::Result<Arc<Self>> {
        let mut pooling = PoolingAllocationConfig::new();
        pooling.total_component_instances(limits.pooled_instances);
        pooling.total_core_instances(limits.pooled_instances.saturating_mul(4));
        pooling.total_memories(limits.pooled_instances.saturating_mul(2));
        pooling.total_tables(limits.pooled_instances.saturating_mul(2));
        pooling.max_memory_size(limits.memory_bytes);
        let mut config = Config::new();
        config.epoch_interruption(true);
        config.consume_fuel(true);
        config.allocation_strategy(InstanceAllocationStrategy::Pooling(pooling));
        let engine = Engine::new(&config)?;

        let mut linker = Linker::new(&engine);
        wasmtime_wasi::p2::add_to_linker_async(&mut linker)?;
        wasmtime_wasi_http::p2::add_only_http_to_linker_async(&mut linker)?;
        crate::storage::add_to_linker(&mut linker)?;

        let origin = |var: &str, value: Option<&str>| -> wasmtime::Result<Option<String>> {
            let Some(value) = value else {
                return Ok(None);
            };
            let url =
                url::Url::parse(value).map_err(|err| wasmtime::format_err!("{var}: {err}"))?;
            let host = url
                .host_str()
                .ok_or_else(|| wasmtime::format_err!("{var} has no host"))?;
            let authority = match url.port() {
                Some(port) => format!("{host}:{port}"),
                None => host.to_owned(),
            };
            Ok(Some(format!("{}://{authority}", url.scheme())))
        };
        let gateway = origin("JC_GATEWAY_URL", gateway)?;
        let portal = origin("JC_WASM_PORTAL_URL", portal)?;
        let host = Arc::new(Self {
            cache: Mutex::new(Cache {
                capacity: limits.cached_components,
                order: VecDeque::new(),
                entries: HashMap::new(),
                hits: 0,
                misses: 0,
            }),
            loading: Mutex::new(HashMap::new()),
            engine,
            linker,
            limits,
            source,
            storage,
            gateway,
            portal,
            client: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .timeout(std::time::Duration::from_secs(10))
                .build()
                .map_err(|err| wasmtime::format_err!("the gateway client: {err}"))?,
            apps: Mutex::new(HashMap::new()),
            tenants: Mutex::new(HashMap::new()),
        });
        // The clock every deadline counts on: one tick per EPOCH_TICK, for as long as the host
        // lives. A thread of its own, never a tokio task: guest code that spins never yields, and
        // as many spinning requests as worker threads would stop a task clock and every deadline
        // with it (T-4591).
        let weak = Arc::downgrade(&host);
        std::thread::Builder::new()
            .name("wasm-epoch".into())
            .spawn(move || {
                while let Some(host) = weak.upgrade() {
                    host.engine.increment_epoch();
                    drop(host);
                    std::thread::sleep(EPOCH_TICK);
                }
            })
            .map_err(|err| wasmtime::format_err!("the epoch clock: {err}"))?;
        Ok(host)
    }

    pub fn limits(&self) -> &Limits {
        &self.limits
    }

    /// How many components are compiled and kept.
    pub fn cached(&self) -> usize {
        self.cache_stats().cached
    }

    /// The compiled-component cache: its size and how its lookups went since the shard started.
    pub fn cache_stats(&self) -> CacheStats {
        self.cache
            .lock()
            .map(|c| CacheStats {
                cached: c.entries.len(),
                hits: c.hits,
                misses: c.misses,
            })
            .unwrap_or_default()
    }

    /// The App's component, compiled once: fetched by its digest and refused unless the bytes
    /// hash to it (AP-143).
    pub async fn component(&self, digest: &str) -> Result<ProxyPre<State>, Failure> {
        if let Some(pre) = self.cached_pre(digest)? {
            return Ok(pre);
        }
        let gate = self
            .loading
            .lock()
            .map_err(|_| Failure::Unavailable("cache".into()))?
            .entry(digest.to_owned())
            .or_default()
            .clone();
        let loaded = {
            let _turn = gate.lock().await;
            match self.cached_pre(digest)? {
                Some(pre) => Ok(pre),
                None => self.load(digest).await,
            }
        };
        if let Ok(mut loading) = self.loading.lock() {
            if loading.get(digest).is_some_and(|g| Arc::ptr_eq(g, &gate))
                && Arc::strong_count(&gate) <= 2
            {
                loading.remove(digest);
            }
        }
        loaded
    }

    /// The cached component of `digest`, counted as a hit; a miss is counted by [`Self::load`].
    fn cached_pre(&self, digest: &str) -> Result<Option<ProxyPre<State>>, Failure> {
        let mut cache = self
            .cache
            .lock()
            .map_err(|_| Failure::Unavailable("cache".into()))?;
        let pre = cache.get(digest);
        if pre.is_some() {
            cache.hits += 1;
        }
        Ok(pre)
    }

    /// Fetches, checks and compiles `digest`, then caches it; a failure is not cached.
    async fn load(&self, digest: &str) -> Result<ProxyPre<State>, Failure> {
        self.cache
            .lock()
            .map_err(|_| Failure::Unavailable("cache".into()))?
            .misses += 1;
        let bytes = self
            .source
            .fetch(digest)
            .await
            .map_err(Failure::Unavailable)?;
        let found = format!("sha256:{}", hex::encode(Sha256::digest(&bytes)));
        if found != digest {
            tracing::error!(%digest, %found, "a component does not match the digest its build recorded");
            return Err(Failure::Unavailable(format!(
                "the component is {found}, its placement names {digest}"
            )));
        }
        let engine = self.engine.clone();
        let component = tokio::task::spawn_blocking(move || Component::new(&engine, &bytes))
            .await
            .map_err(|err| Failure::Unavailable(err.to_string()))?
            .map_err(|err| {
                Failure::Unavailable(format!("the component does not compile: {err}"))
            })?;
        let pre = self
            .linker
            .instantiate_pre(&component)
            .and_then(ProxyPre::new)
            .map_err(|err| {
                Failure::Unavailable(format!("the component is no wasi:http App: {err}"))
            })?;
        self.cache
            .lock()
            .map_err(|_| Failure::Unavailable("cache".into()))?
            .put(digest, pre.clone());
        Ok(pre)
    }

    fn permit(
        map: &Mutex<HashMap<String, Arc<Semaphore>>>,
        key: &str,
        cap: usize,
    ) -> Result<OwnedSemaphorePermit, Failure> {
        let semaphore = map
            .lock()
            .map_err(|_| Failure::Busy)?
            .entry(key.to_owned())
            .or_insert_with(|| Arc::new(Semaphore::new(cap)))
            .clone();
        semaphore.try_acquire_owned().map_err(|_| Failure::Busy)
    }

    /// A fresh store for one instance of `app` under the request's limits, its calls to the
    /// gateway carrying `token`, stopped after `deadline_ticks` epochs. `person` is the request's
    /// person's token and the `Host` it came on, which the services are asked with; a job has none.
    fn store(
        &self,
        app: &Placed,
        token: Option<String>,
        person: Option<(String, Option<String>)>,
        deadline_ticks: u64,
    ) -> Result<Store<State>, Failure> {
        let (person, origin) = person.map_or((None, None), |(token, origin)| (Some(token), origin));
        let wasi = WasiCtx::builder()
            .allow_tcp(false)
            .allow_udp(false)
            .allow_ip_name_lookup(false)
            .build();
        let mut store = Store::new(
            &self.engine,
            State {
                wasi,
                http: WasiHttpCtx::new(),
                table: ResourceTable::new(),
                limits: Budget::new(self.limits.memory_bytes),
                hooks: GatewayOnly {
                    endpoint: match (&self.gateway, &app.endpoint) {
                        (Some(gateway), Some(slug)) => Some((gateway.clone(), slug.clone())),
                        _ => None,
                    },
                    token,
                    client: self.client.clone(),
                    request_bytes: self.limits.request_bytes,
                    response_bytes: self.limits.response_bytes,
                },
                app: app.clone(),
                storage: self.storage.clone(),
                services: crate::services::Services::new(
                    self.portal
                        .clone()
                        .map(|portal| (portal, self.client.clone())),
                    person,
                    origin,
                    self.limits.response_bytes,
                ),
            },
        );
        store.limiter(|state| &mut state.limits);
        store
            .set_fuel(self.limits.fuel)
            .map_err(|err| Failure::Failed(err.to_string()))?;
        // Each tick yields to the runtime, so a spinning instance leaves the worker thread to
        // the other requests; at the wall time it traps (T-4591).
        let ends = std::time::Instant::now()
            + EPOCH_TICK * u32::try_from(deadline_ticks).unwrap_or(u32::MAX);
        store.set_epoch_deadline(1);
        store.epoch_deadline_callback(move |_| {
            if std::time::Instant::now() >= ends {
                Err(Trap::Interrupt.into())
            } else {
                Ok(UpdateDeadline::Yield(1))
            }
        });
        Ok(store)
    }

    /// One run of the job export `export` of `app` (AP-154, AP-159): a fresh instance under the
    /// request's limits and `wall` of wall time, its calls to the App's own Endpoint carrying the
    /// job principal's `token`, which the component never sees. The export is a root-level
    /// `func() -> result<_, string>`; its `err` is the run's sentence.
    pub async fn run_job(
        &self,
        app: &Placed,
        export: &str,
        token: String,
        wall: std::time::Duration,
    ) -> Result<(), String> {
        let admission = self.admit(app).map_err(|_| {
            "the App was at its limit of concurrent requests; the run waits for its next time"
                .to_owned()
        })?;
        let pre = self
            .component(&app.digest)
            .await
            .map_err(|failure| match failure {
                Failure::Unavailable(why) | Failure::Failed(why) => why,
                other => other.detail().to_owned(),
            })?;
        let ticks = (wall.as_millis() / EPOCH_TICK.as_millis()).max(1) as u64;
        let mut store = self
            .store(app, Some(token), None, ticks)
            .map_err(|f| f.detail().to_owned())?;
        let export = export.to_owned();
        let id = app.id.clone();
        // As a request's: the admission lives as long as the instance, and past the wall time the
        // instance is stopped, not left waiting on the host outside the App's cap (F-R2PLT4-010).
        let mut task = tokio::spawn(async move {
            let _admission = admission;
            let instance = pre.instance_pre().instantiate_async(&mut store).await?;
            let Ok(func) =
                instance.get_typed_func::<(), (Result<(), String>,)>(&mut store, export.as_str())
            else {
                return Ok(Err(format!(
                    "the component exports no `{export}: func() -> result<_, string>`"
                )));
            };
            let (outcome,) = func.call_async(&mut store, ()).await?;
            wasmtime::Result::Ok(outcome)
        });
        match tokio::time::timeout(wall + std::time::Duration::from_secs(1), &mut task).await {
            Err(_) => {
                task.abort();
                Err("the job ran past its wall time and was stopped".to_owned())
            }
            Ok(Err(err)) => Err(format!("the run ended abnormally: {err}")),
            Ok(Ok(Ok(Ok(())))) => Ok(()),
            Ok(Ok(Ok(Err(why)))) => Err(sentence(&why)),
            Ok(Ok(Err(err))) => Err(match trapped(&id, &err) {
                Failure::Timeout => "the job ran past its wall time and was stopped".to_owned(),
                Failure::Failed(why) => format!("the job stopped: {}", sentence(&why)),
                other => other.detail().to_owned(),
            }),
        }
    }
    /// One request of one App, in a fresh instance under the request's limits. The App sees the
    /// path below `/apps/{name}`, and neither the caller's `Authorization` nor its cookies: the
    /// caller's token reaches the gateway through the host alone (AP-147).
    pub async fn serve(
        &self,
        app: &Placed,
        request: http::Request<Bytes>,
        token: Option<String>,
    ) -> Result<http::Response<HyperOutgoingBody>, Failure> {
        let admission = self.admit(app)?;
        self.serve_admitted(app, admission, request, token).await
    }

    /// A place among the requests the App and its tenant may run at once, taken before anything of
    /// the request is read, so a flood of bodies to one App waits on that App's cap alone (T-3342).
    pub fn admit(&self, app: &Placed) -> Result<Admission, Failure> {
        let tenant = Self::permit(
            &self.tenants,
            &app.tenant,
            self.limits.per_tenant_concurrency,
        )?;
        let own = Self::permit(&self.apps, &app.id, self.limits.per_app_concurrency)?;
        Ok(Admission {
            _tenant: tenant,
            _app: own,
        })
    }

    /// [`Host::serve`] for a request already admitted.
    pub async fn serve_admitted(
        &self,
        app: &Placed,
        admission: Admission,
        request: http::Request<Bytes>,
        token: Option<String>,
    ) -> Result<http::Response<HyperOutgoingBody>, Failure> {
        if request.body().len() > self.limits.request_bytes {
            return Err(Failure::TooLarge);
        }
        let pre = self.component(&app.digest).await?;
        let (mut parts, body) = request.into_parts();
        // The services are asked on the host the request came on, the App's own (AP-133).
        let came_on = parts
            .uri
            .authority()
            .map(|a| a.as_str().to_owned())
            .or_else(|| {
                parts
                    .headers
                    .get(http::header::HOST)
                    .and_then(|h| h.to_str().ok())
                    .map(str::to_owned)
            });
        let person = token.clone().map(|token| (token, came_on));
        let mut store = self.store(app, token, person, self.limits.deadline_ticks())?;
        parts.headers.remove(http::header::AUTHORIZATION);
        parts.headers.remove(http::header::COOKIE);
        // The App sees the path below `/apps/{name}`, under the authority the caller asked.
        let prefix = format!("/apps/{}", app.name);
        let path = parts
            .uri
            .path_and_query()
            .map(|p| p.as_str())
            .unwrap_or("/");
        let below = path.strip_prefix(&prefix).unwrap_or(path);
        let below = if below.is_empty() { "/" } else { below };
        let authority = parts
            .uri
            .authority()
            .map(|a| a.as_str().to_owned())
            .or_else(|| {
                parts
                    .headers
                    .get(http::header::HOST)
                    .and_then(|h| h.to_str().ok())
                    .map(str::to_owned)
            })
            .unwrap_or_else(|| "localhost".into());
        parts.uri = format!("http://{authority}{below}")
            .parse()
            .map_err(|_| Failure::Failed("the request address".into()))?;
        let body = Full::new(body).map_err(|never: std::convert::Infallible| -> wasmtime_wasi_http::p2::bindings::http::types::ErrorCode { match never {} });
        let request = http::Request::from_parts(parts, body);

        let (sender, receiver) = tokio::sync::oneshot::channel();
        let incoming = store
            .data_mut()
            .http()
            .new_incoming_request(Scheme::Http, request)
            .map_err(|err| Failure::Failed(err.to_string()))?;
        let out = store
            .data_mut()
            .http()
            .new_response_outparam(sender)
            .map_err(|err| Failure::Failed(err.to_string()))?;
        // The admission lives as long as the instance, not the answer: an App that answers and runs
        // on keeps its place until its wall time ends it (T-4777), and its store is dropped before
        // the place is given back (T-4807).
        let task = tokio::spawn(async move {
            let _admission = admission;
            let mut store = store;
            let proxy = pre.instantiate_async(&mut store).await?;
            proxy
                .wasi_http_incoming_handler()
                .call_handle(&mut store, incoming, out)
                .await
        });
        // The wall time holds while the App waits on the host too (a slow gateway call): past it,
        // and a second's grace, the request is over and its instance is dropped (T-3342).
        let receiver = match tokio::time::timeout(
            self.limits.wall_time + std::time::Duration::from_secs(1),
            receiver,
        )
        .await
        {
            Ok(received) => received,
            Err(_) => {
                task.abort();
                tracing::warn!(app = %app.id, "an App ran past its wall time while waiting on the host");
                return Err(Failure::Timeout);
            }
        };
        match receiver {
            Ok(Ok(response)) => Ok(response),
            Ok(Err(code)) => Err(Failure::Failed(format!("{code:?}"))),
            Err(_) => Err(match task.await {
                Ok(Ok(())) => Failure::Failed("the App never answered".into()),
                Ok(Err(err)) => trapped(&app.id, &err),
                Err(err) => Failure::Failed(err.to_string()),
            }),
        }
    }
}

/// A job's own sentence, at most 300 characters on one line, so a record stays readable.
fn sentence(why: &str) -> String {
    let line: String = why.split_whitespace().collect::<Vec<_>>().join(" ");
    match line.char_indices().nth(300) {
        Some((at, _)) => format!("{}…", &line[..at]),
        None => line,
    }
}

/// A trap as the caller reads it: past the wall time is a timeout, anything else a failure; the
/// log keeps what happened, with the App's id.
fn trapped(id: &str, err: &wasmtime::Error) -> Failure {
    let trap = err.downcast_ref::<Trap>().copied();
    tracing::warn!(app = %id, trap = ?trap, error = %format!("{err:#}"), "an App stopped");
    match trap {
        Some(Trap::Interrupt) => Failure::Timeout,
        Some(Trap::OutOfFuel) => Failure::Failed("out of fuel".into()),
        _ => Failure::Failed(format!("{err:#}")),
    }
}

/// The response body cut at `cap` bytes: an App that answers more is stopped mid-body.
pub fn capped(
    body: HyperOutgoingBody,
    cap: usize,
) -> http_body_util::combinators::UnsyncBoxBody<Bytes, Box<dyn std::error::Error + Send + Sync>> {
    http_body_util::Limited::new(body, cap).boxed_unsync()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// T-4777: two memories of one instance share one budget; each alone stays under its limit.
    #[test]
    fn every_memory_of_an_instance_counts_against_one_budget() {
        let mut budget = Budget::new(100);
        assert!(budget.memory_growing(0, 60, None).unwrap());
        let second = budget.memory_growing(0, 60, None).unwrap_err();
        assert!(second.to_string().contains("100 bytes"), "{second}");
        assert!(budget.memory_growing(60, 90, None).unwrap());
        assert!(budget.memory_growing(0, 10, None).unwrap());
        assert!(budget.memory_growing(0, 1, None).is_err());
        assert!(Budget::new(100).memory_growing(0, 101, None).is_err());
    }

    fn target(uri: &str) -> Option<String> {
        endpoint_target("http://gw:8080", "ep1", &uri.parse().expect("uri"))
    }

    #[test]
    fn the_alias_reaches_the_apps_own_endpoint() {
        for (uri, to) in [
            (
                "http://gateway/ngsi-ld/v1/entities?type=Alert&limit=100",
                "http://gw:8080/api/endpoint/ep1/ngsi-ld/v1/entities?type=Alert&limit=100",
            ),
            (
                "http://GATEWAY/ngsi-ld/v1",
                "http://gw:8080/api/endpoint/ep1/ngsi-ld/v1",
            ),
            (
                "http://gateway/schema/v1/json-schema",
                "http://gw:8080/api/endpoint/ep1/schema/v1/json-schema",
            ),
            (
                "http://gateway/ogc/features/collections/Alert/items?bbox=24.9,60.1,25.1,60.3&limit=10",
                "http://gw:8080/api/endpoint/ep1/ogc/features/collections/Alert/items?bbox=24.9,60.1,25.1,60.3&limit=10",
            ),
            (
                "http://gateway/api/endpoint/ep1/ogc/features",
                "http://gw:8080/api/endpoint/ep1/ogc/features",
            ),
            // The long form T-3346's Apps write, with the App's own slug.
            (
                "http://gateway/api/endpoint/ep1/ngsi-ld/v1/entities?type=A",
                "http://gw:8080/api/endpoint/ep1/ngsi-ld/v1/entities?type=A",
            ),
        ] {
            assert_eq!(target(uri).as_deref(), Some(to), "{uri}");
        }
    }

    #[test]
    fn another_origin_path_endpoint_or_a_step_out_is_refused() {
        for uri in [
            "https://gateway/ngsi-ld/v1/entities",
            "http://gw:8080/api/endpoint/ep1/ngsi-ld/v1/entities",
            "http://gateway:8080/ngsi-ld/v1/entities",
            "http://gateway/api/endpoint/other/ngsi-ld/v1/entities",
            "http://gateway/api/endpoint/ep12/ngsi-ld/v1/entities",
            "http://gateway/api/endpoint/ep1/mcp",
            "http://gateway/api/endpoint/ep1",
            "http://gateway/ngsi-ld/v1x",
            "http://gateway/ogc/featuresx",
            "http://gateway/ogc",
            "http://gateway/ogc/features/../../other/ogc/features",
            "http://gateway/api/endpoint/other/ogc/features/collections",
            "http://gateway/schema",
            "http://gateway/schemas/x",
            "http://gateway/schema/../../other/schema/index.json",
            "http://gateway/mcp",
            "http://gateway/ngsi-ld/v1/../../other/ngsi-ld/v1/entities",
            "http://gateway/ngsi-ld/v1/./entities",
            "http://gateway/ngsi-ld/v1/%2e%2e/%2E%2E/other",
            "http://gateway/ngsi-ld/v1/a%2Fb",
            "http://gateway/ngsi-ld/v1/a%5cb",
            "http://169.254.169.254/latest/meta-data",
        ] {
            assert_eq!(target(uri), None, "{uri}");
        }
        assert_eq!(
            target("http://gateway/ngsi-ld/v1/entities?q=name==%22a..b%22").as_deref(),
            Some("http://gw:8080/api/endpoint/ep1/ngsi-ld/v1/entities?q=name==%22a..b%22"),
            "dots in the query are data, not a path"
        );
    }
}
