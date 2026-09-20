//! Edge cases of `pdp::geo::entity_point` (T-1926, EP-26, MP-02).
//!
//! Contract, in one sentence: an entity has a point only when its own top-level `location` holds a
//! GeoJSON `Point` — normalized under `value` or simplified — whose first two coordinates are
//! numbers; everything else is `None`, which every caller turns into a refusal.
//!
//! The refusal is the point of the function. `Areas::admits` does not show an entity it cannot
//! place, and `write_guard::check_location` refuses a write whose `location` it cannot read, so a
//! shape read half-way — a Polygon taken for its first position, a string parsed as WKT — would
//! place an entity outside the granted area inside it.

use context_gateway::pdp::geo::entity_point;
use serde_json::{json, Value};

/// A normalized NGSI-LD GeoProperty, which is what a broker answers by default.
fn normalized(geometry: Value) -> Value {
    json!({ "id": "urn:ngsi-ld:Vehicle:hel.fi:fleet:1", "type": "Vehicle",
            "location": { "type": "GeoProperty", "value": geometry } })
}

/// The simplified representation: the same entity with `?options=keyValues`.
fn simplified(geometry: Value) -> Value {
    json!({ "id": "urn:ngsi-ld:Vehicle:hel.fi:fleet:1", "type": "Vehicle",
            "location": geometry })
}

fn point() -> Value {
    json!({ "type": "Point", "coordinates": [17.1, 48.15] })
}

/// EP-26: a geometry that is not a point is not read as one. A Polygon's first position is not
/// the entity's place, and reading it would put a fleet-wide area inside a district grant.
#[test]
fn a_geometry_that_is_not_a_point_has_no_point() {
    for geometry in [
        json!({ "type": "Polygon", "coordinates": [[[17.0, 48.0], [17.2, 48.0], [17.2, 48.2], [17.0, 48.0]]] }),
        json!({ "type": "LineString", "coordinates": [[17.0, 48.0], [17.2, 48.2]] }),
        json!({ "type": "MultiPoint", "coordinates": [[17.0, 48.0], [17.2, 48.2]] }),
        json!({ "type": "GeometryCollection", "geometries": [point()] }),
        json!({ "type": "Poin", "coordinates": [17.1, 48.15] }),
        json!({ "type": "Point ", "coordinates": [17.1, 48.15] }),
        json!({ "type": " Point", "coordinates": [17.1, 48.15] }),
        json!({ "type": "Point\u{0}", "coordinates": [17.1, 48.15] }),
        json!({ "type": 1, "coordinates": [17.1, 48.15] }),
        json!({ "coordinates": [17.1, 48.15] }),
    ] {
        assert_eq!(
            entity_point(&normalized(geometry.clone())),
            None,
            "{geometry}"
        );
        assert_eq!(
            entity_point(&simplified(geometry.clone())),
            None,
            "{geometry}"
        );
    }
}

/// The GeoJSON type is compared without case, because both `Point` and `point` are on the wire
/// from real sources; nothing else about it is forgiving.
#[test]
fn the_geometry_type_is_compared_without_case() {
    for spelling in ["Point", "point", "POINT", "PoInT"] {
        let geometry = json!({ "type": spelling, "coordinates": [17.1, 48.15] });
        assert_eq!(
            entity_point(&normalized(geometry)),
            Some((17.1, 48.15)),
            "{spelling}"
        );
    }
}

/// MP-02: the normalized and the simplified representation of the same entity are the same place.
#[test]
fn a_normalized_and_a_simplified_location_read_the_same() {
    assert_eq!(entity_point(&normalized(point())), Some((17.1, 48.15)));
    assert_eq!(entity_point(&simplified(point())), Some((17.1, 48.15)));
}

/// An entity that says nothing about where it is has no point, and so does one whose `location`
/// is not an object at all.
#[test]
fn an_entity_without_a_readable_location_has_no_point() {
    for entity in [
        json!({ "id": "urn:ngsi-ld:Vehicle:hel.fi:fleet:1", "type": "Vehicle" }),
        json!({ "location": null }),
        json!({ "location": "POINT(17.1 48.15)" }), // WKT is not GeoJSON here
        json!({ "location": [17.1, 48.15] }),
        json!({ "location": 0 }),
        json!({ "location": {} }),
        json!({}),
        Value::Null,
        json!([{ "location": { "type": "Point", "coordinates": [17.1, 48.15] } }]),
        json!("urn:ngsi-ld:Vehicle:hel.fi:fleet:1"),
    ] {
        assert_eq!(entity_point(&entity), None, "{entity}");
    }
}

/// Only the entity's own `location` counts: a location nested inside another attribute is another
/// attribute's data, not the place the grant is checked against.
#[test]
fn a_location_nested_in_another_attribute_is_not_the_entity_point() {
    let entity = json!({
        "id": "urn:ngsi-ld:Vehicle:hel.fi:fleet:1",
        "type": "Vehicle",
        "address": { "type": "Property", "value": { "location": point() } },
        "observationSpace": { "type": "GeoProperty", "value": point() }
    });

    assert_eq!(entity_point(&entity), None);
}

/// Two numbers, and they are numbers: a coordinate that arrived as a string or as `null` leaves
/// the entity unplaceable rather than half-placed.
#[test]
fn coordinates_that_are_not_two_numbers_are_refused() {
    for coordinates in [
        json!([]),
        json!([17.1]),
        json!(["17.1", "48.15"]),
        json!([17.1, "48.15"]),
        json!([null, 48.15]),
        json!([17.1, null]),
        json!([[17.1, 48.15]]),
        json!({ "lon": 17.1, "lat": 48.15 }),
        json!("17.1,48.15"),
        json!(17.1),
        Value::Null,
    ] {
        let geometry = json!({ "type": "Point", "coordinates": coordinates });
        assert_eq!(
            entity_point(&normalized(geometry.clone())),
            None,
            "{geometry}"
        );
    }
}

/// A position may carry an altitude; the place is still the first two numbers.
#[test]
fn a_third_coordinate_is_ignored() {
    let geometry = json!({ "type": "Point", "coordinates": [17.1, 48.15, 154.0] });
    assert_eq!(entity_point(&normalized(geometry)), Some((17.1, 48.15)));
}

/// The pair is longitude then latitude, GeoJSON order, so a swapped entity is somewhere else and
/// not silently corrected into the grant.
#[test]
fn the_pair_is_longitude_then_latitude() {
    assert_eq!(entity_point(&normalized(point())), Some((17.1, 48.15)));
    let swapped = json!({ "type": "Point", "coordinates": [48.15, 17.1] });
    assert_eq!(entity_point(&normalized(swapped)), Some((48.15, 17.1)));
}

/// A GeoProperty wrapper decides what is read: when `location` carries a `value`, that is the
/// geometry, and a wrapper whose value is not a point has no point — the wrapper's own `type`
/// (`GeoProperty`) is never what is compared.
#[test]
fn the_value_of_a_geoproperty_is_the_geometry_not_the_wrapper() {
    let wrapper_says_point = json!({
        "location": { "type": "Point", "value": { "type": "Polygon", "coordinates": [] } }
    });
    assert_eq!(entity_point(&wrapper_says_point), None);

    let value_says_point = json!({
        "location": { "type": "GeoProperty", "value": point(), "observedAt": "2026-09-18T00:00:00Z" }
    });
    assert_eq!(entity_point(&value_says_point), Some((17.1, 48.15)));
}

/// Negative and zero coordinates are ordinary places, not missing ones.
#[test]
fn zero_and_negative_coordinates_are_a_place() {
    for coordinates in [(0.0, 0.0), (-0.12, 51.5), (17.1, -48.15), (-180.0, -90.0)] {
        let geometry = json!({ "type": "Point", "coordinates": [coordinates.0, coordinates.1] });
        assert_eq!(entity_point(&normalized(geometry)), Some(coordinates));
    }
}

/// Reading the place does not change the entity, so nothing downstream sees a payload the
/// projection did not produce.
#[test]
fn reading_the_point_leaves_the_entity_untouched() {
    let entity = normalized(point());
    let before = entity.clone();
    assert_eq!(entity_point(&entity), Some((17.1, 48.15)));
    assert_eq!(entity, before);
}
