//! Edge cases of `translators::view_mapping::ViewMapping::translate_entity` (T-1961; DM-51,
//! DM-52, EP-54, EP-71, T-0473).
//!
//! Contract, one sentence: an entity leaves a view in the target model and in no other — every
//! member is either one the mapping derives, the NGSI-LD structure, or a member the broker
//! generated, and a source name the mapping does not map never reaches a caller.
//!
//! `view_mapping_tests.rs` covers the six derivations, the two representations and the refusals
//! of a bad IR. These are the shapes around them: what is not an entity, what the broker sent
//! that no slot names, a source attribute that is missing or of the wrong type, and the members
//! that have to survive because a client reads them.

use context_gateway::translators::view_mapping::ViewMapping;
use serde_json::{json, Value};

const STATION: &str = "urn:ngsi-ld:MestskySenzor:banskabystrica.sk:ovzdusie:senzor-01";

fn ir() -> Value {
    json!({
        "version": 2,
        "sourceClass": "MestskySenzor",
        "targetClass": "AirQualityObserved",
        "slots": [
            { "target": "pm25", "source": "pm2p5", "kind": "rename", "filterable": true },
            {
                "target": "temperature", "source": "teplota", "kind": "unitConversion",
                "factor": 1.0, "offset": -273.15, "filterable": true
            },
            {
                "target": "reliability", "source": "spolahlivost", "kind": "valueMappings",
                "forward": { "vysoka": "high" },
                "inverse": { "high": "vysoka" },
                "filterable": true
            },
            { "target": "dataProvider", "kind": "constant", "value": "bb", "filterable": false }
        ]
    })
}

fn mapping() -> ViewMapping {
    ViewMapping::parse(&ir()).expect("the IR parses")
}

fn translated(entity: Value) -> Value {
    let mut entity = entity;
    mapping().translate_entity(&mut entity);
    entity
}

#[test]
fn a_document_that_is_not_an_entity_is_left_exactly_as_it_was() {
    // A problem document, a type list and an attribute list travel the same path as an entity.
    // Rebuilding one in the target model would replace its members with a `type` it never had.
    for untouched in [
        json!({ "type": "https://uri.etsi.org/ngsi-ld/errors/ResourceNotFound", "title": "Not Found" }),
        json!({ "typeList": ["MestskySenzor"], "type": "EntityTypeList" }),
        json!([]),
        json!("text"),
        json!(null),
        json!(7),
    ] {
        assert_eq!(translated(untouched.clone()), untouched, "{untouched}");
    }
}

#[test]
fn an_entity_that_names_itself_with_the_json_ld_keyword_is_still_an_entity() {
    // An expanded answer carries `@id` rather than `id`, and it is the same entity.
    let expanded = translated(json!({
        "@id": STATION,
        "@type": "MestskySenzor",
        "pm2p5": { "type": "Property", "value": 12.5 }
    }));
    assert_eq!(expanded["@id"], json!(STATION));
    assert_eq!(expanded["type"], json!("AirQualityObserved"));
    assert_eq!(expanded["pm25"]["value"], json!(12.5));
}

#[test]
fn a_source_attribute_no_slot_names_does_not_reach_the_caller() {
    // The view serves the target model. An attribute that leaked through under its source name
    // is one no target schema describes, and one the caller's grant was never evaluated against.
    let served = translated(json!({
        "id": STATION,
        "type": "MestskySenzor",
        "pm2p5": { "type": "Property", "value": 12.5 },
        "kalibracia": { "type": "Property", "value": 0.3 },
        "operatorPhone": { "type": "Property", "value": "+421 900 000 000" }
    }));
    let rendered = serde_json::to_string(&served).expect("json");
    assert!(!rendered.contains("kalibracia"), "{rendered}");
    assert!(!rendered.contains("operatorPhone"), "{rendered}");
    assert_eq!(served["type"], json!("AirQualityObserved"));
    // The id is the broker's identity and keeps the source class inside its URN: a view renames
    // what an entity *is* in the answer, never what it is called, or a link a caller saved would
    // stop resolving (PF-42).
    assert_eq!(served["id"], json!(STATION));
}

#[test]
fn a_slot_whose_source_the_broker_did_not_send_is_left_out_rather_than_answered_empty() {
    // An attribute that is not there is different from one that is null: a chart draws a gap for
    // the first and a zero for the second.
    let served = translated(json!({
        "id": STATION,
        "type": "MestskySenzor",
        "pm2p5": { "type": "Property", "value": 12.5 }
    }));
    assert!(served.get("temperature").is_none());
    assert!(served.get("reliability").is_none());
    assert_eq!(served["pm25"]["value"], json!(12.5));
}

#[test]
fn a_constant_slot_is_produced_whether_or_not_the_broker_sent_anything() {
    // It depends on no source attribute, so an entity of only an id still carries it — that is
    // what makes it a statement about the view rather than about the row.
    let served = translated(json!({ "id": STATION, "type": "MestskySenzor" }));
    assert_eq!(
        served["dataProvider"],
        json!({ "type": "Property", "value": "bb" })
    );
}

#[test]
fn a_produced_value_takes_the_form_the_rest_of_the_answer_is_in() {
    // T-0473: in a normalized answer it is a `Property`; in a keyValues answer the value itself.
    // A mixed shape in one entity is what makes a client's parser fall over.
    let normalized = translated(json!({
        "id": STATION,
        "type": "MestskySenzor",
        "pm2p5": { "type": "Property", "value": 12.5 }
    }));
    assert_eq!(normalized["dataProvider"]["value"], json!("bb"));

    let key_values = translated(json!({ "id": STATION, "type": "MestskySenzor", "pm2p5": 12.5 }));
    assert_eq!(key_values["dataProvider"], json!("bb"));
    assert_eq!(key_values["pm25"], json!(12.5));
}

#[test]
fn an_entity_with_nothing_but_its_identity_is_read_as_normalized() {
    // There is no attribute to judge the representation by, and normalized is what a request
    // gets unless it asks for `keyValues`.
    let served = translated(json!({ "id": STATION, "type": "MestskySenzor" }));
    assert_eq!(
        served["dataProvider"],
        json!({ "type": "Property", "value": "bb" })
    );
}

#[test]
fn the_structure_and_the_brokers_own_members_travel_through_untouched() {
    // EP-71: `createdAt` and its siblings say when the data was written, not what it contains,
    // and `scope` and `@context` are how the answer is read at all.
    let served = translated(json!({
        "id": STATION,
        "type": "MestskySenzor",
        "@context": "https://uri.etsi.org/ngsi-ld/v1/ngsi-ld-core-context-v1.8.jsonld",
        "scope": "/geo/SK/BB",
        "createdAt": "2026-09-01T00:00:00Z",
        "modifiedAt": "2026-09-07T08:00:00Z",
        "deletedAt": "2026-09-08T08:00:00Z",
        "expiresAt": "2026-10-01T00:00:00Z",
        "pm2p5": { "type": "Property", "value": 12.5 }
    }));
    for kept in [
        "@context",
        "scope",
        "createdAt",
        "modifiedAt",
        "deletedAt",
        "expiresAt",
    ] {
        assert!(served.get(kept).is_some(), "{kept} was dropped");
    }
    assert_eq!(
        served["type"],
        json!("AirQualityObserved"),
        "the class is the view's"
    );
}

#[test]
fn a_value_the_derivation_cannot_convert_is_passed_through_rather_than_invented() {
    // A temperature that arrived as text is not a number to shift by 273.15. Answering a
    // converted value would be a reading nobody measured; answering the source value says what
    // the broker holds.
    let served = translated(json!({
        "id": STATION,
        "type": "MestskySenzor",
        "teplota": { "type": "Property", "value": "warm" },
        "spolahlivost": { "type": "Property", "value": "neznama" }
    }));
    assert_eq!(served["temperature"]["value"], json!("warm"));
    assert_eq!(
        served["reliability"]["value"],
        json!("neznama"),
        "a value the table does not map keeps its own"
    );
}

#[test]
fn an_attribute_that_is_not_an_object_still_converts_by_its_value() {
    // The keyValues form is the bare value, and a broker answering one attribute flat and
    // another normalized is not this translator's to refuse.
    let served = translated(json!({
        "id": STATION,
        "type": "MestskySenzor",
        "teplota": 292.15,
        "spolahlivost": "vysoka"
    }));
    assert_eq!(served["temperature"], json!(19.0));
    assert_eq!(served["reliability"], json!("high"));
}

#[test]
fn the_metadata_around_a_converted_value_survives_the_conversion() {
    // `observedAt` and `unitCode` describe the reading. Dropping them would leave a temperature
    // with no time and no unit, which is a number rather than a measurement.
    let served = translated(json!({
        "id": STATION,
        "type": "MestskySenzor",
        "teplota": {
            "type": "Property",
            "value": 292.15,
            "unitCode": "KEL",
            "observedAt": "2026-09-07T08:00:00Z"
        }
    }));
    assert_eq!(served["temperature"]["value"], json!(19.0));
    assert_eq!(
        served["temperature"]["observedAt"],
        json!("2026-09-07T08:00:00Z")
    );
    assert_eq!(served["temperature"]["type"], json!("Property"));
}

#[test]
fn a_relationship_is_carried_by_its_object_and_not_by_a_value_it_has_none_of() {
    let ir = json!({
        "version": 2,
        "sourceClass": "MestskySenzor",
        "targetClass": "AirQualityObserved",
        "slots": [
            { "target": "refStation", "source": "senzor", "kind": "rename", "filterable": true }
        ]
    });
    let mut entity = json!({
        "id": STATION,
        "type": "MestskySenzor",
        "senzor": { "type": "Relationship", "object": "urn:ngsi-ld:Station:bb:s-1" }
    });
    ViewMapping::parse(&ir)
        .expect("the IR parses")
        .translate_entity(&mut entity);
    assert_eq!(
        entity["refStation"]["object"],
        json!("urn:ngsi-ld:Station:bb:s-1")
    );
}

#[test]
fn two_slots_reading_the_same_source_both_get_their_value() {
    // A view may serve one source attribute twice, under two names and two conversions. Neither
    // consumes the attribute, because the source entity is read and never written.
    let ir = json!({
        "version": 2,
        "sourceClass": "MestskySenzor",
        "targetClass": "AirQualityObserved",
        "slots": [
            { "target": "temperatureK", "source": "teplota", "kind": "rename", "filterable": true },
            {
                "target": "temperature", "source": "teplota", "kind": "unitConversion",
                "factor": 1.0, "offset": -273.15, "filterable": true
            }
        ]
    });
    let mut entity = json!({
        "id": STATION,
        "type": "MestskySenzor",
        "teplota": { "type": "Property", "value": 292.15 }
    });
    ViewMapping::parse(&ir)
        .expect("the IR parses")
        .translate_entity(&mut entity);
    assert_eq!(entity["temperatureK"]["value"], json!(292.15));
    assert_eq!(entity["temperature"]["value"], json!(19.0));
}

#[test]
fn translating_an_entity_twice_does_not_translate_the_translation() {
    // The reaper and the egress path both hold answers that may pass a view more than once. The
    // second pass finds no source attribute, so the entity keeps its identity and its class and
    // loses what a second conversion would have corrupted.
    let mapping = mapping();
    let mut entity = json!({
        "id": STATION,
        "type": "MestskySenzor",
        "teplota": { "type": "Property", "value": 292.15 }
    });
    mapping.translate_entity(&mut entity);
    let once = entity.clone();
    mapping.translate_entity(&mut entity);
    assert_eq!(entity["id"], once["id"]);
    assert_eq!(entity["type"], json!("AirQualityObserved"));
    assert_eq!(
        entity.get("temperature").and_then(|t| t.get("value")),
        None,
        "a second pass must not convert an already converted value"
    );
}
