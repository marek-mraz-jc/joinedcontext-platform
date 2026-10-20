//! How many requests the gateway works on at once, in total and per caller (T-4782).
//!
//! A read holds the broker's whole answer while it is projected (`MAX_BODY`), so
//! the memory of the gateway is the answers it waits on. With a broker answering in 4-10 s, prod's
//! gateway took about 500 reads in flight and was OOMKilled at its 1 GiB limit (T-4754 step 2).
//! A request past either cap is answered 503 with `Retry-After` at once, before anything of it is
//! read or forwarded: a busy gateway sheds load instead of growing until it is killed and every
//! request in flight fails with it.
//!
//! The caller is the rate limiter's (`rate_limit::caller_key`): the presented credential's
//! digest, else the client address, so one client cannot take every slot from the others.
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

/// The slots of the whole gateway and the ones each caller holds.
pub struct InFlight {
    total: Arc<Semaphore>,
    per_caller: usize,
    callers: Arc<Mutex<HashMap<String, usize>>>,
    downloads: Arc<Semaphore>,
    slow: Arc<Semaphore>,
    slow_after: Duration,
    deadline: Duration,
}

/// One admitted request: its slots come back when it is dropped, answered or abandoned.
pub(crate) struct Ticket {
    _total: OwnedSemaphorePermit,
    _slow: Option<OwnedSemaphorePermit>,
    caller: String,
    weight: usize,
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
        Self {
            total: Arc::new(Semaphore::new(total.max(1))),
            per_caller: per_caller.max(1),
            callers: Arc::default(),
            downloads: Arc::new(Semaphore::new(downloads.max(1))),
            slow: Arc::new(Semaphore::new((total / 2).max(SLOW_WEIGHT))),
            slow_after: SLOW_AFTER,
            deadline: READ_DEADLINE,
        }
    }

    /// A read counts as slow after `slow_after` and is ended at `deadline`.
    pub(crate) fn time_reads(&mut self, slow_after: Duration, deadline: Duration) {
        self.slow_after = slow_after;
        self.deadline = deadline.max(slow_after);
    }

    /// A slot for one `file.csv` download, or `None` while every slot is taken.
    pub(crate) fn download(&self) -> Option<OwnedSemaphorePermit> {
        Arc::clone(&self.downloads).try_acquire_owned().ok()
    }

    /// A ticket for `caller`, or `None` when the gateway or the caller holds all it may.
    pub(crate) fn admit(&self, caller: String) -> Option<Ticket> {
        let total = Arc::clone(&self.total).try_acquire_owned().ok()?;
        let mut callers = self.callers.lock().unwrap_or_else(|held| held.into_inner());
        let count = callers.entry(caller.clone()).or_insert(0);
        if *count >= self.per_caller {
            return None;
        }
        *count += 1;
        Some(Ticket {
            _total: total,
            _slow: None,
            caller,
            weight: 1,
            callers: Arc::clone(&self.callers),
        })
    }

    /// Makes `ticket` a slow read's: [`SLOW_WEIGHT`] against its caller's share and the slow
    /// budget, or `false` when either has no room for it.
    fn weigh(&self, ticket: &mut Ticket) -> bool {
        let extra = SLOW_WEIGHT - ticket.weight;
        let Ok(slow) = Arc::clone(&self.slow).try_acquire_many_owned(SLOW_WEIGHT as u32) else {
            return false;
        };
        let mut callers = self.callers.lock().unwrap_or_else(|held| held.into_inner());
        let Some(count) = callers.get_mut(&ticket.caller) else {
            return false;
        };
        if *count + extra > self.per_caller {
            return false;
        }
        *count += extra;
        ticket.weight = SLOW_WEIGHT;
        ticket._slow = Some(slow);
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
    request: Request,
    next: Next,
) -> Response<Body> {
    let caller = rate_limit::caller_key(request.headers());
    let Some(ticket) = gateway.in_flight.admit(caller) else {
        tracing::info!(path = %request.uri().path(), "answered 503: the gateway or the caller holds as many requests as it may");
        return busy("the gateway works on as many requests as it may at once");
    };
    if !is_plain_read(&request) {
        return next.run(request).await;
    }
    gateway.in_flight.time(ticket, next.run(request)).await
}

/// The 503 of a full gateway: nothing of the request was read or forwarded, so sending it again
/// after `Retry-After` is safe.
pub(crate) fn busy(what: &str) -> Response<Body> {
    unavailable(
        format!("{what}; send this request again after the seconds Retry-After names"),
        RETRY_SECONDS,
    )
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

    #[test]
    fn a_caller_past_its_share_is_refused_while_another_still_gets_in() {
        let gate = InFlight::new(4, 2, 1);
        let first = gate.admit("a".into()).expect("a's first");
        let _second = gate.admit("a".into()).expect("a's second");
        assert!(gate.admit("a".into()).is_none(), "a's third");
        let _other = gate.admit("b".into()).expect("b's first");
        drop(first);
        assert!(gate.admit("a".into()).is_some(), "a's slot came back");
    }

    #[test]
    fn the_total_is_held_across_callers_and_comes_back_on_drop() {
        let gate = InFlight::new(2, 2, 1);
        let a = gate.admit("a".into()).expect("a");
        let _b = gate.admit("b".into()).expect("b");
        assert!(gate.admit("c".into()).is_none(), "the gateway is full");
        drop(a);
        assert!(gate.admit("c".into()).is_some());
    }

    #[test]
    fn a_refused_caller_leaves_no_entry_behind() {
        let gate = InFlight::new(1, 1, 1);
        let held = gate.admit("a".into()).expect("a");
        assert!(gate.admit("b".into()).is_none());
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
        let mut gate = InFlight::new(total, per_caller, 1);
        gate.time_reads(Duration::from_millis(20), Duration::from_millis(80));
        gate
    }

    #[test]
    fn a_slow_read_weighs_against_its_caller_and_the_slow_budget() {
        let gate = InFlight::new(8, 8, 1);
        let mut first = gate.admit("a".into()).expect("a's first");
        assert!(gate.weigh(&mut first));
        assert_eq!(gate.callers.lock().expect("the map")["a"], SLOW_WEIGHT);
        let mut second = gate.admit("b".into()).expect("b's first");
        assert!(!gate.weigh(&mut second), "the slow budget of 4 is taken");
        drop(first);
        assert!(gate.weigh(&mut second), "the weight came back");
        drop(second);
        assert!(gate.callers.lock().expect("the map").is_empty());
    }

    #[test]
    fn a_slow_read_past_its_callers_share_is_refused() {
        let gate = InFlight::new(64, SLOW_WEIGHT, 1);
        let mut slow = gate.admit("a".into()).expect("a's first");
        let _fast = gate.admit("a".into()).expect("a's second");
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
        let ticket = gate.admit("a".into()).expect("a");
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
        let mut held = gate.admit("b".into()).expect("b");
        assert!(gate.weigh(&mut held), "b takes the whole slow budget");
        let ticket = gate.admit("a".into()).expect("a");
        let answer = gate
            .time(ticket, std::future::pending::<Response<Body>>())
            .await;
        assert_eq!(answer.status(), 503);
        assert_eq!(answer.headers()["retry-after"], "1");

        let ticket = gate.admit("a".into()).expect("a again");
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
