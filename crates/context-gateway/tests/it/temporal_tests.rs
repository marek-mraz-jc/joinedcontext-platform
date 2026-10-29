//! A caller's temporal window clamped to what the grants permit (T-0150, GW26).

use chrono::{DateTime, Duration, Utc};
use context_gateway::pdp::temporal::{clamp, parse, Window};

fn now() -> DateTime<Utc> {
    "2026-09-06T12:00:00Z".parse().expect("a fixed instant")
}

fn at(stamp: &str) -> DateTime<Utc> {
    stamp.parse().expect("an instant")
}

/// A sliding "last 24 hours" grant, resolved against the decision's own `now`.
#[test]
fn a_relative_grant_window_slides_with_now() {
    let window = parse("timerel=after;timeAt=P-1D", now()).expect("parses");
    assert_eq!(window.from, Some(now() - Duration::days(1)));
    assert_eq!(window.to, None);

    assert_eq!(
        parse("timerel=after;timeAt=-PT12H", now())
            .expect("parses")
            .from,
        Some(now() - Duration::hours(12)),
        "both spellings of a negative duration"
    );
    assert_eq!(
        parse("timerel=before;timeAt=P-7D", now())
            .expect("parses")
            .to,
        Some(now() - Duration::days(7))
    );
    assert_eq!(
        parse("timerel=after;timeAt=P-1Y", now()),
        None,
        "a year has no fixed length and is not a boundary"
    );
    assert_eq!(parse("timerel=sometime;timeAt=P-1D", now()), None);
    assert_eq!(
        parse("timerel=between;timeAt=P-1D", now()),
        None,
        "between needs its end"
    );
}

/// GW26: the caller asks for a year under a last-day grant and gets the last day; asks
/// for the last hour and gets the last hour untouched.
#[test]
fn the_callers_window_is_clamped_to_the_grant() {
    let grant = "timerel=after;timeAt=P-1D";

    let greedy = clamp(
        Some("timerel=between;timeAt=2025-09-06T12:00:00Z&endTimeAt=2026-09-06T12:00:00Z"),
        &[grant],
        now(),
    );
    assert_eq!(
        greedy.temporal_q.as_deref(),
        Some("timerel=between;timeAt=2026-09-05T12:00:00Z;endTimeAt=2026-09-06T12:00:00Z")
    );
    assert!(greedy.restricted);
    assert!(!greedy.empty);
    assert!(greedy.windows.is_empty(), "one window needs no filtering");

    let modest = clamp(Some("timerel=after;timeAt=PT-1H"), &[grant], now());
    assert_eq!(
        modest.temporal_q.as_deref(),
        Some("timerel=after;timeAt=2026-09-06T11:00:00Z")
    );
    assert!(
        !modest.restricted,
        "the caller asked for less than the grant"
    );

    let silent = clamp(None, &[grant], now());
    assert_eq!(
        silent.temporal_q.as_deref(),
        Some("timerel=after;timeAt=2026-09-05T12:00:00Z"),
        "no window from the caller means the grant's"
    );
    assert!(silent.restricted);

    let free = clamp(Some("timerel=after;timeAt=P-30D"), &[], now());
    assert_eq!(
        free.temporal_q.as_deref(),
        Some("timerel=after;timeAt=P-30D")
    );
    assert!(!free.restricted);
}

/// GW26: a period the grant does not reach at all is an empty answer, not a refusal —
/// and never a query without a window.
#[test]
fn a_disjoint_window_is_empty_rather_than_unbounded() {
    let outcome = clamp(
        Some("timerel=before;timeAt=2026-09-01T00:00:00Z"),
        &["timerel=after;timeAt=P-1D"],
        now(),
    );
    assert!(outcome.empty);
    assert!(outcome.restricted);
    assert_eq!(outcome.temporal_q, None);

    // Touching at one instant is still empty: the interval is half-open.
    let touching = clamp(
        Some("timerel=before;timeAt=2026-09-05T12:00:00Z"),
        &["timerel=after;timeAt=P-1D"],
        now(),
    );
    assert!(touching.empty);

    // A grant that is a retention floor: only data older than a week.
    let archive = clamp(
        Some("timerel=after;timeAt=P-1D"),
        &["timerel=before;timeAt=P-7D"],
        now(),
    );
    assert!(archive.empty, "yesterday is not older than a week");
}

/// Several grants are several windows, and the broker is given their hull.
#[test]
fn several_grant_windows_forward_the_hull() {
    let outcome = clamp(
        None,
        &[
            "timerel=between;timeAt=2026-01-01T00:00:00Z;endTimeAt=2026-02-01T00:00:00Z",
            "timerel=after;timeAt=P-1D",
        ],
        now(),
    );
    assert_eq!(
        outcome.temporal_q.as_deref(),
        Some("timerel=after;timeAt=2026-01-01T00:00:00Z"),
        "the hull: from the earliest start, open-ended because one window is"
    );
    assert_eq!(outcome.windows.len(), 2);

    // One window is the forwarded query itself.
    let single = clamp(None, &["timerel=after;timeAt=P-1D"], now());
    assert!(single.windows.is_empty());
}

#[test]
fn window_algebra() {
    let a = Window {
        from: Some(at("2026-01-01T00:00:00Z")),
        to: Some(at("2026-02-01T00:00:00Z")),
    };
    let b = Window {
        from: Some(at("2026-01-15T00:00:00Z")),
        to: None,
    };
    let overlap = a.intersect(b);
    assert_eq!(overlap.from, Some(at("2026-01-15T00:00:00Z")));
    assert_eq!(overlap.to, Some(at("2026-02-01T00:00:00Z")));
    assert!(!overlap.is_empty());
    assert!(overlap.admits(at("2026-01-20T00:00:00Z")));
    assert!(
        !overlap.admits(at("2026-02-01T00:00:00Z")),
        "half-open at the end"
    );
    assert!(
        overlap.admits(at("2026-01-15T00:00:00Z")),
        "closed at the start"
    );

    assert!(
        Window::default().intersect(a) == a,
        "nothing narrows nothing"
    );
    assert_eq!(Window::default().to_temporal_q(), None);
}
