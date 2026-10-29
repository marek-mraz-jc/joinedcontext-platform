//! Spatial grants intersected with the caller's own area (T-0149, GW11, R14).

use context_gateway::pdp::geo::{granted_polygon, intersect};

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

/// GW11 and R14: a partial overlap forwards the grant, so the broker cannot answer past it.
#[test]
fn a_straddling_caller_area_forwards_the_grant_and_post_filters() {
    let outcome = intersect(Some(&straddling()), &[district().as_str()]);
    assert_eq!(outcome.geo_q.as_deref(), Some(district().as_str()));
    assert_eq!(outcome.caller.as_deref(), Some(straddling().as_str()));
    assert!(outcome.restricted);
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
    assert!(free.grants.is_empty() && free.caller.is_none());
}

/// Several grants are several areas and `geoQ` has no union: the broker's view applies each
/// grant's area (T-4755), so nothing is forwarded as one `geoQ`.
#[test]
fn several_grant_areas_are_no_single_geo_q() {
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
}

/// A grant whose polygon has a hole, and a caller area that encloses that hole (GW11, T-2327).
///
/// This is the leak the carve-out exists to prevent: the caller's own area was forwarded whole,
/// with nothing to filter against, so the broker answered from inside the hole. The grant is
/// forwarded now, and its polygon keeps the hole.
#[test]
fn a_read_enclosing_a_carve_out_is_filtered_against_the_grant() {
    // The district with the protected site in the middle of it cut out.
    let grant = "georel=within;geometry=Polygon;coordinates=[[[19.10,48.70],[19.20,48.70],\
         [19.20,48.76],[19.10,48.76],[19.10,48.70]],[[19.14,48.72],[19.16,48.72],[19.16,48.74],\
         [19.14,48.74],[19.14,48.72]]]";
    let asked = within(&[
        (19.13, 48.71),
        (19.17, 48.71),
        (19.17, 48.75),
        (19.13, 48.75),
        (19.13, 48.71),
    ]);

    let outcome = intersect(Some(&asked), &[grant]);
    assert_eq!(
        outcome.geo_q.as_deref(),
        Some(grant),
        "the grant is what the broker may answer from, not the area drawn around the carve-out"
    );
    assert!(outcome.restricted, "the caller was narrowed (R22)");

    let area = granted_polygon(grant).expect("the grant parses");
    assert!(
        !area.contains((19.15, 48.73)),
        "a point inside the carve-out is outside the grant"
    );
    assert!(area.contains((19.135, 48.73)), "one beside it is inside");
}
