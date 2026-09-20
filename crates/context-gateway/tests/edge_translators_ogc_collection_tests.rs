//! Edge cases of `translators::ogc::collection` (T-1950, EP-31, EP-32, EP-26, MP-02).
//!
//! Contract, in one sentence: one collection describes one entity type with the single CRS the
//! platform stores and the extent the caller's own data spans, and an extent nobody could compute
//! is advertised as the unbounded one rather than as a narrow box that would be a lie.
//!
//! The extent is sampled from the page the caller was served, which is data they may already
//! read; an extent computed over the whole collection would advertise the existence of entities
//! they may not (EP-32). The other half is the links: everything a client follows from here has to
//! stay inside this endpoint.

use context_gateway::translators::ogc::{collection, collections, extent_of, Extent};
use serde_json::{json, Value};

const ENDPOINT: &str = "https://city.example/api/endpoint/mluyob4nz52lok3ssk7pgn5vwt";
const CRS84: &str = "http://www.opengis.net/def/crs/OGC/1.3/CRS84";

fn href(entry: &Value, rel: &str) -> String {
    entry["links"]
        .as_array()
        .expect("links")
        .iter()
        .find(|link| link["rel"] == json!(rel))
        .and_then(|link| link["href"].as_str())
        .unwrap_or_else(|| panic!("a {rel} link"))
        .to_owned()
}

/// EP-32: no extent means "not known", which OGC writes as the whole world and an open interval.
/// A collection nobody sampled must not claim a box.
#[test]
fn an_unknown_extent_is_the_whole_world_and_an_open_interval() {
    let entry = collection(ENDPOINT, "AirQualityObserved", "The stations", None);

    assert_eq!(
        entry["extent"]["spatial"]["bbox"],
        json!([[-180.0, -90.0, 180.0, 90.0]])
    );
    assert_eq!(
        entry["extent"]["temporal"]["interval"],
        json!([[Value::Null, Value::Null]])
    );
    assert_eq!(entry["extent"]["spatial"]["crs"], json!(CRS84));
}

/// An extent that was computed is carried as it was computed, in the order OGC writes a box.
#[test]
fn a_known_extent_is_carried_as_it_was_measured() {
    let extent = Extent {
        bbox: Some([19.1, 48.7, 19.2, 48.8]),
        start: Some("2026-09-18T08:00:00Z".to_owned()),
        end: Some("2026-09-18T09:00:00Z".to_owned()),
    };
    let entry = collection(
        ENDPOINT,
        "AirQualityObserved",
        "The stations",
        Some(&extent),
    );

    assert_eq!(
        entry["extent"]["spatial"]["bbox"],
        json!([[19.1, 48.7, 19.2, 48.8]])
    );
    assert_eq!(
        entry["extent"]["temporal"]["interval"],
        json!([["2026-09-18T08:00:00Z", "2026-09-18T09:00:00Z"]])
    );
}

/// Half an extent is half an extent: a page with times and no geometry says so on each axis
/// separately, rather than one axis silently standing in for the other.
#[test]
fn each_axis_of_a_partial_extent_is_answered_on_its_own() {
    let times_only = Extent {
        bbox: None,
        start: Some("2026-09-18T08:00:00Z".to_owned()),
        end: None,
    };
    let entry = collection(ENDPOINT, "AirQualityObserved", "", Some(&times_only));

    assert_eq!(
        entry["extent"]["spatial"]["bbox"],
        json!([[-180.0, -90.0, 180.0, 90.0]]),
        "nothing carried a geometry"
    );
    assert_eq!(
        entry["extent"]["temporal"]["interval"],
        json!([["2026-09-18T08:00:00Z", Value::Null]]),
        "open at the end nobody has seen yet"
    );
}

/// The one CRS the platform stores is the one advertised, on both members OGC asks for.
#[test]
fn one_crs_is_advertised_and_it_is_the_one_that_is_stored() {
    let entry = collection(ENDPOINT, "AirQualityObserved", "", None);
    assert_eq!(entry["crs"], json!([CRS84]));
    assert_eq!(entry["storageCrs"], json!(CRS84));
    assert_eq!(entry["itemType"], json!("feature"));
}

/// The links stay inside this endpoint and name this collection, so a client never walks out of
/// the door it came in through.
#[test]
fn the_links_stay_inside_this_endpoint() {
    let entry = collection(ENDPOINT, "Depot", "", None);
    assert_eq!(
        href(&entry, "self"),
        format!("{ENDPOINT}/ogc/features/collections/Depot")
    );
    assert_eq!(
        href(&entry, "items"),
        format!("{ENDPOINT}/ogc/features/collections/Depot/items")
    );
}

/// The id and the title are the type's own name: a client that reads the id and asks for
/// `/collections/{id}/items` gets this collection and not another.
#[test]
fn the_id_is_the_type_name_and_the_link_agrees_with_it() {
    for name in ["AirQualityObserved", "Depot", "Vehicle2", "A1"] {
        let entry = collection(ENDPOINT, name, "", None);
        assert_eq!(entry["id"], json!(name));
        assert_eq!(entry["title"], json!(name));
        assert!(
            href(&entry, "self").ends_with(&format!("/collections/{name}")),
            "{name}"
        );
    }
}

/// The name is put into the href without escaping. Struck as impossible: an entity type is
/// validated against `^[A-Z][A-Za-z0-9]{1,63}$` before any manifest carrying it is accepted
/// (`jc-core/src/names.rs:109`), so no name can carry a `/`, a `?`, a space or a `#`. Written down
/// because the link would break silently if that ever loosened.
#[test]
fn a_type_name_is_a_pascal_case_word_and_needs_no_escaping() {
    for refused in ["a/b", "A B", "A#b", "A?b", "A%2Fb", "", "Ä1"] {
        assert!(
            jc_core::names::validate_entity_type(refused).is_err(),
            "{refused:?} would have to be escaped in a link"
        );
    }
}

/// The description is the endpoint's own text, carried as data: a description carrying a quote or
/// a tag is a JSON string and never structure.
#[test]
fn the_description_is_carried_as_data() {
    for description in [
        "Stanice kvality ovzdušia",
        "\"quoted\"",
        "</script><script>alert(1)</script>",
        "a\nnewline",
        "",
    ] {
        let entry = collection(ENDPOINT, "AirQualityObserved", description, None);
        assert_eq!(entry["description"], json!(description));
        let rendered = serde_json::to_string(&entry).expect("serialises");
        assert!(
            !rendered.contains("\n"),
            "{description:?} broke the document"
        );
    }
}

/// The list is the same description for every collection in it, and one entry per type, in the
/// order the caller's grants left them.
#[test]
fn the_list_carries_one_entry_per_type_in_order() {
    let types = vec!["Depot".to_owned(), "AirQualityObserved".to_owned()];
    let list = collections(ENDPOINT, &types, "The endpoint");

    let ids: Vec<&str> = list["collections"]
        .as_array()
        .expect("collections")
        .iter()
        .map(|entry| entry["id"].as_str().expect("an id"))
        .collect();
    assert_eq!(ids, vec!["Depot", "AirQualityObserved"]);
    assert_eq!(
        href(&list["collections"][0], "self"),
        format!("{ENDPOINT}/ogc/features/collections/Depot")
    );
}

/// A caller whose grants leave no type gets a list with no collections, not one with everything.
#[test]
fn a_caller_with_no_types_gets_an_empty_list() {
    let list = collections(ENDPOINT, &[], "The endpoint");
    assert_eq!(list["collections"], json!([]));
    assert!(list["links"]
        .as_array()
        .is_some_and(|links| !links.is_empty()));
}

/// The extent is measured from the entities the caller was served and from nothing else.
#[test]
fn the_extent_is_measured_from_the_page_that_was_served() {
    let page = json!([
        { "id": "urn:ngsi-ld:AirQualityObserved:bb.sk:ovzdusie:station-01", "type": "AirQualityObserved",
          "location": { "type": "GeoProperty", "value": { "type": "Point", "coordinates": [19.1, 48.7] } },
          "pm10": { "type": "Property", "value": 1, "observedAt": "2026-09-18T08:00:00Z" } },
        { "id": "urn:ngsi-ld:AirQualityObserved:bb.sk:ovzdusie:station-02", "type": "AirQualityObserved",
          "location": { "type": "GeoProperty", "value": { "type": "Point", "coordinates": [19.2, 48.8] } },
          "pm10": { "type": "Property", "value": 2, "observedAt": "2026-09-18T09:00:00Z" } },
    ]);
    let extent = extent_of(&page);

    assert_eq!(extent.bbox, Some([19.1, 48.7, 19.2, 48.8]));
    assert_eq!(extent.start.as_deref(), Some("2026-09-18T08:00:00Z"));
    assert_eq!(extent.end.as_deref(), Some("2026-09-18T09:00:00Z"));

    // An empty page measures nothing, and nothing is what is advertised.
    assert_eq!(extent_of(&json!([])), Extent::default());
    assert_eq!(extent_of(&json!("not a page")), Extent::default());
}

/// The same input gives the same document, so a cache in front of the endpoint cannot be split by
/// an ordering this function invented.
#[test]
fn the_same_collection_renders_the_same_way_twice() {
    let extent = Extent {
        bbox: Some([1.0, 2.0, 3.0, 4.0]),
        start: None,
        end: None,
    };
    let once = collection(ENDPOINT, "Depot", "d", Some(&extent));
    assert_eq!(once, collection(ENDPOINT, "Depot", "d", Some(&extent)));
}
