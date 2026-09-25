//! Edge cases of `translators::sta::{thing, datastreams, series_filter, filter_to_q}`
//! (T-1954, T-1955, T-1956, T-1957; EP-12, EP-13, T-0438).
//!
//! Contracts, one sentence each:
//!
//! - `thing`: an entity without a usable id is no `Thing` at all, and the one it builds carries
//!   every non-measurement attribute in `properties` and nothing the projection removed.
//! - `datastreams`: one datastream per numeric measurement, addressed by `{urn}/{attribute}`, and
//!   nothing else becomes a series that would answer `result: null` forever.
//! - `series_filter`: a `phenomenonTime` predicate becomes the temporal window of the whole
//!   request or is refused by name; everything else compiles as an ordinary filter.
//! - `filter_to_q`: the subset a SensorThings client sends compiles to NGSI-LD `q`, and anything
//!   outside it is refused with the operator named rather than dropped.
//!
//! `sta_translator_tests.rs` drives these through the HTTP surface, one happy path each. These
//! are the shapes underneath: what carries no id, what is not a number, what is written in
//! another case, and the filters that have to be refused rather than half applied.

use context_gateway::translators::sta::{datastreams, filter_to_q, series_filter, thing, Series};
use serde_json::{json, Value};

const ENDPOINT: &str = "https://gw.banskabystrica.sk/api/endpoint/n4t8xq2vhm6zc9wrb5sdj3kfp7";
const URN: &str = "urn:ngsi-ld:AirQualityObserved:banskabystrica.sk:ovzdusie:station-01";

fn station() -> Value {
    json!({
        "id": URN,
        "type": "AirQualityObserved",
        "name": { "type": "Property", "value": "Radvaň" },
        "pm10": { "type": "Property", "value": 34.2, "unitCode": "GQ" },
        "operator": { "type": "Property", "value": "Mesto Banská Bystrica" },
        "refDistrict": { "type": "Relationship", "object": "urn:ngsi-ld:District:bb:radvan" },
        "location": {
            "type": "GeoProperty",
            "value": { "type": "Point", "coordinates": [19.15, 48.73] }
        }
    })
}

fn window(of: &Series) -> Vec<(&str, &str)> {
    of.window
        .iter()
        .map(|(name, value)| (name.as_str(), value.as_str()))
        .collect()
}

// --- thing (T-1954) ---------------------------------------------------------------------------

#[test]
fn an_entity_with_no_usable_id_is_no_thing_at_all() {
    // Every link of the Sensing profile is built from the id. A `Thing` without one would carry
    // a self link addressing nothing, and every datastream under it the same.
    for idless in [
        json!({ "type": "AirQualityObserved" }),
        json!({ "id": 7, "type": "AirQualityObserved" }),
        json!({ "id": null }),
        json!({ "id": ["urn:ngsi-ld:X:o:s:1"] }),
        json!([]),
        json!("urn:ngsi-ld:X:o:s:1"),
    ] {
        assert!(thing(ENDPOINT, &idless, &[]).is_none(), "{idless}");
    }
}

#[test]
fn a_thing_that_names_itself_badly_still_names_itself_by_its_urn() {
    // STA insists a Thing has a name, and an empty one tells the reader less than the identifier
    // does. Every shape that leaves nothing usable falls back to the URN rather than to `""`.
    for unnamed in [
        json!(null),
        json!(""),
        json!({ "type": "Property", "value": "" }),
        json!({ "type": "Property", "value": 42 }),
        json!({ "type": "Property" }),
    ] {
        let mut entity = station();
        entity["name"] = unnamed.clone();
        let thing = thing(ENDPOINT, &entity, &[]).expect("a thing");
        assert_eq!(thing["name"], json!(URN), "{unnamed} became the name");
    }

    // A description it has none of is the empty string and never `null`: a client reads the
    // member, and a missing one is a different shape from an empty one.
    let thing = thing(ENDPOINT, &station(), &[]).expect("a thing");
    assert_eq!(thing["description"], json!(""));
}

#[test]
fn a_name_is_read_whether_it_is_an_attribute_or_a_plain_string() {
    // A keyValues answer carries `name: "Radvaň"`, the normalised one carries `{value: …}`. Both
    // reach this function, because the projection leaves the representation the caller asked for.
    let mut flat = station();
    flat["name"] = json!("Radvaň");
    assert_eq!(
        thing(ENDPOINT, &flat, &[]).expect("a thing")["name"],
        json!("Radvaň")
    );
    assert_eq!(
        thing(ENDPOINT, &station(), &[]).expect("a thing")["name"],
        json!("Radvaň")
    );
}

#[test]
fn a_quote_in_an_id_is_doubled_so_the_self_link_stays_one_key() {
    // The id sits inside an OData key literal, where the only escape is a doubled quote. An id
    // carrying one would otherwise close the literal and the link would address something else.
    let mut odd = station();
    odd["id"] = json!("urn:ngsi-ld:X:o:s:it's-here");
    let thing = thing(ENDPOINT, &odd, &[]).expect("a thing");
    assert_eq!(
        thing["@iot.selfLink"],
        json!(format!(
            "{ENDPOINT}/sta/v1.1/Things('urn:ngsi-ld:X:o:s:it''s-here')"
        ))
    );
    assert_eq!(
        thing["@iot.id"],
        json!("urn:ngsi-ld:X:o:s:it's-here"),
        "the id itself is not escaped, only the link"
    );
}

#[test]
fn properties_carries_everything_that_is_not_a_measurement_and_not_a_keyword() {
    // STA has no slot for a municipal entity's own attributes, so they go here flattened. The
    // JSON-LD keywords and the geometry are not properties of the thing: they are its identity
    // and its location, which have slots of their own.
    let thing = thing(ENDPOINT, &station(), &[]).expect("a thing");
    let properties = thing["properties"].as_object().expect("an object");
    assert_eq!(properties["type"], json!("AirQualityObserved"));
    assert_eq!(properties["operator"], json!("Mesto Banská Bystrica"));
    assert_eq!(
        properties["refDistrict"],
        json!("urn:ngsi-ld:District:bb:radvan"),
        "a relationship is flattened to what it points at"
    );
    assert_eq!(properties["name"], json!("Radvaň"));
    for absent in ["id", "@context", "location", "pm10"] {
        assert!(
            properties.get(absent).is_none(),
            "{absent} is not a property"
        );
    }
}

#[test]
fn an_attribute_the_projection_removed_is_in_no_part_of_the_thing() {
    // EP-26: the translator is a view of an entity that was already projected, so an attribute
    // the grant does not name is simply not there — in `properties` or as a datastream.
    let thing = thing(ENDPOINT, &station(), &["Datastreams"]).expect("a thing");
    let rendered = serde_json::to_string(&thing).expect("json");
    assert!(!rendered.contains("operatorPhone"), "{rendered}");
    assert!(!rendered.contains("pm25"), "{rendered}");
}

#[test]
fn a_set_that_is_not_expanded_is_a_navigation_link_and_never_both() {
    let thing = thing(ENDPOINT, &station(), &[]).expect("a thing");
    for set in ["Locations", "Datastreams"] {
        assert!(thing.get(format!("{set}@iot.navigationLink")).is_some());
        assert!(thing.get(set).is_none(), "{set} was inlined unasked");
        assert!(thing.get(format!("{set}@iot.count")).is_none());
    }
}

#[test]
fn an_expanded_set_the_entity_has_nothing_for_is_an_empty_count_and_not_a_link() {
    // `$expand=Locations` on an entity with no geometry answers zero locations rather than a
    // link the client would follow to the same nothing.
    let mut nowhere = station();
    nowhere
        .as_object_mut()
        .expect("an object")
        .remove("location");
    let thing = thing(ENDPOINT, &nowhere, &["Locations"]).expect("a thing");
    assert_eq!(thing["Locations@iot.count"], json!(0));
    assert_eq!(thing["Locations"], json!([]));
    assert!(thing.get("Locations@iot.navigationLink").is_none());
}

#[test]
fn a_set_name_is_expanded_as_it_is_spelt_and_an_unknown_one_expands_nothing() {
    // The names come from `$expand`, which OData spells in the case of the entity set. A name in
    // another case is not a second spelling of it here, and an unknown one changes nothing.
    let thing = thing(ENDPOINT, &station(), &["locations", "Sensors", ""]).expect("a thing");
    assert!(thing.get("Locations@iot.navigationLink").is_some());
    assert!(thing.get("Locations").is_none());
    assert!(
        thing.get("Sensors").is_none(),
        "a set with no data is not inlined"
    );
}

#[test]
fn expanding_both_sets_inlines_both_and_counts_each() {
    let thing = thing(ENDPOINT, &station(), &["Locations", "Datastreams"]).expect("a thing");
    assert_eq!(thing["Locations@iot.count"], json!(1));
    assert_eq!(thing["Datastreams@iot.count"], json!(1));
    assert_eq!(
        thing["Datastreams"][0]["@iot.id"],
        json!(format!("{URN}/pm10"))
    );
}

// --- datastreams (T-1955) ---------------------------------------------------------------------

#[test]
fn only_a_numeric_property_is_a_datastream() {
    // EP-13: a datastream over a string, a boolean, a geometry or a relationship would answer
    // `result: null` for ever, which is worse than the set not being there.
    let mut entity = json!({ "id": URN, "type": "AirQualityObserved" });
    let members = entity.as_object_mut().expect("an object");
    for (name, value) in [
        ("text", json!({ "type": "Property", "value": "ok" })),
        ("flag", json!({ "type": "Property", "value": true })),
        ("empty", json!({ "type": "Property", "value": null })),
        ("list", json!({ "type": "Property", "value": [1, 2] })),
        ("nested", json!({ "type": "Property", "value": { "a": 1 } })),
        ("bare", json!(7)),
        (
            "related",
            json!({ "type": "Relationship", "object": "urn:ngsi-ld:X:o:s:1" }),
        ),
        (
            "here",
            json!({ "type": "GeoProperty", "value": { "type": "Point", "coordinates": [1, 2] } }),
        ),
    ] {
        members.insert(name.to_owned(), value);
    }
    assert!(datastreams(ENDPOINT, &entity).is_empty());
}

#[test]
fn every_number_a_broker_can_send_is_a_measurement() {
    // Zero is a reading, and so is a negative temperature. Treating either as absent would lose
    // the measurement that matters most on a cold day.
    for value in [
        json!(0),
        json!(-17.5),
        json!(34.2),
        json!(1e18),
        json!(-0.0),
    ] {
        let entity = json!({
            "id": URN,
            "type": "AirQualityObserved",
            "reading": { "type": "Property", "value": value }
        });
        let streams = datastreams(ENDPOINT, &entity);
        assert_eq!(streams.len(), 1, "{value} was not a measurement");
        assert_eq!(streams[0]["@iot.id"], json!(format!("{URN}/reading")));
    }
}

#[test]
fn a_unit_the_attribute_does_not_carry_is_said_to_be_unknown_and_not_invented() {
    // A datastream needs a unit of measurement, and a made-up one is a reading a client would
    // convert wrongly. `unknown` with an empty symbol and no definition says what is true.
    let entity = json!({
        "id": URN,
        "type": "AirQualityObserved",
        "count": { "type": "Property", "value": 3 },
        "odd": { "type": "Property", "value": 3, "unitCode": 42 }
    });
    for stream in datastreams(ENDPOINT, &entity) {
        assert_eq!(stream["unitOfMeasurement"]["name"], json!("unknown"));
        assert_eq!(stream["unitOfMeasurement"]["symbol"], json!(""));
        assert_eq!(stream["unitOfMeasurement"]["definition"], json!(""));
    }
}

#[test]
fn a_unit_the_attribute_carries_names_its_definition_in_the_unece_vocabulary() {
    let streams = datastreams(ENDPOINT, &station());
    assert_eq!(streams.len(), 1);
    // The code list names it (DM-06, T-2812); the code stays in the definition.
    assert_eq!(
        streams[0]["unitOfMeasurement"]["name"],
        json!("microgram per cubic metre")
    );
    assert_eq!(streams[0]["unitOfMeasurement"]["symbol"], json!("µg/m³"));
    assert_eq!(
        streams[0]["unitOfMeasurement"]["definition"],
        json!("https://vocabulary.uncefact.org/UnitMeasureCode#GQ")
    );
}

#[test]
fn a_code_the_list_does_not_know_is_kept_as_written_and_a_unit_without_symbol_shows_its_code() {
    let entity = json!({
        "id": URN,
        "type": "AirQualityObserved",
        "made": { "type": "Property", "value": 3, "unitCode": "XQZ" },
        "bikes": { "type": "Property", "value": 3, "unitCode": "H87" }
    });
    let streams = datastreams(ENDPOINT, &entity);
    let unit = |name: &str| {
        streams
            .iter()
            .find(|stream| stream["name"] == json!(name))
            .map(|stream| stream["unitOfMeasurement"].clone())
            .expect("the datastream")
    };
    assert_eq!(unit("made")["name"], json!("XQZ"));
    assert_eq!(unit("made")["symbol"], json!("XQZ"));
    assert_eq!(unit("bikes")["name"], json!("piece"));
    assert_eq!(unit("bikes")["symbol"], json!("H87"));
}

#[test]
fn a_datastream_links_back_to_its_thing_and_on_to_its_observations() {
    // The two navigation links are how a client walks from a series to the entity it measures
    // and to its history. Both are built from the same identity, so a link saved a year ago
    // still resolves.
    let streams = datastreams(ENDPOINT, &station());
    let stream = &streams[0];
    assert_eq!(
        stream["Thing@iot.navigationLink"],
        json!(format!("{ENDPOINT}/sta/v1.1/Things('{URN}')"))
    );
    assert_eq!(
        stream["Observations@iot.navigationLink"],
        json!(format!(
            "{ENDPOINT}/sta/v1.1/Datastreams('{URN}/pm10')/Observations"
        ))
    );
}

#[test]
fn the_keywords_of_the_document_are_never_datastreams() {
    // `@context` and the rest are the shape of the document, not measurements of anything, and
    // a broker may answer them in either spelling.
    let entity = json!({
        "id": URN,
        "@id": URN,
        "type": "AirQualityObserved",
        "@type": "AirQualityObserved",
        "@context": "https://uri.etsi.org/ngsi-ld/v1/ngsi-ld-core-context-v1.8.jsonld",
        "pm10": { "type": "Property", "value": 1 }
    });
    let streams = datastreams(ENDPOINT, &entity);
    assert_eq!(streams.len(), 1);
    assert_eq!(streams[0]["name"], json!("pm10"));
}

#[test]
fn an_attribute_name_with_a_quote_in_it_still_makes_one_addressable_key() {
    let entity = json!({
        "id": URN,
        "type": "AirQualityObserved",
        "it's": { "type": "Property", "value": 1 }
    });
    let streams = datastreams(ENDPOINT, &entity);
    assert_eq!(streams[0]["@iot.id"], json!(format!("{URN}/it's")));
    assert_eq!(
        streams[0]["@iot.selfLink"],
        json!(format!("{ENDPOINT}/sta/v1.1/Datastreams('{URN}/it''s')"))
    );
}

#[test]
fn nothing_but_an_object_has_measurements() {
    for not_an_entity in [json!([]), json!("x"), json!(7), json!(null)] {
        assert!(
            datastreams(ENDPOINT, &not_an_entity).is_empty(),
            "{not_an_entity}"
        );
    }
}

// --- series_filter (T-1956) -------------------------------------------------------------------

#[test]
fn a_filter_with_no_phenomenon_time_is_all_q_and_no_window() {
    let series = series_filter("pm10 gt 30").expect("a filter");
    assert!(series.window.is_empty());
    assert_eq!(series.q.as_deref(), Some("pm10>30"));
}

#[test]
fn each_comparison_on_phenomenon_time_becomes_the_window_it_means() {
    for (filter, expected) in [
        (
            "phenomenonTime gt 2026-09-01T00:00:00Z",
            vec![("timerel", "after"), ("timeAt", "2026-09-01T00:00:00Z")],
        ),
        (
            "phenomenonTime ge 2026-09-01T00:00:00Z",
            vec![("timerel", "after"), ("timeAt", "2026-09-01T00:00:00Z")],
        ),
        (
            "phenomenonTime lt 2026-09-02T00:00:00Z",
            vec![("timerel", "before"), ("timeAt", "2026-09-02T00:00:00Z")],
        ),
        (
            "phenomenonTime le 2026-09-02T00:00:00Z",
            vec![("timerel", "before"), ("timeAt", "2026-09-02T00:00:00Z")],
        ),
        (
            "phenomenonTime eq 2026-09-01T00:00:00Z",
            vec![
                ("timerel", "between"),
                ("timeAt", "2026-09-01T00:00:00Z"),
                ("endTimeAt", "2026-09-01T00:00:00Z"),
            ],
        ),
        (
            "phenomenonTime ge 2026-09-01T00:00:00Z and phenomenonTime lt 2026-09-02T00:00:00Z",
            vec![
                ("timerel", "between"),
                ("timeAt", "2026-09-01T00:00:00Z"),
                ("endTimeAt", "2026-09-02T00:00:00Z"),
            ],
        ),
    ] {
        let series = series_filter(filter).unwrap_or_else(|error| panic!("{filter}: {error}"));
        assert_eq!(window(&series), expected, "{filter}");
        assert_eq!(series.q, None, "{filter} left a q behind");
    }
}

#[test]
fn the_window_and_the_rest_of_the_filter_are_separated_and_both_kept() {
    // The time is not an attribute of the entity and the rest is: dropping either half would
    // answer rows the caller asked not to see.
    let series =
        series_filter("pm10 gt 30 and phenomenonTime gt 2026-09-01T00:00:00Z").expect("a filter");
    assert_eq!(
        window(&series),
        vec![("timerel", "after"), ("timeAt", "2026-09-01T00:00:00Z")]
    );
    assert_eq!(series.q.as_deref(), Some("pm10>30"));
}

#[test]
fn an_instant_may_be_quoted_or_bare_and_the_name_may_be_in_any_case() {
    for filter in [
        "phenomenonTime gt '2026-09-01T00:00:00Z'",
        "PHENOMENONTIME GT 2026-09-01T00:00:00Z",
        "phenomenontime gt 2026-09-01T00:00:00Z",
    ] {
        let series = series_filter(filter).unwrap_or_else(|error| panic!("{filter}: {error}"));
        assert_eq!(
            window(&series),
            vec![("timerel", "after"), ("timeAt", "2026-09-01T00:00:00Z")],
            "{filter}"
        );
    }
}

#[test]
fn a_window_that_ends_before_it_starts_is_refused_rather_than_answered_empty() {
    // An empty answer looks like "there is no data", which is a different thing from "you asked
    // for a window that cannot exist".
    let error = series_filter(
        "phenomenonTime ge 2026-09-02T00:00:00Z and phenomenonTime le 2026-09-01T00:00:00Z",
    )
    .expect_err("refused");
    assert!(
        error.to_string().contains("ends before it starts"),
        "{error}"
    );
}

#[test]
fn a_phenomenon_time_predicate_that_is_not_on_the_and_spine_is_refused_by_name() {
    // The window applies to the whole request, so one inside a disjunction cannot be honoured.
    // Applying it to both branches would answer rows of the other one it never covered.
    for filter in [
        "(phenomenonTime gt 2026-09-01T00:00:00Z or pm10 gt 30)",
        "pm10 gt 30 or phenomenonTime gt 2026-09-01T00:00:00Z",
    ] {
        let error = series_filter(filter).expect_err("refused");
        assert!(
            error
                .to_string()
                .contains("cannot sit inside a parenthesis or an or"),
            "{filter}: {error}"
        );
    }
}

#[test]
fn a_malformed_phenomenon_time_predicate_is_refused_and_says_what_was_expected() {
    for (filter, said) in [
        ("phenomenonTime gt", "`phenomenonTime <op> <instant>`"),
        ("phenomenonTime", "`phenomenonTime <op> <instant>`"),
        (
            "phenomenonTime gt 2026-09-01T00:00:00Z extra",
            "one instant",
        ),
        ("phenomenonTime gt yesterday", "RFC 3339"),
        ("phenomenonTime gt ''", "RFC 3339"),
        ("phenomenonTime gt 2026-09-01", "RFC 3339"),
        ("phenomenonTime ne 2026-09-01T00:00:00Z", "not supported"),
        ("phenomenonTime has 2026-09-01T00:00:00Z", "not supported"),
    ] {
        let error = series_filter(filter).expect_err("refused");
        assert!(error.to_string().contains(said), "{filter}: {error}");
    }
}

#[test]
fn an_empty_filter_selects_everything_rather_than_being_an_error() {
    // `$filter=` is what a client sends when it built the parameter and had nothing to put in
    // it. There is nothing to refuse: no window, no q.
    for filter in ["", "   ", " and "] {
        let series = series_filter(filter).unwrap_or_else(|error| panic!("{filter:?}: {error}"));
        assert!(series.window.is_empty() && series.q.is_none(), "{filter:?}");
    }
}

#[test]
fn an_attribute_whose_name_merely_ends_in_phenomenon_time_is_refused_rather_than_compiled() {
    // Recorded because the message is about parentheses and the filter has none: the term is
    // tested for *containing* `phenomenontime` before it is tested for starting with it. It
    // fails closed — the filter is refused, never silently dropped — and an attribute named this
    // way is not one any Smart Data Model defines, so this is a message defect, not a leak.
    let error = series_filter("lastPhenomenonTime gt 2026-09-01T00:00:00Z").expect_err("refused");
    assert!(
        error
            .to_string()
            .contains("cannot sit inside a parenthesis or an or"),
        "{error}"
    );
}

// --- filter_to_q (T-1957) ---------------------------------------------------------------------

#[test]
fn every_comparison_and_connective_of_the_subset_compiles() {
    for (filter, q) in [
        ("pm10 eq 30", "pm10==30"),
        ("pm10 ne 30", "pm10!=30"),
        ("pm10 gt 30", "pm10>30"),
        ("pm10 ge 30", "pm10>=30"),
        ("pm10 lt 30", "pm10<30"),
        ("pm10 le 30", "pm10<=30"),
        ("pm10 gt 30 and pm25 lt 10", "pm10>30;pm25<10"),
        ("pm10 gt 30 or pm25 lt 10", "pm10>30|pm25<10"),
        (
            "(pm10 gt 30 or pm25 lt 10) and name eq 'Radvan'",
            "(pm10>30|pm25<10);name==\"Radvan\"",
        ),
        ("PM10 GT 30 AND PM25 LT 10", "PM10>30;PM25<10"),
    ] {
        assert_eq!(
            filter_to_q(filter).unwrap_or_else(|error| panic!("{filter}: {error}")),
            q,
            "{filter}"
        );
    }
}

#[test]
fn a_quoted_literal_becomes_an_ngsi_ld_string_and_a_name_stays_a_name() {
    assert_eq!(
        filter_to_q("name eq 'Radvan'").expect("a filter"),
        "name==\"Radvan\""
    );
    assert_eq!(
        filter_to_q("name eq Radvan").expect("a filter"),
        "name==Radvan"
    );
    // A navigation path addresses a property of a linked set; NGSI-LD has one flat namespace, so
    // the last segment is the name that exists here.
    assert_eq!(
        filter_to_q("Datastreams/name eq 'pm10'").expect("a filter"),
        "name==\"pm10\""
    );
}

#[test]
fn substringof_compiles_to_a_pattern_and_its_misuses_are_named() {
    assert_eq!(
        filter_to_q("substringof('Radvan',name)").expect("a filter"),
        "name~=\".*Radvan.*\""
    );
    assert_eq!(
        filter_to_q("substringof('Radvan', name) and pm10 gt 30").expect("a filter"),
        "name~=\".*Radvan.*\";pm10>30"
    );
    for (filter, said) in [
        ("substringof", "needs (text, attribute)"),
        ("substringof(", "needs (text, attribute)"),
        ("substringof('Radvan')", "two arguments"),
        ("substringof('',name)", "non-empty"),
        ("substringof('Radvan',)", "non-empty"),
    ] {
        let error = filter_to_q(filter).expect_err("refused");
        assert!(error.to_string().contains(said), "{filter}: {error}");
    }
}

#[test]
fn an_operator_outside_the_subset_is_refused_with_its_own_name_in_the_message() {
    // A client told only "400" retries the same request. Naming the operator is what lets the
    // person writing the filter fix it.
    for (filter, named) in [
        ("pm10 has 30", "has"),
        ("pm10 mod 2", "mod"),
        ("pm10 add 1", "add"),
        ("pm10 eq 30 xor pm25 eq 1", "xor"),
    ] {
        let error = filter_to_q(filter).expect_err("refused");
        assert!(error.to_string().contains(named), "{filter}: {error}");
    }
    let error = filter_to_q("not pm10 eq 30").expect_err("refused");
    assert!(
        error.to_string().contains("not is not supported"),
        "{error}"
    );
}

#[test]
fn an_empty_filter_is_refused_here_because_a_caller_wrote_one_that_selects_nothing() {
    for filter in ["", "   "] {
        let error = filter_to_q(filter).expect_err("refused");
        assert!(error.to_string().contains("empty"), "{filter:?}: {error}");
    }
}

#[test]
fn a_quoted_literal_with_a_space_in_it_is_refused_rather_than_half_compiled() {
    // Recorded as a limitation with its evidence: the tokenizer splits on every space, so
    // `'North depot'` is read as the operand `'North` followed by the operator `depot'`. It fails
    // closed — the request is refused and no filter is dropped — but the message names a word the
    // caller did not write as an operator.
    let error = filter_to_q("name eq 'North depot'").expect_err("refused");
    assert!(error.to_string().contains("depot'"), "{error}");
}

#[test]
fn a_filter_that_ends_mid_expression_compiles_to_what_was_written_and_no_more() {
    // A dangling operator is the caller's filter, not an injection: nothing is invented to
    // complete it, and the broker refuses what it cannot parse.
    assert_eq!(filter_to_q("pm10 gt").expect("compiles"), "pm10>");
    assert_eq!(filter_to_q("(").expect("compiles"), "(");
}

#[test]
fn two_bounds_of_the_same_side_leave_the_last_one_and_not_the_narrower() {
    // Recorded with its evidence: `start` and `end` are assigned, not intersected
    // (`sta.rs:419`), so `gt B and gt A` with A earlier than B answers the window from A —
    // wider than the filter asks for. It is not a leak: the grant's own temporal windows clamp
    // the answer afterwards (`pdp::temporal::clamp`, GW26), so a caller still reads only what
    // was granted; it returns rows the *caller* asked not to see. Filed in chyby.md.
    let series = series_filter(
        "phenomenonTime gt 2026-09-02T00:00:00Z and phenomenonTime gt 2026-09-01T00:00:00Z",
    )
    .expect("a filter");
    assert_eq!(
        window(&series),
        vec![("timerel", "after"), ("timeAt", "2026-09-01T00:00:00Z")],
        "the narrower bound survived, so this test is stale and the defect is fixed"
    );
}
