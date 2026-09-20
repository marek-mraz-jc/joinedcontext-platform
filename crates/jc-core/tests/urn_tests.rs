//! T-0108: deterministic NGSI-LD URN parser, validator and formatter (ADR 001, PF-10, PF-42, PF-43).

use jc_core::{Error, Urn, UrnError};

/// Every URN example of docs/Architecture/03-domain-model.md section 3.
const DOC_EXAMPLES: &[&str] = &[
    "urn:ngsi-ld:AirQualityObserved:banskabystrica.sk:ovzdusie:station-radvan-01",
    "urn:ngsi-ld:TrafficFlowObserved:banskabystrica.sk:doprava:detector-namestie-snp",
    "urn:ngsi-ld:WasteContainer:odpady-bb.sk:kontajnery:c-77492",
    "urn:ngsi-ld:Policy:banskabystrica.sk:admin:public-air-quality",
    "urn:ngsi-ld:ScopeDefinition:banskabystrica.sk:admin:geo-sk-bb-radvan",
];

fn parse(s: &str) -> Urn {
    s.parse::<Urn>()
        .unwrap_or_else(|e| panic!("`{s}` should parse: {e}"))
}

#[test]
fn doc_examples_parse_and_round_trip_byte_identically() {
    for s in DOC_EXAMPLES {
        let urn = parse(s);
        assert_eq!(&urn.to_string(), s, "stringification must be deterministic");
        assert_eq!(urn.to_string().parse::<Urn>(), Ok(urn));
    }
}

#[test]
fn segments_are_exposed_verbatim() {
    let urn = parse(DOC_EXAMPLES[0]);
    assert_eq!(urn.entity_type(), "AirQualityObserved");
    assert_eq!(urn.org_domain(), "banskabystrica.sk");
    assert_eq!(urn.space(), "ovzdusie");
    assert_eq!(urn.local_id(), "station-radvan-01");
}

#[test]
fn upper_case_prefix_parses_and_is_normalised_to_lower_case() {
    let urn = parse("URN:NGSI-LD:AirQualityObserved:banskabystrica.sk:ovzdusie:station-01");
    assert_eq!(
        urn.to_string(),
        "urn:ngsi-ld:AirQualityObserved:banskabystrica.sk:ovzdusie:station-01"
    );
}

#[test]
fn legacy_uuid_id_without_four_segments_is_rejected() {
    // The classic NGSI-LD form `urn:ngsi-ld:{Type}:{uuid}` has two NSS segments, not four (R34).
    let err = "urn:ngsi-ld:AirQualityObserved:550e8400-e29b-41d4-a716-446655440000"
        .parse::<Urn>()
        .expect_err("a two-segment UUID id must be rejected");
    assert!(matches!(
        err,
        Error::Urn {
            reason: UrnError::InvalidSegmentCount { got: 2 },
            ..
        }
    ));
}

#[test]
fn a_uuid_is_allowed_as_the_local_id() {
    // Architecture/03 section 3 bans random UUIDs only OUTSIDE the final segment.
    let urn = parse(
        "urn:ngsi-ld:AirQualityObserved:banskabystrica.sk:ovzdusie:550e8400-e29b-41d4-a716-446655440000",
    );
    assert_eq!(urn.local_id(), "550e8400-e29b-41d4-a716-446655440000");
}

#[test]
fn a_uuid_in_the_org_domain_segment_is_rejected() {
    let err = "urn:ngsi-ld:AirQualityObserved:550e8400-e29b-41d4-a716-446655440000:ovzdusie:s1"
        .parse::<Urn>()
        .expect_err("a UUID is not a verified internet domain");
    assert!(matches!(
        err,
        Error::Urn {
            reason: UrnError::InvalidOrgDomain { .. },
            ..
        }
    ));
}

#[test]
fn wrong_segment_counts_are_rejected() {
    for (s, got) in [
        (
            "urn:ngsi-ld:AirQualityObserved:banskabystrica.sk:ovzdusie",
            3,
        ),
        (
            "urn:ngsi-ld:AirQualityObserved:banskabystrica.sk:ovzdusie:a:b",
            5,
        ),
    ] {
        let err = s.parse::<Urn>().expect_err("segment count must be 4");
        assert_eq!(
            err,
            Error::Urn {
                urn: s.to_string(),
                reason: UrnError::InvalidSegmentCount { got },
            }
        );
    }
}

#[test]
fn empty_segments_are_rejected() {
    let err = "urn:ngsi-ld:AirQualityObserved::ovzdusie:station-01"
        .parse::<Urn>()
        .expect_err("an empty segment must be rejected");
    assert!(matches!(
        err,
        Error::Urn {
            reason: UrnError::EmptySegment { index: 1 },
            ..
        }
    ));
}

#[test]
fn a_missing_prefix_is_rejected_without_panicking_on_multibyte_input() {
    for s in ["", "urn:", "ovzdusie", "urn:ngsi-ldž:A:b.sk:c:d", "žžžžžžž"] {
        let err = s.parse::<Urn>().expect_err("prefix must be urn:ngsi-ld:");
        assert!(matches!(
            err,
            Error::Urn {
                reason: UrnError::InvalidPrefix,
                ..
            }
        ));
    }
}

#[test]
fn new_rejects_every_malformed_segment() {
    // (type, domain, space, localId, the segment that must be blamed)
    let cases: &[(&str, &str, &str, &str, &str)] = &[
        ("airQualityObserved", "bb.sk", "ovzdusie", "s1", "type"),
        ("Air-Quality", "bb.sk", "ovzdusie", "s1", "type"),
        ("A", "bb.sk", "ovzdusie", "s1", "type"),
        ("AirQualityObserved", "BB.sk", "ovzdusie", "s1", "domain"),
        (
            "AirQualityObserved",
            "bb.sk:8080",
            "ovzdusie",
            "s1",
            "domain",
        ),
        (
            "AirQualityObserved",
            "bb.sk/path",
            "ovzdusie",
            "s1",
            "domain",
        ),
        (
            "AirQualityObserved",
            "localhost",
            "ovzdusie",
            "s1",
            "domain",
        ),
        ("AirQualityObserved", "bb.sk.", "ovzdusie", "s1", "domain"),
        ("AirQualityObserved", "bb..sk", "ovzdusie", "s1", "domain"),
        ("AirQualityObserved", "bb.sk", "Ovzdusie", "s1", "space"),
        ("AirQualityObserved", "bb.sk", "ovz_dusie", "s1", "space"),
        ("AirQualityObserved", "bb.sk", "-ovzdusie", "s1", "space"),
        (
            "AirQualityObserved",
            "bb.sk",
            "ovzdusie",
            "sta:tion",
            "localId",
        ),
        (
            "AirQualityObserved",
            "bb.sk",
            "ovzdusie",
            "sta tion",
            "localId",
        ),
        ("AirQualityObserved", "bb.sk", "ovzdusie", "", "localId"),
    ];
    for (t, d, sp, l, which) in cases {
        let err = match Urn::new(t, d, sp, l) {
            Ok(urn) => panic!("{t}/{d}/{sp}/{l} must be rejected ({which}), minted {urn}"),
            Err(e) => e,
        };
        let blamed = match err {
            Error::Urn {
                reason: UrnError::InvalidEntityType { .. },
                ..
            } => "type",
            Error::Urn {
                reason: UrnError::InvalidOrgDomain { .. },
                ..
            } => "domain",
            Error::Urn {
                reason: UrnError::InvalidSpace { .. },
                ..
            } => "space",
            Error::Urn {
                reason: UrnError::InvalidLocalId { .. },
                ..
            } => "localId",
            other => panic!("unexpected error for {t}/{d}/{sp}/{l}: {other}"),
        };
        assert_eq!(&blamed, which, "wrong segment blamed for {t}/{d}/{sp}/{l}");
    }
}

#[test]
fn new_accepts_the_documented_examples() {
    let urn = Urn::new("WasteContainer", "odpady-bb.sk", "kontajnery", "c-77492")
        .expect("documented example must mint");
    assert_eq!(
        urn.to_string(),
        "urn:ngsi-ld:WasteContainer:odpady-bb.sk:kontajnery:c-77492"
    );
}

#[test]
fn matches_tenant_is_the_pf43_admission_check() {
    let urn = parse(DOC_EXAMPLES[0]);
    assert!(urn.matches_tenant("banskabystrica.sk", "ovzdusie"));
    // foreign organization domain
    assert!(!urn.matches_tenant("odpady-bb.sk", "ovzdusie"));
    // sibling space of the same organization
    assert!(!urn.matches_tenant("banskabystrica.sk", "doprava"));
    // both wrong
    assert!(!urn.matches_tenant("odpady-bb.sk", "kontajnery"));
}

#[test]
fn id_pattern_prefix_escapes_the_domain_dots() {
    let urn = parse(DOC_EXAMPLES[0]);
    assert_eq!(
        urn.id_pattern_prefix(),
        r"^urn:ngsi-ld:AirQualityObserved:banskabystrica\.sk:ovzdusie:.*$"
    );
    let re = regex::Regex::new(&urn.id_pattern_prefix()).expect("prefix must be a valid regex");
    assert!(re.is_match(DOC_EXAMPLES[0]));
    // The escaped dot must not match an arbitrary character (banskabystricaXsk).
    assert!(!re.is_match("urn:ngsi-ld:AirQualityObserved:banskabystricaXsk:ovzdusie:station-01"));
    // A sibling space must not route to this registration (R33).
    assert!(!re.is_match(DOC_EXAMPLES[1]));
}

#[test]
fn serde_round_trips_through_a_json_string_and_rejects_garbage() {
    let urn = parse(DOC_EXAMPLES[0]);
    let json = serde_json::to_string(&urn).expect("serialize");
    assert_eq!(json, format!("\"{}\"", DOC_EXAMPLES[0]));
    assert_eq!(
        serde_json::from_str::<Urn>(&json).expect("deserialize"),
        urn
    );
    assert!(serde_json::from_str::<Urn>("\"urn:ngsi-ld:Bad\"").is_err());
    assert!(serde_json::from_str::<Urn>("42").is_err());
}

/// CC-78: a preview's ids carry its render prefix in the `{space}` segment and nowhere else.
#[test]
fn apply_render_prefix_changes_space_segment() {
    assert_eq!(
        jc_core::apply_render_prefix("urn:ngsi-ld:Vehicle:hel.fi:helsinki-bikes:v:1", "ws-demo-"),
        "urn:ngsi-ld:Vehicle:hel.fi:ws-demo-helsinki-bikes:v:1",
        "a local id that has colons keeps them"
    );
    assert_eq!(
        jc_core::apply_render_prefix(r"^urn:ngsi-ld:Vehicle:hel\.fi:helsinki:.*$", "ws-demo-"),
        r"^urn:ngsi-ld:Vehicle:hel\.fi:ws-demo-helsinki:.*$",
        "an anchored pattern moves the same way"
    );
}

#[test]
fn apply_render_prefix_does_not_change_other_segments_or_prefix_twice() {
    let once = jc_core::apply_render_prefix("urn:ngsi-ld:Endpoint:hel.fi:helsinki:all", "ws-a-");
    assert_eq!(once, "urn:ngsi-ld:Endpoint:hel.fi:ws-a-helsinki:all");
    assert_eq!(jc_core::apply_render_prefix(&once, "ws-a-"), once);
}

#[test]
fn apply_render_prefix_on_non_urn_returns_unchanged() {
    for text in [
        "",
        "helsinki",
        "urn:ngsi-ld:Type",
        "urn:ngsi-ld:T:hel.fi::x",
        "https://hel.fi/x:y",
    ] {
        assert_eq!(jc_core::apply_render_prefix(text, "ws-a-"), text);
    }
    assert_eq!(
        jc_core::apply_render_prefix("urn:ngsi-ld:T:hel.fi:s:x", ""),
        "urn:ngsi-ld:T:hel.fi:s:x"
    );
}
