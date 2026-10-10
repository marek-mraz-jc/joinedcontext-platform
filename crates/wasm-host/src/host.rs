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
    Config, Engine, InstanceAllocationStrategy, PoolingAllocationConfig, Store, StoreLimits,
    StoreLimitsBuilder, Trap,
};
use wasmtime_wasi::{WasiCtx, WasiCtxView, WasiView};
use wasmtime_wasi_http::p2::bindings::http::types::Scheme;
use wasmtime_wasi_http::p2::bindings::ProxyPre;
use wasmtime_wasi_http::p2::body::HyperOutgoingBody;
use wasmtime_wasi_http::{
    RequestOptions, WasiBody, WasiHttpCtx, WasiHttpCtxView, WasiHttpHooks, WasiHttpView,
};

use crate::limits::{Limits, EPOCH_TICK};
use crate::placement::Placed;
use crate::source::Source;
use crate::storage::Storage;

/// What one request's store holds: the WASI context (no environment, no files, no sockets), the
/// HTTP context, the limits, and the App it runs for.
pub struct State {
    wasi: WasiCtx,
    http: WasiHttpCtx,
    table: ResourceTable,
    limits: StoreLimits,
    hooks: GatewayOnly,
    /// The App this instance runs for: the storage interfaces answer as it and no other.
    pub app: Placed,
    pub storage: Arc<dyn Storage>,
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
    response_bytes: usize,
}

/// Where a call of a component goes (AP-147): `http://gateway/api/endpoint/<slug>/…` with the
/// App's own slug, or for short `http://gateway/ngsi-ld/v1/…` and the Endpoint's schema
/// `http://gateway/schema/…`, all to `<gateway>/api/endpoint/<slug>/…`. Any other origin, path or
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
    if !(path == "/ngsi-ld/v1" || path.starts_with("/ngsi-ld/v1/") || path.starts_with("/schema/"))
    {
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
        Box::new(async move {
            let (parts, body) = request.into_parts();
            let body = body
                .collect()
                .await
                .map_err(|_| wasmtime_wasi_http::Error::HttpRequestBodySize(None))?
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
    /// The client an App's calls to the gateway go through: no redirects, so a gateway answer
    /// can never send one elsewhere.
    client: reqwest::Client,
    cache: Mutex<Cache>,
    apps: Mutex<HashMap<String, Arc<Semaphore>>>,
    tenants: Mutex<HashMap<String, Arc<Semaphore>>>,
}

impl Host {
    /// The engine with its limits, and a linker offering WASI (without its sockets' reach),
    /// `wasi:http` and `jc:app`. `gateway` is the one origin an App may call, `https://host[:port]`.
    pub fn new(
        limits: Limits,
        source: Source,
        storage: Arc<dyn Storage>,
        gateway: Option<&str>,
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

        let gateway = match gateway {
            None => None,
            Some(origin) => {
                let url = url::Url::parse(origin)
                    .map_err(|err| wasmtime::format_err!("JC_GATEWAY_URL: {err}"))?;
                let host = url
                    .host_str()
                    .ok_or_else(|| wasmtime::format_err!("JC_GATEWAY_URL has no host"))?;
                let authority = match url.port() {
                    Some(port) => format!("{host}:{port}"),
                    None => host.to_owned(),
                };
                Some(format!("{}://{authority}", url.scheme()))
            }
        };
        let host = Arc::new(Self {
            cache: Mutex::new(Cache {
                capacity: limits.cached_components,
                order: VecDeque::new(),
                entries: HashMap::new(),
            }),
            engine,
            linker,
            limits,
            source,
            storage,
            gateway,
            client: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .timeout(std::time::Duration::from_secs(10))
                .build()
                .map_err(|err| wasmtime::format_err!("the gateway client: {err}"))?,
            apps: Mutex::new(HashMap::new()),
            tenants: Mutex::new(HashMap::new()),
        });
        // The clock every deadline counts on: one tick per EPOCH_TICK, for as long as the host lives.
        let weak = Arc::downgrade(&host);
        tokio::spawn(async move {
            let mut every = tokio::time::interval(EPOCH_TICK);
            while let Some(host) = weak.upgrade() {
                host.engine.increment_epoch();
                drop(host);
                every.tick().await;
            }
        });
        Ok(host)
    }

    pub fn limits(&self) -> &Limits {
        &self.limits
    }

    /// How many components are compiled and kept.
    pub fn cached(&self) -> usize {
        self.cache.lock().map(|c| c.entries.len()).unwrap_or(0)
    }

    /// The App's component, compiled once: fetched by its digest and refused unless the bytes
    /// hash to it (AP-143).
    pub async fn component(&self, digest: &str) -> Result<ProxyPre<State>, Failure> {
        if let Some(pre) = self
            .cache
            .lock()
            .map_err(|_| Failure::Unavailable("cache".into()))?
            .get(digest)
        {
            return Ok(pre);
        }
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
        _admission: Admission,
        request: http::Request<Bytes>,
        token: Option<String>,
    ) -> Result<http::Response<HyperOutgoingBody>, Failure> {
        if request.body().len() > self.limits.request_bytes {
            return Err(Failure::TooLarge);
        }
        let pre = self.component(&app.digest).await?;

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
                limits: StoreLimitsBuilder::new()
                    .memory_size(self.limits.memory_bytes)
                    .instances(64)
                    .trap_on_grow_failure(true)
                    .build(),
                hooks: GatewayOnly {
                    endpoint: match (&self.gateway, &app.endpoint) {
                        (Some(gateway), Some(slug)) => Some((gateway.clone(), slug.clone())),
                        _ => None,
                    },
                    token,
                    client: self.client.clone(),
                    response_bytes: self.limits.response_bytes,
                },
                app: app.clone(),
                storage: self.storage.clone(),
            },
        );
        store.limiter(|state| &mut state.limits);
        store
            .set_fuel(self.limits.fuel)
            .map_err(|err| Failure::Failed(err.to_string()))?;
        store.set_epoch_deadline(self.limits.deadline_ticks());
        store.epoch_deadline_trap();

        let (mut parts, body) = request.into_parts();
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
        let task = tokio::spawn(async move {
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
