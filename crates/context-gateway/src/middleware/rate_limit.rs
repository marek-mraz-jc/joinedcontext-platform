//! Token-bucket rate limiting per endpoint and caller (T-0154, EP-20, MIM0-R7, OPS-35).
//!
//! One bucket per `(slug, caller)`: the presented credential when there is one, the client
//! address when there is not. Keying an anonymous caller by address is the only thing a
//! public endpoint can key on, and keying a credentialed one by its digest is what stops
//! two clients behind one NAT from spending each other's quota.
//!
//! `spec.rateLimits.requestsPerMinute` is the steady rate and `burst` the bucket size, so
//! a caller may spend a burst at once and then goes at the steady rate. An endpoint that
//! declares no limits is not limited here; APISIX in front still has its own.
//!
//! The counter is in this process. Two replicas therefore allow up to twice the configured
//! rate, which is the honest trade for a limiter that costs a mutex and no round trip.
// ponytail: in-process buckets, one lock. A shared counter (Redis, the mesh) is a
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
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Buckets are dropped once they have been full and untouched for this long: a caller that
/// stopped calling has nothing left to remember, and the map must not grow with every
/// address that ever appeared.
const IDLE_EVICTION: Duration = Duration::from_secs(300);

/// The map has to be at least this large before a sweep is worth its scan.
const SWEEP_SIZE: usize = 1_024;

/// And a sweep runs at most this often. Without it, a gateway with more live callers than
/// `SWEEP_SIZE` pays a full scan under the lock on every single request, which is the busiest
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

/// The buckets of every caller of every endpoint, and when they were last swept (EP-20).
#[derive(Debug, Default)]
struct Buckets {
    by_caller: HashMap<(String, String), Bucket>,
    /// `None` until the first sweep.
    swept: Option<Instant>,
}

/// The buckets of every caller of every endpoint (EP-20).
#[derive(Debug, Default)]
pub struct RateLimiter {
    buckets: Mutex<Buckets>,
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
        let mut buckets = match self.buckets.lock() {
            Ok(guard) => guard,
            // A panicking holder poisoned the lock; the counters are just numbers and
            // refusing every request afterwards would be a worse failure than continuing.
            Err(poisoned) => poisoned.into_inner(),
        };
        evict_idle(&mut buckets, now);

        let bucket = buckets
            .by_caller
            .entry((slug.to_owned(), caller.to_owned()))
            .or_insert(Bucket {
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
        match self.buckets.lock() {
            Ok(guard) => guard.by_caller.len(),
            Err(poisoned) => poisoned.into_inner().by_caller.len(),
        }
    }

    /// Whether no caller has been seen yet.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Drops buckets that have been idle long enough to be full again: they would hand out a
/// full burst anyway, so remembering them changes nothing but the memory.
///
/// Fullness is the refilled value, not the stored one (T-2356). `tokens` is what was left at the
/// last touch and every allowed call subtracts one before storing, so a stored value is below the
/// capacity for every bucket that has ever been checked — which is why the earlier `tokens <
/// capacity` clause kept all of them and the map grew with every address that ever appeared.
///
/// A bucket that has not refilled is kept however idle it is: dropping it would give its caller a
/// fresh burst, and a caller must never buy one by waiting for somebody else's traffic to trigger
/// a sweep.
fn evict_idle(buckets: &mut Buckets, now: Instant) {
    if buckets.by_caller.len() < SWEEP_SIZE {
        return;
    }
    if buckets
        .swept
        .is_some_and(|swept| now.saturating_duration_since(swept) < SWEEP_INTERVAL)
    {
        return;
    }
    buckets.swept = Some(now);
    buckets.by_caller.retain(|_, bucket| {
        now.saturating_duration_since(bucket.last) < IDLE_EVICTION
            || bucket.tokens_at(now) < bucket.capacity
    });
}

/// The layer every endpoint surface passes: one bucket spent, the headers of the standard
/// `RateLimit` field set on the answer, 429 when the bucket is empty (EP-20, MIM0-R7).
pub async fn enforce(
    State(gateway): State<Arc<Gateway>>,
    request: Request,
    next: Next,
) -> Response<Body> {
    let path = request.uri().path().to_owned();
    // The canonical surface of a space is served from the same record a published endpoint
    // is, and is counted the same way — in the gateway's own bucket, because a space
    // declares no limit of its own (T-0813, SP-01, EP-20).
    let (endpoint, default) = match slug_of(&path) {
        Some(slug) => (gateway.resolver.resolve(slug), None),
        None => (
            space_of(&path)
                .and_then(|name| gateway.resolver.resolve_space(name))
                .map(|space| Arc::clone(&space.endpoint)),
            Some(SPACE_DEFAULT),
        ),
    };
    let Some(endpoint) = endpoint else {
        // An unknown slug costs nobody quota: the handler answers 404 (EP-03).
        return next.run(request).await;
    };
    let Some(limits) = endpoint.rate_limit.clone().or(default) else {
        return next.run(request).await;
    };

    let caller = caller_key(&request);
    let decision = gateway
        .rate_limiter
        .check(&endpoint.slug, &caller, &limits, Instant::now());
    if !decision.allowed {
        tracing::info!(slug = %endpoint.slug, "rate limit reached");
        let mut refusal = ProblemDetails::new(429, "too-many-requests", "Too Many Requests")
            .with_detail("the endpoint's rate limit is spent; retry after the seconds the RateLimit-Reset header names")
            .into_response();
        set_headers(refusal.headers_mut(), &decision);
        if let Ok(value) = HeaderValue::from_str(&decision.reset.to_string()) {
            refusal.headers_mut().insert("retry-after", value);
        }
        return refusal;
    }

    let mut response = next.run(request).await;
    set_headers(response.headers_mut(), &decision);
    response
}

/// The slug in `/api/endpoint/{slug}/…`, or nothing off the endpoint surface.
fn slug_of(path: &str) -> Option<&str> {
    path.strip_prefix("/api/endpoint/")?
        .split('/')
        .next()
        .filter(|slug| !slug.is_empty())
}

/// The space in `/cs/{space}/…`, or nothing off the canonical surface (SP-01).
fn space_of(path: &str) -> Option<&str> {
    path.strip_prefix("/cs/")?
        .split('/')
        .next()
        .filter(|space| !space.is_empty())
}

/// The bucket every space's canonical surface is counted in when nothing narrower says
/// otherwise (T-0813, EP-20).
///
/// A space carries no rate limit of its own — the manifest has no field for one, because the
/// surface is the space rather than a published product — so this is the gateway's own
/// number, the one the seeded endpoints declare. It is applied here rather than written into
/// the record, so that a space is counted however its record was built. Without it the
/// surface a member uses directly is counted only by the edge, where every anonymous caller
/// shares one bucket and one client can spend the surface for everybody.
pub const SPACE_DEFAULT: RateLimits = RateLimits {
    requests_per_minute: 600,
    burst: Some(50),
};

/// Who is spending the quota: the presented credential when there is one, the client
/// address otherwise (EP-20).
///
/// The credential is keyed by digest, never stored or logged, and the token is not
/// verified here — this is a counter, not a decision, and the handler still rejects a
/// forged token with 401. Keying it means two callers behind one NAT do not spend each
/// other's quota, which keying by address alone cannot give them.
///
/// The address comes from the LAST `X-Forwarded-For` entry, which is the peer the gateway's
/// own proxy saw. A client that sends the header itself only prepends to its own value,
/// so a spoofed entry is never the one read.
fn caller_key(request: &Request) -> String {
    if let Some(credential) = request
        .headers()
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
    {
        let digest = ring::digest::digest(&ring::digest::SHA256, credential.as_bytes());
        let hex: String = digest.as_ref()[..8]
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        return format!("credential:{hex}");
    }
    let address = request
        .headers()
        .get("x-forwarded-for")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.rsplit(',').next())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("unknown");
    format!("address:{address}")
}

/// The three standard fields of the answer (EP-20, MIM0-R7).
fn set_headers(headers: &mut axum::http::HeaderMap, decision: &Decision) {
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
    use super::{caller_key, Decision, RateLimiter};
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

    fn asking(headers: &[(&str, &str)]) -> Request {
        let mut builder = Request::builder().uri("/ngsi-ld/v1/entities?type=Device");
        for (name, value) in headers {
            builder = builder.header(*name, *value);
        }
        builder.body(Body::empty()).expect("a request")
    }

    /// T-0965: a caller who presents a credential is keyed by it, so no `X-Forwarded-For` a
    /// client writes can spend or evade another caller's quota. Two spoofed chains under one
    /// credential are the same bucket; the same chain under two credentials is two.
    #[test]
    fn a_credential_decides_the_bucket_and_no_header_moves_it() {
        let one = caller_key(&asking(&[
            ("authorization", "Bearer token-aaa"),
            ("x-forwarded-for", "10.42.0.9"),
        ]));
        let spoofed = caller_key(&asking(&[
            ("authorization", "Bearer token-aaa"),
            ("x-forwarded-for", "203.0.113.7, 10.42.0.1"),
        ]));
        assert_eq!(
            one, spoofed,
            "the header does not move a credential's bucket"
        );
        assert!(one.starts_with("credential:"), "{one}");

        let other = caller_key(&asking(&[
            ("authorization", "Bearer token-bbb"),
            ("x-forwarded-for", "10.42.0.9"),
        ]));
        assert_ne!(one, other, "two credentials are two buckets");
    }

    /// Without a credential the address decides, and it is the LAST entry of the chain: the peer
    /// the gateway's own proxy saw. A client prepends to its own value, so what it writes is
    /// never the entry read. Reaching the gateway without that proxy is refused by the
    /// NetworkPolicy, which admits only APISIX and the pipeline runner on 8080.
    #[test]
    fn an_anonymous_caller_is_keyed_by_the_peer_the_proxy_saw() {
        assert_eq!(
            caller_key(&asking(&[("x-forwarded-for", "203.0.113.7, 10.42.0.1")])),
            "address:10.42.0.1",
            "the last entry is the proxy's own observation"
        );
        assert_eq!(
            caller_key(&asking(&[("x-forwarded-for", "  10.42.0.1  ")])),
            "address:10.42.0.1",
            "a single entry is trimmed"
        );
        assert_eq!(caller_key(&asking(&[])), "address:unknown");
        assert_eq!(
            caller_key(&asking(&[("x-forwarded-for", "")])),
            "address:unknown",
            "an empty header names no peer"
        );
    }
}
