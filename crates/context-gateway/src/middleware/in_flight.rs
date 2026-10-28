//! How many requests the gateway works on at once, in total and per caller (T-4782).
//!
//! A read holds the broker's whole answer while it is projected (`MAX_BODY`), so
//! the memory of the gateway is the answers it waits on. With a broker answering in 4-10 s, prod's
//! gateway took about 500 reads in flight and was OOMKilled at its 1 GiB limit (T-4754 step 2).
//! A request past either cap is answered 503 with `Retry-After` at once, before anything of it is
//! read or forwarded: a busy gateway sheds load instead of growing until it is killed and every
//! request in flight fails with it.
//!
//! The caller is the rate limiter's (`rate_limit::caller_key`): the principal a genuine token
//! names, else the client address, so one client cannot take every slot from the others.
//!
//! The defaults come from `tests/in_flight_tests.rs` (T-4782): a read of a 1 MiB page costs the
//! gateway well under 1 MiB of peak memory, so 128 reads stay inside the about 300 MiB prod's
//! gateway has left under its 1 GiB limit at rest, while 128 in flight at a 100 ms answer is still
//! 1,280 requests a second per replica. A `file.csv` holds up to `MAX_COLLECTED` of
//! broker JSON besides, hence its own two slots.
//!
//! A slot counts a request, while a read's cost to the broker is its time (T-4903: an anonymous
//! `/attributes` took 13-19 s on dev, and four clients looping it held every slot). So a plain read
//! (GET or HEAD, neither a WebSocket upgrade nor a bulk export) still unanswered after
//! [`SLOW_AFTER`] weighs [`SLOW_WEIGHT`] against its caller's share and must take that weight from
//! a slow budget of half the total: slow reads never hold more than half of the gateway, nor
//! more than a quarter as many of them as a caller's share. A slow read that finds no room is
//! answered 503 then, and one unanswered at [`READ_DEADLINE`] is answered 503 with a problem
//! document; either way its handler future is dropped, which drops the broker call with it.
//!
//! The slots are shared by class (T-5124: a burst of anonymous reads held every slot, and the
//! pipelines' writes got 503). An anonymous request (no bearer token) takes one of the
//! [`anonymous_share`] slots besides, and an anonymous read a quarter of the slow budget at most;
//! a read, or anything anonymous, takes one of the total less the [`write_reserve`], which only a
//! token holder's write may use. So an anonymous flood from any number of addresses leaves the
//! signed-in readers half the gateway and the writers their reserve, and the flood itself is
//! answered 429 with `Retry-After`. A made-up bearer counts as signed in here, and the handler
//! refuses it 401 in microseconds, before any broker call: it holds a slot for no time worth having.
//! The probes (`/healthz`, `/livez`) and `/metrics` never pass this layer.
//!
//! One load generator may hold more than a caller's share (T-5921): a read whose token the realm
//! signed for the deployment's `JC_GATEWAY_LOADTEST_CLIENT` (its `azp`, checked by
//! `Verifier::identify`) is held to the loadtest share instead, while it still takes the total's
//! and the readers' slots like any signed-in read. Any other request of that client is refused
//! 403: the class reads only. An unsigned or forged token naming the same client is no class
//! here, and an address never is one.
// ponytail: counts, not bytes. An answer's size is unknown until the broker sends it; the
// per-request ceiling is MAX_BODY. A byte budget like the writes' when a measurement shows
// large pages in flight together.

use crate::app::Gateway;
use crate::middleware::rate_limit;
use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{header, HeaderValue, Method, Response};
use axum::middleware::Next;
use axum::response::IntoResponse;
use jc_core::ProblemDetails;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

/// Requests in flight at once unless the deployment names another (`JC_GATEWAY_MAX_IN_FLIGHT`).
pub const MAX_IN_FLIGHT: usize = 128;

/// Requests in flight at once for one caller unless the deployment names another
/// (`JC_GATEWAY_MAX_IN_FLIGHT_PER_CALLER`).
pub const MAX_IN_FLIGHT_PER_CALLER: usize = 32;

/// Requests in flight at once for the load generator unless the deployment names another
/// (`JC_GATEWAY_LOADTEST_IN_FLIGHT`): above the readers' share of [`MAX_IN_FLIGHT`], so the total
/// is what holds it (T-5921).
pub const LOADTEST_IN_FLIGHT: usize = 512;

/// `file.csv` downloads at once unless the deployment names another
/// (`JC_GATEWAY_MAX_DOWNLOADS`): one holds up to `MAX_COLLECTED` of broker JSON,
/// its table and its file.
pub const MAX_DOWNLOADS: usize = 2;

/// The seconds a refused request is told to wait.
pub(crate) const RETRY_SECONDS: u32 = 1;

/// How long a plain read runs before it counts as slow (T-4903).
pub const SLOW_AFTER: Duration = Duration::from_secs(2);

/// How many slots a slow read holds, its own included.
pub const SLOW_WEIGHT: usize = 4;

/// How long a plain read may take at all: past it the read is answered 503 and its broker call
/// dropped (T-4903). APISIX's read timeout on the plain routes sits above it.
pub const READ_DEADLINE: Duration = Duration::from_secs(10);

/// The seconds a read that ran out of time is told to wait: the broker just spent the deadline on it.
const RETRY_SLOW_SECONDS: u32 = 10;

/// The slots anonymous requests may hold of `total` (T-5124): half.
pub fn anonymous_share(total: usize) -> usize {
    (total / 2).max(1)
}

/// The slots of `total` only a token holder's write may take (T-5124): an eighth, at least one
/// while the total leaves a read one too.
pub fn write_reserve(total: usize) -> usize {
    (total / 8).max(1).min(total.saturating_sub(1))
}

/// What the class shares look at in a request (T-5124).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Class {
    /// No bearer token: the anonymous public.
    pub anonymous: bool,
    /// A GET, HEAD or OPTIONS, the WebSocket upgrade included.
    pub read: bool,
    /// A read of the load generator's verified token (T-5921).
    pub loadtest: bool,
}

impl Class {
    pub(crate) fn of(request: &Request) -> Self {
        let method = request.method();
        Self {
            anonymous: !jc_core::auth::token::presents(request.headers()),
            read: method == Method::GET || method == Method::HEAD || method == Method::OPTIONS,
            loadtest: false,
        }
    }
}

/// Why a request was not admitted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Refused {
    /// The gateway, its readers or the caller hold all they may: 503.
    Busy,
    /// The anonymous share is taken: 429, the anonymous public is over its rate.
    AnonymousShare,
}

/// The slots of the whole gateway and the ones each caller holds.
pub struct InFlight {
    total: Arc<Semaphore>,
    per_caller: usize,
    /// The load generator's share, in place of `per_caller` (T-5921).
    loadtest_per_caller: usize,
    callers: Arc<Mutex<HashMap<String, usize>>>,
    downloads: Arc<Semaphore>,
    slow: Arc<Semaphore>,
    /// The total less the write reserve: every read and every anonymous request takes one.
    reads: Arc<Semaphore>,
    anonymous: Arc<Semaphore>,
    /// The part of the slow budget anonymous reads may weigh on: half of it.
    anonymous_slow: Arc<Semaphore>,
    slow_after: Duration,
    deadline: Duration,
}

/// One admitted request: its slots come back when it is dropped, answered or abandoned.
pub(crate) struct Ticket {
    _slots: Vec<OwnedSemaphorePermit>,
    anonymous: bool,
    caller: String,
    weight: usize,
    /// The caller's share this request counts against.
    cap: usize,
    callers: Arc<Mutex<HashMap<String, usize>>>,
}

impl Drop for Ticket {
    fn drop(&mut self) {
        let mut callers = self.callers.lock().unwrap_or_else(|held| held.into_inner());
        if let Some(count) = callers.get_mut(&self.caller) {
            *count = count.saturating_sub(self.weight);
            if *count == 0 {
                callers.remove(&self.caller);
            }
        }
    }
}

impl InFlight {
    /// At most `total` requests at once, `per_caller` of them one caller's, and `downloads`
    /// `file.csv` answers built at once.
    pub fn new(total: usize, per_caller: usize, downloads: usize) -> Self {
        let total = total.max(1);
        let slow = (total / 2).max(SLOW_WEIGHT);
        Self {
            total: Arc::new(Semaphore::new(total)),
            per_caller: per_caller.max(1),
            loadtest_per_caller: LOADTEST_IN_FLIGHT,
            callers: Arc::default(),
            downloads: Arc::new(Semaphore::new(downloads.max(1))),
            slow: Arc::new(Semaphore::new(slow)),
            reads: Arc::new(Semaphore::new((total - write_reserve(total)).max(1))),
            anonymous: Arc::new(Semaphore::new(anonymous_share(total))),
            anonymous_slow: Arc::new(Semaphore::new((slow / 2).max(SLOW_WEIGHT))),
            slow_after: SLOW_AFTER,
            deadline: READ_DEADLINE,
        }
    }

    /// Holds the load generator to `per_caller` at once (T-5921).
    pub fn with_loadtest(mut self, per_caller: usize) -> Self {
        self.loadtest_per_caller = per_caller.max(1);
        self
    }

    /// A slot for one `file.csv` download, or `None` while every slot is taken.
    pub(crate) fn download(&self) -> Option<OwnedSemaphorePermit> {
        Arc::clone(&self.downloads).try_acquire_owned().ok()
    }

    /// A ticket for `caller` of `class`, or why there is none: the gateway, its class share or
    /// the caller holds all it may.
    pub(crate) fn admit(&self, caller: String, class: Class) -> Result<Ticket, Refused> {
        let slots = self.slots(class)?;
        let cap = match class.loadtest {
            true => self.loadtest_per_caller,
            false => self.per_caller,
        };
        let mut callers = self.callers.lock().unwrap_or_else(|held| held.into_inner());
        let count = callers.entry(caller.clone()).or_insert(0);
        if *count >= cap {
            return Err(Refused::Busy);
        }
        *count += 1;
        Ok(Ticket {
            _slots: slots,
            anonymous: class.anonymous,
            caller,
            weight: 1,
            cap,
            callers: Arc::clone(&self.callers),
        })
    }

    /// The total's slot, and the anonymous share's and the readers' ones `class` takes besides.
    fn slots(&self, class: Class) -> Result<Vec<OwnedSemaphorePermit>, Refused> {
        let take = |slots: &Arc<Semaphore>, refused| {
            Arc::clone(slots).try_acquire_owned().map_err(|_| refused)
        };
        let share = match class.anonymous {
            true => Refused::AnonymousShare,
            false => Refused::Busy,
        };
        let mut slots = vec![take(&self.total, Refused::Busy)?];
        if class.anonymous {
            slots.push(take(&self.anonymous, share)?);
        }
        if class.read || class.anonymous {
            slots.push(take(&self.reads, share)?);
        }
        Ok(slots)
    }

    /// Makes `ticket` a slow read's: [`SLOW_WEIGHT`] against its caller's share and the slow
    /// budget, or `false` when either has no room for it.
    fn weigh(&self, ticket: &mut Ticket) -> bool {
        let extra = SLOW_WEIGHT - ticket.weight;
        let weight = SLOW_WEIGHT as u32;
        let anonymous_slow = match ticket.anonymous {
            true => match Arc::clone(&self.anonymous_slow).try_acquire_many_owned(weight) {
                Ok(slots) => Some(slots),
                Err(_) => return false,
            },
            false => None,
        };
        let Ok(slow) = Arc::clone(&self.slow).try_acquire_many_owned(weight) else {
            return false;
        };
        let mut callers = self.callers.lock().unwrap_or_else(|held| held.into_inner());
        let Some(count) = callers.get_mut(&ticket.caller) else {
            return false;
        };
        if *count + extra > ticket.cap {
            return false;
        }
        *count += extra;
        ticket.weight = SLOW_WEIGHT;
        ticket._slots.push(slow);
        ticket._slots.extend(anonymous_slow);
        true
    }

    /// Runs a plain read under the slow weight and the deadline; dropping `answer` early drops
    /// the broker call it waits on.
    async fn time(
        &self,
        mut ticket: Ticket,
        answer: impl std::future::Future<Output = Response<Body>>,
    ) -> Response<Body> {
        let mut answer = std::pin::pin!(answer);
        if let Ok(answer) = tokio::time::timeout(self.slow_after, &mut answer).await {
            return answer;
        }
        if !self.weigh(&mut ticket) {
            tracing::info!(
                "answered 503: a slow read found no room in the slow budget or its caller's share"
            );
            return busy("the gateway works on as many slow reads as it may at once");
        }
        let rest = self.deadline.saturating_sub(self.slow_after);
        tokio::time::timeout(rest, answer)
            .await
            .unwrap_or_else(|_| self.ran_out())
    }

    /// The 503 of a read stopped at the deadline.
    fn ran_out(&self) -> Response<Body> {
        tracing::info!(deadline = ?self.deadline, "answered 503: a read ran out of time");
        unavailable(
            format!(
                "the read took longer than the {} s a read may take and was stopped; narrow it \
                 (type, q, limit) or send it again after the seconds Retry-After names",
                self.deadline.as_secs()
            ),
            RETRY_SLOW_SECONDS,
        )
    }
}

/// A GET or HEAD the deadline holds. Not a WebSocket upgrade, which lives on after it, nor a bulk
/// export (`{endpoint}/file.*`, the temporal history), which collects many pages and which the
/// edge bounds at 10 at once per client instead (deployment `context-endpoint-bulk`, T-4717).
fn is_plain_read(request: &Request) -> bool {
    let path = request.uri().path();
    (request.method() == Method::GET || request.method() == Method::HEAD)
        && !request.headers().contains_key(header::UPGRADE)
        && !path.contains("/file.")
        && !path.contains("/ngsi-ld/v1/temporal/")
}

impl Default for InFlight {
    fn default() -> Self {
        Self::new(MAX_IN_FLIGHT, MAX_IN_FLIGHT_PER_CALLER, MAX_DOWNLOADS)
    }
}

/// The layer every endpoint surface passes: a slot held until the handler answers, 503 when
/// there is none.
pub async fn enforce(
    State(gateway): State<Arc<Gateway>>,
    mut request: Request,
    next: Next,
) -> Response<Body> {
    let (caller, class) = match caller_of(&gateway, &request) {
        Ok(found) => found,
        Err(refused) => return *refused,
    };
    request
        .extensions_mut()
        .insert(rate_limit::CallerKey(caller.clone()));
    let ticket = match gateway.in_flight.admit(caller, class) {
        Ok(ticket) => ticket,
        Err(Refused::AnonymousShare) => {
            tracing::info!(path = %request.uri().path(), "answered 429: the anonymous share of the gateway is taken");
            return anonymous_share_taken();
        }
        Err(Refused::Busy) => {
            tracing::info!(path = %request.uri().path(), "answered 503: the gateway or the caller holds as many requests as it may");
            return busy("the gateway works on as many requests as it may at once");
        }
    };
    if !is_plain_read(&request) {
        return next.run(request).await;
    }
    gateway.in_flight.time(ticket, next.run(request)).await
}

/// The caller key and class of `request`, or the 403 of a load generator's request other than a
/// read (T-5921).
fn caller_of(gateway: &Gateway, request: &Request) -> Result<(String, Class), Box<Response<Body>>> {
    let claims = rate_limit::verified(gateway, request.headers());
    let caller = match &claims {
        Some(claims) => rate_limit::subject_key(claims),
        None => rate_limit::address_key(request.headers()),
    };
    let mut class = Class::of(request);
    if is_loadtest(gateway, claims.as_ref()) {
        if !class.read {
            tracing::info!(path = %request.uri().path(), "answered 403: the load generator only reads");
            return Err(Box::new(loadtest_writes()));
        }
        class.loadtest = true;
    }
    Ok((caller, class))
}

/// Whether `claims`, a token the realm signed, were issued to the deployment's load generator.
fn is_loadtest(gateway: &Gateway, claims: Option<&jc_core::auth::token::Claims>) -> bool {
    match (
        &gateway.loadtest_client,
        claims.and_then(|claims| claims.azp.as_ref()),
    ) {
        (Some(client), Some(azp)) => client == azp,
        _ => false,
    }
}

/// The 403 of a load generator's request other than a read: its class reads only (T-5921).
fn loadtest_writes() -> Response<Body> {
    ProblemDetails::new(403, "forbidden", "Access Denied by Policy")
        .with_detail(
            "the load generator's client only reads: send GET or HEAD, or use another client",
        )
        .into_response()
}

/// The 503 of a full gateway: nothing of the request was read or forwarded, so sending it again
/// after `Retry-After` is safe.
pub(crate) fn busy(what: &str) -> Response<Body> {
    unavailable(
        format!("{what}; send this request again after the seconds Retry-After names"),
        RETRY_SECONDS,
    )
}

/// The 429 of a full anonymous share (T-5124): nothing of the request was read or forwarded.
fn anonymous_share_taken() -> Response<Body> {
    let mut answer = ProblemDetails::new(429, "too-many-requests", "Too Many Requests")
        .with_detail(
            "the gateway serves as many requests without a token as it may at once; send this \
             request again after the seconds Retry-After names, or sign in",
        )
        .into_response();
    answer
        .headers_mut()
        .insert("retry-after", HeaderValue::from(RETRY_SECONDS));
    answer
}

fn unavailable(detail: String, retry: u32) -> Response<Body> {
    let mut answer = ProblemDetails::new(503, "service-unavailable", "Service Unavailable")
        .with_detail(detail)
        .into_response();
    answer
        .headers_mut()
        .insert("retry-after", HeaderValue::from(retry));
    answer
}

#[cfg(test)]
mod tests {
    use super::*;

    const SIGNED_READ: Class = Class {
        anonymous: false,
        read: true,
        loadtest: false,
    };
    const SIGNED_WRITE: Class = Class {
        anonymous: false,
        read: false,
        loadtest: false,
    };
    const ANONYMOUS_READ: Class = Class {
        anonymous: true,
        read: true,
        loadtest: false,
    };

    #[test]
    fn the_loadtest_class_holds_its_own_share_inside_the_readers_one() {
        let gate = InFlight::new(16, 2, 1).with_loadtest(8);
        let loadtest = Class {
            loadtest: true,
            ..SIGNED_READ
        };
        let held: Vec<_> = (0..8)
            .map(|_| gate.admit("subject:loadtest".into(), loadtest))
            .collect::<Result<_, _>>()
            .expect("the loadtest share, past the caller's 2");
        assert_eq!(
            gate.admit("subject:loadtest".into(), loadtest).err(),
            Some(Refused::Busy)
        );
        assert_eq!(
            gate.admit("subject:loadtest".into(), SIGNED_READ).err(),
            Some(Refused::Busy),
            "a caller's share is the plain one outside the class"
        );
        drop(held);

        let wide = InFlight::new(8, 2, 1).with_loadtest(64);
        let held: Vec<_> = (0..7)
            .map(|_| wide.admit("subject:loadtest".into(), loadtest))
            .collect::<Result<_, _>>()
            .expect("the readers' share: 8 less the write reserve of 1");
        assert_eq!(
            wide.admit("subject:loadtest".into(), loadtest).err(),
            Some(Refused::Busy)
        );
        let _write = wide
            .admit("subject:pipeline".into(), SIGNED_WRITE)
            .expect("the write reserve stays");
        drop(held);
    }

    #[test]
    fn the_shares_follow_the_total() {
        assert_eq!(
            (anonymous_share(MAX_IN_FLIGHT), write_reserve(MAX_IN_FLIGHT)),
            (64, 16)
        );
        assert_eq!((anonymous_share(1), write_reserve(1)), (1, 0));
        assert_eq!((anonymous_share(2), write_reserve(2)), (1, 1));
    }

    #[test]
    fn an_anonymous_flood_leaves_signed_in_readers_and_writers_their_slots() {
        let gate = InFlight::new(16, 16, 1);
        let flood: Vec<_> = (0..8)
            .map(|n| gate.admit(format!("address:{n}"), ANONYMOUS_READ))
            .collect::<Result<_, _>>()
            .expect("the anonymous half");
        assert_eq!(
            gate.admit("address:8".into(), ANONYMOUS_READ).err(),
            Some(Refused::AnonymousShare)
        );
        let readers: Vec<_> = (0..6)
            .map(|n| gate.admit(format!("credential:{n}"), SIGNED_READ))
            .collect::<Result<_, _>>()
            .expect("the readers' rest: 16 less the reserve of 2 less the 8 anonymous");
        assert_eq!(
            gate.admit("credential:r".into(), SIGNED_READ).err(),
            Some(Refused::Busy)
        );
        let _write = gate
            .admit("credential:pipeline".into(), SIGNED_WRITE)
            .expect("a write takes the reserve");
        drop((flood, readers));
    }

    #[test]
    fn an_anonymous_write_never_takes_the_write_reserve() {
        let gate = InFlight::new(8, 8, 1);
        let anonymous_write = Class {
            anonymous: true,
            read: false,
            loadtest: false,
        };
        let _anonymous: Vec<_> = (0..4)
            .map(|n| gate.admit(format!("address:{n}"), anonymous_write))
            .collect::<Result<_, _>>()
            .expect("the anonymous half");
        let _reads: Vec<_> = (0..3)
            .map(|n| gate.admit(format!("credential:{n}"), SIGNED_READ))
            .collect::<Result<_, _>>()
            .expect("the rest of the readers' 7");
        assert_eq!(
            gate.admit("address:x".into(), anonymous_write).err(),
            Some(Refused::AnonymousShare)
        );
        assert_eq!(
            gate.admit("credential:r".into(), SIGNED_READ).err(),
            Some(Refused::Busy)
        );
        assert!(gate.admit("credential:w".into(), SIGNED_WRITE).is_ok());
    }

    #[test]
    fn anonymous_slow_reads_weigh_on_half_the_slow_budget_at_most() {
        let gate = InFlight::new(16, 16, 1);
        let mut first = gate.admit("address:a".into(), ANONYMOUS_READ).expect("a");
        assert!(gate.weigh(&mut first));
        let mut second = gate.admit("address:b".into(), ANONYMOUS_READ).expect("b");
        assert!(
            !gate.weigh(&mut second),
            "the anonymous half of a slow budget of 8 is taken"
        );
        let mut signed = gate.admit("credential:s".into(), SIGNED_READ).expect("s");
        assert!(
            gate.weigh(&mut signed),
            "a signed-in slow read still has room"
        );
        drop(first);
        assert!(gate.weigh(&mut second), "the anonymous weight came back");
    }

    #[test]
    fn the_class_of_a_request() {
        let request = |method: Method, bearer: bool| {
            let mut builder = Request::builder()
                .method(method)
                .uri("/api/endpoint/e/ngsi-ld/v1/entities");
            if bearer {
                builder = builder.header(header::AUTHORIZATION, "Bearer x.y.z");
            }
            builder.body(Body::empty()).expect("a request")
        };
        assert_eq!(Class::of(&request(Method::GET, false)), ANONYMOUS_READ);
        assert_eq!(Class::of(&request(Method::POST, true)), SIGNED_WRITE);
        assert_eq!(Class::of(&request(Method::HEAD, true)), SIGNED_READ);
        let basic = Request::builder()
            .method(Method::GET)
            .header(header::AUTHORIZATION, "Basic YTpi")
            .body(Body::empty())
            .expect("a request");
        assert_eq!(Class::of(&basic), ANONYMOUS_READ, "only a bearer signs in");
    }

    #[test]
    fn an_anonymous_refusal_is_429_with_retry_after() {
        let answer = anonymous_share_taken();
        assert_eq!(answer.status(), 429);
        assert_eq!(answer.headers()["retry-after"], "1");
    }

    #[test]
    fn a_caller_past_its_share_is_refused_while_another_still_gets_in() {
        let gate = InFlight::new(4, 2, 1);
        let first = gate.admit("a".into(), SIGNED_READ).expect("a's first");
        let _second = gate.admit("a".into(), SIGNED_READ).expect("a's second");
        assert!(gate.admit("a".into(), SIGNED_READ).is_err(), "a's third");
        let _other = gate.admit("b".into(), SIGNED_READ).expect("b's first");
        drop(first);
        assert!(
            gate.admit("a".into(), SIGNED_READ).is_ok(),
            "a's slot came back"
        );
    }

    #[test]
    fn the_total_is_held_across_callers_and_comes_back_on_drop() {
        let gate = InFlight::new(2, 2, 1);
        let a = gate.admit("a".into(), SIGNED_WRITE).expect("a");
        let _b = gate.admit("b".into(), SIGNED_WRITE).expect("b");
        assert!(
            gate.admit("c".into(), SIGNED_WRITE).is_err(),
            "the gateway is full"
        );
        drop(a);
        assert!(gate.admit("c".into(), SIGNED_WRITE).is_ok());
    }

    #[test]
    fn a_refused_caller_leaves_no_entry_behind() {
        let gate = InFlight::new(1, 1, 1);
        let held = gate.admit("a".into(), SIGNED_READ).expect("a");
        assert!(gate.admit("b".into(), SIGNED_READ).is_err());
        drop(held);
        assert!(gate.callers.lock().expect("the map").is_empty());
    }

    /// Sets `flag` when dropped: the read's future, and the broker call in it, went away.
    struct Dropped(Arc<std::sync::atomic::AtomicBool>);

    impl Drop for Dropped {
        fn drop(&mut self) {
            self.0.store(true, std::sync::atomic::Ordering::SeqCst);
        }
    }

    fn timed(total: usize, per_caller: usize) -> InFlight {
        InFlight {
            slow_after: Duration::from_millis(20),
            deadline: Duration::from_millis(80),
            ..InFlight::new(total, per_caller, 1)
        }
    }

    #[test]
    fn a_slow_read_weighs_against_its_caller_and_the_slow_budget() {
        let gate = InFlight::new(8, 8, 1);
        let mut first = gate.admit("a".into(), SIGNED_READ).expect("a's first");
        assert!(gate.weigh(&mut first));
        assert_eq!(gate.callers.lock().expect("the map")["a"], SLOW_WEIGHT);
        let mut second = gate.admit("b".into(), SIGNED_READ).expect("b's first");
        assert!(!gate.weigh(&mut second), "the slow budget of 4 is taken");
        drop(first);
        assert!(gate.weigh(&mut second), "the weight came back");
        drop(second);
        assert!(gate.callers.lock().expect("the map").is_empty());
    }

    #[test]
    fn a_slow_read_past_its_callers_share_is_refused() {
        let gate = InFlight::new(64, SLOW_WEIGHT, 1);
        let mut slow = gate.admit("a".into(), SIGNED_READ).expect("a's first");
        let _fast = gate.admit("a".into(), SIGNED_READ).expect("a's second");
        assert!(
            !gate.weigh(&mut slow),
            "a slow read of 4 and a quick one are past a share of 4"
        );
        assert_eq!(gate.callers.lock().expect("the map")["a"], 2);
    }

    #[tokio::test]
    async fn a_read_unanswered_at_the_deadline_is_stopped_with_its_upstream_call() {
        let gate = timed(8, 8);
        let dropped = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let guard = Dropped(Arc::clone(&dropped));
        let ticket = gate.admit("a".into(), SIGNED_READ).expect("a");
        let answer = gate
            .time(ticket, async move {
                let _guard = guard;
                std::future::pending::<Response<Body>>().await
            })
            .await;
        assert_eq!(answer.status(), 503);
        assert_eq!(answer.headers()["retry-after"], "10");
        assert!(dropped.load(std::sync::atomic::Ordering::SeqCst));
        assert!(gate.callers.lock().expect("the map").is_empty());
    }

    #[tokio::test]
    async fn a_slow_read_without_room_is_refused_at_once_and_a_quick_one_answered() {
        let gate = timed(8, 8);
        let mut held = gate.admit("b".into(), SIGNED_READ).expect("b");
        assert!(gate.weigh(&mut held), "b takes the whole slow budget");
        let ticket = gate.admit("a".into(), SIGNED_READ).expect("a");
        let answer = gate
            .time(ticket, std::future::pending::<Response<Body>>())
            .await;
        assert_eq!(answer.status(), 503);
        assert_eq!(answer.headers()["retry-after"], "1");

        let ticket = gate.admit("a".into(), SIGNED_READ).expect("a again");
        let answer = gate
            .time(ticket, async { Response::new(Body::empty()) })
            .await;
        assert_eq!(answer.status(), 200);
    }

    #[test]
    fn only_plain_reads_are_timed() {
        let request = |method: Method, path: &str| {
            Request::builder()
                .method(method)
                .uri(path)
                .body(Body::empty())
                .expect("a request")
        };
        assert!(is_plain_read(&request(
            Method::GET,
            "/api/endpoint/e/ngsi-ld/v1/entities"
        )));
        assert!(is_plain_read(&request(
            Method::HEAD,
            "/api/endpoint/e/ngsi-ld/v1/types"
        )));
        assert!(!is_plain_read(&request(
            Method::POST,
            "/api/endpoint/e/ngsi-ld/v1/entities"
        )));
        assert!(!is_plain_read(&request(
            Method::GET,
            "/api/endpoint/e/file.csv"
        )));
        assert!(!is_plain_read(&request(
            Method::GET,
            "/api/endpoint/e/file.geojson"
        )));
        assert!(!is_plain_read(&request(
            Method::GET,
            "/api/endpoint/e/ngsi-ld/v1/temporal/entities"
        )));
        let mut upgrade = request(Method::GET, "/api/endpoint/e/ws");
        upgrade
            .headers_mut()
            .insert(header::UPGRADE, HeaderValue::from_static("websocket"));
        assert!(!is_plain_read(&upgrade));
    }
}
