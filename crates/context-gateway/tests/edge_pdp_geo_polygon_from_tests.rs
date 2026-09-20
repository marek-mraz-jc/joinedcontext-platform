//! Edge cases of `pdp::geo::polygon_from` (T-1925, EP-26, MP-02).
//!
//! Contract, in one sentence: a GeoJSON `coordinates` value becomes a polygon only when it is a
//! non-empty array of rings of numeric positions — the first ring the outer one, the rest holes —
//! and every other shape answers `None`, never a polygon built from the part that parsed.
//!
//! That last half is the security half. The callers turn `None` into a refusal (`Areas::of` marks
//! the area set unreadable, `write_guard::check_location` refuses the write), so a polygon that
//! silently dropped the hole it could not read, or kept the outer ring of a malformed list, would
//! be a grant area wider than the one the policy drew.
//!
//! `polygon_containment` in `geo_tests.rs` has the happy path; these are the shapes that arrive
//! when a policy is hand-written, a broker answers something else, or an attacker picks the input.

use context_gateway::pdp::geo::polygon_from;
use serde_json::{json, Value};

/// A square ring from (0,0) to (10,10), written the way GeoJSON closes a ring.
fn square() -> Value {
    json!([
        [0.0, 0.0],
        [10.0, 0.0],
        [10.0, 10.0],
        [0.0, 10.0],
        [0.0, 0.0]
    ])
}

/// A hole from (4,4) to (6,6) inside [`square`].
fn hole() -> Value {
    json!([[4.0, 4.0], [6.0, 4.0], [6.0, 6.0], [4.0, 6.0], [4.0, 4.0]])
}

/// EP-26: a ring the parser cannot read refuses the whole polygon, so a hole is never dropped
/// and the area never grows to the outer ring alone.
#[test]
fn a_ring_that_does_not_parse_refuses_the_whole_polygon() {
    for broken in [
        json!([square(), "hole"]),
        json!([square(), [[4.0, 4.0], [6.0, "4"], [6.0, 6.0]]]),
        json!([square(), [[4.0, 4.0], [6.0], [6.0, 6.0]]]),
        json!([square(), null]),
        json!([square(), { "type": "Polygon" }]),
    ] {
        assert_eq!(
            polygon_from(&broken),
            None,
            "an unreadable hole must not leave the outer ring standing: {broken}"
        );
    }

    // An empty ring is not an unreadable ring: it reads as a hole that encloses nothing, which
    // is the safe direction — it narrows nothing and admits nothing of its own.
    let empty_hole = polygon_from(&json!([square(), []])).expect("an outer ring and an empty hole");
    assert!(
        empty_hole.contains((5.0, 5.0)),
        "an empty hole takes nothing away"
    );

    // An empty ring is not an unreadable ring: it reads as a hole that encloses nothing, which
    // is the safe direction — it narrows nothing and admits nothing of its own.
    let empty_hole = polygon_from(&json!([square(), []])).expect("an outer ring and an empty hole");
    assert!(
        empty_hole.contains((5.0, 5.0)),
        "an empty hole takes nothing away"
    );

    // And when both rings read, the hole is a hole: the point inside it is outside the polygon.
    let polygon = polygon_from(&json!([square(), hole()])).expect("two readable rings");
    assert!(polygon.contains((1.0, 1.0)), "inside the outer ring");
    assert!(!polygon.contains((5.0, 5.0)), "inside the hole is outside");
}

/// MP-02: only an array of rings is a polygon. Everything else answers "cannot tell".
#[test]
fn a_coordinates_value_that_is_not_an_array_of_rings_is_not_a_polygon() {
    for value in [
        Value::Null,
        json!(0),
        json!(-1),
        json!(""),
        json!("[[[0,0],[1,0],[1,1],[0,0]]]"), // the array as a string is still a string
        json!(true),
        json!({ "coordinates": [[[0, 0], [1, 0], [1, 1], [0, 0]]] }),
        json!([]), // no ring at all: there is no outer ring to be inside of
    ] {
        assert_eq!(polygon_from(&value), None, "{value}");
    }
}

/// A MultiPolygon's coordinates are one level deeper. Read as a polygon they would be a ring of
/// rings, and a grant drawn as a MultiPolygon must be refused rather than half-read.
#[test]
fn a_multi_polygon_is_not_read_as_a_polygon() {
    assert_eq!(polygon_from(&json!([[square(), hole()]])), None);
    // A LineString is one level shallower, and its positions are numbers, not pairs.
    assert_eq!(polygon_from(&json!([0.0, 0.0, 10.0, 10.0])), None);
}

/// A position is `[longitude, latitude]` in that order; a swapped pair is a different place, not
/// an error, which is why the order is pinned by a test and not by a comment.
#[test]
fn positions_are_longitude_then_latitude() {
    let bratislava = polygon_from(&json!([[
        [17.0, 48.0],
        [17.2, 48.0],
        [17.2, 48.2],
        [17.0, 48.2],
        [17.0, 48.0]
    ]]))
    .expect("a readable ring");

    assert!(bratislava.contains((17.1, 48.1)));
    assert!(
        !bratislava.contains((48.1, 17.1)),
        "latitude first is somewhere in the Indian Ocean, not in the grant"
    );
}

/// GeoJSON allows a third element (altitude) on a position; it is not part of the area.
#[test]
fn a_third_element_of_a_position_is_ignored() {
    let with_altitude = polygon_from(&json!([[
        [0.0, 0.0, 154.0],
        [10.0, 0.0, 154.0],
        [10.0, 10.0, 154.0],
        [0.0, 10.0, 154.0],
        [0.0, 0.0, 154.0]
    ]]))
    .expect("positions with an altitude");

    assert_eq!(
        with_altitude,
        polygon_from(&square_ring()).expect("the same ring without altitudes")
    );
}

/// A number is a number: a coordinate that arrived as a string, as `null` or as an object is not
/// one, whatever it looks like.
#[test]
fn a_coordinate_that_is_not_a_number_is_refused() {
    for ring in [
        json!([[["0", "0"], [10, 0], [10, 10], [0, 0]]]),
        json!([[[" 0 ", 0], [10, 0], [10, 10], [0, 0]]]),
        json!([[[null, 0], [10, 0], [10, 10], [0, 0]]]),
        json!([[[{ "lon": 0 }, 0], [10, 0], [10, 10], [0, 0]]]),
        json!([[[true, false], [10, 0], [10, 10], [0, 0]]]),
        json!([[[0, 0], [10, 0], [10, 10], [0]]]), // the last position is one number short
    ] {
        assert_eq!(polygon_from(&ring), None, "{ring}");
    }
}

/// A ring of fewer than three positions encloses nothing, and the parser keeps it rather than
/// refusing: what matters is that it admits no point, so a degenerate grant grants nothing.
#[test]
fn a_degenerate_ring_parses_and_admits_no_point() {
    for degenerate in [
        json!([[]]),
        json!([[[0.0, 0.0]]]),
        json!([[[0.0, 0.0], [10.0, 10.0]]]),
    ] {
        let polygon = polygon_from(&degenerate).expect("a ring, however short");
        for point in [(0.0, 0.0), (5.0, 5.0), (10.0, 10.0), (-1.0, -1.0)] {
            assert!(
                !polygon.contains(point),
                "{degenerate} must enclose nothing, and {point:?} is in it"
            );
        }
    }
}

/// The rings are taken in order: the first is the outer one, and any further ring is a hole,
/// however many there are.
#[test]
fn the_first_ring_is_the_outer_one_and_every_later_ring_is_a_hole() {
    let second_hole = json!([[1.0, 1.0], [2.0, 1.0], [2.0, 2.0], [1.0, 2.0], [1.0, 1.0]]);
    let polygon =
        polygon_from(&json!([square(), hole(), second_hole])).expect("an outer ring and two holes");

    assert!(polygon.contains((8.0, 8.0)), "outside both holes");
    assert!(!polygon.contains((5.0, 5.0)), "the first hole");
    assert!(!polygon.contains((1.5, 1.5)), "the second hole");

    // Order decides: the hole read first becomes the area, and the square becomes a hole in it.
    let reversed = polygon_from(&json!([hole(), square()])).expect("the same two rings, swapped");
    assert!(
        !reversed.contains((8.0, 8.0)),
        "a point outside the new outer ring"
    );
}

/// A ring the author did not close is closed by the parser, because the last edge wraps.
#[test]
fn a_ring_that_is_not_closed_is_read_as_closed() {
    let open = json!([[[0.0, 0.0], [10.0, 0.0], [10.0, 10.0], [0.0, 10.0]]]);
    let closed = polygon_from(&open).expect("an open ring");

    assert!(closed.contains((5.0, 5.0)));
    assert!(!closed.contains((11.0, 5.0)));
}

/// Duplicated and repeated positions are a shape a hand-drawn grant produces; they do not change
/// the area and they do not make the ring unreadable.
#[test]
fn duplicate_positions_do_not_change_the_area() {
    let duplicated = json!([[
        [0.0, 0.0],
        [0.0, 0.0],
        [10.0, 0.0],
        [10.0, 10.0],
        [10.0, 10.0],
        [0.0, 10.0],
        [0.0, 0.0]
    ]]);
    let polygon = polygon_from(&duplicated).expect("a ring with repeated positions");

    assert!(polygon.contains((5.0, 5.0)));
    assert!(!polygon.contains((15.0, 5.0)));
}

/// Coordinates well outside the WGS 84 range are still just numbers: the parser does not clamp
/// them, and a point outside such a ring stays outside.
#[test]
fn coordinates_outside_the_wgs84_range_are_read_as_written() {
    let absurd = polygon_from(&json!([[
        [-1.0e6, -1.0e6],
        [1.0e6, -1.0e6],
        [1.0e6, 1.0e6],
        [-1.0e6, 1.0e6],
        [-1.0e6, -1.0e6]
    ]]))
    .expect("a ring of absurd numbers");

    assert!(absurd.contains((0.0, 0.0)), "the whole world is inside it");
    assert!(!absurd.contains((2.0e6, 0.0)), "and this is outside it");
}

/// The same value read twice reads the same, and reading it does not change it.
#[test]
fn reading_a_coordinates_value_leaves_it_untouched() {
    let value = json!([square(), hole()]);
    let once = polygon_from(&value).expect("readable");
    let twice = polygon_from(&value).expect("readable the second time too");

    assert_eq!(once, twice);
    assert_eq!(value, json!([square(), hole()]));
}

fn square_ring() -> Value {
    json!([square()])
}
