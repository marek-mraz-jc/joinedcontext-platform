//! Spatial grants intersected with the caller's own area (T-0149, GW11, R14).

use context_gateway::pdp::geo::{granted_polygon, intersect, Areas};
use serde_json::json;

/// A `within` polygon geoQ from a ring of `(lon, lat)` pairs.
fn within(ring: &[(f64, f64)]) -> String {
    let coordinates: Vec<String> = ring.iter().map(|(x, y)| format!("[{x},{y}]")).collect();
    format!(
        "georel=within;geometry=Polygon;coordinates=[[{}]]",
        coordinates.join(",")
    )
}

/// The Banská Bystrica district of the golden policy.
fn district() -> String {
    within(&[
        (19.10, 48.70),
        (19.20, 48.70),
        (19.20, 48.76),
        (19.10, 48.76),
        (19.10, 48.70),
    ])
}

/// A block inside it.
fn block() -> String {
    within(&[
        (19.12, 48.72),
        (19.15, 48.72),
        (19.15, 48.74),
        (19.12, 48.74),
        (19.12, 48.72),
    ])
}

/// Overlaps the district's eastern edge.
fn straddling() -> String {
    within(&[
        (19.15, 48.72),
        (19.30, 48.72),
        (19.30, 48.74),
        (19.15, 48.74),
        (19.15, 48.72),
    ])
}

fn station(lon: f64, lat: f64) -> serde_json::Value {
    json!({
        "id": "urn:ngsi-ld:AirQualityObserved:banskabystrica.sk:ovzdusie:s1",
        "type": "AirQualityObserved",
        "location": { "type": "GeoProperty", "value": { "type": "Point", "coordinates": [lon, lat] } },
    })
}

/// Containment is exact for the simple shapes a policy draws: inside, straddling, and a
/// polygon that only touches the boundary.
#[test]
fn polygon_containment() {
    let outer = granted_polygon(&district()).expect("parses");
    assert!(granted_polygon(&block()).expect("parses").within(&outer));
    assert!(!granted_polygon(&straddling())
        .expect("parses")
        .within(&outer));
    assert!(
        !outer.within(&granted_polygon(&block()).expect("parses")),
        "not symmetric"
    );
    assert!(outer.within(&outer), "an area is within itself");

    // Touches the eastern edge from the inside: contained, the way a point on the
    // boundary is inside.
    let touching = within(&[
        (19.15, 48.72),
        (19.20, 48.72),
        (19.20, 48.74),
        (19.15, 48.74),
        (19.15, 48.72),
    ]);
    assert!(granted_polygon(&touching).expect("parses").within(&outer));
}

/// GW11: the caller asked for less than it was given, so its own smaller area is the
/// query. Nothing is filtered afterwards, because the broker already did.
#[test]
fn a_contained_caller_area_is_forwarded_as_is() {
    let outcome = intersect(Some(&block()), &[district().as_str()]);
    assert_eq!(outcome.geo_q.as_deref(), Some(block().as_str()));
    assert!(outcome.grants.is_empty());
    assert_eq!(outcome.caller, None);
    assert!(!outcome.restricted);
}

/// GW11 and R14: a partial overlap forwards the grant, so the broker cannot answer past
/// it, and the caller's own area is applied on the way back.
#[test]
fn a_straddling_caller_area_forwards_the_grant_and_post_filters() {
    let outcome = intersect(Some(&straddling()), &[district().as_str()]);
    assert_eq!(outcome.geo_q.as_deref(), Some(district().as_str()));
    assert_eq!(outcome.caller.as_deref(), Some(straddling().as_str()));
    assert!(outcome.restricted);

    let areas = Areas::of(&outcome.grants, outcome.caller.as_deref()).expect("something to filter");
    assert!(areas.admits(&station(19.17, 48.73)), "inside both");
    assert!(
        !areas.admits(&station(19.11, 48.73)),
        "inside the grant, outside what the caller asked for"
    );
    assert!(
        !areas.admits(&station(19.25, 48.73)),
        "inside the caller's area but the broker would never have returned it; the gateway does not either"
    );
}

/// No caller area: the grant is the query. No grant: the caller's area stands.
#[test]
fn one_side_missing() {
    let granted = intersect(None, &[district().as_str()]);
    assert_eq!(granted.geo_q.as_deref(), Some(district().as_str()));
    assert!(granted.restricted, "the caller got less than everything");

    let free = intersect(Some(&block()), &[]);
    assert_eq!(free.geo_q.as_deref(), Some(block().as_str()));
    assert!(!free.restricted);
    assert!(Areas::of(&free.grants, free.caller.as_deref()).is_none());
}

/// Several grants are several areas and `geoQ` has no union: the caller's area goes to
/// the broker and every grant area is applied here. An entity in either district passes;
/// one in neither does not.
#[test]
fn several_grant_areas_are_applied_at_the_gateway() {
    let za = within(&[
        (18.70, 49.20),
        (18.80, 49.20),
        (18.80, 49.26),
        (18.70, 49.26),
        (18.70, 49.20),
    ]);
    let outcome = intersect(None, &[district().as_str(), za.as_str()]);
    assert_eq!(outcome.geo_q, None);
    assert_eq!(outcome.grants.len(), 2);
    assert!(outcome.restricted);

    let areas = Areas::of(&outcome.grants, None).expect("areas");
    assert!(areas.admits(&station(19.15, 48.73)), "Banská Bystrica");
    assert!(areas.admits(&station(18.75, 49.23)), "Žilina");
    assert!(
        !areas.admits(&station(17.10, 48.15)),
        "Bratislava is in neither"
    );
}

/// A spatial query against a type that carries no location: the entity cannot be placed,
/// so it cannot be shown to be inside the area, and it is not served. The same holds for
/// an area nobody can parse.
#[test]
fn an_entity_without_a_location_or_an_unreadable_area_is_not_admitted() {
    let outcome = intersect(Some(&straddling()), &[district().as_str()]);
    let areas = Areas::of(&outcome.grants, outcome.caller.as_deref()).expect("areas");

    let no_location = json!({
        "id": "urn:ngsi-ld:District:banskabystrica.sk:ovzdusie:radvan",
        "type": "District",
        "name": { "type": "Property", "value": "Radvaň" },
    });
    assert!(!areas.admits(&no_location));

    let line = json!({
        "id": "urn:ngsi-ld:Road:banskabystrica.sk:ovzdusie:r1",
        "type": "Road",
        "location": { "type": "GeoProperty", "value": { "type": "LineString", "coordinates": [[19.15, 48.73], [19.16, 48.73]] } },
    });
    assert!(
        !areas.admits(&line),
        "a shape the parser does not read is not placed"
    );

    let unreadable = intersect(
        Some("georel=near;maxDistance==200;geometry=Point;coordinates=[19.15,48.73]"),
        &[district().as_str()],
    );
    assert_eq!(
        unreadable.geo_q.as_deref(),
        Some(district().as_str()),
        "the grant is still what the broker gets"
    );
    let areas = Areas::of(&unreadable.grants, unreadable.caller.as_deref()).expect("areas");
    assert!(
        !areas.admits(&station(19.15, 48.73)),
        "an area nobody can read admits nothing"
    );
}
