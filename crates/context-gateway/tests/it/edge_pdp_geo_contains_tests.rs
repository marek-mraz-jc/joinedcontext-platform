//! Edge cases of `pdp::geo::Polygon::contains` (T-1921, EP-26, MP-02).
//!
//! Contract, in one sentence: it answers true only for a point the grant's area really covers —
//! the boundary counts as inside, a hole counts as outside, and a ring that is not a ring covers
//! nothing — because the write guard turns a false into a refusal and a true into a write that
//! leaves the granted district (GW11, GW16).

use context_gateway::pdp::geo::{granted_polygon, polygon_from, Polygon};
use serde_json::json;

/// A `within` polygon `geoQ` from rings of `(lon, lat)` pairs.
fn polygon(rings: &[&[(f64, f64)]]) -> Polygon {
    let coordinates: Vec<String> = rings
        .iter()
        .map(|ring| {
            let points: Vec<String> = ring.iter().map(|(x, y)| format!("[{x},{y}]")).collect();
            format!("[{}]", points.join(","))
        })
        .collect();
    granted_polygon(&format!(
        "georel=within;geometry=Polygon;coordinates=[{}]",
        coordinates.join(",")
    ))
    .expect("the fixture is a within polygon")
}

/// The Banská Bystrica box of the golden policy.
fn district() -> Polygon {
    polygon(&[&[
        (19.10, 48.70),
        (19.20, 48.70),
        (19.20, 48.76),
        (19.10, 48.76),
        (19.10, 48.70),
    ]])
}

/// The first case: a point outside is outside, including a point just outside the edge, which is
/// what a write one street over looks like.
#[test]
fn a_point_outside_is_outside() {
    let area = district();
    for point in [
        (19.09, 48.73),
        (19.21, 48.73),
        (19.15, 48.69),
        (19.15, 48.77),
        (19.10 - 1e-9, 48.73),
        (0.0, 0.0),
        (-19.15, -48.73),
        (180.0, 90.0),
        (1e308, 1e308),
    ] {
        assert!(!area.contains(point), "{point:?} was admitted");
    }
}

/// A point inside is inside, and the centre is the ordinary case.
#[test]
fn a_point_inside_is_inside() {
    let area = district();
    for point in [(19.15, 48.73), (19.101, 48.701), (19.199, 48.759)] {
        assert!(area.contains(point), "{point:?} was refused");
    }
}

/// The boundary counts as inside: a district border must not silently refuse a write on it.
#[test]
fn the_boundary_and_the_vertices_count_as_inside() {
    let area = district();
    for point in [
        (19.10, 48.70), // a vertex
        (19.20, 48.76), // the opposite vertex
        (19.15, 48.70), // on the southern edge
        (19.10, 48.73), // on the western edge
        (19.20, 48.73), // on the eastern edge
        (19.15, 48.76), // on the northern edge
    ] {
        assert!(
            area.contains(point),
            "{point:?} on the boundary was refused"
        );
    }
}

/// A hole is outside, and its rim is outside with it.
///
/// The outer boundary counts as inside so a write on a district border is not silently refused;
/// a hole's rim goes the other way, because `contains` asks `ring_contains` for the hole too and
/// the boundary is inside the hole. Both directions fail closed — the carve-out is what a hole is
/// for — so this is the behaviour, written down rather than assumed.
#[test]
fn a_hole_and_its_rim_are_both_outside() {
    let area = polygon(&[
        &[
            (0.0, 0.0),
            (10.0, 0.0),
            (10.0, 10.0),
            (0.0, 10.0),
            (0.0, 0.0),
        ],
        &[(4.0, 4.0), (6.0, 4.0), (6.0, 6.0), (4.0, 6.0), (4.0, 4.0)],
    ]);

    assert!(
        area.contains((1.0, 1.0)),
        "outside the hole, inside the outer ring"
    );
    assert!(
        !area.contains((5.0, 5.0)),
        "the middle of the hole is not covered"
    );
    assert!(
        !area.contains((4.0, 5.0)),
        "the rim of the hole belongs to the hole"
    );
    assert!(
        area.contains((4.0 - 1e-6, 5.0)),
        "a hair outside the hole is covered again"
    );
    assert!(!area.contains((11.0, 5.0)), "outside the outer ring");
}

/// A ring that cannot be a ring covers nothing: two points are a line, one is a dot, none is
/// nothing, and a grant drawn like that must refuse every write rather than admit every one.
#[test]
fn a_ring_of_fewer_than_three_points_covers_nothing() {
    for ring in [
        json!([[[0.0, 0.0]]]),
        json!([[[0.0, 0.0], [10.0, 0.0]]]),
        json!([[]]),
    ] {
        let area = polygon_from(&ring).expect("the shape parses, even though it is not an area");
        for point in [(0.0, 0.0), (5.0, 0.0), (1.0, 1.0)] {
            assert!(!area.contains(point), "{ring} admitted {point:?}");
        }
    }
}

/// A degenerate polygon of zero area covers its own point and nothing else.
#[test]
fn a_polygon_of_zero_area_covers_only_its_own_boundary() {
    let area = polygon(&[&[(1.0, 1.0), (1.0, 1.0), (1.0, 1.0)]]);

    assert!(area.contains((1.0, 1.0)));
    assert!(!area.contains((1.0 + 1e-6, 1.0)));
}

/// An unclosed ring is closed implicitly, because the edges wrap: a policy that forgot to repeat
/// its first point still draws the area it meant.
#[test]
fn an_unclosed_ring_is_the_same_area_as_a_closed_one() {
    let closed = polygon(&[&[
        (0.0, 0.0),
        (10.0, 0.0),
        (10.0, 10.0),
        (0.0, 10.0),
        (0.0, 0.0),
    ]]);
    let unclosed = polygon(&[&[(0.0, 0.0), (10.0, 0.0), (10.0, 10.0), (0.0, 10.0)]]);

    for point in [(5.0, 5.0), (0.0, 5.0), (11.0, 5.0), (-1.0, -1.0)] {
        assert_eq!(
            closed.contains(point),
            unclosed.contains(point),
            "{point:?}"
        );
    }
}

/// The answer does not depend on the winding order: a policy drawn clockwise covers what the same
/// polygon drawn counter-clockwise covers.
#[test]
fn the_winding_order_does_not_change_the_area() {
    let clockwise = polygon(&[&[
        (0.0, 0.0),
        (0.0, 10.0),
        (10.0, 10.0),
        (10.0, 0.0),
        (0.0, 0.0),
    ]]);
    let counter = polygon(&[&[
        (0.0, 0.0),
        (10.0, 0.0),
        (10.0, 10.0),
        (0.0, 10.0),
        (0.0, 0.0),
    ]]);

    for point in [(5.0, 5.0), (0.0, 0.0), (11.0, 5.0)] {
        assert_eq!(
            clockwise.contains(point),
            counter.contains(point),
            "{point:?}"
        );
    }
}

/// A concave area does not cover its own notch: the classic ray-casting mistake is to count the
/// bounding box.
#[test]
fn a_concave_area_does_not_cover_its_notch() {
    // An L: the square (0,0)-(10,10) with the top-right quarter cut away.
    let area = polygon(&[&[
        (0.0, 0.0),
        (10.0, 0.0),
        (10.0, 5.0),
        (5.0, 5.0),
        (5.0, 10.0),
        (0.0, 10.0),
        (0.0, 0.0),
    ]]);

    assert!(area.contains((2.0, 2.0)));
    assert!(area.contains((8.0, 2.0)));
    assert!(area.contains((2.0, 8.0)));
    assert!(
        !area.contains((8.0, 8.0)),
        "the cut-away quarter is not covered"
    );
}

/// The same point against the same area always answers the same way, and the answer does not
/// depend on how many times it is asked: the write guard asks per entity of a batch.
#[test]
fn the_answer_is_repeatable() {
    let area = district();
    for _ in 0..16 {
        assert!(area.contains((19.15, 48.73)));
        assert!(!area.contains((19.25, 48.73)));
    }
}

/// A coordinate pair is `(longitude, latitude)` in that order, which is GeoJSON's own order: a
/// swapped pair falls outside, and that is how a grant on Banská Bystrica refuses a point written
/// the other way round.
#[test]
fn the_pair_is_longitude_then_latitude() {
    let area = district();

    assert!(area.contains((19.15, 48.73)));
    assert!(
        !area.contains((48.73, 19.15)),
        "a swapped pair is not the same point"
    );
}
