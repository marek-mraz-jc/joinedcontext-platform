//! Clamping a caller's temporal window to what the grants permit (T-0150, GW26).
//!
//! A grant like "the last 24 hours" is a moving window, so it is written relative
//! (`timerel=after;timeAt=P-1D`) and resolved against the same `now` the rest of the
//! decision uses. The caller's window is intersected with it, never unioned: asking for a
//! year of history under a 24-hour grant returns 24 hours, and asking for last week under
//! a grant that ends last month returns an empty list rather than a refusal, because the
//! request was well formed and the answer is genuinely nothing (GW26).
//!
//! Several grants with several windows cannot be one interval. Their union is forwarded
//! as its hull, which is a superset the broker can execute, and the gateway then drops
//! the instances that fall in the gaps — the hull is never the answer, only the query.

use chrono::{DateTime, Duration, Utc};

/// A half-open interval `[from, to)`; an open end is the beginning or the end of time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Window {
    /// The earliest instant the window admits.
    pub from: Option<DateTime<Utc>>,
    /// The first instant it no longer admits.
    pub to: Option<DateTime<Utc>>,
}

impl Window {
    /// Whether the window admits nothing at all.
    pub fn is_empty(&self) -> bool {
        matches!((self.from, self.to), (Some(from), Some(to)) if from >= to)
    }

    /// Whether an instant falls inside the window.
    pub fn admits(&self, at: DateTime<Utc>) -> bool {
        self.from.is_none_or(|from| at >= from) && self.to.is_none_or(|to| at < to)
    }

    /// The overlap of two windows, which is what a clamp is.
    pub fn intersect(self, other: Window) -> Window {
        Window {
            from: max_bound(self.from, other.from),
            to: min_bound(self.to, other.to),
        }
    }

    /// The window as a `temporalQ`, or `None` when it constrains nothing.
    pub fn to_temporal_q(self) -> Option<String> {
        match (self.from, self.to) {
            (Some(from), Some(to)) => Some(format!(
                "timerel=between;timeAt={};endTimeAt={}",
                stamp(from),
                stamp(to)
            )),
            (Some(from), None) => Some(format!("timerel=after;timeAt={}", stamp(from))),
            (None, Some(to)) => Some(format!("timerel=before;timeAt={}", stamp(to))),
            (None, None) => None,
        }
    }
}

/// What the clamp decided (GW26).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Clamped {
    /// The `temporalQ` to forward, `None` when nothing constrains the query.
    pub temporal_q: Option<String>,
    /// The windows an instance must fall into, when the forwarded hull is wider than the
    /// union it stands for. Empty means the broker's answer needs no filtering.
    pub windows: Vec<Window>,
    /// The caller asked for a period the grants do not reach at all: the answer is an
    /// empty list, not a refusal (GW26).
    pub empty: bool,
    /// Whether the caller's own window was narrowed (R22).
    pub restricted: bool,
}

/// Intersects the caller's window with each grant's, resolved against `now` (GW26).
pub fn clamp(caller: Option<&str>, grants: &[&str], now: DateTime<Utc>) -> Clamped {
    let asked = caller.and_then(|q| parse(q, now)).unwrap_or_default();

    // No grant narrows time, so the caller's own window stands as it was written: a
    // relative `timeAt` the caller sent is the caller's business, not ours to resolve.
    if grants.is_empty() {
        return Clamped {
            temporal_q: caller.map(str::to_owned),
            ..Clamped::default()
        };
    }

    let mut windows: Vec<Window> = grants
        .iter()
        .filter_map(|grant| parse(grant, now))
        .map(|granted| granted.intersect(asked))
        .filter(|window| !window.is_empty())
        .collect();
    windows.sort_by_key(|window| (window.from, window.to));
    windows.dedup();

    // Every grant window ends before the caller's begins, or begins after it ends. The
    // request is legal and its answer is nothing.
    if windows.is_empty() {
        return Clamped {
            empty: true,
            restricted: true,
            ..Clamped::default()
        };
    }

    // One window is exactly forwardable. Several are not: their hull is the query and the
    // windows themselves are the filter.
    let hull = Window {
        from: windows.iter().map(|window| window.from).min().flatten(),
        to: to_bound(windows.iter().map(|window| window.to)),
    };
    Clamped {
        temporal_q: hull.to_temporal_q(),
        windows: if windows.len() > 1 {
            windows
        } else {
            Vec::new()
        },
        empty: false,
        restricted: hull != asked,
    }
}

/// The window of a `temporalQ`, resolved against `now` (CIM 009 clause 4.11).
///
/// `timeAt` is either an absolute instant or an ISO 8601 duration, which a grant uses to
/// say "the last day" without being rewritten every day.
pub fn parse(temporal_q: &str, now: DateTime<Utc>) -> Option<Window> {
    let mut timerel = None;
    let mut time_at = None;
    let mut end_time_at = None;
    for part in temporal_q.split([';', '&']) {
        match part.split_once('=') {
            Some(("timerel", value)) => timerel = Some(value.trim()),
            Some(("timeAt", value)) => time_at = instant(value.trim(), now),
            Some(("endTimeAt", value)) => end_time_at = instant(value.trim(), now),
            _ => {}
        }
    }

    match timerel? {
        "after" => Some(Window {
            from: Some(time_at?),
            to: None,
        }),
        "before" => Some(Window {
            from: None,
            to: Some(time_at?),
        }),
        "between" => Some(Window {
            from: Some(time_at?),
            to: Some(end_time_at?),
        }),
        // A relation the vocabulary does not know constrains nothing that can be checked,
        // and a window nobody can check must not be treated as one.
        _ => None,
    }
}

/// An absolute instant, or one relative to `now` when the grant is written as a duration.
fn instant(value: &str, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
    if let Ok(absolute) = DateTime::parse_from_rfc3339(value) {
        return Some(absolute.with_timezone(&Utc));
    }
    duration(value).map(|offset| now + offset)
}

/// An ISO 8601 duration as a signed offset: `P-1D` and `-P1D` are both "a day ago".
///
/// Only the units a retention window is written in are understood — days, hours, minutes,
/// seconds and weeks. A duration carrying months or years has no fixed length and is
/// refused rather than approximated, because a policy boundary that drifts by three days
/// depending on the month is not a boundary.
fn duration(value: &str) -> Option<Duration> {
    let (sign, rest) = match value.strip_prefix('-') {
        Some(rest) => (-1i32, rest),
        None => (1i32, value),
    };
    let rest = rest.strip_prefix('P')?;
    let (date, time) = match rest.split_once('T') {
        Some((date, time)) => (date, Some(time)),
        None => (rest, None),
    };

    let mut total = Duration::zero();
    let mut sign: i32 = sign;
    let mut digits = String::new();
    for (part, units) in [(date, "WD"), (time.unwrap_or_default(), "HMS")] {
        for character in part.chars() {
            match character {
                '-' if digits.is_empty() => sign = -sign,
                '0'..='9' => digits.push(character),
                unit if units.contains(unit) => {
                    let count: i64 = digits.parse().ok()?;
                    digits.clear();
                    total += match unit {
                        'W' => Duration::try_weeks(count)?,
                        'D' => Duration::try_days(count)?,
                        'H' => Duration::try_hours(count)?,
                        'M' => Duration::try_minutes(count)?,
                        _ => Duration::try_seconds(count)?,
                    };
                }
                // A month or a year has no fixed length, and anything else is not a
                // duration at all.
                _ => return None,
            }
        }
    }

    (digits.is_empty() && total != Duration::zero()).then_some(total * sign)
}

fn stamp(at: DateTime<Utc>) -> String {
    at.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

/// The later of two lower bounds; an absent bound is the beginning of time.
fn max_bound(a: Option<DateTime<Utc>>, b: Option<DateTime<Utc>>) -> Option<DateTime<Utc>> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a.max(b)),
        (bound, None) | (None, bound) => bound,
    }
}

/// The earlier of two upper bounds; an absent bound is the end of time.
fn min_bound(a: Option<DateTime<Utc>>, b: Option<DateTime<Utc>>) -> Option<DateTime<Utc>> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (bound, None) | (None, bound) => bound,
    }
}

/// The upper bound of a hull: one open end makes the whole hull open.
fn to_bound(bounds: impl Iterator<Item = Option<DateTime<Utc>>>) -> Option<DateTime<Utc>> {
    let mut highest: Option<DateTime<Utc>> = None;
    for bound in bounds {
        let at = bound?;
        highest = Some(highest.map_or(at, |current| current.max(at)));
    }
    highest
}

/// The `observedAt` an NGSI-LD temporal instance carries, when it carries one.
fn instant_of(instance: &serde_json::Value) -> Option<DateTime<Utc>> {
    for member in ["observedAt", "modifiedAt", "createdAt"] {
        if let Some(stamp) = instance.get(member).and_then(serde_json::Value::as_str) {
            return DateTime::parse_from_rfc3339(stamp)
                .ok()
                .map(|at| at.with_timezone(&Utc));
        }
    }
    None
}

/// Drops the attribute instances that fall in the gaps of the forwarded hull (GW26).
///
/// The hull of several grant windows is a query the broker can execute; it is not the
/// permission. What lies between the windows was never granted, and it leaves here.
pub fn keep_windows(payload: &mut serde_json::Value, windows: &[Window]) {
    if windows.is_empty() {
        return;
    }
    match payload {
        serde_json::Value::Array(entities) => entities
            .iter_mut()
            .for_each(|entity| keep_windows(entity, windows)),
        serde_json::Value::Object(members) => {
            for (name, value) in members.iter_mut() {
                // `id`, `type` and `@context` are the entity, not its history.
                if name.starts_with('@') || name == "id" || name == "type" {
                    continue;
                }
                if let serde_json::Value::Array(instances) = value {
                    instances.retain(|instance| {
                        instant_of(instance)
                            .is_none_or(|at| windows.iter().any(|window| window.admits(at)))
                    });
                }
            }
        }
        _ => {}
    }
}
