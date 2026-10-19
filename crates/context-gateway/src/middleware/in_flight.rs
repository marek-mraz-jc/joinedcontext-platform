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
// ponytail: counts, not bytes. An answer's size is unknown until the broker sends it; the
// per-request ceiling is MAX_BODY. A byte budget like the writes' when a measurement shows
// large pages in flight together.

use crate::app::Gateway;
use crate::middleware::rate_limit;
use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{HeaderValue, Response};
use axum::middleware::Next;
use axum::response::IntoResponse;
use jc_core::ProblemDetails;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
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

/// The slots of the whole gateway and the ones each caller holds.
pub struct InFlight {
    total: Arc<Semaphore>,
    per_caller: usize,
    callers: Arc<Mutex<HashMap<String, usize>>>,
    downloads: Arc<Semaphore>,
}

/// One admitted request: its slots come back when it is dropped, answered or abandoned.
pub(crate) struct Ticket {
    _total: OwnedSemaphorePermit,
    caller: String,
    callers: Arc<Mutex<HashMap<String, usize>>>,
}

impl Drop for Ticket {
    fn drop(&mut self) {
        let mut callers = self.callers.lock().unwrap_or_else(|held| held.into_inner());
        if let Some(count) = callers.get_mut(&self.caller) {
            *count = count.saturating_sub(1);
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
        }
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
            caller,
            callers: Arc::clone(&self.callers),
        })
    }
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
    let Some(_ticket) = gateway.in_flight.admit(caller) else {
        tracing::info!(path = %request.uri().path(), "answered 503: the gateway or the caller holds as many requests as it may");
        return busy("the gateway works on as many requests as it may at once");
    };
    next.run(request).await
}

/// The 503 of a full gateway: nothing of the request was read or forwarded, so sending it again
/// after `Retry-After` is safe.
pub(crate) fn busy(what: &str) -> Response<Body> {
    let mut answer = ProblemDetails::new(503, "service-unavailable", "Service Unavailable")
        .with_detail(format!(
            "{what}; send this request again after the seconds Retry-After names"
        ))
        .into_response();
    answer
        .headers_mut()
        .insert("retry-after", HeaderValue::from(RETRY_SECONDS));
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
}
