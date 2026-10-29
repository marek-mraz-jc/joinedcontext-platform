//! Edge cases of `middleware::rate_limit::RateLimiter::check` (T-1904, EP-26, MP-02).
//!
//! Contract, in one sentence: one call spends one token of the bucket that belongs to exactly
//! this `(slug, caller)` pair and to no other, the bucket refills at `requestsPerMinute` and
//! never past `burst`, and what comes back says truthfully how much is left and how long a
//! refused caller has to wait — because that decision is what a caller is told in the
//! `RateLimit-*` headers and what a 429 hangs on (EP-20, MIM0-R7, OPS-35).
//!
//! `rate_limit_tests.rs` covers the burst, the refill, two callers, the ceiling and the
//! surface's own headers. These are the edges: the smallest and largest limits the manifest
//! can declare, a clock that does not move, keys that merely look alike, a bucket that is
//! spent exactly to the last token, and the eviction that keeps the map from growing with
//! every address that ever appeared.

use context_gateway::middleware::rate_limit::RateLimiter;
use jc_core::kinds::RateLimits;
use std::time::{Duration, Instant};

const SLUG: &str = "d7m2xq9vkt4zc6wrb8shj5nfp3";
const CALLER: &str = "sha256:7f3a91c4";

fn limits(per_minute: u32, burst: Option<u32>) -> RateLimits {
    RateLimits {
        requests_per_minute: per_minute,
        burst,
    }
}

/// Spends `count` requests at one instant and returns the last decision.
fn spend(
    limiter: &RateLimiter,
    count: usize,
    limits: &RateLimits,
    now: Instant,
) -> context_gateway::middleware::rate_limit::Decision {
    let mut last = limiter.check(SLUG, CALLER, limits, now);
    for _ in 1..count {
        last = limiter.check(SLUG, CALLER, limits, now);
    }
    last
}

/// A bucket is spent to its last token and the next call is refused: the boundary, and the one
/// past it.
#[test]
fn the_last_token_is_spent_and_the_call_after_it_is_refused() {
    let limiter = RateLimiter::new();
    let limits = limits(60, Some(5));
    let now = Instant::now();

    for spent in 1..=5 {
        let decision = limiter.check(SLUG, CALLER, &limits, now);
        assert!(decision.allowed, "call {spent} of the burst");
        assert_eq!(decision.remaining, 5 - spent, "call {spent}");
        assert_eq!(decision.reset, 0, "nothing to wait for while tokens remain");
    }

    let refused = limiter.check(SLUG, CALLER, &limits, now);
    assert!(!refused.allowed);
    assert_eq!(refused.remaining, 0);
    assert_eq!(refused.reset, 1, "one second buys one token at 60 a minute");
    assert_eq!(refused.limit, 60, "the limit reported is the steady rate");
}

/// The smallest limit a manifest may declare. One request a minute means one token, and the
/// wait reported is the whole minute rather than a rounded-down zero.
#[test]
fn one_request_a_minute_is_one_token_and_a_minute_of_waiting() {
    let limiter = RateLimiter::new();
    let limits = limits(1, None);
    let now = Instant::now();

    let first = limiter.check(SLUG, CALLER, &limits, now);
    assert!(first.allowed);
    assert_eq!(first.remaining, 0);

    let refused = limiter.check(SLUG, CALLER, &limits, now);
    assert!(!refused.allowed);
    assert_eq!(refused.reset, 60);

    // A caller that waits exactly what it was told is never refused for being a fraction early.
    let after = limiter.check(
        SLUG,
        CALLER,
        &limits,
        now + Duration::from_secs(refused.reset),
    );
    assert!(after.allowed, "the wait it was told is enough");
}

/// A burst smaller than one is not a bucket: the capacity floor keeps one token, so a
/// misdeclared `burst: 0` limits to one request rather than refusing every one of them.
#[test]
fn a_burst_of_zero_still_leaves_one_token_rather_than_refusing_everything() {
    let limiter = RateLimiter::new();
    let limits = limits(60, Some(0));
    let now = Instant::now();

    assert!(limiter.check(SLUG, CALLER, &limits, now).allowed);
    assert!(!limiter.check(SLUG, CALLER, &limits, now).allowed);
}

/// A limit of zero requests a minute is refused by `RateLimits::validate` before it reaches a
/// manifest. If one ever did, the bucket still holds its one floor token and then refills at
/// nothing — and the wait reported is a minute rather than an infinity or a zero that would
/// spin a client.
#[test]
fn a_rate_of_zero_hands_out_its_floor_token_and_then_says_wait_a_minute() {
    assert!(
        limits(0, None).validate().is_err(),
        "a manifest cannot declare it"
    );

    let limiter = RateLimiter::new();
    let limits = limits(0, None);
    let now = Instant::now();

    assert!(limiter.check(SLUG, CALLER, &limits, now).allowed);
    let refused = limiter.check(SLUG, CALLER, &limits, now);
    assert!(!refused.allowed);
    assert_eq!(
        refused.reset, 60,
        "never zero, which a client would spin on"
    );
    assert!(
        !limiter
            .check(SLUG, CALLER, &limits, now + Duration::from_secs(3_600))
            .allowed,
        "and nothing refills it",
    );
}

/// The largest limit a manifest can declare: nothing overflows, and the remaining count is
/// reported as the whole number of requests it really is.
#[test]
fn the_largest_declarable_limit_neither_overflows_nor_lies_about_what_is_left() {
    let limiter = RateLimiter::new();
    let limits = limits(u32::MAX, Some(u32::MAX));
    let now = Instant::now();

    let first = limiter.check(SLUG, CALLER, &limits, now);
    assert!(first.allowed);
    assert_eq!(first.limit, u32::MAX);
    assert!(
        first.remaining >= u32::MAX - 2,
        "one spent of a very large bucket"
    );

    // And a very long wait does not push the bucket past its capacity.
    let later = limiter.check(SLUG, CALLER, &limits, now + Duration::from_secs(86_400));
    assert!(later.allowed);
    assert!(
        later.remaining > 0,
        "a full bucket after a day, not an emptied one"
    );
}

/// A clock that does not move between calls is the normal case inside one burst, and the one a
/// monotonic clock guarantees: `saturating_duration_since` means a `now` that is *earlier*
/// than the bucket's last touch adds nothing rather than draining or overfilling it.
#[test]
fn a_clock_that_stands_still_or_steps_back_neither_refills_nor_drains_the_bucket() {
    let limiter = RateLimiter::new();
    let limits = limits(60, Some(3));
    let now = Instant::now();

    assert!(limiter.check(SLUG, CALLER, &limits, now).allowed);
    let backwards = now - Duration::from_secs(600);
    let decision = limiter.check(SLUG, CALLER, &limits, backwards);
    assert!(decision.allowed);
    assert_eq!(
        decision.remaining, 1,
        "ten minutes backwards is not ten minutes of refill",
    );
}

/// One bucket per `(slug, caller)`, and the pair is compared as it is written: two callers
/// whose keys merely look alike never spend each other's quota, and neither do two endpoints.
#[test]
fn a_bucket_belongs_to_exactly_one_slug_and_one_caller() {
    let limiter = RateLimiter::new();
    let limits = limits(60, Some(1));
    let now = Instant::now();

    assert!(limiter.check(SLUG, CALLER, &limits, now).allowed);
    assert!(!limiter.check(SLUG, CALLER, &limits, now).allowed);

    for other in [
        "sha256:7f3a91c5",
        "SHA256:7F3A91C4",
        " sha256:7f3a91c4",
        "sha256:7f3a91c4 ",
        "sha256:7f3a91c",
        "sha256:7f3a91c40",
        "",
    ] {
        assert!(
            limiter.check(SLUG, other, &limits, now).allowed,
            "{other:?} is another caller with a bucket of its own",
        );
    }

    for other_slug in [
        "z3f8kq5vmt7xc2wrb9shd4njp6",
        &SLUG.to_uppercase(),
        &format!("{SLUG} "),
        "",
    ] {
        assert!(
            limiter.check(other_slug, CALLER, &limits, now).allowed,
            "{other_slug:?} is another endpoint with a bucket of its own",
        );
    }
}

/// What the caller is told has to be true: after spending the whole burst, the wait reported
/// really is enough, and a second before it is not.
#[test]
fn the_wait_reported_is_the_wait_that_works() {
    let limiter = RateLimiter::new();
    let limits = limits(12, Some(2));
    let now = Instant::now();

    spend(&limiter, 2, &limits, now);
    let refused = limiter.check(SLUG, CALLER, &limits, now);
    assert!(!refused.allowed);
    assert_eq!(
        refused.reset, 5,
        "one token of twelve a minute is five seconds"
    );

    assert!(
        !limiter
            .check(
                SLUG,
                CALLER,
                &limits,
                now + Duration::from_secs(refused.reset - 1)
            )
            .allowed,
        "a second early is still refused",
    );
    assert!(
        limiter
            .check(
                SLUG,
                CALLER,
                &limits,
                now + Duration::from_secs(refused.reset)
            )
            .allowed,
        "and the second it named is enough",
    );
}

/// A refusal costs nothing: a caller that keeps knocking while empty does not push its own
/// wait further out, because a refused call spends no token.
#[test]
fn knocking_while_refused_does_not_make_the_wait_longer() {
    let limiter = RateLimiter::new();
    let limits = limits(60, Some(1));
    let now = Instant::now();

    assert!(limiter.check(SLUG, CALLER, &limits, now).allowed);
    let first = limiter.check(SLUG, CALLER, &limits, now);
    for _ in 0..50 {
        let again = limiter.check(SLUG, CALLER, &limits, now);
        assert!(!again.allowed);
        assert_eq!(
            again.reset, first.reset,
            "the wait does not grow with knocking"
        );
    }
}

/// The map is not allowed to grow with every address that ever appeared, and `evict_idle` is
/// what stops it: a bucket that has been idle long enough to be full again would hand out a full
/// burst whether it is remembered or not (OPS-35, T-2356).
///
/// It dropped nothing until this case landed. A bucket's `tokens` is the value at its last touch,
/// and an allowed call subtracts one before storing, so the old `tokens < capacity` clause held
/// for every bucket that had ever been checked. The refill is computed on read and never written
/// back, which is exactly what that clause was trying to read.
#[test]
fn eviction_drops_a_bucket_that_has_refilled_to_its_burst() {
    let limiter = RateLimiter::new();
    let limits = limits(60, Some(2));
    let start = Instant::now();

    for caller in 0..1_200 {
        limiter.check(SLUG, &format!("caller-{caller}"), &limits, start);
    }
    let before = limiter.len();
    assert!(
        before > 1_024,
        "the map is large enough for eviction to run"
    );

    // Long past `IDLE_EVICTION`, so every one of those buckets has refilled to its burst, and
    // one more call to trigger the sweep.
    let later = start + Duration::from_secs(3_600);
    limiter.check(SLUG, "somebody-new", &limits, later);

    assert_eq!(
        limiter.len(),
        1,
        "every idle, refilled bucket is gone and the caller that swept them is what is left"
    );
}

/// A bucket is dropped only when dropping it changes nothing: the caller gets the same number of
/// calls whether it was remembered or not.
#[test]
fn a_caller_whose_bucket_was_dropped_gets_no_more_than_an_idle_caller_would() {
    let limits = limits(60, Some(2));
    let start = Instant::now();
    let later = start + Duration::from_secs(3_600);

    let swept = RateLimiter::new();
    for caller in 0..1_200 {
        swept.check(SLUG, &format!("caller-{caller}"), &limits, start);
    }
    swept.check(SLUG, "somebody-new", &limits, later);
    let dropped = swept.check(SLUG, "caller-7", &limits, later);

    let kept = RateLimiter::new();
    kept.check(SLUG, "caller-7", &limits, start);
    let remembered = kept.check(SLUG, "caller-7", &limits, later);

    assert_eq!(dropped.allowed, remembered.allowed);
    assert_eq!(
        dropped.remaining, remembered.remaining,
        "a dropped bucket and a kept one that refilled allow the same calls"
    );
    assert_eq!(
        remembered.remaining, 1,
        "refilled to its burst, less this one"
    );
}

/// A caller that is still out of tokens keeps its bucket, whatever the sweep finds around it: the
/// sweep must never be a way to buy a fresh burst by waiting for somebody else's traffic.
#[test]
fn the_sweep_never_hands_a_drained_caller_a_fresh_burst() {
    let limiter = RateLimiter::new();
    // One request a minute, a burst of sixty: a drained bucket needs an hour to fill, so it is
    // still drained at the moment the idle sweep runs.
    let slow = limits(1, Some(60));
    let start = Instant::now();

    for _ in 0..60 {
        limiter.check(SLUG, "heavy", &slow, start);
    }
    let refused = limiter.check(SLUG, "heavy", &slow, start);
    assert!(!refused.allowed, "the burst is spent");

    let busy = limits(600, Some(600));
    for caller in 0..1_200 {
        limiter.check(SLUG, &format!("caller-{caller}"), &busy, start);
    }
    let later = start + Duration::from_secs(600);
    limiter.check(SLUG, "somebody-new", &busy, later);

    let again = limiter.check(SLUG, "heavy", &slow, later);
    assert_eq!(
        again.remaining, 9,
        "ten minutes buys ten of the sixty tokens it spent, one of which this call took — \
         a swept bucket would have answered 59"
    );
}

/// The counter is in this process and behind one lock: many threads spending one bucket spend
/// it exactly once each, so a burst of `n` is `n` allowed calls and not one more.
#[test]
fn a_burst_spent_from_many_threads_at_once_is_still_exactly_the_burst() {
    let limiter = std::sync::Arc::new(RateLimiter::new());
    let limits = limits(60, Some(50));
    let now = Instant::now();

    let allowed = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let threads: Vec<_> = (0..8)
        .map(|_| {
            let limiter = std::sync::Arc::clone(&limiter);
            let allowed = std::sync::Arc::clone(&allowed);
            let limits = limits.clone();
            std::thread::spawn(move || {
                for _ in 0..25 {
                    if limiter.check(SLUG, CALLER, &limits, now).allowed {
                        allowed.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    }
                }
            })
        })
        .collect();
    for thread in threads {
        thread.join().expect("the thread finishes");
    }

    assert_eq!(
        allowed.load(std::sync::atomic::Ordering::Relaxed),
        50,
        "two hundred callers of a fifty-token bucket, and fifty get through",
    );
}

/// A bucket that has never been seen starts full, so the first request of a new caller is
/// never the one that is refused.
#[test]
fn a_caller_that_has_never_called_starts_with_a_full_bucket() {
    let limiter = RateLimiter::new();
    let limits = limits(60, Some(10));
    assert!(limiter.is_empty());

    let first = limiter.check(SLUG, "brand-new", &limits, Instant::now());
    assert!(first.allowed);
    assert_eq!(first.remaining, 9);
    assert_eq!(limiter.len(), 1);
}
