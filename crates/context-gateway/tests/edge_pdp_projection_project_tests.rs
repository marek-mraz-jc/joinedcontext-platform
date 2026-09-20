//! Edge cases of `pdp::projection::project` (T-1930, EP-26, MP-02).
//!
//! Contract, in one sentence: whatever the broker answered, every entity in it is projected by
//! the same rule — one entity, or every element of one array of entities, one level deep — and no
//! element is skipped because of its shape or its place in the page.
//!
//! The dangerous failure here is not a wrong answer, it is a missed one: an answer where entity
//! 500 of a page kept an attribute the grant does not name is a leak that no happy-path test
//! sees. The rule the function does NOT have — it does not walk deeper than one level — is
//! written down here too, because the callers (`app.rs`'s `retain`) are what has to refuse an
//! element that is not an entity; T-2335 tracks that.

use context_gateway::pdp::projection::project;
use serde_json::{json, Value};
use std::collections::BTreeSet;

fn names(names: &[&str]) -> BTreeSet<String> {
    names.iter().map(|name| (*name).to_owned()).collect()
}

fn nothing() -> BTreeSet<String> {
    BTreeSet::new()
}

/// The canary: an attribute no answer of these tests may contain.
const SECRET: &str = "operatorPhone";

fn station(number: usize) -> Value {
    json!({
        "id": format!("urn:ngsi-ld:AirQualityObserved:bb.sk:ovzdusie:station-{number:03}"),
        "type": "AirQualityObserved",
        "pm10": { "type": "Property", "value": 34.2 },
        SECRET: { "type": "Property", "value": "+421 900 000 000" }
    })
}

/// R9: every entity of a page is projected, not the first and not a sample. A page is exactly
/// where a missed element is invisible.
#[test]
fn every_entity_of_a_long_page_is_projected() {
    let mut page = Value::Array((0..1024).map(station).collect());
    project(&mut page, &names(&["pm10"]), &nothing());

    let entities = page.as_array().expect("a page");
    assert_eq!(entities.len(), 1024, "projection drops nothing");
    for (index, entity) in entities.iter().enumerate() {
        assert!(
            entity.get(SECRET).is_none(),
            "entity {index} kept the phone number"
        );
        assert_eq!(entity["pm10"]["value"], json!(34.2), "entity {index}");
    }
    assert!(
        !page.to_string().contains(SECRET),
        "and the page as a whole says it nowhere"
    );
}

/// An element the function cannot project does not stop the ones after it.
#[test]
fn an_element_that_is_not_an_entity_does_not_stop_the_page() {
    let mut page = json!([
        station(1),
        null,
        "urn:ngsi-ld:AirQualityObserved:bb.sk:ovzdusie:x",
        7,
        station(2)
    ]);
    project(&mut page, &names(&["pm10"]), &nothing());

    let entities = page.as_array().expect("a page");
    assert_eq!(
        entities.len(),
        5,
        "nothing is removed here; that is the caller's decision"
    );
    for index in [0, 4] {
        assert!(entities[index].get(SECRET).is_none(), "entity at {index}");
    }
}

/// One entity is an answer too: `GET /entities/{id}` is not an array, and it is projected by the
/// same call.
#[test]
fn a_single_entity_answer_is_projected() {
    let mut entity = station(1);
    project(&mut entity, &names(&["pm10"]), &nothing());

    assert!(entity.get(SECRET).is_none());
    assert_eq!(
        entity["id"],
        station(1)["id"],
        "still the entity that was asked for"
    );
}

/// The projection reaches one level down and no further. An element that is itself an array is
/// not an entity, and the read path — not this function — is what must refuse it (T-2335).
#[test]
fn the_projection_reaches_one_level_and_says_so() {
    let mut nested = json!([[station(1)]]);
    project(&mut nested, &names(&["pm10"]), &nothing());

    assert!(
        nested[0][0].get(SECRET).is_some(),
        "documented, not desired: an element that is not an object is left as it is, which is why \
         T-2335 drops it in app.rs before it is ever projected"
    );
}

/// The order of a page is the broker's, and projecting does not shuffle, sort or deduplicate it.
#[test]
fn the_page_keeps_its_order_and_its_length() {
    let mut page = json!([station(3), station(1), station(3), station(2)]);
    project(&mut page, &names(&["pm10"]), &nothing());

    let ids: Vec<&str> = page
        .as_array()
        .expect("a page")
        .iter()
        .map(|entity| entity["id"].as_str().expect("an id"))
        .collect();
    assert_eq!(
        ids,
        vec![
            "urn:ngsi-ld:AirQualityObserved:bb.sk:ovzdusie:station-003",
            "urn:ngsi-ld:AirQualityObserved:bb.sk:ovzdusie:station-001",
            "urn:ngsi-ld:AirQualityObserved:bb.sk:ovzdusie:station-003",
            "urn:ngsi-ld:AirQualityObserved:bb.sk:ovzdusie:station-002",
        ]
    );
}

/// An empty page is an empty page, not an error and not a page of one.
#[test]
fn an_empty_page_stays_empty() {
    let mut page = json!([]);
    project(&mut page, &names(&["pm10"]), &names(&[SECRET]));
    assert_eq!(page, json!([]));
}

/// EP-61: the endpoint's denial applies to every entity of the page, including one that carries
/// nothing else.
#[test]
fn a_denial_applies_to_every_entity_of_the_page() {
    let mut page = json!([
        station(1),
        { "id": "urn:ngsi-ld:AirQualityObserved:bb.sk:ovzdusie:station-002", "type": "AirQualityObserved", SECRET: "+421 900 000 001" }
    ]);
    project(&mut page, &nothing(), &names(&[SECRET]));

    assert!(!page.to_string().contains(SECRET));
}

/// Entities of a page are projected by their own members: one that carries an ungranted
/// attribute does not make its neighbour lose a granted one.
#[test]
fn each_entity_is_projected_by_its_own_members() {
    let mut page = json!([
        { "id": "urn:ngsi-ld:Vehicle:hel.fi:fleet:1", "type": "Vehicle", "weight": 1, SECRET: "x" },
        { "id": "urn:ngsi-ld:Vehicle:hel.fi:fleet:2", "type": "Vehicle" },
        { "id": "urn:ngsi-ld:Vehicle:hel.fi:fleet:3", "type": "Vehicle", "weight": 3 }
    ]);
    project(&mut page, &names(&["weight"]), &nothing());

    assert_eq!(page[0]["weight"], json!(1));
    assert!(page[0].get(SECRET).is_none());
    assert!(page[1].get("weight").is_none(), "it never had one");
    assert_eq!(page[2]["weight"], json!(3));
}

/// No grant and no denial is no projection, whatever the answer looks like.
#[test]
fn no_grant_and_no_denial_leaves_the_answer_byte_for_byte() {
    for mut answer in [
        json!([station(1), station(2)]),
        station(1),
        json!([]),
        Value::Null,
    ] {
        let before = answer.clone();
        project(&mut answer, &nothing(), &nothing());
        assert_eq!(answer, before);
    }
}

/// An answer that is not an entity at all — a type list, a problem document, a count — is left
/// alone or stripped as the object it is, and never turned into something else.
#[test]
fn an_answer_that_is_not_an_entity_is_not_made_into_one() {
    let mut list = json!(["AirQualityObserved", "Vehicle"]);
    project(&mut list, &names(&["pm10"]), &nothing());
    assert_eq!(list, json!(["AirQualityObserved", "Vehicle"]));

    let mut count = json!(42);
    project(&mut count, &names(&["pm10"]), &nothing());
    assert_eq!(count, json!(42));
}

/// Projecting a projected page changes nothing further.
#[test]
fn projecting_a_page_twice_is_projecting_it_once() {
    let mut once = json!([station(1), station(2)]);
    project(&mut once, &names(&["pm10"]), &names(&[SECRET]));
    let mut twice = once.clone();
    project(&mut twice, &names(&["pm10"]), &names(&[SECRET]));

    assert_eq!(once, twice);
}

/// A page of entities that all look alike is still projected entity by entity: the last one is
/// not taken on trust from the first.
#[test]
fn the_last_entity_of_a_page_is_projected_too() {
    let mut page = Value::Array(std::iter::repeat_with(|| station(1)).take(50).collect());
    page.as_array_mut()
        .expect("a page")
        .last_mut()
        .expect("a last entity")["extra"] = json!({ "type": "Property", "value": "only here" });

    project(&mut page, &names(&["pm10"]), &nothing());

    let last = page
        .as_array()
        .expect("a page")
        .last()
        .expect("a last entity");
    assert!(
        last.get("extra").is_none(),
        "the last entity is projected like the first"
    );
    assert!(last.get(SECRET).is_none());
}
