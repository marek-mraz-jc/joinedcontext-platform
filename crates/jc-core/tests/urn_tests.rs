//! T-0108, T-3081: NGSI-LD URN parser, validator and formatter (ADR-N-041, PF-10, PF-42, PF-43).
//! Any NGSI-LD URN of a PascalCase type is valid; the prefixed shape is one of them.

use jc_core::{EntityRef, Error, Urn, UrnError};

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
    assert_eq!(urn.org_domain(), Some("banskabystrica.sk"));
    assert_eq!(urn.space(), Some("ovzdusie"));
    assert_eq!(urn.local_id(), "station-radvan-01");
    assert_eq!(urn.id(), "banskabystrica.sk:ovzdusie:station-radvan-01");
}

#[test]
fn upper_case_prefix_parses_and_is_normalised_to_lower_case() {
    let urn = parse("URN:NGSI-LD:AirQualityObserved:banskabystrica.sk:ovzdusie:station-01");
    assert_eq!(
        urn.to_string(),
        "urn:ngsi-ld:AirQualityObserved:banskabystrica.sk:ovzdusie:station-01"
    );
}

/// ADR-N-041: ids that arrive with their own shape are valid and kept byte for byte, with no
/// prefix read into them.
#[test]
fn unprefixed_ids_are_valid_and_carry_no_prefix() {
    for (s, id) in [
        // FIWARE style, as a source publishes it
        ("urn:ngsi-ld:WeatherObserved:Helsinki-001", "Helsinki-001"),
        // the classic NGSI-LD UUID form
        (
            "urn:ngsi-ld:AirQualityObserved:550e8400-e29b-41d4-a716-446655440000",
            "550e8400-e29b-41d4-a716-446655440000",
        ),
        // more or fewer than three segments after the type
        (
            "urn:ngsi-ld:AirQualityObserved:banskabystrica.sk:ovzdusie",
            "banskabystrica.sk:ovzdusie",
        ),
        ("urn:ngsi-ld:Vehicle:fleet:7:wheel:2", "fleet:7:wheel:2"),
        // a segment that is no domain: not prefixed, still valid
        (
            "urn:ngsi-ld:Device:550e8400:ovzdusie:s1",
            "550e8400:ovzdusie:s1",
        ),
        // RFC 8141 characters beyond the prefixed local id's charset
        (
            "urn:ngsi-ld:Building:Bratislava/Stare-Mesto;%C5%BD@1",
            "Bratislava/Stare-Mesto;%C5%BD@1",
        ),
    ] {
        let urn = parse(s);
        assert_eq!(urn.to_string(), s);
        assert_eq!(urn.id(), id);
        assert_eq!(urn.prefixed(), None, "{s} is not in the prefixed shape");
        assert_eq!((urn.org_domain(), urn.space()), (None, None));
        assert_eq!(urn.local_id(), id);
    }
}

#[test]
fn a_uuid_is_allowed_as_the_local_id_of_a_prefixed_urn() {
    let urn = parse(
        "urn:ngsi-ld:AirQualityObserved:banskabystrica.sk:ovzdusie:550e8400-e29b-41d4-a716-446655440000",
    );
    assert_eq!(urn.local_id(), "550e8400-e29b-41d4-a716-446655440000");
}

/// PF-43: what is still refused, each with the reason a person can act on.
#[test]
fn what_is_no_ngsi_ld_urn_is_refused_with_its_reason() {
    for (s, expected) in [
        (
            "urn:ngsi-ld:AirQualityObserved",
            "nothing after the entity type",
        ),
        (
            "urn:ngsi-ld:AirQualityObserved:",
            "nothing after the entity type",
        ),
        ("urn:ngsi-ld:airQualityObserved:x-1", "invalid entity type"),
        ("urn:ngsi-ld:A:x-1", "invalid entity type"),
        ("urn:ngsi-ld:Device:sta tion", "RFC 8141"),
        ("urn:ngsi-ld:Device:d#1", "RFC 8141"),
        ("urn:ngsi-ld:Device:%zz", "RFC 8141"),
        ("urn:ngsi-ld:Device:/leading-slash", "RFC 8141"),
    ] {
        let err = s.parse::<Urn>().expect_err(s);
        assert!(err.to_string().contains(expected), "{s}: {err}");
    }
    let long = format!("urn:ngsi-ld:Device:{}", "a".repeat(257));
    let err = long.parse::<Urn>().expect_err("over 256 characters");
    assert!(
        matches!(
            err,
            Error::Urn {
                reason: UrnError::InvalidId { .. },
                ..
            }
        ),
        "{err}"
    );
    let edge = format!("urn:ngsi-ld:Device:{}", "a".repeat(256));
    assert_eq!(parse(&edge).id().len(), 256);
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

/// ADR-N-041 §3.1: the same URN in two spaces is two entities, and the URN names neither space.
#[test]
fn the_same_urn_in_two_spaces_is_two_entity_refs() {
    let urn = parse("urn:ngsi-ld:WeatherObserved:Helsinki-001");
    let here = EntityRef::new("helsinki", urn.clone()).expect("a space");
    let there = EntityRef::new("helsinki-kpi", urn.clone()).expect("a space");
    assert_ne!(here, there);
    assert_eq!(here.urn(), there.urn());
    assert_eq!(
        here.to_string(),
        "/cs/helsinki/ngsi-ld/v1/entities/urn:ngsi-ld:WeatherObserved:Helsinki-001"
    );
    // A prefixed URN naming one space can be an entity of another: the space is the ref's.
    let prefixed = parse(DOC_EXAMPLES[0]);
    let copied = EntityRef::new("ovzdusie-kpi", prefixed).expect("a space");
    assert_eq!(copied.space(), "ovzdusie-kpi");
    assert!(EntityRef::new("Not A Space", urn).is_err());
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
