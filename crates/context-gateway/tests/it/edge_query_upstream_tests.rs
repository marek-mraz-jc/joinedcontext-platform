//! Edge cases of `query::upstream` (T-1945, EP-26, MP-02, GW2).
//!
//! Contract, in one sentence: the query the broker receives is the caller's parameters that are on
//! the allow list, plus the constraint set, which replaces every dimension a grant can narrow —
//! so no parameter the caller wrote can select entities, attributes, places or times that the PDP
//! did not put there.
//!
//! The allow list is the whole of the defence: `type`, `attrs`, `q`, `georel`, `geometry`,
//! `coordinates`, `timerel`, `timeAt` and `endTimeAt` are never carried over from the request, and
//! a parameter nobody thought about is not carried over either, because the list names what
//! survives rather than what is refused.

use context_gateway::pdp::evaluator::Constraints;
use context_gateway::query::{parse, upstream};
use std::collections::BTreeSet;

fn set(values: &[&str]) -> BTreeSet<String> {
    values.iter().map(|value| (*value).to_owned()).collect()
}

fn sent(query: &str, constraints: &Constraints, fallback: &[String]) -> Vec<(String, String)> {
    parse(&upstream(&parse(query), constraints, fallback))
}

fn values<'a>(sent: &'a [(String, String)], name: &str) -> Vec<&'a str> {
    sent.iter()
        .filter(|(key, _)| key == name)
        .map(|(_, value)| value.as_str())
        .collect()
}

/// GW2: every selector the caller wrote is dropped, whatever it says. What selects upstream is
/// the constraint set and nothing else.
#[test]
fn no_selector_the_caller_wrote_reaches_the_broker() {
    let asked = "type=Secret&attrs=operatorPhone&q=operatorPhone!=null&georel=within\
                 &geometry=Polygon&coordinates=%5B%5B%5B0%2C0%5D%5D%5D&timerel=after\
                 &timeAt=1970-01-01T00%3A00%3A00Z&endTimeAt=2100-01-01T00%3A00%3A00Z\
                 &tenant=other&NGSILD-Tenant=other&geoproperty=location&orderBy=operatorPhone";
    let constraints = Constraints {
        types: set(&["AirQualityObserved"]),
        attrs: set(&["pm10"]),
        ..Constraints::default()
    };

    let sent = sent(asked, &constraints, &[]);
    assert_eq!(values(&sent, "type"), vec!["AirQualityObserved"]);
    assert_eq!(values(&sent, "attrs"), vec!["pm10"]);
    for dropped in [
        "q",
        "georel",
        "geometry",
        "coordinates",
        "timerel",
        "timeAt",
        "endTimeAt",
        "tenant",
        "NGSILD-Tenant",
        "geoproperty",
        "orderBy",
    ] {
        assert!(
            values(&sent, dropped).is_empty(),
            "{dropped} reached the broker"
        );
    }
    assert!(
        !upstream(&parse(asked), &constraints, &[]).contains("Secret"),
        "not even as a value"
    );
}

/// The parameters that only shape an answer ride through unchanged, in the order they were sent.
#[test]
fn the_shaping_parameters_ride_through_in_order() {
    let asked = "limit=5&offset=10&count=true&options=keyValues&lang=sk&pick=pm10&omit=pm25\
                 &scopeQ=%2Fovzdusie&datasetId=urn%3Angsi-ld%3ADataset%3A1&format=geojson";
    let sent = sent(asked, &Constraints::default(), &[]);
    let names: Vec<&str> = sent.iter().map(|(name, _)| name.as_str()).collect();

    assert_eq!(
        names,
        vec![
            "limit",
            "offset",
            "count",
            "options",
            "lang",
            "pick",
            "omit",
            "scopeQ",
            "datasetId",
            "format"
        ]
    );
    assert_eq!(values(&sent, "scopeQ"), vec!["/ovzdusie"]);
}

/// GW26: one request cannot make the shared broker read an unbounded history.
#[test]
fn last_n_is_capped_and_a_smaller_one_is_left_alone() {
    for (asked, expected) in [
        ("lastN=10", "10"),
        ("lastN=1000", "1000"),
        ("lastN=1001", "1000"),
        ("lastN=99999999", "1000"),
    ] {
        let sent = sent(asked, &Constraints::default(), &[]);
        assert_eq!(values(&sent, "lastN"), vec![expected], "{asked}");
    }

    // A `lastN` that is not a number is forwarded as written and refused by the broker, rather
    // than being read as a cap this gateway invented.
    let odd = sent("lastN=-1", &Constraints::default(), &[]);
    assert_eq!(values(&odd, "lastN"), vec!["-1"]);
}

/// A grant's single-string geo and temporal queries are split back into the parameters NGSI-LD
/// spreads them over, and the caller's own are already gone.
#[test]
fn a_compound_constraint_is_split_into_its_parameters() {
    let constraints = Constraints {
        geo_q: Some(
            "georel=within;geometry=Polygon;coordinates=[[[19.1,48.7],[19.2,48.7],[19.2,48.8],[19.1,48.7]]]"
                .to_owned(),
        ),
        temporal_q: Some("timerel=between;timeAt=2026-01-01T00:00:00Z;endTimeAt=2026-02-01T00:00:00Z".to_owned()),
        ..Constraints::default()
    };

    let sent = sent("georel=near", &constraints, &[]);
    assert_eq!(values(&sent, "georel"), vec!["within"]);
    assert_eq!(values(&sent, "geometry"), vec!["Polygon"]);
    assert_eq!(values(&sent, "timerel"), vec!["between"]);
    assert_eq!(values(&sent, "timeAt"), vec!["2026-01-01T00:00:00Z"]);
    assert_eq!(values(&sent, "endTimeAt"), vec!["2026-02-01T00:00:00Z"]);
    assert_eq!(
        values(&sent, "coordinates"),
        vec!["[[[19.1,48.7],[19.2,48.7],[19.2,48.8],[19.1,48.7]]]"]
    );
}

/// EP-09: the fallback selector is the one of last resort and never widens a query that already
/// selects something.
#[test]
fn the_fallback_types_are_used_only_when_nothing_else_selects() {
    let fallback = vec!["AirQualityObserved".to_owned(), "Depot".to_owned()];

    let empty = sent("limit=5", &Constraints::default(), &fallback);
    assert_eq!(values(&empty, "type"), vec!["AirQualityObserved,Depot"]);

    for narrowing in [
        Constraints {
            types: set(&["Vehicle"]),
            ..Constraints::default()
        },
        Constraints {
            attrs: set(&["pm10"]),
            ..Constraints::default()
        },
        Constraints {
            q: Some("pm10>1".to_owned()),
            ..Constraints::default()
        },
        Constraints {
            geo_q: Some("georel=within;geometry=Polygon;coordinates=[]".to_owned()),
            ..Constraints::default()
        },
    ] {
        let sent = sent("limit=5", &narrowing, &fallback);
        assert!(
            !values(&sent, "type").contains(&"AirQualityObserved,Depot"),
            "the fallback widened a query that already selected: {narrowing:?}"
        );
    }
}

/// Nothing granted and no fallback is a query with only the caller's shaping parameters — the
/// NGSI-LD surface then answers the `400` CIM 009 5.7.2 asks for, rather than asking the broker
/// for everything.
#[test]
fn an_empty_constraint_set_selects_nothing_by_itself() {
    assert_eq!(upstream(&parse(""), &Constraints::default(), &[]), "");
    assert_eq!(
        upstream(&parse("limit=5"), &Constraints::default(), &[]),
        "limit=5"
    );
}

/// Every name and value is percent-encoded on the way out, so a value carrying `&` or `=` cannot
/// become a second parameter.
#[test]
fn a_value_cannot_smuggle_a_second_parameter() {
    let smuggled = "limit=5%26type%3DSecret";
    let constraints = Constraints {
        types: set(&["Vehicle"]),
        ..Constraints::default()
    };

    let raw = upstream(&parse(smuggled), &constraints, &[]);
    assert!(raw.contains("limit=5%26type%3DSecret"), "{raw}");
    let sent = parse(&raw);
    assert_eq!(
        values(&sent, "type"),
        vec!["Vehicle"],
        "one type, the granted one"
    );
    assert_eq!(values(&sent, "limit"), vec!["5&type=Secret"]);
}

/// A caller who repeats a shaping parameter gets it repeated: the broker's own precedence rule
/// decides, and nothing here silently picks one.
#[test]
fn a_repeated_shaping_parameter_is_forwarded_as_sent() {
    let sent = sent("limit=1&limit=9999", &Constraints::default(), &[]);
    assert_eq!(values(&sent, "limit"), vec!["1", "9999"]);
}

/// The granted lists are joined in one stable order, so the same grants always produce the same
/// upstream query and a cache in front of the broker cannot be split by ordering.
#[test]
fn the_granted_lists_are_joined_in_a_stable_order() {
    let constraints = Constraints {
        types: set(&["Vehicle", "AirQualityObserved", "Depot"]),
        attrs: set(&["pm25", "location", "pm10"]),
        ..Constraints::default()
    };

    let once = upstream(&parse("limit=1"), &constraints, &[]);
    assert_eq!(once, upstream(&parse("limit=1"), &constraints, &[]));
    let sent = parse(&once);
    assert_eq!(
        values(&sent, "type"),
        vec!["AirQualityObserved,Depot,Vehicle"]
    );
    assert_eq!(values(&sent, "attrs"), vec!["location,pm10,pm25"]);
}

/// An empty value is not a value: a caller who sends `limit=` gets it forwarded as it was, and it
/// selects nothing here either.
#[test]
fn an_empty_parameter_value_is_forwarded_as_written() {
    let sent = sent("limit=&count=", &Constraints::default(), &[]);
    assert_eq!(values(&sent, "limit"), vec![""]);
    assert_eq!(values(&sent, "count"), vec![""]);
}

/// A parameter name the allow list does not carry is dropped whatever its case or shape, because
/// the list is compared exactly.
#[test]
fn a_parameter_name_is_matched_exactly_against_the_allow_list() {
    let sent = sent(
        "LIMIT=5&Limit=5&limit%20=5&li%6Dit=9&scopeq=%2Fx&SCOPEQ=%2Fx",
        &Constraints::default(),
        &[],
    );
    assert_eq!(
        values(&sent, "limit"),
        vec!["9"],
        "only the decoded `limit` is one"
    );
    assert!(values(&sent, "scopeq").is_empty());
    assert!(values(&sent, "SCOPEQ").is_empty());
}

/// The caller's `q` is never merged with the grants': what goes out is the constraint set's own,
/// which already carries every policy's filter folded with its scopes (R12, R13).
#[test]
fn only_the_constraint_sets_q_is_sent() {
    let constraints = Constraints {
        q: Some("(pm10>1;%22%2Fovzdusie%22)".to_owned()),
        ..Constraints::default()
    };
    let sent = sent("q=operatorPhone!=null", &constraints, &[]);

    assert_eq!(values(&sent, "q"), vec!["(pm10>1;%22%2Fovzdusie%22)"]);
}
