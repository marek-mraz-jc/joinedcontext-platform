//! Edge cases of `translators::ogc::datetime` (T-1953, T-2347, EP-34, EP-26, MP-02).
//!
//! Contract, in one sentence: an RFC 3339 instant becomes a closed NGSI-LD interval of itself, an
//! interval becomes `after`, `before` or `between` with the caller's own spelling of each end, and
//! anything else — a value that is not RFC 3339, an interval that ends before it starts, one open
//! at both ends — is a named `400` rather than a filter the broker cannot honour.
//!
//! The two ends are compared as moments, not as text (T-2347). `2026-01-02T00:00:00+05:00` is
//! an hour *before* `2026-01-01T23:00:00Z` and sorts after it as a string, so the textual
//! comparison refused intervals that were perfectly ordered and accepted intervals that were not.

use context_gateway::translators::ogc::datetime;

fn parameters(raw: &str) -> Vec<(String, String)> {
    datetime(raw).unwrap_or_else(|error| panic!("{raw:?} is a datetime: {error}"))
}

fn refused(raw: &str) -> String {
    let error = datetime(raw).expect_err(&format!("{raw:?} is not a datetime"));
    assert_eq!(error.parameter, "datetime", "{raw:?}");
    error.detail
}

/// T-2347: the ends of an interval are moments. An offset that makes the text sort the other way
/// round must not turn a well-ordered interval into a `400`.
#[test]
fn the_ends_of_an_interval_are_compared_as_moments_not_as_text() {
    // 2026-01-01T19:00:00Z to 2026-01-01T23:00:00Z: four hours, in order.
    assert_eq!(
        parameters("2026-01-02T00:00:00+05:00/2026-01-01T23:00:00Z"),
        vec![
            ("timerel".to_owned(), "between".to_owned()),
            ("timeAt".to_owned(), "2026-01-02T00:00:00+05:00".to_owned()),
            ("endTimeAt".to_owned(), "2026-01-01T23:00:00Z".to_owned()),
        ],
        "the caller's own spelling of each end, and no refusal"
    );

    // And the other direction: 2026-01-02T04:00:00Z to 2026-01-02T01:00:00Z ends before it starts,
    // though the text says otherwise.
    assert!(!refused("2026-01-01T23:00:00-05:00/2026-01-02T01:00:00Z").is_empty());
}

/// An instant is the observations of that instant, which NGSI-LD writes as a closed interval of
/// itself because it has no `at` relation.
#[test]
fn an_instant_becomes_a_closed_interval_of_itself() {
    assert_eq!(
        parameters("2026-09-18T09:00:00Z"),
        vec![
            ("timerel".to_owned(), "between".to_owned()),
            ("timeAt".to_owned(), "2026-09-18T09:00:00Z".to_owned()),
            ("endTimeAt".to_owned(), "2026-09-18T09:00:00Z".to_owned()),
        ]
    );
}

/// An interval open at one end is `after` or `before`, in both spellings OGC allows.
#[test]
fn an_interval_open_at_one_end_is_after_or_before() {
    for raw in ["2026-09-18T09:00:00Z/..", "2026-09-18T09:00:00Z/"] {
        assert_eq!(
            parameters(raw),
            vec![
                ("timerel".to_owned(), "after".to_owned()),
                ("timeAt".to_owned(), "2026-09-18T09:00:00Z".to_owned()),
            ],
            "{raw}"
        );
    }
    for raw in ["../2026-09-18T09:00:00Z", "/2026-09-18T09:00:00Z"] {
        assert_eq!(
            parameters(raw),
            vec![
                ("timerel".to_owned(), "before".to_owned()),
                ("timeAt".to_owned(), "2026-09-18T09:00:00Z".to_owned()),
            ],
            "{raw}"
        );
    }
}

/// An interval open at both ends selects everything, which is what omitting the parameter does —
/// and a client that sent it believes it narrowed something, so it is told.
#[test]
fn an_interval_open_at_both_ends_is_refused() {
    for raw in ["../..", "/"] {
        assert!(refused(raw).contains("omit"), "{raw}");
    }
}

/// A value that is not an RFC 3339 instant is refused by name, with the value in the message so a
/// client can show its user which end was wrong.
#[test]
fn a_value_that_is_not_rfc3339_is_refused_and_named() {
    for raw in [
        "",
        "yesterday",
        "2026-09-18",
        "2026-09-18T09:00:00",
        "2026-13-01T00:00:00Z",
        "2026-09-31T00:00:00Z",
        "2026-09-18T25:00:00Z",
        "2026-09-18T09:00:00+25:00",
        " 2026-09-18T09:00:00Z",
        "2026-09-18T09:00:00Z ",
        "2026-09-18T09:00:00Z/2026-09-19T09:00:00Z/2026-09-20T09:00:00Z",
    ] {
        refused(raw);
    }
}

/// A fraction of a second and a leap second are instants a real client sends.
#[test]
fn a_fraction_of_a_second_is_an_instant() {
    assert_eq!(
        parameters("2026-09-18T09:00:00.123456789Z")[1].1,
        "2026-09-18T09:00:00.123456789Z",
        "the caller's own precision is forwarded, not a rounded one"
    );
    assert_eq!(parameters("2016-12-31T23:59:60Z").len(), 3, "a leap second");
}

/// An offset is kept exactly as the caller wrote it: the broker is given the caller's own
/// spelling, not a normalised one this gateway invented.
#[test]
fn an_offset_is_forwarded_as_written() {
    for raw in [
        "2026-09-18T09:00:00+02:00",
        "2026-09-18T09:00:00-05:00",
        "2026-09-18T09:00:00+00:00",
    ] {
        assert_eq!(parameters(raw)[1].1, raw, "{raw}");
    }
}

/// An interval of one instant to itself is a closed interval, not an error.
#[test]
fn an_interval_of_an_instant_to_itself_is_allowed() {
    assert_eq!(
        parameters("2026-09-18T09:00:00Z/2026-09-18T09:00:00Z"),
        parameters("2026-09-18T09:00:00Z"),
        "the same three parameters an instant produces"
    );
}

/// The same instant written in two time zones is one moment, so an interval between them is empty
/// rather than inverted.
#[test]
fn the_same_moment_in_two_zones_is_not_an_inverted_interval() {
    assert_eq!(
        parameters("2026-09-18T11:00:00+02:00/2026-09-18T09:00:00Z").len(),
        3,
        "the same moment at both ends"
    );
}

/// An interval that plainly ends before it starts is refused, offsets or not.
#[test]
fn an_interval_that_ends_before_it_starts_is_refused() {
    for raw in [
        "2026-09-19T09:00:00Z/2026-09-18T09:00:00Z",
        "2026-09-18T09:00:01Z/2026-09-18T09:00:00Z",
        "2100-01-01T00:00:00Z/1970-01-01T00:00:00Z",
    ] {
        assert!(refused(raw).contains("ends before"), "{raw}");
    }
}

/// The parameters come back in the order the handler appends them, every time.
#[test]
fn the_parameters_come_back_in_one_order() {
    let sent = parameters("2026-09-18T09:00:00Z/2026-09-19T09:00:00Z");
    let names: Vec<&str> = sent.iter().map(|(name, _)| name.as_str()).collect();
    assert_eq!(names, vec!["timerel", "timeAt", "endTimeAt"]);
}

/// A value carrying something that is not a timestamp at all is refused without any of it
/// reaching the broker: no `timerel`, no half a filter.
#[test]
fn nothing_of_a_refused_value_becomes_a_parameter() {
    for raw in [
        "2026-09-18T09:00:00Z/yesterday",
        "yesterday/2026-09-18T09:00:00Z",
        "2026-09-18T09:00:00Z%2F..",
        "2026-09-18T09:00:00Z;timerel=after",
    ] {
        assert!(datetime(raw).is_err(), "{raw}");
    }
}
