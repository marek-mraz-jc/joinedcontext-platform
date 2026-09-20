//! Edge cases of `translators::ogc::items` (T-1951, EP-33, EP-36, EP-26, MP-02).
//!
//! Contract, in one sentence: the FeatureCollection carries exactly the entities the caller was
//! given that can be drawn, counts what it carries, and offers a `next` link only when the page
//! the broker returned was full — the cursor being the gateway's own, never a number a client
//! computes.
//!
//! The entities are already projected and already filtered by the PDP when they arrive here
//! (`app.rs` retains and projects before translating), so this function's job is to add nothing:
//! no entity it was not given, no attribute the projection removed, and no count that says more
//! than it returned — a total that disagrees with the page is the same oracle the entity itself
//! would be (R22).

use context_gateway::translators::ogc::{cursor, items, offset_of};
use serde_json::{json, Value};

const ENDPOINT: &str = "https://city.example/api/endpoint/mluyob4nz52lok3ssk7pgn5vwt";
const TYPE: &str = "AirQualityObserved";
const STAMP: &str = "2026-09-18T09:00:00Z";

fn station(local: usize) -> Value {
    json!({
        "id": format!("urn:ngsi-ld:AirQualityObserved:bb.sk:ovzdusie:station-{local:03}"),
        "type": TYPE,
        "pm10": { "type": "Property", "value": 34.2 },
        "location": { "type": "GeoProperty", "value": { "type": "Point", "coordinates": [19.15, 48.73] } }
    })
}

fn page(count: usize) -> Value {
    Value::Array((0..count).map(station).collect())
}

fn link_of<'a>(collection: &'a Value, rel: &str) -> Option<&'a Value> {
    collection
        .get("links")?
        .as_array()?
        .iter()
        .find(|link| link.get("rel").and_then(Value::as_str) == Some(rel))
}

fn href(collection: &Value, rel: &str) -> String {
    link_of(collection, rel)
        .and_then(|link| link.get("href"))
        .and_then(Value::as_str)
        .unwrap_or_else(|| panic!("a {rel} link"))
        .to_owned()
}

/// EP-36: the `next` link is offered on the only evidence a page carries — that it was full —
/// and never on a page the broker did not fill, or a client walks into an empty page believing
/// there is more.
#[test]
fn the_next_link_follows_the_page_being_full() {
    for (returned, limit, expected) in [
        (10, 10, true),
        (11, 10, true),
        (9, 10, false),
        (0, 10, false),
        (1, 1, true),
        (0, 0, true), // a limit of zero is a full page of nothing; the handler never sends one
    ] {
        let collection = items(ENDPOINT, TYPE, &page(returned), limit, 0, "", STAMP);
        assert_eq!(
            link_of(&collection, "next").is_some(),
            expected,
            "{returned} entities against a limit of {limit}"
        );
    }
}

/// The cursor is the gateway's own and decodes back to the offset of the page after this one.
#[test]
fn the_next_cursor_is_this_offset_plus_the_limit() {
    for (limit, offset) in [(10, 0), (10, 10), (1, 0), (3, 999)] {
        let collection = items(ENDPOINT, TYPE, &page(limit), limit, offset, "", STAMP);
        let next = href(&collection, "next");
        let cursor = next
            .rsplit("next=")
            .next()
            .expect("the cursor is the last parameter");
        assert_eq!(offset_of(cursor), Some(offset + limit), "{next}");
    }
}

/// The caller's own query rides on the `self` link, and the `next` link is the same query with
/// this gateway's cursor — one cursor, whatever the caller sent.
#[test]
fn the_callers_query_is_carried_and_its_cursor_is_replaced() {
    let asked = "limit=10&bbox=19.1%2C48.7%2C19.2%2C48.8&next=Z2FyYmFnZQ";
    let collection = items(ENDPOINT, TYPE, &page(10), 10, 10, asked, STAMP);

    assert!(
        href(&collection, "self").ends_with(asked),
        "the query as it was sent"
    );
    let next = href(&collection, "next");
    assert!(next.contains("bbox=19.1%2C48.7%2C19.2%2C48.8"), "{next}");
    assert_eq!(next.matches("next=").count(), 1, "one cursor only: {next}");
    assert!(
        !next.contains("Z2FyYmFnZQ"),
        "the caller's stale cursor is gone: {next}"
    );
    assert_eq!(
        next.rsplit("next=").next().and_then(offset_of),
        Some(20),
        "{next}"
    );
}

/// A cursor is opaque: it is this gateway's encoding and a client that invents one starts the
/// collection from the beginning rather than being told how the paging works.
#[test]
fn a_cursor_that_this_gateway_did_not_write_is_not_an_offset() {
    assert_eq!(offset_of(&cursor(42)), Some(42));
    for invented in [
        "",
        "42",
        "b2Zmc2V0",
        "b2Zmc2V0PS0x",
        "!!!",
        "b2Zmc2V0PWFiYw",
    ] {
        assert_eq!(offset_of(invented), None, "{invented:?}");
    }
}

/// An entity that cannot be drawn is not a feature, and the count says what came out, not what
/// went in.
#[test]
fn an_entity_without_a_geometry_is_not_counted_as_a_feature() {
    let mixed = json!([
        station(1),
        { "id": "urn:ngsi-ld:AirQualityObserved:bb.sk:ovzdusie:station-002", "type": TYPE },
        station(3),
        "urn:ngsi-ld:AirQualityObserved:bb.sk:ovzdusie:station-004",
        null,
    ]);
    let collection = items(ENDPOINT, TYPE, &mixed, 10, 0, "", STAMP);

    assert_eq!(collection["numberReturned"], json!(2));
    assert_eq!(
        collection["features"].as_array().expect("features").len(),
        2
    );
}

/// An empty page is an empty FeatureCollection, not an error and not a missing member: a client
/// reading `features` must always find an array.
#[test]
fn an_empty_page_is_an_empty_feature_collection() {
    for empty in [json!([]), json!({}), json!("nothing"), Value::Null] {
        let collection = items(ENDPOINT, TYPE, &empty, 10, 0, "", STAMP);
        assert_eq!(collection["type"], json!("FeatureCollection"), "{empty}");
        assert_eq!(collection["numberReturned"], json!(0), "{empty}");
        assert_eq!(collection["features"], json!([]), "{empty}");
        assert!(link_of(&collection, "next").is_none(), "{empty}");
    }
}

/// Every feature keeps the attributes the projection left on its entity and gains nothing.
#[test]
fn a_feature_carries_what_its_entity_carried_and_nothing_more() {
    let collection = items(ENDPOINT, TYPE, &page(1), 10, 0, "", STAMP);
    let feature = &collection["features"][0];

    assert_eq!(feature["type"], json!("Feature"));
    assert_eq!(feature["geometry"]["type"], json!("Point"));
    assert_eq!(feature["properties"]["pm10"], json!(34.2));
    assert!(
        feature["properties"].get("operatorPhone").is_none(),
        "nothing the entity did not carry"
    );
}

/// The timestamp is the handler's, forwarded as written: the translator does not read a clock.
#[test]
fn the_timestamp_is_the_one_the_handler_passed() {
    for stamp in [STAMP, "", "not a time"] {
        let collection = items(ENDPOINT, TYPE, &page(1), 10, 0, "", stamp);
        assert_eq!(collection["timeStamp"], json!(stamp));
    }
}

/// The links are the endpoint's own root, so a client never follows one to another endpoint.
#[test]
fn every_link_stays_inside_this_endpoint() {
    let collection = items(ENDPOINT, TYPE, &page(10), 10, 0, "limit=10", STAMP);
    for rel in ["self", "collection", "next"] {
        assert!(
            href(&collection, rel).starts_with(&format!("{ENDPOINT}/ogc")),
            "{rel}: {}",
            href(&collection, rel)
        );
    }
    assert_eq!(
        href(&collection, "collection"),
        format!("{ENDPOINT}/ogc/features/collections/{TYPE}")
    );
}

/// A query that carries characters of its own is carried into the link as it was, so a client
/// that follows `next` sends the same filter it sent the first time.
#[test]
fn a_query_with_its_own_punctuation_survives_into_the_next_link() {
    let asked = "datetime=2026-09-18T09%3A00%3A00Z%2F..&filter=pm10%20%3E%2010&limit=10";
    let next = href(
        &items(ENDPOINT, TYPE, &page(10), 10, 0, asked, STAMP),
        "next",
    );

    assert!(
        next.contains("datetime=2026-09-18T09%3A00%3A00Z%2F.."),
        "{next}"
    );
    assert!(next.contains("filter=pm10%20%3E%2010"), "{next}");
}

/// The same page twice is the same answer: nothing in here carries state between calls.
#[test]
fn the_same_page_translates_the_same_way_twice() {
    let once = items(ENDPOINT, TYPE, &page(3), 10, 0, "limit=10", STAMP);
    let twice = items(ENDPOINT, TYPE, &page(3), 10, 0, "limit=10", STAMP);
    assert_eq!(once, twice);
}

/// A large page is translated whole, and the count is the count.
#[test]
fn a_large_page_is_translated_to_its_last_entity() {
    let collection = items(ENDPOINT, TYPE, &page(1000), 1000, 0, "", STAMP);
    assert_eq!(collection["numberReturned"], json!(1000));
    assert_eq!(
        collection["features"][999]["id"],
        json!("urn:ngsi-ld:AirQualityObserved:bb.sk:ovzdusie:station-999")
    );
}
