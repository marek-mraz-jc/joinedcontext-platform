//! Edge cases of `pdp::temporal::{to_temporal_q, clamp, parse, keep_windows}` (T-1935, T-1936,
//! T-1937, T-1938; EP-26, MP-02, GW26).
//!
//! Contracts, one sentence each:
//!
//! - `parse`: a `temporalQ` becomes a window only when the vocabulary can check it, and a window
//!   nobody can check is never treated as one.
//! - `clamp`: the caller's window is intersected with the grants' and never unioned, so no caller
//!   ever reads an instant outside every grant.
//! - `to_temporal_q`: the window is written as the relation that exactly describes it, in the one
//!   format the broker reads.
//! - `keep_windows`: what lies between the grant windows was never granted, and it leaves before
//!   the answer does.
//!
//! `temporal_tests.rs` covers a sliding grant, a clamp, a disjoint pair, the hull of several and
//! the window algebra. These are the inputs around them: the durations a policy can be written
//! with, the relations it cannot, and what the filter does to a payload that is not the shape it
//! expects.

use chrono::{DateTime, Duration, Utc};
use context_gateway::pdp::temporal::{clamp, keep_windows, parse, Window};
use serde_json::json;

fn now() -> DateTime<Utc> {
    "2026-09-06T12:00:00Z".parse().expect("a fixed instant")
}

fn at(stamp: &str) -> DateTime<Utc> {
    stamp.parse().expect("an instant")
}

// --- parse (T-1937) -------------------------------------------------------------------------

#[test]
fn a_relation_the_vocabulary_does_not_know_is_no_window_at_all() {
    // CIM 009 clause 4.11 has three relations. Anything else constrains nothing that can be
    // checked, and a window nobody can check must never stand in for a grant: reading it as
    // "unconstrained" would hand over the whole history.
    for unknown in [
        "timerel=around;timeAt=2026-09-06T00:00:00Z",
        "timerel=AFTER;timeAt=2026-09-06T00:00:00Z",
        "timerel=;timeAt=2026-09-06T00:00:00Z",
        "timeAt=2026-09-06T00:00:00Z",
        "",
        "   ",
        "timerel=after",
    ] {
        assert!(
            parse(unknown, now()).is_none(),
            "{unknown:?} parsed as a window"
        );
    }
}

#[test]
fn between_without_both_ends_is_no_window() {
    assert!(parse("timerel=between;timeAt=2026-09-01T00:00:00Z", now()).is_none());
    assert!(parse("timerel=between;endTimeAt=2026-09-01T00:00:00Z", now()).is_none());
    let both = parse(
        "timerel=between;timeAt=2026-09-01T00:00:00Z;endTimeAt=2026-09-02T00:00:00Z",
        now(),
    )
    .expect("both ends parse");
    assert_eq!(both.from, Some(at("2026-09-01T00:00:00Z")));
    assert_eq!(both.to, Some(at("2026-09-02T00:00:00Z")));
}

#[test]
fn a_duration_with_no_fixed_length_is_refused_rather_than_approximated() {
    // A policy boundary that drifts by three days depending on the month is not a boundary, so
    // months and years are refused. The refusal is the safe direction: no window, no grant.
    for drifting in ["P1M", "P-1Y", "P1Y2M", "P1MT1H"] {
        assert!(
            parse(&format!("timerel=after;timeAt={drifting}"), now()).is_none(),
            "{drifting} was resolved into an instant"
        );
    }
}

#[test]
fn the_units_a_retention_window_is_written_in_all_resolve_against_now() {
    for (written, expected) in [
        ("P1D", Duration::days(1)),
        ("P1W", Duration::weeks(1)),
        ("PT1H", Duration::hours(1)),
        ("PT30M", Duration::minutes(30)),
        ("PT45S", Duration::seconds(45)),
        ("P1DT12H", Duration::days(1) + Duration::hours(12)),
        (
            "P2W3DT4H5M6S",
            Duration::weeks(2)
                + Duration::days(3)
                + Duration::hours(4)
                + Duration::minutes(5)
                + Duration::seconds(6),
        ),
    ] {
        let window = parse(&format!("timerel=after;timeAt={written}"), now())
            .unwrap_or_else(|| panic!("{written} did not parse"));
        assert_eq!(window.from, Some(now() + expected), "{written}");
    }
}

#[test]
fn a_duration_is_negative_written_either_way_round() {
    // `P-1D` and `-P1D` are both "a day ago"; a policy author uses whichever their tooling emits,
    // and reading one of them as "a day from now" would grant a window in the future.
    let a_day_ago = Some(now() - Duration::days(1));
    for written in ["P-1D", "-P1D"] {
        let window = parse(&format!("timerel=after;timeAt={written}"), now())
            .unwrap_or_else(|| panic!("{written} did not parse"));
        assert_eq!(window.from, a_day_ago, "{written}");
    }
    // And the minute sign inside the time part is read the same way.
    let window = parse("timerel=after;timeAt=PT-30M", now()).expect("PT-30M parses");
    assert_eq!(window.from, Some(now() - Duration::minutes(30)));
}

#[test]
fn a_duration_of_nothing_is_not_a_duration() {
    // `P0D` resolves to `now` exactly, which as a written grant means "from this instant", and
    // the implementation refuses it rather than letting a zero stand for an open end.
    for empty in ["P", "PT", "P0D", "PT0S", "P0DT0H"] {
        assert!(
            parse(&format!("timerel=after;timeAt={empty}"), now()).is_none(),
            "{empty} was read as a duration"
        );
    }
}

#[test]
fn a_duration_with_digits_left_over_is_refused() {
    // `P1D5` has a count with no unit after it. Accepting it would silently drop the 5.
    for broken in ["P1D5", "PT1H30", "P5", "Pfoo", "1D"] {
        assert!(
            parse(&format!("timerel=after;timeAt={broken}"), now()).is_none(),
            "{broken} was read as a duration"
        );
    }
}

#[test]
fn an_absolute_instant_is_taken_as_written_and_a_broken_one_is_not_guessed() {
    let window = parse("timerel=after;timeAt=2026-09-06T12:00:00+02:00", now()).expect("parses");
    assert_eq!(
        window.from,
        Some(at("2026-09-06T10:00:00Z")),
        "an offset is converted to UTC rather than dropped"
    );

    for broken in ["2026-09-06", "2026-09-06 12:00:00", "yesterday", "0"] {
        assert!(
            parse(&format!("timerel=after;timeAt={broken}"), now()).is_none(),
            "{broken} was read as an instant"
        );
    }
}

#[test]
fn the_parameters_are_read_however_the_query_separates_them() {
    // A `temporalQ` reaches here both as a policy field and off a URL, so `;` and `&` are both
    // separators.
    let semicolons = parse(
        "timerel=between;timeAt=2026-09-01T00:00:00Z;endTimeAt=2026-09-02T00:00:00Z",
        now(),
    );
    let ampersands = parse(
        "timerel=between&timeAt=2026-09-01T00:00:00Z&endTimeAt=2026-09-02T00:00:00Z",
        now(),
    );
    assert_eq!(semicolons, ampersands);
    assert!(semicolons.is_some());
}

#[test]
fn a_value_may_be_spaced_but_a_parameter_name_may_not_and_the_difference_fails_closed() {
    // The value is trimmed and the name is not, which a policy author writing YAML by hand can
    // trip over. It fails in the safe direction: the unmatched name leaves the relation without
    // its instant, so the grant parses to no window at all and `clamp` grants nothing rather
    // than granting everything.
    assert!(
        parse("timerel=after;timeAt= 2026-09-01T00:00:00Z ", now()).is_some(),
        "a spaced value is read"
    );
    assert!(
        parse("timerel=after; timeAt=2026-09-01T00:00:00Z", now()).is_none(),
        "a spaced name is not, and the window is none rather than open"
    );

    let clamped = clamp(None, &["timerel=after; timeAt=P-1D"], now());
    assert!(clamped.empty, "a grant nobody can read grants nothing");
    assert_eq!(clamped.temporal_q, None);
}

#[test]
fn a_parameter_the_vocabulary_does_not_know_is_ignored_rather_than_fatal() {
    let window = parse(
        "timerel=after;timeAt=2026-09-01T00:00:00Z;timeproperty=observedAt;lastN=5",
        now(),
    )
    .expect("the relation and the instant are there");
    assert_eq!(window.from, Some(at("2026-09-01T00:00:00Z")));
}

#[test]
fn the_last_value_of_a_repeated_parameter_wins_and_a_broken_one_clears_it() {
    // A caller can repeat a parameter. What must not happen is a broken second value being
    // ignored while the first one stands: the window would then be one the caller did not ask
    // for, and `timerel=between` with a cleared `endTimeAt` is no window at all.
    let repeated = parse(
        "timerel=after;timeAt=2026-09-01T00:00:00Z;timeAt=2026-09-05T00:00:00Z",
        now(),
    )
    .expect("parses");
    assert_eq!(repeated.from, Some(at("2026-09-05T00:00:00Z")));

    assert!(
        parse(
            "timerel=after;timeAt=2026-09-01T00:00:00Z;timeAt=nonsense",
            now()
        )
        .is_none(),
        "a broken repeat leaves no window rather than the earlier one"
    );
}

// --- to_temporal_q (T-1935) -----------------------------------------------------------------

#[test]
fn a_window_is_written_as_the_relation_that_exactly_describes_it() {
    let from = at("2026-09-01T00:00:00Z");
    let to = at("2026-09-02T00:00:00Z");

    assert_eq!(
        Window {
            from: Some(from),
            to: Some(to)
        }
        .to_temporal_q()
        .as_deref(),
        Some("timerel=between;timeAt=2026-09-01T00:00:00Z;endTimeAt=2026-09-02T00:00:00Z")
    );
    assert_eq!(
        Window {
            from: Some(from),
            to: None
        }
        .to_temporal_q()
        .as_deref(),
        Some("timerel=after;timeAt=2026-09-01T00:00:00Z")
    );
    assert_eq!(
        Window {
            from: None,
            to: Some(to)
        }
        .to_temporal_q()
        .as_deref(),
        Some("timerel=before;timeAt=2026-09-02T00:00:00Z")
    );
    // Open at both ends constrains nothing, and forwarding a `temporalQ` that says so would be
    // a query the broker has to answer for no reason.
    assert_eq!(Window::default().to_temporal_q(), None);
}

#[test]
fn what_is_written_is_read_back_as_the_same_window() {
    // The two halves are used together: a grant is parsed, intersected and written out again for
    // the broker. A round trip that lost a second would move a policy boundary.
    for window in [
        Window {
            from: Some(at("2026-09-01T00:00:00Z")),
            to: Some(at("2026-09-02T03:04:05Z")),
        },
        Window {
            from: Some(at("2026-09-01T00:00:01Z")),
            to: None,
        },
        Window {
            from: None,
            to: Some(at("2099-12-31T23:59:59Z")),
        },
    ] {
        let written = window
            .to_temporal_q()
            .expect("a constrained window is written");
        assert_eq!(parse(&written, now()), Some(window), "{written}");
    }
}

#[test]
fn an_empty_window_is_still_written_as_the_empty_interval_it_is() {
    // `from >= to` admits nothing. It is written as the between it is rather than as an open
    // query, because a broker asked for an impossible interval answers nothing, which is right.
    let empty = Window {
        from: Some(at("2026-09-02T00:00:00Z")),
        to: Some(at("2026-09-01T00:00:00Z")),
    };
    assert!(empty.is_empty());
    let written = empty.to_temporal_q().expect("it is still two bounds");
    assert!(written.starts_with("timerel=between"), "{written}");
}

// --- clamp (T-1936) -------------------------------------------------------------------------

#[test]
fn no_grant_narrowing_time_leaves_the_callers_own_query_exactly_as_written() {
    // A relative `timeAt` the caller sent is the caller's business: resolving it here would
    // answer a different question from the one asked, and the grants did not ask us to.
    let clamped = clamp(Some("timerel=after;timeAt=P-7D"), &[], now());
    assert_eq!(
        clamped.temporal_q.as_deref(),
        Some("timerel=after;timeAt=P-7D")
    );
    assert!(!clamped.restricted);
    assert!(!clamped.empty);
    assert!(clamped.windows.is_empty());
}

#[test]
fn no_grant_and_no_caller_window_constrains_nothing() {
    let clamped = clamp(None, &[], now());
    assert_eq!(clamped.temporal_q, None);
    assert!(!clamped.restricted && !clamped.empty && clamped.windows.is_empty());
}

#[test]
fn a_caller_who_asked_for_nothing_gets_the_grant_and_is_told_it_was_narrowed() {
    let clamped = clamp(None, &["timerel=after;timeAt=P-1D"], now());
    assert_eq!(
        clamped.temporal_q.as_deref(),
        Some("timerel=after;timeAt=2026-09-05T12:00:00Z")
    );
    assert!(
        clamped.restricted,
        "the answer is not the whole history the caller asked for"
    );
    assert!(clamped.windows.is_empty(), "one window needs no filtering");
}

#[test]
fn a_grant_the_vocabulary_cannot_parse_narrows_nothing_and_is_dropped() {
    // A grant written with a relation or a duration this cannot check leaves no window. What
    // must not happen is it becoming an unbounded window and granting the whole history.
    let clamped = clamp(
        None,
        &["timerel=around;timeAt=P-1D", "timerel=after;timeAt=P-1D"],
        now(),
    );
    assert_eq!(
        clamped.temporal_q.as_deref(),
        Some("timerel=after;timeAt=2026-09-05T12:00:00Z"),
        "only the grant that could be checked is forwarded"
    );

    // And when no grant parses at all, nothing is granted rather than everything.
    let none = clamp(None, &["timerel=around;timeAt=P-1D"], now());
    assert!(none.empty, "an uncheckable grant is not an open one");
    assert!(none.restricted);
    assert_eq!(none.temporal_q, None);
}

#[test]
fn identical_grant_windows_are_one_window_and_need_no_filtering() {
    // Two policies commonly say the same thing. Left as two, the hull would equal the window and
    // the filter would run over every instance for nothing.
    let same = "timerel=after;timeAt=P-1D";
    let clamped = clamp(None, &[same, same, same], now());
    assert!(clamped.windows.is_empty(), "deduplicated to one");
    assert_eq!(
        clamped.temporal_q.as_deref(),
        Some("timerel=after;timeAt=2026-09-05T12:00:00Z")
    );
}

#[test]
fn a_hull_with_one_open_end_is_open_and_the_windows_still_filter_the_gaps() {
    // One grant runs to the end of time; the hull therefore has no upper bound, and the gap
    // between the two windows is what `keep_windows` removes afterwards.
    let clamped = clamp(
        None,
        &[
            "timerel=between;timeAt=2026-01-01T00:00:00Z;endTimeAt=2026-02-01T00:00:00Z",
            "timerel=after;timeAt=2026-06-01T00:00:00Z",
        ],
        now(),
    );
    assert_eq!(
        clamped.temporal_q.as_deref(),
        Some("timerel=after;timeAt=2026-01-01T00:00:00Z"),
        "the hull is open at the end because one grant is"
    );
    assert_eq!(
        clamped.windows.len(),
        2,
        "the gap between them is not granted"
    );
    assert!(!clamped.empty);
}

#[test]
fn a_caller_inside_one_grant_is_not_told_the_answer_was_narrowed() {
    // R22: `restricted` says the caller got less than they asked for. Saying so when they did
    // not would train a reader to ignore it.
    let clamped = clamp(
        Some("timerel=between;timeAt=2026-09-06T00:00:00Z;endTimeAt=2026-09-06T06:00:00Z"),
        &["timerel=after;timeAt=P-1D"],
        now(),
    );
    assert!(
        !clamped.restricted,
        "the caller's own window was inside the grant"
    );
    assert_eq!(
        clamped.temporal_q.as_deref(),
        Some("timerel=between;timeAt=2026-09-06T00:00:00Z;endTimeAt=2026-09-06T06:00:00Z")
    );
}

#[test]
fn a_caller_window_touching_a_grant_only_at_its_edge_is_empty() {
    // The interval is half-open: a caller asking for exactly the instant a grant ends gets
    // nothing, and the answer is an empty list rather than a refusal (GW26).
    let clamped = clamp(
        Some("timerel=after;timeAt=2026-09-02T00:00:00Z"),
        &["timerel=before;timeAt=2026-09-02T00:00:00Z"],
        now(),
    );
    assert!(
        clamped.empty,
        "the windows meet at a point and a point admits nothing"
    );
    assert!(clamped.restricted);
    assert_eq!(clamped.temporal_q, None);
    assert!(clamped.windows.is_empty());
}

#[test]
fn a_callers_unparseable_window_is_read_as_no_window_and_the_grant_still_holds() {
    // A caller sending nonsense must not widen anything: the grant is what is forwarded.
    let clamped = clamp(
        Some("timerel=whenever"),
        &["timerel=after;timeAt=P-1D"],
        now(),
    );
    assert_eq!(
        clamped.temporal_q.as_deref(),
        Some("timerel=after;timeAt=2026-09-05T12:00:00Z")
    );
    assert!(!clamped.empty);
}

// --- keep_windows (T-1938) ------------------------------------------------------------------

fn windows() -> Vec<Window> {
    vec![
        Window {
            from: Some(at("2026-01-01T00:00:00Z")),
            to: Some(at("2026-02-01T00:00:00Z")),
        },
        Window {
            from: Some(at("2026-06-01T00:00:00Z")),
            to: Some(at("2026-07-01T00:00:00Z")),
        },
    ]
}

#[test]
fn an_instance_in_the_gap_between_two_grants_leaves_the_answer() {
    let mut payload = json!({
        "id": "urn:ngsi-ld:AirQualityObserved:bbsk.sk:kraj:s-1",
        "type": "AirQualityObserved",
        "pm10": [
            { "type": "Property", "value": 1, "observedAt": "2026-01-15T00:00:00Z" },
            { "type": "Property", "value": 2, "observedAt": "2026-03-15T00:00:00Z" },
            { "type": "Property", "value": 3, "observedAt": "2026-06-15T00:00:00Z" }
        ]
    });
    keep_windows(&mut payload, &windows());
    let kept = payload["pm10"]
        .as_array()
        .expect("the instances are a list");
    assert_eq!(kept.len(), 2);
    assert_eq!(kept[0]["value"], 1);
    assert_eq!(kept[1]["value"], 3, "the March instance was never granted");
}

#[test]
fn an_empty_window_list_is_a_hull_that_is_the_permission_and_nothing_is_dropped() {
    let original = json!({
        "id": "urn:ngsi-ld:X:a:b:1",
        "type": "X",
        "pm10": [{ "type": "Property", "value": 1, "observedAt": "2099-01-01T00:00:00Z" }]
    });
    let mut payload = original.clone();
    keep_windows(&mut payload, &[]);
    assert_eq!(payload, original);
}

#[test]
fn an_instance_with_no_stamp_at_all_is_kept_rather_than_silently_dropped() {
    // The gateway filters what it can place in time. An instance carrying no `observedAt`,
    // `modifiedAt` or `createdAt` cannot be placed, and dropping it would remove data the grant
    // never excluded; it is the broker's to narrow, having been sent the hull.
    let mut payload = json!({
        "id": "urn:ngsi-ld:X:a:b:1",
        "type": "X",
        "pm10": [
            { "type": "Property", "value": 1 },
            { "type": "Property", "value": 2, "observedAt": "2026-03-15T00:00:00Z" }
        ]
    });
    keep_windows(&mut payload, &windows());
    let kept = payload["pm10"].as_array().expect("a list");
    assert_eq!(kept.len(), 1);
    assert_eq!(kept[0]["value"], 1);
}

#[test]
fn the_stamps_are_read_in_the_order_observed_modified_created() {
    // An instance carrying several is placed by the one that says when it was observed, which is
    // what a temporal grant is written about.
    let mut payload = json!({
        "id": "urn:ngsi-ld:X:a:b:1",
        "type": "X",
        "pm10": [{
            "type": "Property", "value": 1,
            "observedAt": "2026-01-15T00:00:00Z",
            "modifiedAt": "2026-03-15T00:00:00Z",
            "createdAt": "2026-03-15T00:00:00Z"
        }]
    });
    keep_windows(&mut payload, &windows());
    assert_eq!(payload["pm10"].as_array().map(Vec::len), Some(1));

    // And one with no `observedAt` falls back to `modifiedAt`.
    let mut fallback = json!({
        "id": "urn:ngsi-ld:X:a:b:1",
        "type": "X",
        "pm10": [{ "type": "Property", "value": 1, "modifiedAt": "2026-03-15T00:00:00Z" }]
    });
    keep_windows(&mut fallback, &windows());
    assert_eq!(fallback["pm10"].as_array().map(Vec::len), Some(0));
}

#[test]
fn a_stamp_that_is_not_a_stamp_leaves_the_instance_unplaceable_and_therefore_kept() {
    let mut payload = json!({
        "id": "urn:ngsi-ld:X:a:b:1",
        "type": "X",
        "pm10": [{ "type": "Property", "value": 1, "observedAt": "not a date" }]
    });
    keep_windows(&mut payload, &windows());
    assert_eq!(payload["pm10"].as_array().map(Vec::len), Some(1));
}

#[test]
fn the_entity_members_are_never_mistaken_for_history() {
    // `id`, `type` and anything under `@` are the entity, not its instances. A filter that
    // walked them would corrupt the entity rather than narrow it.
    let mut payload = json!({
        "@context": ["https://example.org/ctx.jsonld"],
        "id": "urn:ngsi-ld:X:a:b:1",
        "type": "X",
        "pm10": [{ "type": "Property", "value": 1, "observedAt": "2026-03-15T00:00:00Z" }]
    });
    keep_windows(&mut payload, &windows());
    assert_eq!(payload["id"], "urn:ngsi-ld:X:a:b:1");
    assert_eq!(payload["type"], "X");
    assert_eq!(payload["@context"].as_array().map(Vec::len), Some(1));
    assert_eq!(payload["pm10"].as_array().map(Vec::len), Some(0));
}

#[test]
fn every_entity_of_a_list_answer_is_filtered_and_not_only_the_first() {
    let mut payload = json!([
        { "id": "urn:ngsi-ld:X:a:b:1", "type": "X",
          "pm10": [{ "type": "Property", "value": 1, "observedAt": "2026-03-15T00:00:00Z" }] },
        { "id": "urn:ngsi-ld:X:a:b:2", "type": "X",
          "pm10": [{ "type": "Property", "value": 2, "observedAt": "2026-03-15T00:00:00Z" }] }
    ]);
    keep_windows(&mut payload, &windows());
    for entity in payload.as_array().expect("a list") {
        assert_eq!(entity["pm10"].as_array().map(Vec::len), Some(0), "{entity}");
    }
}

#[test]
fn a_payload_that_is_not_an_entity_is_left_alone_rather_than_panicking() {
    // A broker can answer with a problem document, a number or a string; the filter runs over
    // whatever came back, so it has to survive all of them.
    for original in [
        json!(null),
        json!(7),
        json!("text"),
        json!({"title": "Not Found"}),
    ] {
        let mut payload = original.clone();
        keep_windows(&mut payload, &windows());
        assert_eq!(payload, original);
    }
}

#[test]
fn an_attribute_that_is_not_a_list_of_instances_is_untouched() {
    // A normalized (non-temporal) entity has objects here, not lists. Filtering one would be
    // filtering something that was never history.
    let original = json!({
        "id": "urn:ngsi-ld:X:a:b:1",
        "type": "X",
        "pm10": { "type": "Property", "value": 1, "observedAt": "2026-03-15T00:00:00Z" }
    });
    let mut payload = original.clone();
    keep_windows(&mut payload, &windows());
    assert_eq!(payload, original);
}
