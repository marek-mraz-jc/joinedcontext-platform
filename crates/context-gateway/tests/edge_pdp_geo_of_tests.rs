//! Edge cases of `pdp::geo::Areas::of` (T-1927, EP-26, MP-02).
//!
//! Contract, in one sentence: `of` answers `None` only when neither a grant nor the caller drew an
//! area — nothing to filter — and otherwise answers an area set that admits nothing at all unless
//! every one of the given `geoQ` strings parsed as a `within` polygon.
//!
//! An area the gateway cannot read is an area no entity can be shown to be inside: the broker is
//! not what enforces a policy (GW11), so a grant the parser does not understand may not turn into
//! "no filter". Every case below asks the question through `admits`, because that is the only
//! thing the rest of the gateway ever does with the answer.

use context_gateway::pdp::geo::Areas;
use serde_json::{json, Value};

/// A `geoQ` around the square given by its corners, the way a Policy writes one.
fn within(west: f64, south: f64, east: f64, north: f64) -> String {
    format!(
        "georel=within;geometry=Polygon;coordinates=[[[{west},{south}],[{east},{south}],\
         [{east},{north}],[{west},{north}],[{west},{south}]]]"
    )
}

/// The whole of a small city.
fn city() -> String {
    within(17.0, 48.0, 17.4, 48.4)
}

/// One district of it, inside [`city`].
fn district() -> String {
    within(17.0, 48.0, 17.2, 48.2)
}

/// A district of another city, sharing no point with [`city`].
fn elsewhere() -> String {
    within(21.0, 48.0, 21.2, 48.2)
}

fn at(lon: f64, lat: f64) -> Value {
    json!({
        "id": "urn:ngsi-ld:Vehicle:hel.fi:fleet:1",
        "type": "Vehicle",
        "location": { "type": "GeoProperty", "value": { "type": "Point", "coordinates": [lon, lat] } }
    })
}

fn areas(grants: &[String], caller: Option<&str>) -> Areas {
    Areas::of(grants, caller).expect("an area set, because something was drawn")
}

/// GW11: one unreadable grant area refuses everything. Not "the readable ones decide", which
/// would widen the filter to whatever the parser happened to understand.
#[test]
fn one_unreadable_area_admits_nothing_at_all() {
    let inside = at(17.1, 48.1);
    for grants in [
        vec![String::new()],
        vec!["not a geoQ at all".to_owned()],
        vec![
            district(),
            "georel=near;maxDistance==2000;geometry=Point;coordinates=[17.1,48.1]".to_owned(),
        ],
        vec![
            district(),
            "georel=within;geometry=Polygon;coordinates=nonsense".to_owned(),
        ],
        vec![district(), "georel=within;geometry=Polygon".to_owned()],
    ] {
        assert!(
            !areas(&grants, None).admits(&inside),
            "an area the gateway cannot read must admit nothing: {grants:?}"
        );
    }

    // The caller's own area is held to the same rule.
    assert!(!areas(
        &[district()],
        Some("georel=within;geometry=Polygon;coordinates=[]")
    )
    .admits(&inside));
    assert!(!areas(&[], Some("garbage")).admits(&inside));
}

/// Nothing drawn is nothing to filter: the answer needs no second pass, and saying so is not the
/// same as an empty area set that would admit nothing.
#[test]
fn no_area_at_all_is_no_filter() {
    assert!(Areas::of(&[], None).is_none());
    assert!(Areas::of(&[], Some(&city())).is_some());
    assert!(Areas::of(&[city()], None).is_some());
    assert!(Areas::of(&[city()], Some(&district())).is_some());
}

/// Several grant areas are a union: an entity has to be inside one of them, not all of them.
#[test]
fn several_grant_areas_are_a_union() {
    let both = [district(), elsewhere()];
    assert!(areas(&both, None).admits(&at(17.1, 48.1)), "in the first");
    assert!(areas(&both, None).admits(&at(21.1, 48.1)), "in the second");
    assert!(
        !areas(&both, None).admits(&at(19.0, 48.1)),
        "between the two is in neither"
    );
}

/// The caller's own area and the grants are an intersection: the caller can only ever narrow.
#[test]
fn the_caller_area_narrows_the_grants_and_never_widens_them() {
    let set = areas(&[district()], Some(&city()));
    assert!(set.admits(&at(17.1, 48.1)), "inside both");
    assert!(
        !set.admits(&at(17.3, 48.3)),
        "inside the caller's wider area but outside the grant"
    );

    let narrow_caller = areas(&[city()], Some(&district()));
    assert!(
        !narrow_caller.admits(&at(17.3, 48.3)),
        "outside what was asked for"
    );
}

/// `within` is the only relation this parser claims to understand, and it is compared exactly:
/// anything else is unreadable, not "close enough".
#[test]
fn a_relation_other_than_within_is_unreadable() {
    let inside = at(17.1, 48.1);
    for georel in [
        "intersects",
        "contains",
        "disjoint",
        "overlaps",
        "WITHIN",
        "Within",
        "with in",
    ] {
        let grant = city().replace("georel=within", &format!("georel={georel}"));
        assert!(
            !areas(&[grant], None).admits(&inside),
            "georel={georel} must not pass as a containment check"
        );
    }
}

/// The geometry name is compared without case, because `Polygon` and `polygon` both arrive; a
/// geometry that is not a polygon is unreadable.
#[test]
fn the_geometry_must_be_a_polygon_whatever_its_case() {
    let inside = at(17.1, 48.1);
    for spelling in ["Polygon", "polygon", "POLYGON"] {
        let grant = city().replace("geometry=Polygon", &format!("geometry={spelling}"));
        assert!(areas(&[grant], None).admits(&inside), "{spelling}");
    }
    for other in ["Point", "MultiPolygon", "LineString", "Poly gon"] {
        let grant = city().replace("geometry=Polygon", &format!("geometry={other}"));
        assert!(!areas(&[grant], None).admits(&inside), "{other}");
    }
}

/// The parts of a `geoQ` are named, so their order does not matter, and a part nobody named is
/// ignored rather than making the whole unreadable.
#[test]
fn the_parts_may_come_in_any_order_and_an_unknown_part_is_ignored() {
    let inside = at(17.1, 48.1);
    let coordinates = "[[[17.0,48.0],[17.4,48.0],[17.4,48.4],[17.0,48.4],[17.0,48.0]]]";
    for geo_q in [
        format!("geometry=Polygon;coordinates={coordinates};georel=within"),
        format!("georel=within;coordinates={coordinates};geometry=Polygon"),
        format!("georel=within;geometry=Polygon;coordinates={coordinates};geoproperty=location"),
        format!("georel= within ;geometry= Polygon ;coordinates= {coordinates} "),
    ] {
        assert!(
            areas(std::slice::from_ref(&geo_q), None).admits(&inside),
            "{geo_q}"
        );
    }
}

/// A part that is missing or empty leaves the area unreadable rather than unbounded.
#[test]
fn a_missing_part_is_unreadable_not_unbounded() {
    let inside = at(17.1, 48.1);
    for grant in [
        "georel=within;geometry=Polygon",
        "georel=within;coordinates=[[[17.0,48.0],[17.4,48.0],[17.4,48.4],[17.0,48.0]]]",
        "geometry=Polygon;coordinates=[[[17.0,48.0],[17.4,48.0],[17.4,48.4],[17.0,48.0]]]",
        "georel=;geometry=Polygon;coordinates=[[[17.0,48.0],[17.4,48.0],[17.4,48.4],[17.0,48.0]]]",
        "georel=within;geometry=;coordinates=[[[17.0,48.0],[17.4,48.0],[17.4,48.4],[17.0,48.0]]]",
        "georel=within;geometry=Polygon;coordinates=",
        "",
        ";;;",
    ] {
        assert!(
            !areas(&[grant.to_owned()], None).admits(&inside),
            "{grant:?} draws no area, so it admits nothing"
        );
    }
}

/// The same area given twice is the same area: a duplicated grant does not make the set stricter
/// or looser, and it does not make it unreadable.
#[test]
fn a_duplicated_grant_area_changes_nothing() {
    let twice = [district(), district()];
    assert!(areas(&twice, None).admits(&at(17.1, 48.1)));
    assert!(!areas(&twice, None).admits(&at(17.3, 48.3)));
}

/// A large grant list is parsed once and every area in it counts, so the last grant of a long
/// policy set is not quietly dropped.
#[test]
fn every_area_of_a_long_grant_list_counts() {
    let mut grants: Vec<String> = (0..64)
        .map(|step| {
            let west = 100.0 + f64::from(step);
            within(west, 10.0, west + 0.5, 10.5)
        })
        .collect();
    grants.push(district());

    let set = areas(&grants, None);
    assert!(set.admits(&at(17.1, 48.1)), "the area added last");
    assert!(set.admits(&at(163.2, 10.2)), "the sixty-fourth area");
    assert!(
        !set.admits(&at(163.8, 10.2)),
        "and the gap between two of them"
    );
}

/// An entity the areas cannot place is never admitted, whichever side drew the area.
#[test]
fn an_entity_without_a_place_is_admitted_by_no_area_set() {
    let nowhere = json!({ "id": "urn:ngsi-ld:Vehicle:hel.fi:fleet:1", "type": "Vehicle" });
    assert!(!areas(&[district()], None).admits(&nowhere));
    assert!(!areas(&[], Some(&city())).admits(&nowhere));
    assert!(!areas(&[district()], Some(&city())).admits(&nowhere));
}
