//! Edge cases of `pdp::geo::Polygon::within` (T-1922, EP-26, MP-02).
//!
//! Contract, in one sentence: it answers true only when this polygon really lies inside the other
//! — every vertex covered and no edge crossing one of the other's edges — because `geo::intersect`
//! forwards the caller's own area to the broker exactly when this says the caller asked for less
//! than it was granted (GW11, T-0149). A false positive here hands the broker an area wider than
//! the grant.

use context_gateway::pdp::geo::{granted_polygon, Polygon};

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

fn box_of(west: f64, south: f64, east: f64, north: f64) -> Polygon {
    polygon(&[&[
        (west, south),
        (east, south),
        (east, north),
        (west, north),
        (west, south),
    ]])
}

/// The first case: a polygon that sticks out anywhere is not inside, however small the overhang.
#[test]
fn a_polygon_that_sticks_out_anywhere_is_not_within() {
    let outer = box_of(0.0, 0.0, 10.0, 10.0);
    for inner in [
        box_of(-1.0, 0.0, 5.0, 5.0),
        box_of(5.0, 5.0, 11.0, 11.0),
        box_of(0.0, -1e-9, 10.0, 10.0),
        box_of(-100.0, -100.0, 100.0, 100.0),
        box_of(20.0, 20.0, 30.0, 30.0),
    ] {
        assert!(
            !inner.within(&outer),
            "an overhanging area was called contained"
        );
    }
}

/// A smaller area inside is inside, which is the case that lets the caller's own `geoQ` be
/// forwarded instead of the grant's.
#[test]
fn a_smaller_area_inside_is_within() {
    let outer = box_of(0.0, 0.0, 10.0, 10.0);

    assert!(box_of(2.0, 2.0, 8.0, 8.0).within(&outer));
    assert!(box_of(0.1, 0.1, 0.2, 0.2).within(&outer));
}

/// An area that is exactly the grant is contained: the caller asked for precisely what it holds,
/// and refusing that would forward the grant instead for no reason.
#[test]
fn the_same_area_is_within_itself() {
    let area = box_of(0.0, 0.0, 10.0, 10.0);

    assert!(area.within(&area));
    assert!(box_of(0.0, 0.0, 10.0, 10.0).within(&area));
}

/// Touching is not crossing: an area sharing an edge or a corner with the grant, and inside it,
/// stays contained.
#[test]
fn an_area_touching_the_boundary_from_inside_is_within() {
    let outer = box_of(0.0, 0.0, 10.0, 10.0);

    assert!(
        box_of(0.0, 0.0, 5.0, 5.0).within(&outer),
        "sharing two edges"
    );
    assert!(
        box_of(0.0, 5.0, 10.0, 10.0).within(&outer),
        "sharing three edges"
    );
    assert!(
        box_of(9.0, 9.0, 10.0, 10.0).within(&outer),
        "sharing a corner"
    );
}

/// A concave grant does not contain an area that reaches into its notch, even when every vertex
/// of that area is inside: the crossing test is what catches it.
#[test]
fn an_area_crossing_a_concave_grant_is_not_within() {
    // A C shape: the square with a bite taken out of its eastern side.
    let grant = polygon(&[&[
        (0.0, 0.0),
        (10.0, 0.0),
        (10.0, 4.0),
        (4.0, 4.0),
        (4.0, 6.0),
        (10.0, 6.0),
        (10.0, 10.0),
        (0.0, 10.0),
        (0.0, 0.0),
    ]]);
    // Both vertices columns sit in the two arms, but the middle of the area is in the bite.
    let across = polygon(&[&[(2.0, 2.0), (8.0, 2.0), (8.0, 8.0), (2.0, 8.0), (2.0, 2.0)]]);

    assert!(
        !across.within(&grant),
        "an area spanning the bite is not contained"
    );
}

/// An area inside a hole of the grant is not inside the grant: the hole is a carve-out.
#[test]
fn an_area_inside_a_hole_is_not_within() {
    let grant = polygon(&[
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
        !box_of(4.5, 4.5, 5.5, 5.5).within(&grant),
        "an area entirely inside the hole is not granted"
    );
    assert!(
        box_of(0.5, 0.5, 3.0, 3.0).within(&grant),
        "an area beside the hole is"
    );
    // An area that ENCLOSES the hole is judged contained today, and a read is then answered from
    // inside the carve-out: T-2327 holds that case and its red test, which is not on main.
}

/// A polygon that is not an area contains nothing, so a grant drawn as a line can never say the
/// caller asked for less than it holds.
///
/// The other direction is contained: a caller that sends a degenerate area lying on the grant is
/// forwarded its own line, which narrows the answer to nothing rather than widening it, so the
/// asymmetry is safe and is written down here.
#[test]
fn a_grant_drawn_as_a_line_contains_no_area() {
    let line = polygon(&[&[(0.0, 0.0), (10.0, 0.0)]]);
    let area = box_of(0.0, 0.0, 10.0, 10.0);

    assert!(!area.within(&line), "a line contains no area");
    assert!(
        line.within(&area),
        "a degenerate caller area on the grant's edge narrows to nothing, which is safe"
    );
}

/// Containment is not symmetric, and that asymmetry is the whole decision: the smaller one is the
/// one that may be forwarded.
#[test]
fn containment_is_not_symmetric() {
    let outer = box_of(0.0, 0.0, 10.0, 10.0);
    let inner = box_of(2.0, 2.0, 8.0, 8.0);

    assert!(inner.within(&outer));
    assert!(!outer.within(&inner));
}

/// Two areas that overlap without either containing the other are not contained either way, which
/// is the case where the grant is forwarded and the caller's area is applied on the way back.
#[test]
fn two_overlapping_areas_contain_neither() {
    let one = box_of(0.0, 0.0, 10.0, 10.0);
    let other = box_of(5.0, 5.0, 15.0, 15.0);

    assert!(!one.within(&other));
    assert!(!other.within(&one));
}

/// The same pair always answers the same way, and neither polygon is changed by the question.
#[test]
fn the_answer_is_repeatable() {
    let outer = box_of(0.0, 0.0, 10.0, 10.0);
    let inner = box_of(2.0, 2.0, 8.0, 8.0);

    for _ in 0..16 {
        assert!(inner.within(&outer));
        assert!(!outer.within(&inner));
    }
    assert_eq!(inner, box_of(2.0, 2.0, 8.0, 8.0));
    assert_eq!(outer, box_of(0.0, 0.0, 10.0, 10.0));
}
