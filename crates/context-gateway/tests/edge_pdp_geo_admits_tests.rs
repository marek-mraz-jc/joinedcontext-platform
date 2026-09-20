//! Edge cases of `pdp::geo::Areas::admits` (T-1928, EP-26, MP-02).
//!
//! Contract, in one sentence: an entity survives the areas only when it carries a readable point
//! that lies inside the caller's own area, when there is one, and inside at least one grant area,
//! when there are any — and an entity that cannot be placed, or an area set that could not be
//! read, is refused rather than passed.
//!
//! This is the last filter before a broker's answer reaches the caller (`app.rs`'s `retain`,
//! `egress::notifications`), so "cannot tell" has to mean "not shown". `edge_pdp_geo_of_tests.rs`
//! covers how the areas are built; these are the entities that arrive from the broker.

use context_gateway::pdp::geo::Areas;
use serde_json::{json, Value};

fn ring(west: f64, south: f64, east: f64, north: f64) -> String {
    format!(
        "georel=within;geometry=Polygon;coordinates=[[[{west},{south}],[{east},{south}],\
         [{east},{north}],[{west},{north}],[{west},{south}]]]"
    )
}

/// A square grant from (0,0) to (10,10) with a hole from (4,4) to (6,6): a district with a
/// restricted block cut out of it, which is how a policy says "everywhere but there".
fn with_hole() -> String {
    "georel=within;geometry=Polygon;coordinates=[\
     [[0,0],[10,0],[10,10],[0,10],[0,0]],\
     [[4,4],[6,4],[6,6],[4,6],[4,4]]]"
        .to_owned()
}

fn at(lon: f64, lat: f64) -> Value {
    json!({
        "id": "urn:ngsi-ld:Vehicle:hel.fi:fleet:1",
        "type": "Vehicle",
        "location": { "type": "GeoProperty", "value": { "type": "Point", "coordinates": [lon, lat] } }
    })
}

fn grants(areas: &[String]) -> Areas {
    Areas::of(areas, None).expect("an area set")
}

/// EP-26: an entity the gateway cannot place is not shown. A broker answer with no location, a
/// location in a shape the parser does not read, or something that is not an entity at all, all
/// leave the same answer: no.
#[test]
fn an_entity_that_cannot_be_placed_is_never_admitted() {
    let areas = grants(&[ring(0.0, 0.0, 10.0, 10.0)]);
    for entity in [
        json!({ "id": "urn:ngsi-ld:Vehicle:hel.fi:fleet:1", "type": "Vehicle" }),
        json!({ "location": null }),
        json!({ "location": { "type": "GeoProperty", "value": { "type": "Polygon", "coordinates": [[[1, 1], [2, 1], [2, 2], [1, 1]]] } } }),
        json!({ "location": { "type": "GeoProperty", "value": { "type": "Point", "coordinates": ["5", "5"] } } }),
        json!({ "location": { "type": "GeoProperty", "value": { "type": "Point", "coordinates": [5] } } }),
        json!({ "location": "POINT(5 5)" }),
        json!([]),
        json!("urn:ngsi-ld:Vehicle:hel.fi:fleet:1"),
        Value::Null,
    ] {
        assert!(!areas.admits(&entity), "{entity}");
    }
}

/// GW16: a point exactly on the boundary is inside, so a vehicle parked on the district border is
/// not made invisible by a rounding rule.
#[test]
fn a_point_on_the_boundary_is_inside() {
    let areas = grants(&[ring(0.0, 0.0, 10.0, 10.0)]);
    for corner in [
        (0.0, 0.0),
        (10.0, 10.0),
        (0.0, 10.0),
        (10.0, 0.0),
        (5.0, 0.0),
        (0.0, 5.0),
    ] {
        assert!(
            areas.admits(&at(corner.0, corner.1)),
            "{corner:?} is on the edge"
        );
    }
}

/// And a hair outside is outside: the tolerance is the one a coordinate carries, not a margin.
#[test]
fn a_point_just_outside_is_refused() {
    let areas = grants(&[ring(0.0, 0.0, 10.0, 10.0)]);
    for outside in [
        (-0.000_001, 5.0),
        (10.000_001, 5.0),
        (5.0, -0.000_001),
        (5.0, 10.000_001),
    ] {
        assert!(!areas.admits(&at(outside.0, outside.1)), "{outside:?}");
    }
}

/// A hole in a grant is a place the grant does not cover, and an entity in it is not shown.
#[test]
fn an_entity_inside_a_hole_is_outside_the_grant() {
    let areas = grants(&[with_hole()]);
    assert!(areas.admits(&at(1.0, 1.0)), "inside the district");
    assert!(
        !areas.admits(&at(5.0, 5.0)),
        "inside the block cut out of it"
    );
    // The boundary rule counts a point on an edge as inside the ring it sits on, so a point on
    // the hole's own edge is inside the hole and therefore outside the grant. That is the
    // conservative direction on both surfaces: a read shows one entity less, a write is refused
    // on the line rather than allowed into the block the policy cut out.
    assert!(!areas.admits(&at(4.0, 4.0)), "on the block's own boundary");
    assert!(!areas.admits(&at(11.0, 11.0)), "outside the district");
}

/// The place is read as longitude then latitude: an entity whose coordinates are swapped is
/// somewhere else, and the filter does not guess that it meant the granted district.
#[test]
fn a_swapped_coordinate_pair_is_not_quietly_corrected() {
    let areas = grants(&[ring(17.0, 48.0, 17.4, 48.4)]);
    assert!(areas.admits(&at(17.1, 48.1)));
    assert!(!areas.admits(&at(48.1, 17.1)));
}

/// The caller's own area is an `and`, not an `or`: being inside a grant is not enough when the
/// caller asked for less, and asking for more does not widen the grant.
#[test]
fn the_caller_area_and_the_grants_both_have_to_hold() {
    let areas = Areas::of(
        &[ring(0.0, 0.0, 10.0, 10.0)],
        Some(&ring(5.0, 5.0, 20.0, 20.0)),
    )
    .expect("a grant and a caller area");

    assert!(areas.admits(&at(7.0, 7.0)), "inside both");
    assert!(
        !areas.admits(&at(2.0, 2.0)),
        "inside the grant, outside what was asked"
    );
    assert!(
        !areas.admits(&at(15.0, 15.0)),
        "inside what was asked, outside the grant"
    );
}

/// An unreadable area set admits nothing, even an entity that is plainly inside the area that did
/// parse: half a filter is not a filter (GW11).
#[test]
fn an_unreadable_area_set_admits_nothing_it_could_place() {
    let areas = grants(&[
        ring(0.0, 0.0, 10.0, 10.0),
        "georel=within;geometry=Circle".to_owned(),
    ]);
    assert!(!areas.admits(&at(5.0, 5.0)));
    assert!(!areas.admits(&at(50.0, 50.0)));
}

/// The rest of the entity is not the filter's business: the same place is admitted whatever the
/// entity carries beside it, and nothing about the entity is changed by asking.
#[test]
fn only_the_location_decides_and_the_entity_is_left_alone() {
    let areas = grants(&[ring(0.0, 0.0, 10.0, 10.0)]);
    let mut entity = at(5.0, 5.0);
    entity["type"] = json!("Depot");
    entity["secret"] = json!({ "type": "Property", "value": "Sirota" });
    entity["geoQ"] = json!(ring(100.0, 100.0, 110.0, 110.0));
    let before = entity.clone();

    assert!(areas.admits(&entity));
    assert_eq!(
        entity, before,
        "admits reads the entity, it does not rewrite it"
    );
}

/// Several grant areas are a union and every one of them is asked, including the last.
#[test]
fn an_entity_in_any_one_grant_area_is_admitted() {
    let areas = grants(&[
        ring(0.0, 0.0, 1.0, 1.0),
        ring(10.0, 10.0, 11.0, 11.0),
        ring(20.0, 20.0, 21.0, 21.0),
    ]);

    assert!(areas.admits(&at(0.5, 0.5)));
    assert!(areas.admits(&at(10.5, 10.5)));
    assert!(areas.admits(&at(20.5, 20.5)));
    assert!(
        !areas.admits(&at(5.0, 5.0)),
        "the gap between the first two"
    );
}

/// A caller area alone still filters, with no grant to fall back on.
#[test]
fn a_caller_area_alone_still_filters() {
    let areas = Areas::of(&[], Some(&ring(0.0, 0.0, 10.0, 10.0))).expect("a caller area");
    assert!(areas.admits(&at(5.0, 5.0)));
    assert!(!areas.admits(&at(15.0, 5.0)));
}

/// Asking twice answers the same: the area set holds no state that an earlier entity could move.
#[test]
fn the_same_question_twice_gets_the_same_answer() {
    let areas = grants(&[with_hole()]);
    for _ in 0..3 {
        assert!(areas.admits(&at(1.0, 1.0)));
        assert!(!areas.admits(&at(5.0, 5.0)));
        assert!(!areas.admits(&json!({ "id": "urn:ngsi-ld:Vehicle:hel.fi:fleet:1" })));
    }
}

/// Negative coordinates are ordinary places: an area west of Greenwich or south of the equator
/// filters like any other.
#[test]
fn an_area_across_zero_works_like_any_other() {
    let areas = grants(&[ring(-1.0, -1.0, 1.0, 1.0)]);
    assert!(areas.admits(&at(0.0, 0.0)));
    assert!(areas.admits(&at(-0.5, -0.5)));
    assert!(!areas.admits(&at(-1.5, 0.0)));
}
