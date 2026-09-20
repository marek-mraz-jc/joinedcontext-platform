//! Edge cases of `pdp::geo::granted_polygon` (T-1924, EP-26, MP-02).
//!
//! Contract, in one sentence: it answers `Some` only for a `geoQ` that really is
//! `georel=within` over a polygon whose coordinates it read completely, and `None` for everything
//! else — because `None` for a grant makes `intersect` forward that grant and filter the answer
//! against an unreadable area, which admits nothing, while a `Some` built out of a half-understood
//! string would be an area the policy never drew (GW11, T-0149).

use context_gateway::pdp::geo::{granted_polygon, polygon_from};
use serde_json::json;

const RING: &str = "[[[0,0],[10,0],[10,10],[0,10],[0,0]]]";

fn within(coordinates: &str) -> String {
    format!("georel=within;geometry=Polygon;coordinates={coordinates}")
}

/// The first case: a relation that is not `within` is not a containment check, so it is not read as
/// one, whatever it says next.
#[test]
fn a_relation_other_than_within_is_not_an_area() {
    for georel in [
        "intersects",
        "equals",
        "disjoint",
        "overlaps",
        "contains",
        "near;maxDistance==2000",
        "withiN",
        "WITHIN",
        "Within",
        "within-ish",
        "",
    ] {
        let geo_q = format!("georel={georel};geometry=Polygon;coordinates={RING}");
        assert!(
            granted_polygon(&geo_q).is_none(),
            "georel={georel:?} is not `within`, so no area is read from it"
        );
    }
}

/// The happy shape, so the refusals above are known to be about the relation and not the fixture:
/// a box is read, its inside is inside and its outside is not.
#[test]
fn a_within_polygon_is_read_and_holds_its_own_inside() {
    let area = granted_polygon(&within(RING)).expect("a within polygon is readable");

    assert!(area.contains((5.0, 5.0)), "the middle is inside");
    assert!(
        area.contains((0.0, 5.0)),
        "the edge is inside, a border write is not refused"
    );
    assert!(
        !area.contains((10.5, 5.0)),
        "and a point beyond the edge is not"
    );
}

/// `geometry` is matched without regard to case, because NGSI-LD clients spell the type both ways;
/// anything that is not a polygon is refused, a `MultiPolygon` included.
#[test]
fn the_geometry_name_is_case_insensitive_but_only_polygon_is_read() {
    for spelling in ["Polygon", "polygon", "POLYGON", "pOLYgon"] {
        let geo_q = format!("georel=within;geometry={spelling};coordinates={RING}");
        assert!(granted_polygon(&geo_q).is_some(), "{spelling} is a polygon");
    }
    for spelling in [
        "MultiPolygon",
        "Point",
        "LineString",
        "Polygonal",
        "Poly gon",
        "",
    ] {
        let geo_q = format!("georel=within;geometry={spelling};coordinates={RING}");
        assert!(
            granted_polygon(&geo_q).is_none(),
            "{spelling} is not a polygon"
        );
    }
}

/// A missing part is not guessed: without a relation, without a geometry or without coordinates
/// there is no area, and an empty `geoQ` is the same answer.
#[test]
fn a_missing_part_is_never_guessed() {
    for geo_q in [
        "".to_owned(),
        "georel=within".to_owned(),
        format!("georel=within;coordinates={RING}"),
        "georel=within;geometry=Polygon".to_owned(),
        format!("geometry=Polygon;coordinates={RING}"),
        format!("coordinates={RING}"),
        ";;;".to_owned(),
        "georel;geometry;coordinates".to_owned(),
    ] {
        assert!(
            granted_polygon(&geo_q).is_none(),
            "{geo_q:?} is not a complete area"
        );
    }
}

/// The key is matched exactly: a space, a mixed case or a prefix on the name is not that key, so a
/// `geoQ` whose parts arrived with whitespace around them is refused rather than half-read.
#[test]
fn a_key_with_a_space_or_another_case_is_not_that_key() {
    for geo_q in [
        format!(" georel=within;geometry=Polygon;coordinates={RING}"),
        format!("georel =within;geometry=Polygon;coordinates={RING}"),
        format!("GeoRel=within;geometry=Polygon;coordinates={RING}"),
        format!("georel=within; geometry =Polygon;coordinates={RING}"),
        format!("georel=within;geometry=Polygon; coordinates ={RING}"),
    ] {
        assert!(
            granted_polygon(&geo_q).is_none(),
            "{geo_q:?} has no key this reads"
        );
    }
    // The value, unlike the key, is trimmed: a space after the `=` is a client's spacing, not a
    // different relation.
    let padded = format!("georel= within ;geometry= Polygon ;coordinates= {RING} ");
    assert!(
        granted_polygon(&padded).is_some(),
        "a padded value is still that value"
    );
}

/// A repeated key is the last one: the string is one caller's own question, and reading the last
/// spelling of it cannot widen anything, because `within` still decides against the grant.
#[test]
fn a_repeated_key_is_the_last_one() {
    let narrowed = format!(
        "{};coordinates=[[[0,0],[1,0],[1,1],[0,1],[0,0]]]",
        within(RING)
    );
    let area = granted_polygon(&narrowed).expect("the last coordinates are read");
    assert!(area.contains((0.5, 0.5)), "the last area is the one read");
    assert!(
        !area.contains((5.0, 5.0)),
        "the first is gone, not merged into it"
    );

    let relaxed = format!("{};georel=intersects", within(RING));
    assert!(
        granted_polygon(&relaxed).is_none(),
        "a last relation that is not `within` refuses"
    );
}

/// Coordinates that are not a list of rings of pairs of numbers are not coordinates: a string, a
/// number, `null`, an object, a bare ring, a `MultiPolygon`'s depth and a pair of strings all refuse.
#[test]
fn coordinates_of_the_wrong_shape_refuse() {
    for coordinates in [
        "not-json",
        "\"[[[0,0]]]\"",
        "0",
        "null",
        "{}",
        "{\"type\":\"Polygon\"}",
        "[[0,0],[10,0],[10,10],[0,0]]",
        "[[[[0,0],[10,0],[10,10],[0,0]]]]",
        "[[[\"0\",\"0\"],[\"10\",\"0\"],[\"10\",\"10\"],[\"0\",\"0\"]]]",
        "[[[0],[10],[10]]]",
        "[[[0,0],[10,0],null]]",
        "[[[0,0],[10,0],[10,true]]]",
    ] {
        assert!(
            granted_polygon(&within(coordinates)).is_none(),
            "coordinates={coordinates} is not a readable polygon"
        );
    }
}

/// A list of one and an empty list are read, and both hold nothing: a ring under three points is not
/// an area, so an entity can never be shown to be inside it.
#[test]
fn a_ring_too_short_to_be_an_area_holds_nothing() {
    for coordinates in ["[[]]", "[[[0,0]]]", "[[[0,0],[10,10]]]"] {
        let area = granted_polygon(&within(coordinates))
            .unwrap_or_else(|| panic!("coordinates={coordinates} parses"));
        for point in [(0.0, 0.0), (5.0, 5.0), (10.0, 10.0), (-1.0, -1.0)] {
            assert!(
                !area.contains(point),
                "coordinates={coordinates} holds no point, {point:?} included"
            );
        }
    }
}

/// The rings after the first are holes, and every one of them is a carve-out: a point in any hole is
/// outside the area.
#[test]
fn every_ring_after_the_first_is_a_hole() {
    let two_holes = "[[[0,0],[30,0],[30,30],[0,30],[0,0]],\
                     [[2,2],[8,2],[8,8],[2,8],[2,2]],\
                     [[20,20],[28,20],[28,28],[20,28],[20,20]]]";
    let area = granted_polygon(&within(two_holes)).expect("a polygon with two holes is readable");

    assert!(area.contains((15.0, 15.0)), "between the holes is inside");
    assert!(!area.contains((5.0, 5.0)), "the first hole is out");
    assert!(!area.contains((24.0, 24.0)), "and so is the second");
}

/// A third number per point is altitude: it is ignored rather than refused, because a client that
/// sends `[lon,lat,alt]` asked for the same area on the ground.
#[test]
fn a_third_number_per_point_is_ignored() {
    let with_altitude = "[[[0,0,12.5],[10,0,12.5],[10,10,12.5],[0,10,12.5],[0,0,12.5]]]";
    let area = granted_polygon(&within(with_altitude)).expect("altitude does not refuse");

    assert!(
        area.contains((5.0, 5.0)),
        "the area on the ground is the same"
    );
    assert!(!area.contains((11.0, 5.0)));
}

/// Coordinates are read in GeoJSON order, longitude first: a `geoQ` written latitude-first is a
/// different area and not the same one read leniently.
#[test]
fn the_order_is_longitude_then_latitude() {
    let helsinki = "[[[24.9,60.1],[25.1,60.1],[25.1,60.3],[24.9,60.3],[24.9,60.1]]]";
    let area = granted_polygon(&within(helsinki)).expect("readable");

    assert!(
        area.contains((25.0, 60.2)),
        "lon 25, lat 60.2 is in Helsinki"
    );
    assert!(
        !area.contains((60.2, 25.0)),
        "the swapped pair is in the Arabian Sea, not the area"
    );
}

/// Percent-encoding, once or twice, is not decoded here: the caller's query is decoded by the web
/// layer, and a string that still carries `%5B` is not JSON, so no area is read from it.
#[test]
fn percent_encoded_coordinates_are_not_decoded_here() {
    for coordinates in [
        "%5B%5B%5B0,0%5D,%5B10,0%5D,%5B10,10%5D,%5B0,0%5D%5D%5D",
        "%255B%255B%255B0,0%255D%255D%255D",
    ] {
        assert!(
            granted_polygon(&within(coordinates)).is_none(),
            "{coordinates} is not JSON"
        );
    }
}

/// Unicode digits, a NUL and CR/LF inside the coordinates are refused: an area is read from JSON
/// numbers only, and a value that would carry a line break into a log or a header is never an area.
#[test]
fn unicode_digits_and_control_characters_refuse() {
    for coordinates in [
        "[[[０,０],[１０,０],[１０,１０],[０,０]]]",
        "[[[0,0],[10,0],[10,10],[0,0]]]\0",
        "[[[0,0],[10,0],[10,10],[0,0]]]\r\nX-Injected: 1",
        "[[[٠,٠],[١٠,٠],[١٠,١٠],[٠,٠]]]",
    ] {
        assert!(
            granted_polygon(&within(coordinates)).is_none(),
            "{coordinates:?} is not a readable area"
        );
    }
}

/// A `;` is the separator of a `geoQ`, so anything appended after the coordinates is another part:
/// a part with no `=` is ignored, and the area read is the coordinates and nothing else.
#[test]
fn a_part_appended_after_the_coordinates_is_ignored() {
    let appended = format!(
        "{}; DROP TABLE entity",
        within("[[[0,0],[10,0],[10,10],[0,0]]]")
    );
    let area = granted_polygon(&appended).expect("the coordinates are still coordinates");

    assert!(
        area.contains((5.0, 1.0)),
        "the triangle that was written is the area"
    );
    assert!(
        !area.contains((1.0, 5.0)),
        "and it is not widened by what followed it"
    );
}

/// The same refusals hold for `polygon_from`, the value-level door the write guard uses, so a
/// payload cannot reach a polygon by a route the `geoQ` parser guards.
#[test]
fn the_value_level_parser_refuses_the_same_shapes() {
    for value in [
        json!(null),
        json!(0),
        json!("[[[0,0]]]"),
        json!({}),
        json!([]),
        json!([[[0, 0], [10, 0], [10, "10"]]]),
        json!([[[[0, 0], [10, 0], [10, 10], [0, 0]]]]),
    ] {
        assert!(polygon_from(&value).is_none(), "{value} is not a polygon");
    }
    let area = polygon_from(&json!([[[0, 0], [10, 0], [10, 10], [0, 10], [0, 0]]]))
        .expect("a ring of pairs is a polygon");
    assert!(area.contains((5.0, 5.0)));
}
