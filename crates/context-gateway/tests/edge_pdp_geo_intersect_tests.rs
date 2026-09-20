//! Edge cases of `pdp::geo::intersect` (T-1923, EP-26, MP-02).
//!
//! Contract, in one sentence: whatever it returns, the entities the caller can end up seeing are
//! inside every grant area — either because the `geo_q` it forwards is no wider than the grant, or
//! because the area the broker was not given comes back in `grants`/`caller` and is applied to the
//! answer here (GW11, T-0149, R14). It never clips two areas into a third, and `areas` always lists
//! every grant, because a write is decided against the grant and not against what was forwarded
//! (T-0807).

use context_gateway::pdp::geo::{intersect, Areas};
use serde_json::json;

/// `georel=within` over a box, the shape a policy's `geoQ` actually carries.
fn box_q(west: f64, south: f64, east: f64, north: f64) -> String {
    format!(
        "georel=within;geometry=Polygon;coordinates=[[[{west},{south}],[{east},{south}],\
         [{east},{north}],[{west},{north}],[{west},{south}]]]"
    )
}

fn at(lon: f64, lat: f64) -> serde_json::Value {
    json!({
        "id": "urn:ngsi-ld:AirQualityObserved:hel.fi:helsinki:probe-1",
        "type": "AirQualityObserved",
        "location": {"type": "GeoProperty", "value": {"type": "Point", "coordinates": [lon, lat]}},
    })
}

/// What a caller of this result would actually be shown, grants and own area both applied.
fn admits(result: &context_gateway::pdp::geo::Intersected, entity: &serde_json::Value) -> bool {
    match Areas::of(&result.grants, result.caller.as_deref()) {
        Some(areas) => areas.admits(entity),
        // No second filter: the broker was given a `geo_q` that is already the narrower area.
        None => true,
    }
}

/// The first case, and the one most likely to be red: two areas that only overlap. The broker may
/// not be handed the caller's wider corner, and the part of the caller's area outside the grant may
/// not survive the filter either.
#[test]
fn two_overlapping_areas_forward_the_grant_and_filter_the_rest_here() {
    let grant = box_q(0.0, 0.0, 10.0, 10.0);
    let asked = box_q(5.0, 5.0, 20.0, 20.0);

    let held = intersect(Some(&asked), &[&grant]);

    assert_eq!(
        held.geo_q.as_deref(),
        Some(grant.as_str()),
        "the broker gets the grant"
    );
    assert_eq!(
        held.grants,
        vec![grant.clone()],
        "and the grant is applied here as well"
    );
    assert_eq!(
        held.caller.as_deref(),
        Some(asked.as_str()),
        "with the caller's own area"
    );
    assert!(held.restricted, "the caller asked for more than it holds");
    assert!(admits(&held, &at(7.0, 7.0)), "the overlap is shown");
    assert!(
        !admits(&held, &at(15.0, 15.0)),
        "the caller's corner outside the grant is not"
    );
    assert!(
        !admits(&held, &at(1.0, 1.0)),
        "nor the grant's corner the caller did not ask for"
    );
}

/// Nothing granted narrows space, so the caller's own area is the whole query and nothing is
/// filtered twice.
#[test]
fn without_a_grant_the_callers_own_area_stands() {
    let asked = box_q(0.0, 0.0, 10.0, 10.0);

    let held = intersect(Some(&asked), &[]);

    assert_eq!(held.geo_q.as_deref(), Some(asked.as_str()));
    assert!(
        held.grants.is_empty(),
        "there is nothing to apply on the way back"
    );
    assert!(
        held.areas.is_empty(),
        "and no grant area a write could be held to"
    );
    assert_eq!(held.caller, None);
    assert!(!held.restricted, "the caller was not narrowed");
}

/// No grant and no question: a spatial filter is not invented for a caller that asked for none.
#[test]
fn without_a_grant_and_without_a_question_no_area_is_invented() {
    let held = intersect(None, &[]);

    assert_eq!(held, Default::default(), "every field empty");
    assert!(
        Areas::of(&held.grants, held.caller.as_deref()).is_none(),
        "and no filter is built for the answer"
    );
}

/// A grant and no question: the grant is the query, and no second filter is needed because the
/// broker was given the grant itself.
#[test]
fn a_grant_alone_becomes_the_query() {
    let grant = box_q(0.0, 0.0, 10.0, 10.0);

    let held = intersect(None, &[&grant]);

    assert_eq!(held.geo_q.as_deref(), Some(grant.as_str()));
    assert!(
        held.grants.is_empty(),
        "the broker already narrowed to the grant"
    );
    assert_eq!(held.areas, vec![grant], "the write still knows the area");
    assert_eq!(held.caller, None);
    assert!(
        held.restricted,
        "a caller that asked for everything got one area"
    );
}

/// The caller asked for less than it holds: its own area is the narrower query and the answer needs
/// no filtering at all.
#[test]
fn a_caller_asking_for_less_is_forwarded_unchanged() {
    let grant = box_q(0.0, 0.0, 10.0, 10.0);
    let asked = box_q(2.0, 2.0, 4.0, 4.0);

    let held = intersect(Some(&asked), &[&grant]);

    assert_eq!(held.geo_q.as_deref(), Some(asked.as_str()));
    assert!(
        held.grants.is_empty(),
        "the forwarded area is inside the grant"
    );
    assert_eq!(
        held.areas,
        vec![grant],
        "the grant is still recorded for a write"
    );
    assert_eq!(held.caller, None);
    assert!(!held.restricted, "nothing was taken from the caller");
}

/// The bound itself: an area exactly the grant is inside it, the same way a point on the boundary is
/// inside, so a caller drawing its district's own border is not narrowed.
#[test]
fn an_area_exactly_the_grant_is_forwarded_as_asked() {
    let grant = box_q(0.0, 0.0, 10.0, 10.0);
    let asked = box_q(0.0, 0.0, 10.0, 10.0);

    let held = intersect(Some(&asked), &[&grant]);

    assert_eq!(held.geo_q.as_deref(), Some(asked.as_str()));
    assert!(held.grants.is_empty());
    assert!(!held.restricted);
}

/// The bound plus one: a millionth of a degree over the grant's edge is not inside it.
#[test]
fn an_area_a_millionth_over_the_grant_is_not_forwarded() {
    let grant = box_q(0.0, 0.0, 10.0, 10.0);
    let asked = box_q(0.0, 0.0, 10.000001, 10.0);

    let held = intersect(Some(&asked), &[&grant]);

    assert_eq!(
        held.geo_q.as_deref(),
        Some(grant.as_str()),
        "the grant is forwarded instead"
    );
    assert_eq!(held.grants, vec![grant]);
    assert!(held.restricted);
    assert!(
        !admits(&held, &at(10.0000005, 5.0)),
        "and the sliver is filtered out here"
    );
}

/// A `geoQ` this parser cannot read is not a licence: a relation other than `within`, a geometry
/// that is not a polygon, an empty string and a coordinate list that is not JSON all end with the
/// grant forwarded and both areas applied here.
#[test]
fn an_unreadable_question_still_forwards_the_grant() {
    let grant = box_q(0.0, 0.0, 10.0, 10.0);

    for asked in [
        "georel=near;maxDistance==2000;geometry=Point;coordinates=[5,5]",
        "georel=within;geometry=MultiPolygon;coordinates=[[[[0,0],[9,0],[9,9],[0,0]]]]",
        "georel=within;geometry=Polygon;coordinates=not-json",
        "",
        "georel=within;geometry=polygon",
    ] {
        let held = intersect(Some(asked), &[&grant]);

        assert_eq!(
            held.geo_q.as_deref(),
            Some(grant.as_str()),
            "{asked:?} forwards the grant"
        );
        assert_eq!(
            held.grants,
            vec![grant.clone()],
            "{asked:?} is filtered here too"
        );
        assert_eq!(
            held.caller.as_deref(),
            Some(asked),
            "{asked:?} is kept verbatim"
        );
        assert!(held.restricted, "{asked:?} narrowed the caller");
        assert!(
            !admits(&held, &at(5.0, 5.0)),
            "{asked:?} cannot be read, so nothing is admitted at all"
        );
    }
}

/// A grant nobody can parse admits nothing rather than everything: it is forwarded, and it is also
/// applied to the answer, where an unreadable area lets no entity through.
#[test]
fn an_unreadable_grant_admits_nothing() {
    let grant = "georel=intersects;geometry=Polygon;coordinates=[[[0,0],[9,0],[9,9],[0,0]]]";
    let asked = box_q(0.0, 0.0, 1.0, 1.0);

    let held = intersect(Some(&asked), &[grant]);

    assert_eq!(held.geo_q.as_deref(), Some(grant));
    assert_eq!(held.grants, vec![grant.to_owned()]);
    assert_eq!(held.caller.as_deref(), Some(asked.as_str()));
    assert!(held.restricted);
    assert!(
        !admits(&held, &at(0.5, 0.5)),
        "an area the gateway cannot read admits nothing"
    );
}

/// Several grants are several areas and `geoQ` has no union: the broker gets the caller's question,
/// every grant is applied here, and an entity outside all of them does not come out.
#[test]
fn several_grants_are_all_applied_on_the_way_back() {
    let west = box_q(0.0, 0.0, 10.0, 10.0);
    let east = box_q(20.0, 0.0, 30.0, 10.0);
    let asked = box_q(-5.0, -5.0, 40.0, 40.0);

    let held = intersect(Some(&asked), &[&west, &east]);

    assert_eq!(
        held.geo_q.as_deref(),
        Some(asked.as_str()),
        "the broker gets the question"
    );
    assert_eq!(
        held.grants,
        vec![west.clone(), east.clone()],
        "both areas are applied here"
    );
    assert_eq!(
        held.areas,
        vec![west, east],
        "and both are recorded for a write"
    );
    assert_eq!(
        held.caller, None,
        "the caller's area is what the broker already had"
    );
    assert!(held.restricted);
    assert!(admits(&held, &at(5.0, 5.0)), "inside the first grant");
    assert!(admits(&held, &at(25.0, 5.0)), "inside the second");
    assert!(
        !admits(&held, &at(15.0, 5.0)),
        "the gap between them is not granted"
    );
}

/// Several grants and no question at all: the broker is given no area, so the whole narrowing is the
/// filter built here, and it must still hold.
#[test]
fn several_grants_without_a_question_are_narrowed_only_here() {
    let west = box_q(0.0, 0.0, 10.0, 10.0);
    let east = box_q(20.0, 0.0, 30.0, 10.0);

    let held = intersect(None, &[&west, &east]);

    assert_eq!(held.geo_q, None, "there is no `geoQ` that means two areas");
    assert_eq!(
        held.grants,
        vec![west, east],
        "so both are the answer's filter"
    );
    assert!(held.restricted);
    assert!(admits(&held, &at(25.0, 5.0)));
    assert!(
        !admits(&held, &at(50.0, 50.0)),
        "outside every grant, and outside the answer"
    );
}

/// The same grant twice is still one area: duplicates take the many-grants path, and neither the
/// query nor the filter is widened by the repetition.
#[test]
fn the_same_grant_twice_does_not_widen_anything() {
    let grant = box_q(0.0, 0.0, 10.0, 10.0);

    let held = intersect(None, &[&grant, &grant]);

    assert_eq!(held.geo_q, None);
    assert_eq!(held.grants, vec![grant.clone(), grant.clone()]);
    assert_eq!(held.areas, vec![grant.clone(), grant]);
    assert!(admits(&held, &at(5.0, 5.0)));
    assert!(
        !admits(&held, &at(11.0, 5.0)),
        "twice granted is not twice as wide"
    );
}

/// A grant carrying CR/LF or a NUL is handed on as it stands — quoting it belongs to the client that
/// builds the URL, not here — but it is no longer a polygon this parser reads, so the answer is
/// filtered against an unreadable area and nothing comes out.
#[test]
fn a_grant_with_control_characters_is_not_a_readable_area() {
    for grant in [
        "georel=within\r\nX-Injected: 1;geometry=Polygon;coordinates=[[[0,0],[9,0],[9,9],[0,0]]]",
        "georel=within;geometry=Polygon;coordinates=[[[0,0],[9,0],[9,9],[0,0]]]\0",
    ] {
        let held = intersect(None, &[grant]);

        assert_eq!(
            held.geo_q.as_deref(),
            Some(grant),
            "the string is not rewritten here"
        );
        assert_eq!(held.areas, vec![grant.to_owned()]);
        let held = intersect(Some(&box_q(0.0, 0.0, 1.0, 1.0)), &[grant]);
        assert!(
            !admits(&held, &at(0.5, 0.5)),
            "and it admits nothing on the way back"
        );
    }
}
