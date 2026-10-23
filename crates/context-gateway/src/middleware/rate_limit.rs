//! Token-bucket rate limiting per endpoint and caller (T-0154, EP-20, MIM0-R7, OPS-35).
//!
//! One bucket per `(slug, caller)`: the principal a genuine token names when there is one, the
//! client address when there is not. Keying an anonymous caller by address is the only thing a
//! public endpoint can key on, and keying a signed-in one by its principal is what stops two
//! clients behind one NAT from spending each other's quota. A token the realm did not sign is
//! no principal: it spends its address's bucket, so a fresh made-up token per request buys no
//! fresh bucket (T-5290).
//!
//! `spec.rateLimits.requestsPerMinute` is the steady rate and `burst` the bucket size, so
//! a caller may spend a burst at once and then goes at the steady rate. An endpoint that
//! declares no limits is not limited here; APISIX in front still has its own.
//!
//! The counter is in this process. Two replicas therefore allow up to twice the configured
//! rate, which is the honest trade for a limiter that costs a mutex and no round trip.
// ponytail: in-process buckets behind a few locks. A shared counter (Redis, the mesh) is a
// different component, and worth it only when a measurement says the per-replica bound
// is too loose.

use crate::app::Gateway;
use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{HeaderValue, Response};
use axum::middleware::Next;
use axum::response::IntoResponse;
use jc_core::kinds::RateLimits;
use jc_core::ProblemDetails;
use std::collections::hash_map::RandomState;
use std::collections::HashMap;
use std::hash::BuildHasher;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Buckets are dropped once they have been full and untouched for this long: a caller that
/// stopped calling has nothing left to remember, and the map must not grow with every
/// address that ever appeared.
const IDLE_EVICTION: Duration = Duration::from_secs(300);

/// The buckets are split over this many locks by key, so a sweep holds one shard at a time
/// while requests of the other shards go on (T-5290).
const SHARDS: usize = 16;

/// The map has to be at least this large before a sweep is worth its scan.
const SWEEP_SIZE: usize = 1_024;

/// The most callers a shard holds, 65,536 over all shards, a few MiB. A caller who arrives at a
/// full shard spends that shard's overflow bucket of the slug, shared with every other such
/// caller, so a flood of new addresses is throttled together instead of growing the map
/// (T-5290).
const SHARD_CAP: usize = 4_096;

/// The caller of the overflow bucket; no key [`caller_key`] makes has this form.
const OVERFLOW: &str = "overflow";

/// And the map is measured, and swept, at most this often. Without it, a gateway with more live
/// callers than `SWEEP_SIZE` pays a full scan on every single request, which is the busiest
/// case there is (T-2356).
const SWEEP_INTERVAL: Duration = Duration::from_secs(60);

/// What the limiter decided, and everything the response headers need (EP-20).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Decision {
    /// Whether the request may proceed.
    pub allowed: bool,
    /// The steady quota, in requests per minute: `RateLimit-Limit`.
    pub limit: u32,
    /// Whole requests still available right now: `RateLimit-Remaining`.
    pub remaining: u32,
    /// Seconds until the caller may spend again: `RateLimit-Reset`, and `Retry-After`
    /// when the answer is 429. Zero while requests are still available.
    pub reset: u64,
}

/// One caller's bucket.
///
/// It carries the rate and the capacity of the endpoint it belongs to, because the sweep has to
/// judge each bucket by its own limits: one endpoint's `burst` says nothing about whether
/// another's bucket has refilled, and deciding with the limits of whichever request happened to
/// trigger the sweep would drop a bucket that is still drained — which hands that caller a fresh
/// burst it had not earned (T-2356).
#[derive(Debug, Clone, Copy)]
struct Bucket {
    tokens: f64,
    last: Instant,
    /// Tokens a second, from `requestsPerMinute`.
    rate: f64,
    /// The bucket size, from `burst`.
    capacity: f64,
}

impl Bucket {
    /// The tokens this bucket holds at `now`, which is the stored value plus what has refilled
    /// since it was last touched. The refill is computed on read and never written back, so it
    /// is the only honest way to ask whether a bucket is full.
    fn tokens_at(&self, now: Instant) -> f64 {
        let elapsed = now.saturating_duration_since(self.last).as_secs_f64();
        (self.tokens + elapsed * self.rate).min(self.capacity)
    }
}

/// One shard's buckets, by slug and caller (EP-20).
type Buckets = HashMap<(String, String), Bucket>;

/// The buckets of every caller of every endpoint (EP-20).
#[derive(Debug, Default)]
pub struct RateLimiter {
    shards: [Mutex<Buckets>; SHARDS],
    /// Random per process, so no client can aim its keys at one shard.
    hasher: RandomState,
    /// When the map was last measured for a sweep; `None` until the first request. Held by the
    /// one request that sweeps, never by the others.
    swept: Mutex<Option<Instant>>,
}

impl RateLimiter {
    /// A limiter with no buckets yet.
    pub fn new() -> Self {
        Self::default()
    }

    /// Spends one request of `caller`'s quota on `slug`, at `now`.
    ///
    /// `now` is a parameter so a test can drive the clock instead of sleeping through a
    /// minute of refill.
    pub fn check(&self, slug: &str, caller: &str, limits: &RateLimits, now: Instant) -> Decision {
        let rate = f64::from(limits.requests_per_minute) / 60.0;
        let capacity = f64::from(limits.burst.unwrap_or(limits.requests_per_minute)).max(1.0);
        self.sweep(now);
        let key = (slug.to_owned(), caller.to_owned());
        let mut buckets = lock(&self.shards[shard_of(&self.hasher, &key)]);
        let key = if buckets.len() >= SHARD_CAP && !buckets.contains_key(&key) {
            (key.0, OVERFLOW.to_owned())
        } else {
            key
        };
        let bucket = buckets.entry(key).or_insert(Bucket {
            tokens: capacity,
            last: now,
            rate,
            capacity,
        });
        bucket.tokens = bucket.tokens_at(now);
        bucket.last = now;
        // The manifest may have changed its limits since this bucket was made; the bucket
        // follows it, and the tokens it holds are capped by the new size.
        bucket.rate = rate;
        bucket.capacity = capacity;
        bucket.tokens = bucket.tokens.min(capacity);

        if bucket.tokens >= 1.0 {
            bucket.tokens -= 1.0;
            Decision {
                allowed: true,
                limit: limits.requests_per_minute,
                remaining: bucket.tokens as u32,
                reset: 0,
            }
        } else {
            // Seconds until one whole token exists again, rounded up: a client that waits
            // exactly this long is never refused for being a fraction early.
            let missing = 1.0 - bucket.tokens;
            let wait = if rate > 0.0 { missing / rate } else { 60.0 };
            Decision {
                allowed: false,
                limit: limits.requests_per_minute,
                remaining: 0,
                reset: wait.ceil() as u64,
            }
        }
    }

    /// How many buckets are held, which is what an eviction test looks at.
    pub fn len(&self) -> usize {
        self.shards.iter().map(|shard| lock(shard).len()).sum()
    }

    /// Whether no caller has been seen yet.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// The shard `key` lives in.
fn shard_of(hasher: &RandomState, key: &(String, String)) -> usize {
    // Truncating the hash on a 32-bit target still spreads the keys.
    hasher.hash_one(key) as usize % SHARDS
}

/// Locks a shard. A panicking holder poisoned the lock; the counters are just numbers and refusing
/// every request afterwards would be a worse failure than continuing.
fn lock(shard: &Mutex<Buckets>) -> std::sync::MutexGuard<'_, Buckets> {
    shard
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

impl RateLimiter {
    /// Drops buckets that have been idle long enough to be full again: they would hand out a full
    /// burst anyway, so remembering them changes nothing but the memory.
    ///
    /// Fullness is the refilled value, not the stored one (T-2356). `tokens` is what was left at
    /// the last touch and every allowed call subtracts one before storing, so a stored value is
    /// below the capacity for every bucket that has ever been checked — which is why the earlier
    /// `tokens < capacity` clause kept all of them and the map grew with every address that ever
    /// appeared.
    ///
    /// A bucket that has not refilled is kept however idle it is: dropping it would give its caller
    /// a fresh burst, and a caller must never buy one by waiting for somebody else's traffic to
    /// trigger a sweep.
    ///
    /// One request at a time sweeps, the others skip it; the sweep holds one shard's lock at a
    /// time, so it never stops the whole gateway for a scan of the whole map (T-5290).
    fn sweep(&self, now: Instant) {
        let mut swept = match self.swept.try_lock() {
            Ok(guard) => guard,
            Err(std::sync::TryLockError::Poisoned(poisoned)) => poisoned.into_inner(),
            Err(std::sync::TryLockError::WouldBlock) => return,
        };
        if swept.is_some_and(|swept| now.saturating_duration_since(swept) < SWEEP_INTERVAL) {
            return;
        }
        *swept = Some(now);
        if self.len() < SWEEP_SIZE {
            return;
        }
        for shard in &self.shards {
            lock(shard).retain(|_, bucket| {
                now.saturating_duration_since(bucket.last) < IDLE_EVICTION
                    || bucket.tokens_at(now) < bucket.capacity
            });
        }
    }
}

/// The layer every endpoint surface passes: one bucket spent, the headers of the standard
/// `RateLimit` field set on the answer, 429 when the bucket is empty (EP-20, MIM0-R7).
pub async fn enforce(
    State(gateway): State<Arc<Gateway>>,
    request: Request,
    next: Next,
) -> Response<Body> {
    // Only a published endpoint that declares a limit is counted. The canonical surface of a
    // space and an endpoint without `rateLimits` are not: a limit is an explicit choice, never
    // a default of the gateway's own (T-2775, EP-20, SP-22).
    let Some(endpoint) =
        slug_of(request.uri().path()).and_then(|slug| gateway.resolver.resolve(slug))
    else {
        // An unknown slug costs nobody quota: the handler answers 404 (EP-03).
        return next.run(request).await;
    };
    let Some(limits) = endpoint.rate_limit.clone() else {
        return next.run(request).await;
    };

    let caller = match request.extensions().get::<CallerKey>() {
        Some(CallerKey(caller)) => caller.clone(),
        None => caller_key(&gateway, request.headers()),
    };
    let decision = gateway
        .rate_limiter
        .check(&endpoint.slug, &caller, &limits, Instant::now());
    if !decision.allowed {
        tracing::info!(slug = %endpoint.slug, "rate limit reached");
        return too_many(
            &decision,
            "the endpoint's rate limit is spent; retry after the seconds the RateLimit-Reset header names",
        );
    }

    let mut response = next.run(request).await;
    set_headers(response.headers_mut(), &decision);
    response
}

/// The 429 of a spent bucket, with the `RateLimit` fields and `Retry-After` (EP-20).
pub(crate) fn too_many(decision: &Decision, detail: &str) -> Response<Body> {
    let mut refusal = ProblemDetails::new(429, "too-many-requests", "Too Many Requests")
        .with_detail(detail)
        .into_response();
    set_headers(refusal.headers_mut(), decision);
    if let Ok(value) = HeaderValue::from_str(&decision.reset.to_string()) {
        refusal.headers_mut().insert("retry-after", value);
    }
    refusal
}

/// The slug in `/api/endpoint/{slug}/…`, or nothing off the endpoint surface.
fn slug_of(path: &str) -> Option<&str> {
    path.strip_prefix("/api/endpoint/")?
        .split('/')
        .next()
        .filter(|slug| !slug.is_empty())
}

/// The caller the outer `in_flight` layer already keyed, so the limiter checks the token's
/// signature once per request and not twice (T-5290).
#[derive(Debug, Clone)]
pub(crate) struct CallerKey(pub(crate) String);

/// Who is spending the quota: the principal a genuine bearer token names when there is one,
/// the client address otherwise (EP-20).
///
/// The token's signature, issuer and lifetime are checked against the realm, its audience is
/// not: this is a counter, not a decision, and the handler still answers 401 to a token bound
/// to another resource. Keying the principal means two callers behind one NAT do not spend
/// each other's quota, which keying by address alone cannot give them, and one principal's
/// many tokens are one bucket. A token the realm did not sign, an expired one, any other
/// `Authorization` scheme (T-4590) or a gateway without a realm is no principal: the address
/// decides, so a fresh made-up credential per request is never a fresh bucket (T-5290).
///
/// The address comes from the LAST `X-Forwarded-For` entry, which is the peer the gateway's
/// own proxy saw. A client that sends the header itself only prepends to its own value,
/// so a spoofed entry is never the one read.
pub(crate) fn caller_key(gateway: &Gateway, headers: &axum::http::HeaderMap) -> String {
    let presented = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok());
    if let (Ok(raw), Some(verifier)) = (crate::auth::token::bearer(presented), &gateway.verifier) {
        if let Ok(claims) = verifier.identify(raw) {
            return format!("subject:{}", claims.sub);
        }
    }
    address_key(headers)
}

/// The client address alone, whatever credential the request presents (see [`caller_key`]).
pub(crate) fn address_key(headers: &axum::http::HeaderMap) -> String {
    let address = headers
        .get("x-forwarded-for")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.rsplit(',').next())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("unknown");
    format!("address:{address}")
}

/// The bucket every MCP request without a token spends, on whichever MCP surface it arrives:
/// the hub, a named server, an Endpoint's or a Context Space's (T-4168). One per client address,
/// so four surfaces and fifty public Endpoints are one allowance, not two hundred. It holds a
/// space, so no Endpoint slug and no server path is it.
const ANONYMOUS_MCP_BUCKET: &str = "mcp anonymous";

/// Spends the anonymous MCP bucket of the client address; the `Err` is the 429 to answer.
///
/// Keyed by the address alone: a credential the request carries cannot open a bucket of its
/// own here, since a caller without a token is by definition not the credential's holder. Only
/// a request that came through the edge is counted. APISIX appends the client's address to
/// `X-Forwarded-For` on every request, and the gateway's NetworkPolicy admits nothing else from
/// outside the cluster, so a request without the header is an in-cluster workload: the public
/// assistant calls Endpoint MCP that way for every visitor, and would otherwise put all of
/// them in one `address:unknown` bucket. The assistant limits its own visitors (T-4149).
pub(crate) fn spend_anonymous_mcp(
    gateway: &Gateway,
    headers: &axum::http::HeaderMap,
) -> Result<(), Box<Response<Body>>> {
    if !headers.contains_key("x-forwarded-for") {
        return Ok(());
    }
    let caller = address_key(headers);
    let decision = gateway.rate_limiter.check(
        ANONYMOUS_MCP_BUCKET,
        &caller,
        &gateway.anonymous_mcp_rate_limit,
        Instant::now(),
    );
    if decision.allowed {
        return Ok(());
    }
    tracing::info!(%caller, "anonymous MCP rate limit reached");
    Err(Box::new(too_many(
        &decision,
        "the MCP rate limit for callers without a token is spent; retry after the seconds the \
         RateLimit-Reset header names, or sign in",
    )))
}

/// The three standard fields of the answer (EP-20, MIM0-R7).
pub(crate) fn set_headers(headers: &mut axum::http::HeaderMap, decision: &Decision) {
    for (name, value) in [
        ("ratelimit-limit", decision.limit.to_string()),
        ("ratelimit-remaining", decision.remaining.to_string()),
        ("ratelimit-reset", decision.reset.to_string()),
    ] {
        if let Ok(value) = HeaderValue::from_str(&value) {
            headers.insert(name, value);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{address_key, Decision, RateLimiter, SHARDS, SHARD_CAP};
    use axum::body::Body;
    use axum::extract::Request;
    use jc_core::kinds::RateLimits;
    use std::time::{Duration, Instant};

    fn limits(per_minute: u32, burst: Option<u32>) -> RateLimits {
        RateLimits {
            requests_per_minute: per_minute,
            burst,
        }
    }

    /// T-1073, EP-20: `requestsPerMinute` is a rate per sixty seconds, and the bucket refills at
    /// that rate. The 60 in `check` is the seconds in a minute — the unit of the field's own
    /// name — and not a window anybody may retune: halving it would double every endpoint's
    /// real rate while every manifest still said `requestsPerMinute`.
    #[test]
    fn a_minute_of_waiting_refills_a_minute_of_requests() {
        let limiter = RateLimiter::new();
        let start = Instant::now();
        let quota = limits(60, Some(60));

        // The burst is spent, and the next one is refused.
        for _ in 0..60 {
            assert!(limiter.check("slug", "caller", &quota, start).allowed);
        }
        let refused = limiter.check("slug", "caller", &quota, start);
        assert!(!refused.allowed);
        // Sixty a minute is one a second: the wait for one token is a second, not a minute.
        assert_eq!(refused.reset, 1);

        // Half a minute refills half the quota, and a whole one fills it to the ceiling.
        let half = start + Duration::from_secs(30);
        for _ in 0..30 {
            assert!(limiter.check("slug", "caller", &quota, half).allowed);
        }
        assert!(!limiter.check("slug", "caller", &quota, half).allowed);

        let minute = half + Duration::from_secs(60);
        let Decision { remaining, .. } = limiter.check("slug", "caller", &quota, minute);
        assert_eq!(
            remaining, 59,
            "the bucket refilled to its ceiling, not past it"
        );
    }

    /// The slowest limit a manifest may carry is one a minute (`RateLimits::validate` refuses
    /// zero), and the wait it names is the minute the field is named for.
    #[test]
    fn one_a_minute_makes_the_second_caller_wait_a_minute() {
        let limiter = RateLimiter::new();
        let start = Instant::now();
        let slowest = limits(1, Some(1));

        assert!(limiter.check("slug", "caller", &slowest, start).allowed);
        let refused = limiter.check("slug", "caller", &slowest, start);
        assert!(!refused.allowed);
        assert_eq!(refused.reset, 60, "one a minute is a minute of waiting");

        // And after that minute the next one passes.
        let later = start + Duration::from_secs(60);
        assert!(limiter.check("slug", "caller", &slowest, later).allowed);
    }

    fn asking(headers: &[(&str, &str)]) -> axum::http::HeaderMap {
        let mut builder = Request::builder().uri("/ngsi-ld/v1/entities?type=Device");
        for (name, value) in headers {
            builder = builder.header(*name, *value);
        }
        builder
            .body(Body::empty())
            .expect("a request")
            .headers()
            .clone()
    }

    /// T-5290: the map holds at most `SHARDS * SHARD_CAP` buckets whatever number of callers
    /// arrive, and a caller past the cap spends its shard's overflow bucket, shared with every
    /// other such caller, instead of a fresh burst of its own.
    #[test]
    fn a_flood_of_new_callers_stops_at_the_cap_and_shares_the_overflow() {
        let limiter = RateLimiter::new();
        let now = Instant::now();
        let quota = limits(60, Some(1));
        let callers = SHARDS * SHARD_CAP + 10_000;
        let allowed = (0..callers)
            .filter(|n| {
                limiter
                    .check("slug", &format!("address:{n}"), &quota, now)
                    .allowed
            })
            .count();
        // Each shard holds its callers and one overflow bucket.
        assert!(
            limiter.len() <= SHARDS * (SHARD_CAP + 1),
            "{}",
            limiter.len()
        );
        // Each full shard's overflow bucket handed out its one token and refused the rest.
        assert!(
            allowed <= SHARDS * SHARD_CAP + SHARDS,
            "{allowed} of {callers}"
        );
        // A caller the map already holds keeps its own bucket at the cap.
        assert!(!limiter.check("slug", "address:0", &quota, now).allowed);
    }

    /// Without a credential the address decides, and it is the LAST entry of the chain: the peer
    /// the gateway's own proxy saw. A client prepends to its own value, so what it writes is
    /// never the entry read. Reaching the gateway without that proxy is refused by the
    /// NetworkPolicy, which admits only APISIX and the pipeline runner on 8080.
    #[test]
    fn an_anonymous_caller_is_keyed_by_the_peer_the_proxy_saw() {
        assert_eq!(
            address_key(&asking(&[("x-forwarded-for", "203.0.113.7, 10.42.0.1")])),
            "address:10.42.0.1",
            "the last entry is the proxy's own observation"
        );
        assert_eq!(
            address_key(&asking(&[("x-forwarded-for", "  10.42.0.1  ")])),
            "address:10.42.0.1",
            "a single entry is trimmed"
        );
        assert_eq!(address_key(&asking(&[])), "address:unknown");
        assert_eq!(
            address_key(&asking(&[("x-forwarded-for", "")])),
            "address:unknown",
            "an empty header names no peer"
        );
    }
}
