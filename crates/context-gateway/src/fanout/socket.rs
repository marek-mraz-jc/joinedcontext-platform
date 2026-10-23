//! The sockets of the fan-out (ADR-N-049): the registry of hubs, each hub's one upstream to the
//! broker view, and each client's session.

use super::hub::{self, Answer, Hub, Out, Queue, KEPT};
use crate::app::Gateway;
use crate::auth::subject::Subject;
use crate::middleware::tenancy;
use crate::resolver::Endpoint;
use axum::body::Body;
use axum::extract::ws::{self, WebSocket};
use axum::http::header::ACCEPT;
use axum::http::{HeaderMap, HeaderValue, Method};
use futures_util::stream::SplitSink;
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Map, Value};
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::time::{Duration, Instant};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_tungstenite::tungstenite::Message;

/// Connections one space may hold when `JC_GATEWAY_WS_PER_SPACE` names no other number
/// (API/07 §3).
pub const PER_SPACE: usize = 1000;

/// `JC_GATEWAY_WS_PER_SPACE`: the space cap, raised for a measurement (the `ws-fanout` rig).
pub fn per_space() -> usize {
    static CAP: OnceLock<usize> = OnceLock::new();
    *CAP.get_or_init(|| env_cap("JC_GATEWAY_WS_PER_SPACE", PER_SPACE))
}
/// Hubs the gateway holds when `JC_GATEWAY_WS_HUBS` names no other number (API/07 §3): each is one
/// broker WebSocket and up to `JC_GATEWAY_WS_ENTITIES` entities, so this bounds both.
pub const HUBS: usize = 64;
/// Hubs one space may hold when `JC_GATEWAY_WS_HUBS_PER_SPACE` names no other number: under the
/// broker's 50 connections per tenant, so the broker never refuses a hub the gateway admitted.
pub const HUBS_PER_SPACE: usize = 32;

/// A positive whole number from `name`, else `default`.
fn env_cap(name: &str, default: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .filter(|n| *n > 0)
        .unwrap_or(default)
}

/// `JC_GATEWAY_WS_HUBS`: the hubs, and so the broker connections, of this replica.
pub fn hubs_cap() -> usize {
    static CAP: OnceLock<usize> = OnceLock::new();
    *CAP.get_or_init(|| env_cap("JC_GATEWAY_WS_HUBS", HUBS))
}

/// `JC_GATEWAY_WS_HUBS_PER_SPACE`: the hubs one space may hold in this replica.
pub fn hubs_per_space() -> usize {
    static CAP: OnceLock<usize> = OnceLock::new();
    *CAP.get_or_init(|| env_cap("JC_GATEWAY_WS_HUBS_PER_SPACE", HUBS_PER_SPACE))
}

/// Frames a client may send at once, then [`MESSAGES_PER_SEC`] a second; past that the
/// connection closes with `1008` (API/07 §3).
const MESSAGE_BURST: f64 = 100.0;
const MESSAGES_PER_SEC: f64 = 20.0;

/// How long a hub keeps its upstream after its last client left (ADR-N-049 §2.1).
const LINGER: Duration = Duration::from_secs(60);
/// The upstream's reconnect delay grows to this.
const MAX_BACKOFF: Duration = Duration::from_secs(30);
/// How often a hub checks its Endpoint and reports its table.
const TICK: Duration = Duration::from_secs(5);
/// The server pings this often and ends a connection that answered no ping for twice as long
/// (API/07 §3).
const PING: Duration = Duration::from_secs(30);
/// A connection with no subscription that sent nothing for this long ends (API/07 §3).
const IDLE: Duration = Duration::from_secs(60);
/// A client whose socket takes no frame for this long is not reading: it is closed as `overflow`
/// and gives its slot back, instead of parking its session on a full send buffer (T-4902).
const STALL: Duration = Duration::from_secs(10);
/// The entities a snapshot page asks for.
const PAGE: usize = 1000;
/// What the broker writes: a notification of up to 1 MiB (Antares `MAX_NOTIFICATION_BYTES`).
const FROM_BROKER: usize = 1024 * 1024;
/// RFC 6455 7.4.1 close codes.
const NORMAL: u16 = 1000;
const UNSUPPORTED: u16 = 1003;
const TOO_BIG: u16 = 1009;
const POLICY: u16 = 1008;
const INTERNAL: u16 = 1011;

/// `JC_GATEWAY_WS_ENTITIES`, the entity table's bound per Endpoint (ADR-N-049 §2.2).
fn bound() -> usize {
    static BOUND: OnceLock<usize> = OnceLock::new();
    *BOUND.get_or_init(|| env_cap("JC_GATEWAY_WS_ENTITIES", hub::DEFAULT_ENTITIES))
}

/// Every hub, keyed by `hub_key`: one per Endpoint and caller identity (T-4755).
#[derive(Default)]
pub struct Hubs(Mutex<HashMap<String, (String, Arc<Hub>)>>);

/// Why a connection could not join.
#[derive(Debug, PartialEq, Eq)]
pub enum Refused {
    /// The space holds [`per_space`] connections.
    SpaceFull,
    /// The caller's identity is no header value.
    NoIdentity,
    /// The gateway holds [`hubs_cap`] hubs, or the space [`hubs_per_space`], each with a client.
    HubsFull,
}

impl Hubs {
    /// Opens a connection on the hub of the Endpoint and the caller's identity, starting the hub
    /// and its upstream when it has none; under the registry's lock, so a hub never stops between
    /// the join and the open.
    pub fn join(
        &self,
        gateway: &Arc<Gateway>,
        endpoint: Arc<Endpoint>,
        subject: Subject,
        principal: String,
    ) -> Result<hub::Joined, Refused> {
        let headers = upstream_headers(&endpoint.space, &subject).ok_or(Refused::NoIdentity)?;
        let key = hub_key(&endpoint.slug, &headers);
        let mut hubs = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        let in_space: usize = hubs
            .values()
            .filter(|(space, _)| *space == endpoint.space)
            .map(|(_, hub)| hub.connections())
            .sum();
        if in_space >= per_space() {
            return Err(Refused::SpaceFull);
        }
        let hub = match hubs.get(&key) {
            Some((_, hub)) => Arc::clone(hub),
            None => {
                make_room(&mut hubs, &endpoint.space, hubs_cap(), hubs_per_space())?;
                let hub = Arc::new(Hub::new(&endpoint.slug, bound()));
                hubs.insert(key, (endpoint.space.clone(), Arc::clone(&hub)));
                metrics::gauge!("jc_gateway_ws_hubs").set(hubs.len() as f64);
                tokio::spawn(upstream(Arc::clone(gateway), Arc::clone(&hub), headers));
                hub
            }
        };
        let (conn, queue) = hub.open(endpoint, principal, subject.client);
        Ok(hub::Joined::new(hub, conn, queue))
    }

    /// Judges every open connection again after a reload of the Endpoint or account tables
    /// (T-4874): what [`Hub::recheck`] names ends at once.
    pub fn recheck(&self, gateway: &Gateway) {
        let hubs = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        for (_, hub) in hubs.values() {
            recheck(gateway, hub);
        }
    }

    /// Drops `hub` when it has been without a connection for [`LINGER`]; `true` when it did, or
    /// when [`make_room`] dropped it already.
    fn leave_if_idle(&self, hub: &Arc<Hub>) -> bool {
        self.leave(hub, |hub| hub.idle_for().is_some_and(|idle| idle >= LINGER))
    }

    /// Drops `hub` when the registry no longer holds it or `done` says so; `true` when it is gone.
    fn leave(&self, hub: &Arc<Hub>, done: impl Fn(&Hub) -> bool) -> bool {
        let mut hubs = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        let held = hubs.values().any(|(_, held)| Arc::ptr_eq(held, hub));
        if held && !done(hub) {
            return false;
        }
        hubs.retain(|_, (_, held)| !Arc::ptr_eq(held, hub));
        metrics::gauge!("jc_gateway_ws_hubs").set(hubs.len() as f64);
        true
    }
}

/// Makes room for one more hub in `space`: at a cap, drops the hub idle longest (no connection, in
/// its linger), whose upstream then stops at its next tick; refused when every hub has a client.
fn make_room(
    hubs: &mut HashMap<String, (String, Arc<Hub>)>,
    space: &str,
    total: usize,
    per_space: usize,
) -> Result<(), Refused> {
    for (cap, in_space) in [(per_space, true), (total, false)] {
        let counted = |held: &str| !in_space || held == space;
        if hubs.values().filter(|(held, _)| counted(held)).count() < cap {
            continue;
        }
        let idlest = hubs
            .iter()
            .filter(|(_, (held, _))| counted(held))
            .filter_map(|(key, (_, hub))| Some((hub.idle_for()?, key.clone())))
            .max();
        let (_, key) = idlest.ok_or(Refused::HubsFull)?;
        hubs.remove(&key);
    }
    Ok(())
}

/// A token bucket over the frames one client sends (API/07 §3). It runs before a message is
/// parsed, so a flood of `x-subscribe`, `x-update` or Pings costs a comparison a frame.
struct Rate {
    tokens: f64,
    at: Instant,
}

impl Rate {
    fn new(now: Instant) -> Self {
        Self {
            tokens: MESSAGE_BURST,
            at: now,
        }
    }

    /// Whether one more frame at `now` stays within the bound.
    fn take(&mut self, now: Instant) -> bool {
        let earned = now.duration_since(self.at).as_secs_f64() * MESSAGES_PER_SEC;
        self.tokens = (self.tokens + earned).min(MESSAGE_BURST);
        self.at = now;
        if self.tokens < 1.0 {
            return false;
        }
        self.tokens -= 1.0;
        true
    }
}

/// [`Hub::recheck`] of `hub` against the gateway's current tables.
fn recheck(gateway: &Gateway, hub: &Hub) {
    let accounts = gateway.accounts.load();
    let closed = hub.recheck(gateway.resolver.resolve(&hub.slug).as_ref(), |azp| {
        accounts.admits_on(azp, &hub.slug)
    });
    if !closed.is_empty() {
        tracing::info!(slug = %hub.slug, closed = closed.len(), "WebSocket connections closed by a reload");
    }
}

type Upstream =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// The headers of a hub's upstream hops: the space's tenant and the caller's identity, as a REST
/// read through the view carries them (T-4755). The broker's view and engine decide what the
/// upstream subscription and its snapshot read, so a delivery holds only what this caller may
/// read; nothing in the gateway cuts it again. `None` when the identity is no header value.
fn upstream_headers(space: &str, subject: &Subject) -> Option<HeaderMap> {
    let mut request = axum::extract::Request::new(Body::empty());
    tenancy::pin_caller(&mut request, space, subject).ok()?;
    let mut headers = std::mem::take(request.headers_mut());
    // Provenance names a writer and one request; the upstream writes nothing, and a hub is
    // shared by identity.
    headers.remove(tenancy::PROVENANCE_CHANNEL);
    headers.remove(tenancy::PROVENANCE_ACTOR);
    headers.remove(tenancy::PROVENANCE_ORGANIZATION);
    headers.remove(tenancy::PROVENANCE_ACTOR_KIND);
    headers.remove(tenancy::REQUEST_ID);
    Some(headers)
}

/// The registry key of a hub: the Endpoint and every header its upstream speaks with, so two
/// callers share a hub only when the broker cannot tell them apart.
fn hub_key(slug: &str, headers: &HeaderMap) -> String {
    let mut pairs: Vec<String> = headers
        .iter()
        .map(|(name, value)| format!("{name}={}", String::from_utf8_lossy(value.as_bytes())))
        .collect();
    pairs.sort_unstable();
    format!("{slug}\n{}", pairs.join("\n"))
}

async fn connect(gateway: &Gateway, slug: &str, headers: &HeaderMap) -> Result<Upstream, String> {
    let path = format!("/x/views/{slug}/ws");
    let base = gateway
        .broker
        .base_for(&path, headers)
        .map_err(|e| e.to_string())?;
    let authority = base
        .strip_prefix("http://")
        .ok_or("the fan-out reaches only an http:// broker")?
        .trim_end_matches('/');
    let mut request = format!("ws://{authority}{path}")
        .into_client_request()
        .map_err(|e| e.to_string())?;
    for (name, value) in headers {
        request.headers_mut().append(name.clone(), value.clone());
    }
    let config = WebSocketConfig::default()
        .max_message_size(Some(FROM_BROKER))
        .max_frame_size(Some(FROM_BROKER));
    tokio_tungstenite::connect_async_with_config(request, Some(config), false)
        .await
        .map(|(socket, _)| socket)
        .map_err(|e| e.to_string())
}

/// The one subscription of the hub: every type the view serves, every attribute, normalized,
/// and the entity's deletion so the table lets it go.
fn upstream_subscription() -> Value {
    json!({
        "metadata": {"x-op": "x-subscribe", "x-id": "up"},
        "body": {
            "type": "Subscription",
            "entities": [{"type": "*"}],
            "notificationTrigger": ["entityCreated", "entityUpdated", "entityDeleted"],
            "notification": {"format": "normalized"},
        },
    })
}

/// A hub's upstream: connect, subscribe once (or resume where the last connection ended), read
/// the snapshot, then apply every notification; reconnect with backoff; stop once the hub has
/// been idle for [`LINGER`].
async fn upstream(gateway: Arc<Gateway>, hub: Arc<Hub>, headers: HeaderMap) {
    let mut backoff = Duration::from_secs(1);
    // The broker's connection name, resume token and last x-seq, after a clean lifetime end.
    let mut resume: Option<(String, String, u64)> = None;
    let mut started = false;
    loop {
        if gateway.hubs.leave_if_idle(&hub) {
            tracing::info!(slug = %hub.slug, "the fan-out stopped: no client left");
            return;
        }
        let socket = match connect(&gateway, &hub.slug, &headers).await {
            Ok(socket) => socket,
            Err(error) => {
                tracing::warn!(slug = %hub.slug, %error, "the fan-out upstream did not open");
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(MAX_BACKOFF);
                continue;
            }
        };
        backoff = Duration::from_secs(1);
        let ended = run(
            &gateway,
            &hub,
            &headers,
            socket,
            resume.take(),
            &mut started,
        )
        .await;
        resume = ended;
        match hub.refused() {
            Some(status) if lasting(status) => hold(&gateway, &hub).await,
            Some(_) => tokio::time::sleep(MAX_BACKOFF).await,
            None => {}
        }
    }
}

/// Whether the broker answers `status` to the hub's subscription until the Endpoint changes:
/// 403 (no Policy lists the gateway), 404 (no such view) or 405 (the view does not serve
/// subscriptions). Any other refusal is retried after [`MAX_BACKOFF`].
fn lasting(status: u16) -> bool {
    matches!(status, 403..=405)
}

/// After a [`lasting`] refusal the upstream stays closed while the Endpoint resolves to the same
/// record (T-5321): a reconnect would only be refused again. The hub keeps the refusal, so a
/// client's `x-subscribe` is answered at once without a broker round. A changed record (the
/// next reconcile) opens the upstream again; a removed one, or [`make_room`] taking the idle
/// hub, stops it.
async fn hold(gateway: &Gateway, hub: &Arc<Hub>) {
    let refused_on = gateway.resolver.resolve(&hub.slug);
    tracing::info!(slug = %hub.slug, "the fan-out upstream waits for the Endpoint to change");
    let mut tick = tokio::time::interval(TICK);
    loop {
        tick.tick().await;
        recheck(gateway, hub);
        let now = gateway.resolver.resolve(&hub.slug);
        if gateway.hubs.leave(hub, |_| now.is_none()) || now != refused_on {
            return;
        }
    }
}

/// One upstream connection, until it ends; what a resume needs when it ended by its lifetime.
async fn run(
    gateway: &Arc<Gateway>,
    hub: &Arc<Hub>,
    headers: &HeaderMap,
    socket: Upstream,
    resume: Option<(String, String, u64)>,
    started: &mut bool,
) -> Option<(String, String, u64)> {
    let (mut to_broker, mut from_broker) = socket.split();
    let mut name = String::new();
    let mut token = String::new();
    let mut seq = 0_u64;
    let mut tick = tokio::time::interval(TICK);
    let first = match &resume {
        Some((name, token, seq)) => json!({"metadata": {
            "x-op": "x-resume", "x-id": "resume",
            "x-connection": name, "x-resume-token": token, "x-seq": seq}}),
        None => upstream_subscription(),
    };
    if let Some((_, _, last)) = &resume {
        seq = *last;
    }
    let mut sent_first = false;
    loop {
        let frame = tokio::select! {
            frame = from_broker.next() => frame,
            // A first filter on a hub whose table is due: read it now (T-5182).
            () = hub.snapshot_wanted(), if *started => {
                read_if_wanted(gateway, hub, headers);
                continue;
            }
            _ = tick.tick() => {
                // The reload rechecks at once (Hubs::recheck); this tick catches a connection
                // that joined on the old table while the reload ran.
                recheck(gateway, hub);
                // ponytail: an Endpoint with several identities' hubs reports the last one's
                // table; a sum over the registry when a dashboard needs the Endpoint's total.
                metrics::gauge!("jc_gateway_ws_entities", "endpoint" => hub.slug.clone())
                    .set(hub.entities() as f64);
                if gateway.hubs.leave_if_idle(hub) {
                    let _ = to_broker.close().await;
                    return None;
                }
                continue;
            }
        };
        let text = match frame {
            Some(Ok(Message::Text(text))) => text,
            Some(Ok(Message::Close(_))) | None => return lost(hub, &name, &token, seq),
            Some(Ok(_)) => continue,
            Some(Err(error)) => {
                tracing::warn!(slug = %hub.slug, %error, "the fan-out upstream failed");
                return lost(hub, &name, &token, seq);
            }
        };
        let Ok((metadata, body)) = hub::envelope(text.as_str()) else {
            continue;
        };
        let op = metadata.get("x-op").and_then(Value::as_str).unwrap_or("");
        match op {
            "x-welcome" => {
                name = text_of(&metadata, "x-connection");
                token = text_of(&metadata, "x-resume-token");
                if !sent_first {
                    sent_first = true;
                    if to_broker
                        .send(Message::text(first.to_string()))
                        .await
                        .is_err()
                    {
                        return None;
                    }
                }
            }
            "x-result" => {
                let status = metadata
                    .get("x-status")
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                match (text_of(&metadata, "x-id").as_str(), status) {
                    ("resume", 200..=299) | ("up", 200..=299) if *started => {}
                    ("up", 200..=299) => {
                        *started = true;
                        hub.live();
                        read_if_wanted(gateway, hub, headers);
                    }
                    ("resume", _) => {
                        // The broker no longer holds our place: subscribe again, read again,
                        // and tell every client it may have missed something.
                        seq = 0;
                        hub.gap();
                        let again = upstream_subscription().to_string();
                        if to_broker.send(Message::text(again)).await.is_err() {
                            return None;
                        }
                        read_if_wanted(gateway, hub, headers);
                    }
                    (id, status) => {
                        tracing::warn!(slug = %hub.slug, id, status, ?body, "the broker refused the fan-out subscription");
                        // Never a silent 201: the clients learn it (ADR-N-049 §2 item 10).
                        let detail = body
                            .as_ref()
                            .and_then(|b| b.get("detail"))
                            .and_then(Value::as_str)
                            .unwrap_or("no detail");
                        hub.refuse(u16::try_from(status).unwrap_or(0), detail.to_owned());
                        *started = false;
                        return None;
                    }
                }
            }
            "x-notification" => {
                seq = metadata.get("x-seq").and_then(Value::as_u64).unwrap_or(seq);
                let now = chrono::Utc::now();
                for entity in body
                    .as_ref()
                    .and_then(|b| b.get("data"))
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                {
                    hub.apply(entity.clone(), now);
                }
            }
            "x-gap" => {
                hub.gap();
                read_if_wanted(gateway, hub, headers);
            }
            "x-closing" => return lost(hub, &name, &token, seq),
            _ => {}
        }
    }
}

/// The upstream ended: a later connection resumes it (the broker holds it 30 s).
fn lost(hub: &Hub, name: &str, token: &str, seq: u64) -> Option<(String, String, u64)> {
    tracing::info!(slug = %hub.slug, seq, "the fan-out upstream ended; it resumes");
    (!name.is_empty()).then(|| (name.to_owned(), token.to_owned(), seq))
}

fn text_of(metadata: &Map<String, Value>, key: &str) -> String {
    metadata
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

/// Snapshots read at once across every hub: a sweep that subscribes on many Endpoints at once
/// would otherwise hold a page of each in memory together (T-5182).
const SNAPSHOTS: usize = 2;

/// Starts [`snapshot`] when the hub's table is due and a filter needs it.
fn read_if_wanted(gateway: &Arc<Gateway>, hub: &Arc<Hub>, headers: &HeaderMap) {
    if hub.snapshot_now() {
        tokio::spawn(snapshot(
            Arc::clone(gateway),
            Arc::clone(hub),
            headers.clone(),
        ));
    }
}

/// Reads the view's entities once, page by page, into the table (ADR-N-049 §2.2), at most
/// [`SNAPSHOTS`] hubs at a time.
async fn snapshot(gateway: Arc<Gateway>, hub: Arc<Hub>, headers: HeaderMap) {
    static TURNS: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(SNAPSHOTS);
    let Ok(_turn) = TURNS.acquire().await else {
        return;
    };
    let mut offset = 0;
    while offset < bound() {
        let mut headers = headers.clone();
        headers.insert(ACCEPT, HeaderValue::from_static("application/json"));
        let path = format!(
            "/x/views/{}/ngsi-ld/v1/entities?type=*&limit={PAGE}&offset={offset}",
            hub.slug
        );
        let answer = match gateway
            .broker
            .send(Method::GET, &path, headers, Body::empty())
            .await
        {
            Ok(answer) if answer.status().is_success() => answer,
            Ok(answer) => {
                tracing::warn!(slug = %hub.slug, status = %answer.status(), "the fan-out snapshot was refused");
                return;
            }
            Err(error) => {
                tracing::warn!(slug = %hub.slug, %error, "the fan-out snapshot failed");
                return;
            }
        };
        let Ok(bytes) = axum::body::to_bytes(answer.into_body(), 64 * 1024 * 1024).await else {
            return;
        };
        let Ok(Value::Array(page)) = serde_json::from_slice::<Value>(&bytes) else {
            return;
        };
        let read = page.len();
        for entity in page {
            hub.seed(entity);
        }
        if read < PAGE {
            return;
        }
        offset += read;
    }
}

/// How a session ended.
#[derive(Debug, PartialEq, Eq)]
pub enum Ended {
    /// The client closed or went away.
    Closed,
    /// The client sent a message over the bound.
    TooBig,
    /// The client sent a binary frame.
    Binary,
    /// The server ended it with this `x-closing` reason.
    Closing(&'static str),
    /// The client answered no ping.
    NoPong,
    /// The broker refused the hub's upstream subscription.
    Refused,
    /// The client broke RFC 6455; closed with this code (7.4.1).
    Broken(u16, &'static str),
}

impl Ended {
    /// The `reason` label of `jc_gateway_ws_closed_total`.
    pub fn reason(&self) -> &'static str {
        match self {
            Self::Closed => "closed",
            Self::TooBig => "too-big",
            Self::Binary => "binary",
            Self::Closing(reason) => reason,
            Self::NoPong => "no-pong",
            Self::Refused => "refused",
            Self::Broken(1007, _) => "not-utf8",
            Self::Broken(..) => "protocol-error",
        }
    }
}

/// One client connection: welcome, then its messages answered and its queue written, until it
/// ends; its subscriptions leave the index then and its place is held for a resume.
pub async fn session(
    client: WebSocket,
    hub: Arc<Hub>,
    conn: u64,
    queue: Arc<Queue>,
    principal: String,
    lifetime: Duration,
) -> Ended {
    let (mut to_client, mut from_client) = client.split();
    let name = hub::random_name();
    let token = hub::random_name();
    let mut seq = 0_u64;
    let mut sent: VecDeque<(u64, String)> = VecDeque::new();
    let welcome =
        json!({"metadata": {"x-op": "x-welcome", "x-connection": name, "x-resume-token": token}});
    let mut ended = if to_client
        .send(ws::Message::text(welcome.to_string()))
        .await
        .is_err()
    {
        Some(Ended::Closed)
    } else {
        None
    };
    let mut ping = tokio::time::interval(PING);
    ping.tick().await;
    // When the oldest ping the client has not answered yet went out.
    let mut unanswered: Option<Instant> = None;
    let mut last_message = Instant::now();
    let mut first = true;
    let mut rate = Rate::new(Instant::now());
    let expired = tokio::time::sleep(lifetime);
    tokio::pin!(expired);
    while ended.is_none() {
        let mut outgoing: Vec<String> = Vec::new();
        // In this order: a Pong already read is counted before a late tick judges it, and the
        // queue, ready on every turn under a backlog, cannot hold back the ping or the lifetime.
        // The client's frames cannot hold back the rest: they are rate limited.
        tokio::select! {
            biased;
            read = from_client.next() => {
                last_message = Instant::now();
                if matches!(read, Some(Ok(_))) && !rate.take(last_message) {
                    tracing::warn!(slug = %hub.slug, %principal, "WebSocket client past its message rate");
                    ended = Some(Ended::Closing("rate"));
                } else {
                match read {
                    Some(Ok(ws::Message::Text(text))) => {
                        let replies = answer(&hub, conn, &principal, text.as_str(), first, &mut seq, &mut sent);
                        first = false;
                        outgoing.extend(replies);
                    }
                    Some(Ok(ws::Message::Binary(_))) => ended = Some(Ended::Binary),
                    Some(Ok(ws::Message::Pong(_))) => unanswered = None,
                    Some(Ok(ws::Message::Ping(_))) => {}
                    Some(Ok(ws::Message::Close(_))) | None => ended = Some(Ended::Closed),
                    Some(Err(error)) => ended = Some(read_error(&error)),
                }
                }
            }
            _ = ping.tick() => {
                ended = tick(unanswered, last_message, || hub.subscriptions(conn));
                if ended.is_none() {
                    ended = deliver(&mut to_client, ws::Message::Ping(Vec::new().into())).await;
                    unanswered.get_or_insert_with(Instant::now);
                }
            }
            () = &mut expired => ended = Some(Ended::Closing("lifetime")),
            () = queue.ready() => {
                let (items, overflowed) = queue.take();
                for item in items {
                    match item {
                        Out::Notification { subscription, body, .. } => {
                            seq += 1;
                            let text = json!({"metadata": {
                                "x-op": "x-notification", "x-seq": seq,
                                "x-subscription": subscription, "Content-Type": "application/json"},
                                "body": body}).to_string();
                            if sent.len() >= KEPT {
                                sent.pop_front();
                            }
                            sent.push_back((seq, text.clone()));
                            outgoing.push(text);
                        }
                        Out::Gap => {
                            seq += 1;
                            outgoing.push(gap(seq, seq));
                        }
                        Out::Closing(reason) => {
                            // Nothing queued behind a close is sent (T-4874).
                            ended = Some(Ended::Closing(reason));
                            break;
                        }
                        Out::Refused => ended = Some(Ended::Refused),
                    }
                }
                if overflowed {
                    outgoing.push(gap(seq + 1, seq + 1));
                    ended = Some(Ended::Closing("overflow"));
                }
            }
        }
        for text in outgoing {
            if let Some(end) = deliver(&mut to_client, ws::Message::text(text)).await {
                ended = Some(end);
                break;
            }
        }
    }
    let ended = ended.unwrap_or(Ended::Closed);
    // A stalled client takes no close frame either.
    let _ = tokio::time::timeout(STALL, finish(&mut to_client, &ended)).await;
    hub.close(conn, &name, &token, sent);
    ended
}

/// What a ping tick decides: a client that left a ping unanswered for longer than [`PING`] ends,
/// so the next tick after that one (60 s after the ping, API/07 §3), and one silent past [`IDLE`]
/// with no subscription. The deadline runs from a ping sent: a tick the gateway ran late sent
/// none, and a client is never ended for a ping it was not sent (T-5351). `subscriptions` takes the hub's lock, which
/// `Hub::apply` holds through a change's whole fan-out: it is asked only of a silent client, since
/// every connection asking on each tick parked the runtime's threads behind a large hub and no
/// Pong was read in time (T-5351).
fn tick(
    unanswered: Option<Instant>,
    last_message: Instant,
    subscriptions: impl FnOnce() -> usize,
) -> Option<Ended> {
    if unanswered.is_some_and(|sent| sent.elapsed() > PING) {
        Some(Ended::NoPong)
    } else if last_message.elapsed() > IDLE && subscriptions() == 0 {
        Some(Ended::Closing("idle"))
    } else {
        None
    }
}

/// Sends one frame within [`STALL`]: how the session ends when it cannot.
async fn deliver(
    to_client: &mut SplitSink<WebSocket, ws::Message>,
    message: ws::Message,
) -> Option<Ended> {
    match tokio::time::timeout(STALL, to_client.send(message)).await {
        Ok(Ok(())) => None,
        Ok(Err(_)) => Some(Ended::Closed),
        Err(_) => Some(Ended::Closing("overflow")),
    }
}

/// The close frame that ends a session. After the client's own close, the answering close frame
/// tungstenite queued (RFC 6455 5.5.1) reaches the client only when the sink is flushed.
async fn finish(to_client: &mut SplitSink<WebSocket, ws::Message>, ended: &Ended) {
    let close = match ended {
        Ended::Closing(reason) => {
            let closing = json!({"metadata": {"x-op": "x-closing", "x-reason": reason}});
            let _ = to_client.send(ws::Message::text(closing.to_string())).await;
            Some((if *reason == "rate" { POLICY } else { NORMAL }, *reason))
        }
        Ended::TooBig => Some((TOO_BIG, "a message over 64 KiB")),
        Ended::Binary => Some((UNSUPPORTED, "text frames only")),
        Ended::NoPong => Some((NORMAL, "no pong")),
        Ended::Refused => Some((
            INTERNAL,
            "the context broker refuses this Endpoint's fan-out",
        )),
        Ended::Broken(code, reason) => Some((*code, *reason)),
        Ended::Closed => None,
    };
    if let Some((code, reason)) = close {
        let frame = ws::CloseFrame {
            code,
            reason: reason.into(),
        };
        let _ = to_client.send(ws::Message::Close(Some(frame))).await;
    } else {
        let _ = to_client.flush().await;
    }
}

fn gap(from: u64, to: u64) -> String {
    json!({"metadata": {"x-op": "x-gap", "x-from": from, "x-to": to}}).to_string()
}

/// How a failed read ends the session: a broken bound or frame closes with its code, any other
/// error, a dropped connection included, means the client is gone.
fn read_error(error: &axum::Error) -> Ended {
    use tokio_tungstenite::tungstenite::{error::ProtocolError, Error};
    match std::error::Error::source(error).and_then(|s| s.downcast_ref::<Error>()) {
        Some(Error::Capacity(_)) => Ended::TooBig,
        Some(Error::Protocol(p)) if !matches!(p, ProtocolError::ResetWithoutClosingHandshake) => {
            Ended::Broken(1002, "protocol error")
        }
        Some(Error::Utf8(_)) => Ended::Broken(1007, "text is not UTF-8"),
        _ => Ended::Closed,
    }
}

/// The messages that answer one client message, in order.
fn answer(
    hub: &Hub,
    conn: u64,
    principal: &str,
    text: &str,
    first: bool,
    seq: &mut u64,
    sent: &mut VecDeque<(u64, String)>,
) -> Vec<String> {
    let (metadata, body) = match hub::envelope(text) {
        Ok(message) => message,
        Err(why) => return vec![result(&Map::new(), &bad(&why))],
    };
    let op = metadata.get("x-op").and_then(Value::as_str).unwrap_or("");
    let named = metadata.get("x-subscription").and_then(Value::as_str);
    let empty = Value::Object(Map::new());
    let answered = match (op, named) {
        ("x-subscribe", _) => hub.subscribe(conn, None, body.as_ref().unwrap_or(&empty), false),
        ("x-update", Some(id)) => {
            hub.subscribe(conn, Some(id), body.as_ref().unwrap_or(&empty), true)
        }
        ("x-unsubscribe", Some(id)) => hub.unsubscribe(conn, id),
        ("x-update" | "x-unsubscribe", None) => {
            bad("metadata.x-subscription names the subscription")
        }
        ("x-resume", _) if first => return resume(hub, conn, principal, &metadata, seq, sent),
        ("x-resume", _) => bad("x-resume is the first message of a connection"),
        _ => bad("metadata.x-op is x-subscribe, x-update, x-unsubscribe or x-resume"),
    };
    vec![result(&metadata, &answered)]
}

fn bad(detail: &str) -> Answer {
    Answer {
        status: 400,
        body: Some(json!({
            "type": "https://uri.etsi.org/ngsi-ld/errors/BadRequestData",
            "title": "BadRequestData",
            "status": 400,
            "detail": detail,
        })),
    }
}

fn result(metadata: &Map<String, Value>, answer: &Answer) -> String {
    let mut out = Map::new();
    out.insert("x-op".into(), json!("x-result"));
    if let Some(id) = metadata.get("x-id") {
        out.insert("x-id".into(), id.clone());
    }
    out.insert("x-status".into(), json!(answer.status));
    let mut message = Map::new();
    message.insert("metadata".into(), Value::Object(out));
    if let Some(body) = &answer.body {
        message.insert("body".into(), body.clone());
    }
    Value::Object(message).to_string()
}

/// `x-resume`: the closed connection's subscriptions move to this one, and what it sent after
/// the client's `x-seq` is sent again, an `x-gap` first when some of it is no longer held.
fn resume(
    hub: &Hub,
    conn: u64,
    principal: &str,
    metadata: &Map<String, Value>,
    seq: &mut u64,
    sent: &mut VecDeque<(u64, String)>,
) -> Vec<String> {
    let name = text_of(metadata, "x-connection");
    let token = text_of(metadata, "x-resume-token");
    let after = metadata.get("x-seq").and_then(Value::as_u64).unwrap_or(0);
    let Some(closed) = hub.take_closed(&name, &token, principal) else {
        let refused = Answer {
            status: 404,
            body: Some(json!({
                "type": "https://uri.etsi.org/ngsi-ld/errors/ResourceNotFound",
                "title": "ResourceNotFound",
                "status": 404,
                "detail": "no connection of yours to resume here; read again over REST",
            })),
        };
        return vec![result(metadata, &refused)];
    };
    for (id, body) in &closed.subs {
        hub.subscribe(conn, Some(id), body, false);
    }
    let mut out = Vec::new();
    let missed: Vec<&(u64, String)> = closed.sent.iter().filter(|(n, _)| *n > after).collect();
    let last = closed.sent.back().map_or(after, |(n, _)| *n).max(after);
    if missed.first().is_some_and(|(n, _)| *n > after + 1) || (missed.is_empty() && last > after) {
        out.push(gap(after + 1, missed.first().map_or(last, |(n, _)| n - 1)));
    }
    for (n, text) in missed {
        out.push(text.clone());
        sent.push_back((*n, text.clone()));
    }
    *seq = last;
    out.insert(
        0,
        result(
            metadata,
            &Answer {
                status: 200,
                body: None,
            },
        ),
    );
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hub_with_closed(sent: &[u64]) -> (Hub, u64) {
        let hub = Hub::new("s", 10);
        let endpoint = crate::fanout::hub::tests_support::endpoint();
        let (old, _) = hub.open(Arc::clone(&endpoint), "anonymous".into(), None);
        let held: VecDeque<(u64, String)> = sent.iter().map(|n| (*n, format!("n{n}"))).collect();
        hub.close(old, "name", "token", held);
        let (new, _) = hub.open(endpoint, "anonymous".into(), None);
        (hub, new)
    }

    fn resumed(hub: &Hub, conn: u64, after: u64) -> (Vec<String>, u64) {
        let metadata: Map<String, Value> = serde_json::from_value(json!({
            "x-op": "x-resume", "x-id": "r", "x-connection": "name", "x-resume-token": "token", "x-seq": after
        }))
        .expect("metadata");
        let mut seq = 0;
        let mut sent = VecDeque::new();
        (
            resume(hub, conn, "anonymous", &metadata, &mut seq, &mut sent),
            seq,
        )
    }

    /// T-4755: the upstream speaks as the caller, as a REST read through the view does, so the
    /// broker cuts each delivery; callers share a hub only when the broker cannot tell them apart.
    #[test]
    fn the_upstream_speaks_as_the_caller_and_hubs_are_shared_by_identity() {
        let anonymous = upstream_headers("helsinki", &Subject::anonymous()).expect("headers");
        assert_eq!(anonymous[tenancy::TENANT], "helsinki");
        assert_eq!(anonymous["x-jc-roles"], "public");
        assert!(anonymous.get("x-jc-client").is_none(), "{anonymous:?}");
        assert!(
            anonymous.get(tenancy::PROVENANCE_ACTOR).is_none(),
            "{anonymous:?}"
        );
        let jana = Subject {
            user: Some("jana".to_owned()),
            roles: ["steward".to_owned()].into(),
            ..Subject::default()
        };
        let person = upstream_headers("helsinki", &jana).expect("headers");
        assert_eq!(person["x-jc-subject"], "jana");
        assert_eq!(person["x-jc-roles"], "steward");
        // A token of the gateway's own client never makes the upstream the gateway's.
        let posing = Subject {
            service_account: Some(tenancy::GATEWAY_CLIENT.to_owned()),
            ..Subject::default()
        };
        let posing = upstream_headers("helsinki", &posing).expect("headers");
        assert!(posing.get("x-jc-client").is_none(), "{posing:?}");

        let again = upstream_headers("helsinki", &Subject::anonymous()).expect("headers");
        assert_eq!(hub_key("e", &anonymous), hub_key("e", &again));
        assert_ne!(hub_key("e", &anonymous), hub_key("e", &person));
        assert_ne!(hub_key("e", &anonymous), hub_key("f", &anonymous));
        assert!(upstream_headers("bad\nspace", &Subject::anonymous()).is_none());

        let subscription = upstream_subscription();
        assert_eq!(subscription["body"]["entities"], json!([{"type": "*"}]));
        assert!(subscription["body"]["notification"]
            .get("endpoint")
            .is_none());
    }

    /// API/07 §3: at either hub cap the hub idle longest goes; one hub past the cap with every hub
    /// holding a client is refused, and the other space's hubs stay.
    #[test]
    fn hubs_are_capped_per_space_and_in_all_and_idle_ones_make_room() {
        let endpoint = crate::fanout::hub::tests_support::endpoint();
        let busy = |slug: &str| {
            let hub = Arc::new(Hub::new(slug, 10));
            hub.open(Arc::clone(&endpoint), "anonymous".into(), None);
            hub
        };
        let mut hubs: HashMap<String, (String, Arc<Hub>)> = HashMap::new();
        hubs.insert("a1".into(), ("a".into(), busy("a1")));
        hubs.insert("b1".into(), ("b".into(), busy("b1")));
        hubs.insert("a2".into(), ("a".into(), busy("a2")));
        assert_eq!(make_room(&mut hubs, "a", 10, 2), Err(Refused::HubsFull));
        assert_eq!(make_room(&mut hubs, "b", 3, 2), Err(Refused::HubsFull));
        assert_eq!(make_room(&mut hubs, "b", 4, 2), Ok(()));

        hubs.insert("a3".into(), ("a".into(), Arc::new(Hub::new("a3", 10))));
        assert_eq!(make_room(&mut hubs, "a", 10, 3), Ok(()), "a3 lingers");
        assert!(!hubs.contains_key("a3"), "{:?}", hubs.keys());
        assert_eq!(hubs.len(), 3);
    }

    /// API/07 §3: the burst passes, the next frame is refused, a twentieth of a second earns one
    /// back, and idle time saves no more than the burst.
    #[test]
    fn the_message_rate_holds_a_burst_then_the_pace() {
        let start = Instant::now();
        let mut rate = Rate::new(start);
        assert!((0..100).all(|_| rate.take(start)));
        assert!(!rate.take(start), "one past the burst");
        let later = start + Duration::from_millis(50);
        assert!(rate.take(later));
        assert!(!rate.take(later));
        let idle = later + Duration::from_secs(60);
        assert!((0..100).all(|_| rate.take(idle)));
        assert!(!rate.take(idle));
    }

    #[test]
    fn a_resume_sends_what_was_missed_after_the_clients_seq() {
        let (hub, conn) = hub_with_closed(&[3, 4, 5]);
        let (out, seq) = resumed(&hub, conn, 3);
        assert!(out[0].contains("\"x-status\":200"), "{out:?}");
        assert_eq!(&out[1..], ["n4", "n5"]);
        assert_eq!(seq, 5);
    }

    #[test]
    fn a_resume_from_before_what_is_held_starts_with_a_gap() {
        let (hub, conn) = hub_with_closed(&[3, 4, 5]);
        let (out, _) = resumed(&hub, conn, 1);
        assert!(
            out[1].contains("x-gap")
                && out[1].contains("\"x-from\":2")
                && out[1].contains("\"x-to\":2"),
            "{out:?}"
        );
        assert_eq!(&out[2..], ["n3", "n4", "n5"]);
    }

    #[test]
    fn a_resume_of_an_unknown_connection_is_404() {
        let (hub, conn) = hub_with_closed(&[1]);
        let metadata: Map<String, Value> = serde_json::from_value(
            json!({"x-op": "x-resume", "x-connection": "other", "x-resume-token": "token"}),
        )
        .expect("metadata");
        let out = resume(
            &hub,
            conn,
            "anonymous",
            &metadata,
            &mut 0,
            &mut VecDeque::new(),
        );
        assert!(out[0].contains("\"x-status\":404"), "{out:?}");
    }

    #[test]
    fn a_message_out_of_place_is_answered_400_and_the_connection_stays() {
        let (hub, conn) = hub_with_closed(&[]);
        for (text, says) in [
            ("{", "one JSON object"),
            (
                r#"{"metadata": {"x-op": "x-dance", "x-id": "a"}}"#,
                "metadata.x-op",
            ),
            (
                r#"{"metadata": {"x-op": "x-update"}, "body": {}}"#,
                "x-subscription",
            ),
        ] {
            let out = answer(
                &hub,
                conn,
                "anonymous",
                text,
                false,
                &mut 0,
                &mut VecDeque::new(),
            );
            assert!(
                out[0].contains("\"x-status\":400") && out[0].contains(says),
                "{text}: {out:?}"
            );
        }
        let late = answer(
            &hub,
            conn,
            "anonymous",
            r#"{"metadata": {"x-op": "x-resume", "x-id": "r"}}"#,
            false,
            &mut 0,
            &mut VecDeque::new(),
        );
        assert!(late[0].contains("first message"), "{late:?}");
        let echoed = answer(
            &hub,
            conn,
            "anonymous",
            r#"{"metadata": {"x-op": "x-subscribe", "x-id": "s1"}, "body": {"type": "Subscription", "entities": [{"type": "Vehicle"}]}}"#,
            true,
            &mut 0,
            &mut VecDeque::new(),
        );
        assert!(
            echoed[0].contains("\"x-id\":\"s1\"") && echoed[0].contains("\"x-status\":201"),
            "{echoed:?}"
        );
    }

    /// A ping tick asks the hub only about a silent client, and ends one only for a ping it was
    /// sent: the gateway's own late tick sent none (T-5351).
    #[test]
    fn a_tick_asks_the_hub_only_about_a_silent_client() {
        let now = Instant::now();
        let long_ago = now.checked_sub(PING * 3).expect("an instant");
        let unasked = || -> usize { panic!("the hub's lock was taken for a live client") };
        assert_eq!(tick(None, now, unasked), None);
        assert_eq!(tick(Some(now), now, unasked), None);
        assert_eq!(tick(Some(long_ago), now, unasked), Some(Ended::NoPong));
        assert_eq!(tick(None, long_ago, || 0), Some(Ended::Closing("idle")));
        assert_eq!(tick(None, long_ago, || 1), None);
    }

    /// A session behind a real upgrade, for the RFC 6455 tests.
    async fn served() -> String {
        let app = axum::Router::new().route(
            "/ws",
            axum::routing::get(|up: ws::WebSocketUpgrade| async move {
                up.on_upgrade(|client| async move {
                    let hub = Arc::new(Hub::new("s", 10));
                    let endpoint = crate::fanout::hub::tests_support::endpoint();
                    let (conn, queue) = hub.open(endpoint, "anonymous".into(), None);
                    let lifetime = Duration::from_secs(60);
                    session(client, hub, conn, queue, "anonymous".into(), lifetime).await;
                })
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("addr");
        tokio::spawn(async move { axum::serve(listener, app).await });
        format!("ws://{addr}/ws")
    }

    /// The code of the close frame the gateway answers `message` with, skipping its own messages.
    async fn closes_with(url: &str, message: Message) -> Option<u16> {
        let (mut ws, _) = tokio_tungstenite::connect_async(url)
            .await
            .expect("connect");
        ws.send(message).await.expect("send");
        loop {
            match tokio::time::timeout(Duration::from_secs(5), ws.next()).await {
                Ok(Some(Ok(Message::Close(frame)))) => return frame.map(|f| f.code.into()),
                Ok(Some(Ok(_))) => {}
                _ => return None,
            }
        }
    }

    /// RFC 6455 5.5.1 and 7.4.1: a client's close is answered with a close frame, and a frame
    /// that breaks the protocol closes with 1002, text that is not UTF-8 with 1007.
    #[tokio::test]
    async fn a_close_is_answered_and_a_broken_frame_closes_with_its_code() {
        use tokio_tungstenite::tungstenite::protocol::frame::coding::{Data, OpCode};
        use tokio_tungstenite::tungstenite::protocol::frame::Frame;
        use tokio_tungstenite::tungstenite::protocol::CloseFrame;
        let url = served().await;
        let bye = CloseFrame {
            code: NORMAL.into(),
            reason: "bye".into(),
        };
        assert_eq!(
            closes_with(&url, Message::Close(Some(bye))).await,
            Some(NORMAL)
        );
        let mut rsv = Frame::message(b"{}".to_vec(), OpCode::Data(Data::Text), true);
        rsv.header_mut().rsv1 = true;
        assert_eq!(closes_with(&url, Message::Frame(rsv)).await, Some(1002));
        let reserved = Frame::message(Vec::new(), OpCode::Data(Data::Reserved(3)), true);
        assert_eq!(
            closes_with(&url, Message::Frame(reserved)).await,
            Some(1002)
        );
        let latin = Frame::message(vec![0xce, 0xba, 0xff], OpCode::Data(Data::Text), true);
        assert_eq!(closes_with(&url, Message::Frame(latin)).await, Some(1007));
    }
}
