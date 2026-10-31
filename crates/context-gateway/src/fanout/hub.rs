//! One Endpoint's WebSocket clients served from one broker subscription (ADR-N-049, EP-114).
//!
//! A [`Hub`] per Endpoint and caller identity holds the latest entity per id, every client's
//! filters in one [`Index`], and one queue per connection. Its upstream is a single WebSocket to
//! the broker view under that identity, so the broker's view and policy engine have already cut
//! each change to what the caller may read, as they cut a REST read (T-4755); the clients'
//! filters only narrow it further.

use super::{changed, compile, FilterId, Index};
use crate::pdp::projection;
use crate::resolver::Endpoint;
use antares_jsonld::{expand_entity, Context, ExpandOpts};
use serde_json::{json, Map, Value};
use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::time::{Duration, Instant};

/// Filters one connection may hold (API/07 §3).
pub const MAX_FILTERS: usize = 64;
/// Unread notifications one connection may have queued (API/07 §3).
pub const MAX_QUEUED: usize = 256;
/// How long a closed connection can be resumed (API/07 §4).
pub const RESUME_WINDOW: Duration = Duration::from_secs(30);
/// Notifications kept per connection for a resume.
pub const KEPT: usize = 256;
/// Closed connections one hub holds for a resume: past it the oldest goes, and its resume
/// answers a gap (F-R2PLT2-010).
pub const MAX_CLOSED: usize = 256;
/// The notification bytes the closed connections of one hub keep, cut the same way.
pub const MAX_CLOSED_BYTES: usize = 4 * 1024 * 1024;
/// The bound of the entity table when `JC_GATEWAY_WS_ENTITIES` names none (ADR-N-049 §2.2).
pub const DEFAULT_ENTITIES: usize = 50_000;

/// Filter ids carry their connection in the high bits, so a candidate names its connection.
const SUB_BITS: u32 = 20;

/// The core `@context`, which the grants' own conditions are judged with too
/// (`pdp::grants`): a filter and an entity are expanded alike, so a short name means the same
/// on both sides.
pub fn core() -> &'static Context {
    static CORE: OnceLock<Context> = OnceLock::new();
    CORE.get_or_init(antares_jsonld::core_context)
}

/// An entity in the stored form the matcher reads; `None` for one that does not expand.
fn expanded(entity: &Value) -> Option<Value> {
    let members = entity.as_object()?;
    expand_entity(members, core(), ExpandOpts::default()).ok()
}

/// One message a connection's writer sends, in order.
#[derive(Debug, Clone, PartialEq)]
pub enum Out {
    /// A notification for one subscription and one entity; a newer one for the same pair
    /// replaces it while it is still queued.
    Notification {
        /// The client's subscription id.
        subscription: String,
        /// The entity id it is about.
        entity: String,
        /// The NGSI-LD Notification.
        body: Value,
    },
    /// Changes were lost (the upstream lost its place); the client reads again over REST.
    Gap,
    /// The connection ends with this `x-closing` reason.
    Closing(&'static str),
    /// The broker refused the hub's upstream subscription: the connection ends with 1011.
    Refused,
}

/// A connection's queue: bounded, coalesced per subscription and entity (ADR-N-049 §2.6).
#[derive(Debug, Default)]
pub struct Queue {
    inner: Mutex<QueueState>,
    ready: tokio::sync::Notify,
}

#[derive(Debug, Default)]
struct QueueState {
    items: VecDeque<Out>,
    overflowed: bool,
}

impl Queue {
    /// Queues `out`, replacing a queued notification of the same subscription and entity in
    /// its place, so the order per entity holds. A full queue marks itself overflowed.
    pub fn push(&self, out: Out) {
        {
            let mut state = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
            if let Out::Notification {
                subscription,
                entity,
                body,
            } = &out
            {
                // ponytail: a linear scan of at most MAX_QUEUED items; a map beside the deque
                // when a profile shows it.
                let held = state.items.iter_mut().find(|item| {
                    matches!(item, Out::Notification { subscription: s, entity: e, .. }
                        if s == subscription && e == entity)
                });
                if let Some(Out::Notification { body: held, .. }) = held {
                    held.clone_from(body);
                    drop(state);
                    self.ready.notify_one();
                    return;
                }
            }
            if state.items.len() >= MAX_QUEUED {
                state.overflowed = true;
            } else {
                state.items.push_back(out);
            }
        }
        self.ready.notify_one();
    }

    /// Everything queued, and whether the queue overflowed since the last take.
    pub fn take(&self) -> (Vec<Out>, bool) {
        let mut state = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        let overflowed = std::mem::take(&mut state.overflowed);
        (state.items.drain(..).collect(), overflowed)
    }

    /// Waits until something is queued.
    pub async fn ready(&self) {
        self.ready.notified().await;
    }
}

/// One client subscription of a connection.
#[derive(Debug, Clone)]
struct Sub {
    id: String,
    filter: FilterId,
    /// The body as the client wrote it, for `x-update` and a resume.
    body: Value,
    /// `notification.attributes`, the names a notification is cut to.
    attributes: Option<BTreeSet<String>>,
}

/// One open connection.
struct Conn {
    endpoint: Arc<Endpoint>,
    principal: String,
    /// The Keycloak client its token was issued to (`azp`), which a reload may stop admitting.
    client: Option<String>,
    queue: Arc<Queue>,
    subs: BTreeMap<u32, Sub>,
    next: u32,
}

/// A closed connection, held for [`RESUME_WINDOW`].
#[derive(Clone)]
pub struct Closed {
    /// The secret its `x-welcome` carried.
    pub token: String,
    /// Who held it; only the same caller may resume it.
    pub principal: String,
    /// Its subscriptions, as `(id, body)`.
    pub subs: Vec<(String, Value)>,
    /// The notifications it sent last, as `(x-seq, text)`.
    pub sent: VecDeque<(u64, String)>,
    /// When it closed.
    pub at: Instant,
}

/// Everything but the resume secret (T-4320).
impl std::fmt::Debug for Closed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Closed")
            .field("token", &"<redacted>")
            .field("principal", &self.principal)
            .field("subs", &self.subs)
            .field("sent", &self.sent)
            .field("at", &self.at)
            .finish()
    }
}

/// The answer to one client message, as `x-result` carries it.
#[derive(Debug, PartialEq)]
pub struct Answer {
    /// The HTTP status the same request would get over REST.
    pub status: u16,
    /// The created Subscription, a ProblemDetails, or nothing.
    pub body: Option<Value>,
}

impl Answer {
    fn problem(status: u16, kind: &str, detail: &str) -> Self {
        Answer {
            status,
            body: Some(json!({
                "type": format!("https://uri.etsi.org/ngsi-ld/errors/{kind}"),
                "title": kind,
                "status": status,
                "detail": detail,
            })),
        }
    }
}

/// A connection opened before its upgrade: dropped without [`Joined::take`] (the upgrade
/// failed and no session ever runs), it leaves the hub, so it never holds a space-cap slot.
pub struct Joined(Option<(Arc<Hub>, u64, Arc<Queue>)>);

impl Joined {
    /// Guards connection `conn` of `hub`.
    pub fn new(hub: Arc<Hub>, conn: u64, queue: Arc<Queue>) -> Self {
        Joined(Some((hub, conn, queue)))
    }

    /// The connection, for the session that now owns its close.
    pub fn take(mut self) -> (Arc<Hub>, u64, Arc<Queue>) {
        self.0
            .take()
            .unwrap_or_else(|| unreachable!("taken once, by value"))
    }
}

impl Drop for Joined {
    fn drop(&mut self) {
        if let Some((hub, conn, _)) = self.0.take() {
            hub.discard(conn);
        }
    }
}

/// One Endpoint's live state.
pub struct Hub {
    /// The Endpoint's slug, which is its broker view's id.
    pub slug: String,
    state: Mutex<State>,
    /// Woken by the first filter of a hub whose table is due, so the upstream reads it then.
    wanted: tokio::sync::Notify,
}

struct State {
    table: HashMap<String, (u64, Value)>,
    tick: u64,
    bound: usize,
    index: Index,
    conns: HashMap<u64, Conn>,
    next_conn: u64,
    closed: HashMap<String, Closed>,
    idle_since: Option<Instant>,
    /// The broker's status and detail while it refuses the upstream subscription.
    refused: Option<(u16, String)>,
    /// The table is to be read from the view before it judges a change: never read, lost by a
    /// gap, or dropped with the last filter.
    due: bool,
}

impl Hub {
    /// An empty hub whose table holds at most `bound` entities.
    pub fn new(slug: &str, bound: usize) -> Self {
        Hub {
            slug: slug.to_owned(),
            state: Mutex::new(State {
                table: HashMap::new(),
                tick: 0,
                bound: bound.max(1),
                index: Index::default(),
                conns: HashMap::new(),
                next_conn: 1,
                closed: HashMap::new(),
                idle_since: Some(Instant::now()),
                refused: None,
                due: true,
            }),
            wanted: tokio::sync::Notify::new(),
        }
    }

    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Opens a connection of `principal` on `endpoint`.
    pub fn open(
        &self,
        endpoint: Arc<Endpoint>,
        principal: String,
        client: Option<String>,
    ) -> (u64, Arc<Queue>) {
        let queue = Arc::new(Queue::default());
        let mut state = self.state();
        let id = state.next_conn;
        state.next_conn += 1;
        state.idle_since = None;
        state.conns.insert(
            id,
            Conn {
                endpoint,
                principal,
                client,
                queue: Arc::clone(&queue),
                subs: BTreeMap::new(),
                next: 1,
            },
        );
        (id, queue)
    }

    /// Closes connection `id`: its filters leave the index, and what a resume needs is held
    /// under `name` for [`RESUME_WINDOW`].
    pub fn close(&self, id: u64, name: &str, token: &str, sent: VecDeque<(u64, String)>) {
        let mut state = self.state();
        let Some(conn) = state.conns.remove(&id) else {
            return;
        };
        for sub in conn.subs.values() {
            state.index.remove(sub.filter);
        }
        state.forget_unwatched();
        let now = Instant::now();
        state
            .closed
            .retain(|_, c| now.duration_since(c.at) < RESUME_WINDOW);
        state.closed.insert(
            name.to_owned(),
            Closed {
                token: token.to_owned(),
                principal: conn.principal,
                subs: conn.subs.into_values().map(|s| (s.id, s.body)).collect(),
                sent,
                at: now,
            },
        );
        let kept = |closed: &Closed| {
            closed
                .sent
                .iter()
                .map(|(_, text)| text.len())
                .sum::<usize>()
        };
        let mut bytes: usize = state.closed.values().map(kept).sum();
        while state.closed.len() > MAX_CLOSED || bytes > MAX_CLOSED_BYTES {
            let oldest = state
                .closed
                .iter()
                .min_by_key(|(_, closed)| closed.at)
                .map(|(name, _)| name.clone());
            let Some(gone) = oldest.and_then(|name| state.closed.remove(&name)) else {
                break;
            };
            bytes = bytes.saturating_sub(kept(&gone));
        }
        if state.conns.is_empty() {
            state.idle_since = Some(now);
        }
    }

    /// Drops connection `id` that never reached a session (its upgrade failed): nothing is held
    /// for a resume.
    fn discard(&self, id: u64) {
        let mut state = self.state();
        if let Some(conn) = state.conns.remove(&id) {
            for sub in conn.subs.values() {
                state.index.remove(sub.filter);
            }
        }
        state.forget_unwatched();
        if state.conns.is_empty() && state.idle_since.is_none() {
            state.idle_since = Some(Instant::now());
        }
    }

    /// How long the hub has had no connection, `None` while it has one.
    pub fn idle_for(&self) -> Option<Duration> {
        self.state().idle_since.map(|since| since.elapsed())
    }

    /// The closed connection `name`, taken out, when `token` and `principal` match and it closed
    /// within [`RESUME_WINDOW`] (API/07 §4).
    pub fn take_closed(&self, name: &str, token: &str, principal: &str) -> Option<Closed> {
        let mut state = self.state();
        let held = state.closed.get(name)?;
        if held.token != token || held.principal != principal || held.at.elapsed() >= RESUME_WINDOW
        {
            return None;
        }
        state.closed.remove(name)
    }

    /// `x-subscribe` (`id` absent) or `x-update` of subscription `id`: the body compiled into a
    /// filter of connection `conn`. A resume passes the id it held.
    pub fn subscribe(&self, conn: u64, id: Option<&str>, body: &Value, update: bool) -> Answer {
        let mut state = self.state();
        let state = &mut *state;
        if let Some((status, detail)) = &state.refused {
            let problem = jc_core::ProblemDetails::for_status(503).with_detail(format!(
                "the context broker refuses this Endpoint's fan-out ({status}: {detail})"
            ));
            return Answer {
                status: 503,
                body: serde_json::to_value(problem).ok(),
            };
        }
        let Some(c) = state.conns.get_mut(&conn) else {
            return Answer::problem(410, "Gone", "the connection is closed");
        };
        let held = id.and_then(|id| c.subs.iter().find(|(_, s)| s.id == id).map(|(n, _)| *n));
        let body = match (update, held) {
            (true, None) => {
                return Answer::problem(
                    404,
                    "ResourceNotFound",
                    "no such subscription on this connection",
                );
            }
            (true, Some(n)) => {
                let mut merged = c.subs[&n].body.clone();
                if let (Some(merged), Some(changes)) = (merged.as_object_mut(), body.as_object()) {
                    merged.extend(changes.clone());
                }
                merged
            }
            (false, _) => body.clone(),
        };
        if !update && c.subs.len() >= MAX_FILTERS {
            return Answer::problem(
                400,
                "BadRequestData",
                &format!("a connection holds at most {MAX_FILTERS} subscriptions"),
            );
        }
        let filter = match compile(&body, core()) {
            Ok(filter) => filter,
            Err(why) => return Answer::problem(400, "BadRequestData", &why),
        };
        let n = held.unwrap_or_else(|| {
            let n = c.next;
            c.next += 1;
            n
        });
        if n >= 1 << SUB_BITS {
            return Answer::problem(
                400,
                "BadRequestData",
                "this connection made too many subscriptions",
            );
        }
        let filter_id = (conn << SUB_BITS) | u64::from(n);
        let sub_id = id.map_or_else(
            || format!("urn:ngsi-ld:Subscription:ws:{}:{conn}:{n}", self.slug),
            str::to_owned,
        );
        let attributes = body
            .get("notification")
            .and_then(|n| n.get("attributes"))
            .and_then(Value::as_array)
            .map(|names| {
                names
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect()
            });
        state.index.insert(filter_id, filter);
        if state.due {
            self.wanted.notify_one();
        }
        c.subs.insert(
            n,
            Sub {
                id: sub_id.clone(),
                filter: filter_id,
                body,
                attributes,
            },
        );
        if update {
            Answer {
                status: 204,
                body: None,
            }
        } else {
            Answer {
                status: 201,
                body: Some(json!({"id": sub_id, "type": "Subscription"})),
            }
        }
    }

    /// `x-unsubscribe` of subscription `id`.
    pub fn unsubscribe(&self, conn: u64, id: &str) -> Answer {
        let mut state = self.state();
        let state = &mut *state;
        let Some(c) = state.conns.get_mut(&conn) else {
            return Answer::problem(410, "Gone", "the connection is closed");
        };
        let Some(n) = c.subs.iter().find(|(_, s)| s.id == id).map(|(n, _)| *n) else {
            return Answer::problem(
                404,
                "ResourceNotFound",
                "no such subscription on this connection",
            );
        };
        if let Some(sub) = c.subs.remove(&n) {
            state.index.remove(sub.filter);
        }
        state.forget_unwatched();
        Answer {
            status: 204,
            body: None,
        }
    }

    /// How many subscriptions connection `conn` holds.
    pub fn subscriptions(&self, conn: u64) -> usize {
        self.state().conns.get(&conn).map_or(0, |c| c.subs.len())
    }

    /// Follows every connection's Endpoint through a reload (T-4874): a connection whose Endpoint
    /// is gone or no longer serves the WebSocket ends with `view-removed`; one whose Endpoint
    /// record changed in any way (audience, roles, grants, Policies, hidden attributes) or whose
    /// token's client `admits` no longer passes ends with `endpoint-changed`, so no decision
    /// taken at the upgrade outlives a change and the client reconnects under the new record.
    /// The reason names no detail of the change. Answers the connections it closes.
    pub fn recheck(&self, now: Option<&Arc<Endpoint>>, admits: impl Fn(&str) -> bool) -> Vec<u64> {
        let mut state = self.state();
        let mut gone = Vec::new();
        for (id, conn) in &mut state.conns {
            let reason = match now {
                Some(endpoint) if !endpoint.serves(jc_core::kinds::Representation::WebSocket) => {
                    "view-removed"
                }
                Some(endpoint)
                    if **endpoint == *conn.endpoint
                        && conn.client.as_deref().is_none_or(&admits) =>
                {
                    // The same record in a new table: nothing to judge again.
                    conn.endpoint = Arc::clone(endpoint);
                    continue;
                }
                Some(_) => "endpoint-changed",
                None => "view-removed",
            };
            conn.queue.push(Out::Closing(reason));
            gone.push(*id);
        }
        // A closing connection leaves the index now: no change applied after this sends it more.
        for id in &gone {
            if let Some(conn) = state.conns.get_mut(id) {
                for (_, sub) in std::mem::take(&mut conn.subs) {
                    state.index.remove(sub.filter);
                }
            }
        }
        state.forget_unwatched();
        gone
    }

    /// How many connections it holds.
    pub fn connections(&self) -> usize {
        self.state().conns.len()
    }

    /// An entity the snapshot read: kept unless a notification already brought a newer one.
    pub fn seed(&self, entity: Value) {
        let Some(id) = entity.get("id").and_then(Value::as_str).map(str::to_owned) else {
            return;
        };
        let mut state = self.state();
        if !state.index.is_empty() && !state.table.contains_key(&id) {
            state.keep(id, entity);
        }
    }

    /// Whether the upstream reads the table now: it is due and a filter needs it (T-5182). A hub
    /// whose clients only connect holds no table: a sweep that opens and closes `/ws` on every
    /// Endpoint seeded one per Endpoint and took the gateway past 1 GiB.
    pub fn snapshot_now(&self) -> bool {
        let mut state = self.state();
        let now = state.due && !state.index.is_empty();
        if now {
            state.due = false;
        }
        now
    }

    /// Resolves when a filter arrives on a hub whose table is due.
    pub async fn snapshot_wanted(&self) {
        self.wanted.notified().await;
    }

    /// The broker refused the upstream subscription with `status`: every connection ends, and
    /// every `x-subscribe` is answered 503 until [`Hub::live`].
    pub fn refuse(&self, status: u16, detail: String) {
        let mut state = self.state();
        state.refused = Some((status, detail));
        for conn in state.conns.values() {
            conn.queue.push(Out::Refused);
        }
    }

    /// The broker's status while it refuses the upstream subscription.
    pub fn refused(&self) -> Option<u16> {
        self.state().refused.as_ref().map(|(status, _)| *status)
    }

    /// The broker accepted the upstream subscription.
    pub fn live(&self) {
        self.state().refused = None;
    }

    /// The upstream lost its place: every connection is told to read again.
    pub fn gap(&self) {
        let mut state = self.state();
        state.due = true;
        for conn in state.conns.values() {
            conn.queue.push(Out::Gap);
        }
    }

    /// How many entities the table holds.
    pub fn entities(&self) -> usize {
        self.state().table.len()
    }

    /// One change from the upstream, already cut to this hub's identity by the broker: the table
    /// keeps it, and each connection with a candidate filter is sent what its filters make of it
    /// (ADR-N-049 §2.5).
    pub fn apply(&self, entity: Value, at: chrono::DateTime<chrono::Utc>) {
        let Some(id) = entity.get("id").and_then(Value::as_str).map(str::to_owned) else {
            return;
        };
        let mut state = self.state();
        let state = &mut *state;
        // No filter, nothing to judge: the table waits for the first one and its snapshot.
        if state.index.is_empty() {
            return;
        }
        let previous = if entity.get("deletedAt").is_some() {
            // An entity that left sends nothing: that message would reveal a hidden change.
            state.table.remove(&id);
            return;
        } else {
            state.keep(id.clone(), entity.clone())
        };
        let Some(full) = expanded(&entity) else {
            return;
        };
        let mut by_conn: BTreeMap<u64, Vec<FilterId>> = BTreeMap::new();
        for filter in state.index.candidates(&full, core()) {
            by_conn.entry(filter >> SUB_BITS).or_default().push(filter);
        }
        if by_conn.is_empty() {
            return;
        }
        let delta = changed(previous.as_ref().and_then(expanded).as_ref(), &full);
        for (conn_id, filters) in by_conn {
            let Some(conn) = state.conns.get(&conn_id) else {
                continue;
            };
            for filter in filters {
                if !state.index.decides(filter, &full, &delta, core()) {
                    continue;
                }
                let Some(sub) = conn.subs.values().find(|s| s.filter == filter) else {
                    continue;
                };
                let mut data = entity.clone();
                if let Some(names) = &sub.attributes {
                    projection::project_entity(&mut data, names, &BTreeSet::new());
                }
                conn.queue.push(Out::Notification {
                    subscription: sub.id.clone(),
                    entity: id.clone(),
                    body: notification(&sub.id, data, at),
                });
                if let Some(held) = state.index.get_mut(filter) {
                    held.sent(at);
                }
            }
        }
    }
}

impl State {
    /// Drops the table once no filter is left: nothing judges a change against it, and the next
    /// filter reads it again (T-5182).
    fn forget_unwatched(&mut self) {
        if self.index.is_empty() {
            if !self.table.is_empty() {
                self.table = HashMap::new();
            }
            self.due = true;
        }
    }

    /// Keeps `entity` as the latest of its id, evicting the least recently changed beyond the
    /// bound; answers the state it replaces.
    fn keep(&mut self, id: String, entity: Value) -> Option<Value> {
        self.tick += 1;
        let previous = self.table.insert(id, (self.tick, entity)).map(|(_, e)| e);
        if self.table.len() > self.bound {
            // ponytail: an O(n) scan per eviction batch of 1 % of the bound; a linked LRU when
            // a table at its bound changes faster than the scan.
            let batch = (self.bound / 100).max(1);
            let mut oldest: Vec<(u64, String)> = self
                .table
                .iter()
                .map(|(id, (tick, _))| (*tick, id.clone()))
                .collect();
            oldest.sort_unstable();
            for (_, id) in oldest.into_iter().take(batch) {
                self.table.remove(&id);
            }
            metrics::counter!("jc_gateway_ws_entities_evicted_total").increment(batch as u64);
        }
        previous
    }
}

/// The NGSI-LD Notification (TS 104 175 §5.2.6.9.1) of one entity for one subscription.
fn notification(subscription: &str, entity: Value, at: chrono::DateTime<chrono::Utc>) -> Value {
    json!({
        "id": format!("urn:ngsi-ld:Notification:{}", random_name()),
        "type": "Notification",
        "subscriptionId": subscription,
        "notifiedAt": at.to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
        "data": [entity],
    })
}

/// 128 random bits, base32 (lower case), as a connection name, a resume token or an id.
pub fn random_name() -> String {
    use ring::rand::SecureRandom;
    let mut bytes = [0_u8; 16];
    // The system generator does not fail on a supported platform; zeros would still be unique
    // per process only by luck, so a failure is logged.
    if ring::rand::SystemRandom::new().fill(&mut bytes).is_err() {
        tracing::error!("the system random generator failed");
    }
    const ALPHABET: &[u8; 32] = b"abcdefghijklmnopqrstuvwxyz234567";
    let mut out = String::with_capacity(26);
    let mut buffer = 0_u32;
    let mut bits = 0;
    for byte in bytes {
        buffer = (buffer << 8) | u32::from(byte);
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            out.push(char::from(ALPHABET[((buffer >> bits) & 31) as usize]));
        }
    }
    if bits > 0 {
        out.push(char::from(ALPHABET[((buffer << (5 - bits)) & 31) as usize]));
    }
    out
}

/// A client's message envelope (TS 104 243 §5.2): exactly `metadata` and an optional `body`.
pub fn envelope(text: &str) -> Result<(Map<String, Value>, Option<Value>), String> {
    let value: Value =
        serde_json::from_str(text).map_err(|_| "a message is one JSON object".to_owned())?;
    let mut members = match value {
        Value::Object(members) => members,
        _ => return Err("a message is one JSON object".into()),
    };
    if members.keys().any(|k| k != "metadata" && k != "body") {
        return Err("a message has the members metadata and body only".into());
    }
    let metadata = match members.remove("metadata") {
        Some(Value::Object(metadata)) => metadata,
        _ => return Err("a message carries a metadata object".into()),
    };
    Ok((metadata, members.remove("body")))
}

#[cfg(test)]
pub(crate) mod tests_support {
    use super::*;

    /// An Endpoint that serves the WebSocket, for the fan-out's tests.
    pub fn endpoint() -> Arc<Endpoint> {
        Arc::new(Endpoint {
            declared_types: None,
            roles: Default::default(),
            slug: "s".to_owned(),
            title: BTreeMap::new(),
            description: BTreeMap::new(),
            space: "city".to_owned(),
            project: "city".to_owned(),
            audience: jc_core::kinds::Audience::Public,
            allowed_projects: Vec::new(),
            representations: vec![jc_core::kinds::Representation::WebSocket],
            rate_limit: None,
            creates: None,
            file_limits: None,
            hidden_attributes: Default::default(),
            projection: None,
            view_mapping: None,
            catalog: None,
            deprecation: None,
            feeds: Vec::new(),
            calendars: Vec::new(),
            map_defaults: Default::default(),
            base_path: "/api/endpoint/s".to_owned(),
            models: Vec::new(),
            policy_names: Vec::new(),
            manifest_name: String::new(),
            policies: Vec::new(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::tests_support::endpoint;
    use super::*;

    #[test]
    fn a_closed_debug_never_shows_the_resume_token() {
        let closed = Closed {
            token: "s3cr3t-test".into(),
            principal: "anna".into(),
            subs: Vec::new(),
            sent: VecDeque::new(),
            at: Instant::now(),
        };
        let text = format!("{closed:?}");
        assert!(!text.contains("s3cr3t-test"), "{text}");
        assert!(
            text.contains("anna") && text.contains("<redacted>"),
            "{text}"
        );
    }

    fn bus(id: &str, speed: f64, secret: &str) -> Value {
        json!({
            "id": id,
            "type": "Vehicle",
            "speed": {"type": "Property", "value": speed},
            "secret": {"type": "Property", "value": secret},
        })
    }

    fn open(hub: &Hub) -> (u64, Arc<Queue>) {
        hub.open(endpoint(), "anonymous".into(), None)
    }

    /// A connection whose upgrade failed leaves the hub with its guard; a taken one stays.
    #[test]
    fn a_joined_connection_without_a_session_leaves_the_hub() {
        let hub = Arc::new(Hub::new("s", 10));
        let (conn, queue) = open(&hub);
        subscribe(&hub, conn, json!({}));
        drop(Joined::new(Arc::clone(&hub), conn, queue));
        assert_eq!(hub.connections(), 0);
        assert!(hub.idle_for().is_some());
        assert!(hub.state().index.is_empty());

        let (conn, queue) = open(&hub);
        let (_, taken, _) = Joined::new(Arc::clone(&hub), conn, queue).take();
        assert_eq!((taken, hub.connections()), (conn, 1));
    }

    /// T-4874: a reload judges each connection again. The same record in a new table keeps it;
    /// a changed record, a client the accounts stop admitting, a removed Endpoint or one that
    /// stops serving the WebSocket each end it, and a change applied after that sends nothing.
    #[test]
    fn a_reload_closes_what_changed_and_nothing_follows_the_close() {
        let at = chrono::DateTime::UNIX_EPOCH;
        let same = |_: &str| true;
        let hub = Hub::new("s", 10);
        let (kept, kept_queue) = open(&hub);
        subscribe(&hub, kept, json!({}));
        assert!(
            hub.recheck(Some(&endpoint()), same).is_empty(),
            "equal record, new Arc"
        );

        let mut narrowed = (*endpoint()).clone();
        narrowed.audience = jc_core::kinds::Audience::ProjectList;
        assert_eq!(hub.recheck(Some(&Arc::new(narrowed)), same), [kept]);
        hub.apply(bus("urn:ngsi-ld:Vehicle:1", 1.0, "x"), at);
        let (items, _) = kept_queue.take();
        assert_eq!(items, [Out::Closing("endpoint-changed")]);
        assert!(hub.state().index.is_empty(), "its filters left at once");

        let (app, _) = hub.open(endpoint(), "user:jana".into(), Some("app-x".into()));
        assert!(hub
            .recheck(Some(&endpoint()), |azp| azp != "app-x")
            .contains(&app));

        let (_, gone_queue) = open(&hub);
        hub.recheck(None, same);
        assert_eq!(gone_queue.take().0, [Out::Closing("view-removed")]);
        let mut quiet = (*endpoint()).clone();
        quiet.representations.clear();
        let (off, off_queue) = open(&hub);
        assert!(hub.recheck(Some(&Arc::new(quiet)), same).contains(&off));
        assert_eq!(off_queue.take().0, [Out::Closing("view-removed")]);
    }

    fn subscribe(hub: &Hub, conn: u64, extra: Value) -> String {
        let mut body = json!({"type": "Subscription", "entities": [{"type": "Vehicle"}]});
        if let (Some(b), Some(e)) = (body.as_object_mut(), extra.as_object()) {
            b.extend(e.clone());
        }
        let answer = hub.subscribe(conn, None, &body, false);
        assert_eq!(answer.status, 201, "{answer:?}");
        answer.body.expect("created")["id"]
            .as_str()
            .expect("an id")
            .to_owned()
    }

    fn data(queue: &Queue) -> Vec<Value> {
        queue
            .take()
            .0
            .into_iter()
            .filter_map(|out| match out {
                Out::Notification { body, .. } => Some(body["data"][0].clone()),
                Out::Gap | Out::Closing(_) | Out::Refused => None,
            })
            .collect()
    }

    /// The upstream carries what the broker let this hub's identity read; a filter narrows it
    /// and never sees a change that is no change of what it watches.
    #[test]
    fn a_filter_narrows_what_the_upstream_carries() {
        let hub = Hub::new("s", 100);
        let (all, all_queue) = open(&hub);
        let (fast, fast_queue) = open(&hub);
        subscribe(&hub, all, json!({}));
        subscribe(&hub, fast, json!({"q": "speed>50"}));
        hub.apply(bus("urn:ngsi-ld:Vehicle:1", 10.0, "x"), chrono::Utc::now());
        let whole = data(&all_queue);
        assert_eq!(whole.len(), 1);
        assert_eq!(
            whole[0]["secret"]["value"], "x",
            "nothing is cut again: {whole:?}"
        );
        assert!(data(&fast_queue).is_empty());
        hub.apply(bus("urn:ngsi-ld:Vehicle:1", 60.0, "x"), chrono::Utc::now());
        assert_eq!(data(&fast_queue).len(), 1);
    }

    #[test]
    fn notification_attributes_cut_the_entity_further() {
        let hub = Hub::new("s", 100);
        let (conn, queue) = open(&hub);
        subscribe(
            &hub,
            conn,
            json!({"notification": {"attributes": ["speed"]}}),
        );
        hub.apply(bus("urn:ngsi-ld:Vehicle:1", 10.0, "x"), chrono::Utc::now());
        let sent = data(&queue);
        assert_eq!(sent[0]["speed"]["value"], 10.0);
        assert!(sent[0].get("secret").is_none());
    }

    #[test]
    fn subscriptions_are_capped_updated_and_removed() {
        let hub = Hub::new("s", 100);
        let (conn, queue) = open(&hub);
        let id = subscribe(&hub, conn, json!({"q": "speed>50"}));
        hub.apply(bus("urn:ngsi-ld:Vehicle:1", 10.0, "x"), chrono::Utc::now());
        assert!(data(&queue).is_empty());
        let updated = hub.subscribe(conn, Some(&id), &json!({"q": "speed>5"}), true);
        assert_eq!(updated.status, 204);
        hub.apply(bus("urn:ngsi-ld:Vehicle:1", 11.0, "x"), chrono::Utc::now());
        assert_eq!(data(&queue).len(), 1);
        let bad = hub.subscribe(conn, Some(&id), &json!({"q": "(speed"}), true);
        assert_eq!(bad.status, 400);
        assert_eq!(hub.unsubscribe(conn, &id).status, 204);
        assert_eq!(hub.unsubscribe(conn, &id).status, 404);
        assert_eq!(hub.subscribe(conn, Some(&id), &json!({}), true).status, 404);
        for _ in 0..MAX_FILTERS {
            subscribe(&hub, conn, json!({}));
        }
        let over = hub.subscribe(
            conn,
            None,
            &json!({"type": "Subscription", "entities": [{"type": "Vehicle"}]}),
            false,
        );
        assert_eq!(over.status, 400);
        assert_eq!(hub.subscriptions(conn), MAX_FILTERS);
    }

    #[test]
    fn a_full_queue_overflows_and_a_queued_entity_is_replaced_in_place() {
        let queue = Queue::default();
        let note = |entity: &str, n: u64| Out::Notification {
            subscription: "s".into(),
            entity: entity.into(),
            body: json!(n),
        };
        queue.push(note("a", 1));
        queue.push(note("b", 1));
        queue.push(note("a", 2));
        let (items, overflowed) = queue.take();
        assert_eq!(items, [note("a", 2), note("b", 1)]);
        assert!(!overflowed);
        for n in 0..=MAX_QUEUED as u64 {
            queue.push(note(&n.to_string(), n));
        }
        let (items, overflowed) = queue.take();
        assert_eq!(items.len(), MAX_QUEUED);
        assert!(overflowed);
    }

    #[test]
    fn the_table_is_bounded_and_a_deleted_entity_leaves_it() {
        let hub = Hub::new("s", 100);
        let (conn, _) = open(&hub);
        subscribe(&hub, conn, json!({}));
        for n in 0..150 {
            hub.seed(bus(&format!("urn:ngsi-ld:Vehicle:{n}"), 1.0, "x"));
        }
        assert!(hub.entities() <= 100, "{}", hub.entities());
        let mut gone = bus("urn:ngsi-ld:Vehicle:149", 1.0, "x");
        gone["deletedAt"] = json!("2026-10-13T00:00:00Z");
        let before = hub.entities();
        hub.apply(gone, chrono::Utc::now());
        assert_eq!(hub.entities(), before - 1);
    }

    #[test]
    fn a_hub_without_a_filter_holds_no_table() {
        let hub = Hub::new("s", 100);
        let (conn, _) = open(&hub);
        hub.apply(bus("urn:ngsi-ld:Vehicle:1", 1.0, "x"), chrono::Utc::now());
        hub.seed(bus("urn:ngsi-ld:Vehicle:2", 1.0, "x"));
        assert_eq!(hub.entities(), 0);
        assert!(!hub.snapshot_now(), "no filter, no snapshot");
        let id = subscribe(&hub, conn, json!({}));
        assert!(hub.snapshot_now(), "the first filter reads the table");
        assert!(!hub.snapshot_now(), "once");
        hub.seed(bus("urn:ngsi-ld:Vehicle:2", 1.0, "x"));
        assert_eq!(hub.entities(), 1);
        assert_eq!(hub.unsubscribe(conn, &id).status, 204);
        assert_eq!(hub.entities(), 0, "the last filter took the table with it");
        subscribe(&hub, conn, json!({}));
        assert!(hub.snapshot_now(), "the next filter reads it again");
        hub.gap();
        assert!(hub.snapshot_now(), "a gap reads it again");
    }

    #[test]
    fn a_seed_never_replaces_a_newer_notification() {
        let hub = Hub::new("s", 100);
        let (conn, queue) = open(&hub);
        subscribe(&hub, conn, json!({"watchedAttributes": ["speed"]}));
        hub.apply(bus("urn:ngsi-ld:Vehicle:1", 20.0, "x"), chrono::Utc::now());
        hub.seed(bus("urn:ngsi-ld:Vehicle:1", 5.0, "x"));
        assert_eq!(data(&queue).len(), 1);
        // Against the newer state the same speed is no change.
        hub.apply(bus("urn:ngsi-ld:Vehicle:1", 20.0, "x"), chrono::Utc::now());
        assert!(data(&queue).is_empty());
    }

    #[test]
    fn a_closed_connection_resumes_only_for_its_caller_with_its_token() {
        let hub = Hub::new("s", 100);
        let (conn, _) = open(&hub);
        let id = subscribe(&hub, conn, json!({}));
        hub.close(
            conn,
            "name",
            "token",
            VecDeque::from([(1, "one".to_owned())]),
        );
        assert_eq!(hub.subscriptions(conn), 0);
        assert!(hub.idle_for().is_some());
        assert!(hub.take_closed("name", "wrong", "anonymous").is_none());
        assert!(hub.take_closed("name", "token", "someone-else").is_none());
        let held = hub
            .take_closed("name", "token", "anonymous")
            .expect("resumable");
        assert_eq!(held.subs[0].0, id);
        assert!(
            hub.take_closed("name", "token", "anonymous").is_none(),
            "once"
        );
    }

    /// Reconnect churn holds no more than the caps: the oldest closed connections go first.
    #[test]
    fn closed_connections_held_for_a_resume_stay_under_the_caps() {
        let hub = Hub::new("s", 100);
        let large = "x".repeat(64 * 1024);
        for n in 0..(MAX_CLOSED + 40) {
            let (conn, _) = open(&hub);
            let sent = VecDeque::from([(1, large.clone())]);
            hub.close(conn, &format!("c{n}"), "token", sent);
        }
        let state = hub.state();
        let bytes: usize = state.closed.values().map(|c| c.sent[0].1.len()).sum();
        assert!(state.closed.len() <= MAX_CLOSED, "{}", state.closed.len());
        assert!(bytes <= MAX_CLOSED_BYTES, "{bytes}");
        let newest = format!("c{}", MAX_CLOSED + 39);
        assert!(state.closed.contains_key(&newest), "the newest is kept");
        assert!(!state.closed.contains_key("c0"), "the oldest went");
    }

    #[test]
    fn a_message_is_metadata_and_a_body_and_nothing_else() {
        assert!(envelope(r#"{"metadata": {"x-op": "x-subscribe"}, "body": {}}"#).is_ok());
        assert!(envelope(r#"{"metadata": {"x-op": "x-unsubscribe"}}"#).is_ok());
        for bad in [
            "[]",
            "{",
            r#"{"body": {}}"#,
            r#"{"metadata": [], "body": {}}"#,
            r#"{"metadata": {}, "x": 1}"#,
        ] {
            assert!(envelope(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn random_names_are_26_base32_characters_and_differ() {
        let (a, b) = (random_name(), random_name());
        assert_eq!(a.len(), 26);
        assert!(a
            .bytes()
            .all(|c| c.is_ascii_lowercase() || (b'2'..=b'7').contains(&c)));
        assert_ne!(a, b);
    }
}
