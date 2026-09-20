use context_gateway::pdp::evaluator::Constraints;
use context_gateway::pdp::write_guard::{check, check_granted_id, Refusal};
use jc_core::ProblemDetails;
use serde_json::{json, Value};
use std::collections::BTreeSet;

const SPACE: &str = "ovzdusie";
/// The bounding box of the city, as the golden policy draws it.
const CITY: &str = "georel=within;geometry=Polygon;coordinates=[[[19.10,48.70],[19.20,48.70],[19.20,48.76],[19.10,48.76],[19.10,48.70]]]";
/// A second district of the same grant, well away from the first.
const KOSICE: &str = "georel=within;geometry=Polygon;coordinates=[[[21.20,48.70],[21.30,48.70],[21.30,48.76],[21.20,48.76],[21.20,48.70]]]";
const ORG: &str = "banskabystrica.sk";

/// The grant the golden policy expresses: one type, four attributes, Radvaň and below,
/// inside the bounding box of the city.
fn grant() -> Constraints {
    Constraints {
        tenant: SPACE.to_owned(),
        types: names(&["AirQualityObserved"]),
        attrs: names(&["pm10", "pm25", "dateObserved", "location"]),
        granted_scopes: Some("/geo/SK/BB".to_owned()),
        geo_q: Some(CITY.to_owned()),
        // What the broker was given and what the grant draws are two different things, and a
        // write is decided against the grant (T-0807). The evaluator fills both.
        geo_areas: vec![CITY.to_owned()],
        ..Constraints::default()
    }
}

fn names(names: &[&str]) -> BTreeSet<String> {
    names.iter().map(|name| (*name).to_owned()).collect()
}

fn entity() -> Value {
    json!({
        "id": "urn:ngsi-ld:AirQualityObserved:banskabystrica.sk:ovzdusie:station-01",
        "type": "AirQualityObserved",
        "pm10": { "type": "Property", "value": 34.2 },
        "scope": "/geo/SK/BB/Radvan",
        "location": {
            "type": "GeoProperty",
            "value": { "type": "Point", "coordinates": [19.15, 48.73] }
        }
    })
}

#[test]
fn a_write_inside_the_grant_passes() {
    check(&entity(), &grant(), SPACE, ORG).expect("everything about it is granted");
}

/// GW16, PF-10: a caller with a legitimate grant in this space cannot write an entity that
/// belongs to another organization or another space.
#[test]
fn a_cross_domain_urn_is_refused_as_a_bad_request() {
    for foreign in [
        "urn:ngsi-ld:AirQualityObserved:zilina.sk:ovzdusie:station-01",
        "urn:ngsi-ld:AirQualityObserved:banskabystrica.sk:doprava:station-01",
    ] {
        let mut payload = entity();
        payload["id"] = json!(foreign);

        let refusal = check(&payload, &grant(), SPACE, ORG).expect_err("a foreign URN");
        assert!(
            matches!(&refusal, Refusal::ForeignUrn { id, space } if id == foreign && space == SPACE),
            "{refusal:?}"
        );

        // The caller can see and fix what is wrong with an identifier, so this one is a
        // 400 and says so, unlike a policy refusal (PF-42).
        let problem = ProblemDetails::from(refusal);
        assert_eq!(problem.status, 400);
        assert!(
            problem.type_uri.ends_with("urn-scheme"),
            "{}",
            problem.type_uri
        );
    }
}

#[test]
fn an_identifier_that_is_not_an_ngsi_ld_urn_is_refused() {
    for bad in [
        json!("station-01"),
        json!("urn:ngsi-ld:station-01"),
        json!(7),
    ] {
        let mut payload = entity();
        payload["id"] = bad.clone();
        assert!(
            matches!(
                check(&payload, &grant(), SPACE, ORG),
                Err(Refusal::MalformedId(_))
            ),
            "{bad} was accepted as an entity id"
        );
    }

    // The type in the URN and the type in the body must agree (PF-44).
    let mut lying = entity();
    lying["type"] = json!("ParkingSpot");
    assert!(check(&lying, &grant(), SPACE, ORG).is_err());
}

/// R29, R31: the grant is a scope subtree, and the entity has to sit inside it. The
/// prefix is a path prefix, so `/geo/SK/BBB` is a different place, not a child.
#[test]
fn a_scope_outside_the_granted_subtree_is_refused() {
    for illegal in ["/geo/SK/ZA", "/geo/SK/BBB", "/geo/SK", "/org/mesto"] {
        let mut payload = entity();
        payload["scope"] = json!(illegal);
        let refusal = check(&payload, &grant(), SPACE, ORG).expect_err("an illegal scope");
        assert_eq!(refusal, Refusal::ScopeOutsideGrant(illegal.to_owned()));
        assert_eq!(ProblemDetails::from(refusal).status, 403);
    }

    for legal in [
        "/geo/SK/BB",
        "/geo/SK/BB/Radvan",
        "/geo/SK/BB/Sasova/Pod-Rybou",
    ] {
        let mut payload = entity();
        payload["scope"] = json!(legal);
        check(&payload, &grant(), SPACE, ORG).unwrap_or_else(|e| panic!("{legal}: {e}"));
    }

    // Every scope of a multi-scope entity has to be inside the grant, not just one.
    let mut mixed = entity();
    mixed["scope"] = json!(["/geo/SK/BB/Radvan", "/geo/SK/ZA"]);
    assert!(check(&mixed, &grant(), SPACE, ORG).is_err());
}

/// GW16: "may edit only data in location X" is checked against the entity's own
/// coordinates, not against a query parameter the caller could simply leave out.
#[test]
fn coordinates_outside_the_granted_area_are_refused() {
    let mut outside = entity();
    outside["location"]["value"]["coordinates"] = json!([21.24, 48.72]); // Košice
    assert_eq!(
        check(&outside, &grant(), SPACE, ORG),
        Err(Refusal::LocationOutsideGrant)
    );
    assert_eq!(
        ProblemDetails::from(Refusal::LocationOutsideGrant).status,
        403
    );

    // The boundary itself is inside the area: a station on the edge of the district is in
    // the district.
    let mut on_the_edge = entity();
    on_the_edge["location"]["value"]["coordinates"] = json!([19.10, 48.73]);
    check(&on_the_edge, &grant(), SPACE, ORG).expect("the boundary counts as inside");

    // An entity that says nothing about where it is cannot be placed outside the grant;
    // one whose location cannot be read must not pass a check that never ran.
    let mut nowhere = entity();
    nowhere
        .as_object_mut()
        .expect("an object")
        .remove("location");
    check(&nowhere, &grant(), SPACE, ORG).expect("no location is not a location outside");

    let mut unreadable = entity();
    unreadable["location"] = json!({ "type": "GeoProperty", "value": "somewhere in Radvaň" });
    assert_eq!(
        check(&unreadable, &grant(), SPACE, ORG),
        Err(Refusal::LocationOutsideGrant),
        "an unparseable location fails closed"
    );
}

/// GW17: a write that touches anything outside the grant is refused whole. Trimming it
/// would store a version of the entity the caller never sent.
#[test]
fn a_write_touching_an_ungranted_attribute_or_type_is_refused_whole() {
    let mut extra = entity();
    extra["operatorPhone"] = json!({ "type": "Property", "value": "+421 900 000 000" });
    assert_eq!(
        check(&extra, &grant(), SPACE, ORG),
        Err(Refusal::AttributeOutsideGrant("operatorPhone".to_owned()))
    );

    let mut wrong_type = entity();
    wrong_type["id"] = json!("urn:ngsi-ld:ParkingSpot:banskabystrica.sk:ovzdusie:spot-01");
    wrong_type["type"] = json!("ParkingSpot");
    assert_eq!(
        check(&wrong_type, &grant(), SPACE, ORG),
        Err(Refusal::TypeOutsideGrant("ParkingSpot".to_owned()))
    );
}

/// GW28, GW29: access control lives in `Policy` manifests. An entity that carries its own
/// permissions is refused, whatever the grant says, so nothing downstream can be tempted
/// to honour it.
#[test]
fn an_entity_carrying_its_own_access_control_is_refused() {
    for smuggled in [
        "owner",
        "acl",
        "allowedRoles",
        "visibility",
        "permissions",
        "policy",
    ] {
        let mut payload = entity();
        payload[smuggled] = json!({ "type": "Property", "value": "public" });
        assert_eq!(
            check(&payload, &grant(), SPACE, ORG),
            Err(Refusal::SmuggledPolicyAttribute(smuggled.to_owned()))
        );
    }
}

/// GW6: a policy refusal never says which rule refused. The reason stays in the log.
#[test]
fn a_policy_refusal_discloses_no_rule() {
    let problem = ProblemDetails::from(Refusal::ScopeOutsideGrant("/geo/SK/ZA".to_owned()));
    assert_eq!(problem.status, 403);
    assert_eq!(problem.detail, None, "the body must not name the scope");
}

/// T-0806, GW11, R24: an identifier alone, with no body to read, is held to the grant's
/// types and patterns, so an addressed write and a bare URN in a batch cannot slip past the
/// checks an entity payload gets.
#[test]
fn an_identifier_outside_the_granted_type_or_pattern_is_refused_on_its_own() {
    let lamps = Constraints {
        types: names(&["Device"]),
        id_patterns: names(&["^urn:ngsi-ld:Device:banskabystrica\\.sk:ovzdusie:lamps-.*$"]),
        ..grant()
    };
    check_granted_id(
        "urn:ngsi-ld:Device:banskabystrica.sk:ovzdusie:lamps-7",
        &lamps,
    )
    .expect("a lamp is granted");
    assert_eq!(
        check_granted_id(
            "urn:ngsi-ld:Device:banskabystrica.sk:ovzdusie:traffic-1",
            &lamps
        ),
        Err(Refusal::IdOutsideGrant(
            "urn:ngsi-ld:Device:banskabystrica.sk:ovzdusie:traffic-1".to_owned()
        ))
    );
    assert_eq!(
        check_granted_id("urn:ngsi-ld:Secret:banskabystrica.sk:ovzdusie:1", &lamps),
        Err(Refusal::TypeOutsideGrant("Secret".to_owned()))
    );
    assert_eq!(
        check_granted_id("not-a-urn", &lamps),
        Err(Refusal::MalformedId("not-a-urn".to_owned()))
    );

    // A whole-type grant names no pattern and takes every id of its type.
    check_granted_id(
        "urn:ngsi-ld:AirQualityObserved:banskabystrica.sk:ovzdusie:anything",
        &grant(),
    )
    .expect("no pattern is no restriction");

    // An entity payload whose URN matches no pattern is refused whole, whatever it declares.
    let mut traffic = entity();
    traffic["id"] = json!("urn:ngsi-ld:Device:banskabystrica.sk:ovzdusie:traffic-1");
    traffic["type"] = json!("Device");
    assert_eq!(
        check(&traffic, &lamps, SPACE, ORG),
        Err(Refusal::IdOutsideGrant(
            "urn:ngsi-ld:Device:banskabystrica.sk:ovzdusie:traffic-1".to_owned()
        ))
    );
    // And the refusal is a policy decision: 403, no rule named (GW6).
    let problem = ProblemDetails::from(Refusal::IdOutsideGrant("x".to_owned()));
    assert_eq!(problem.status, 403);
}

/// T-0807: with two or more granted areas the broker is given the caller's own `geoQ`, so a
/// payload checked against `geo_q` would be checked against a polygon the caller drew itself.
/// Every granted area decides, and the entity has to be inside one of them.
#[test]
fn coordinates_outside_every_granted_area_are_refused_when_the_grant_draws_several() {
    let two_districts = Constraints {
        // What the broker was given: the caller's own area, which happens to contain the
        // point the payload carries.
        geo_q: Some(
            "georel=within;geometry=Polygon;coordinates=[[[22.00,48.00],[23.00,48.00],[23.00,49.00],[22.00,49.00],[22.00,48.00]]]"
                .to_owned(),
        ),
        geo_areas: vec![CITY.to_owned(), KOSICE.to_owned()],
        ..grant()
    };

    let mut outside = entity();
    outside["location"]["value"]["coordinates"] = json!([22.5, 48.5]);
    assert_eq!(
        check(&outside, &two_districts, SPACE, ORG),
        Err(Refusal::LocationOutsideGrant),
        "inside the caller's own polygon is not inside a grant"
    );

    // Inside the second of the two granted districts: one of them is enough.
    let mut inside = entity();
    inside["location"]["value"]["coordinates"] = json!([21.25, 48.73]);
    check(&inside, &two_districts, SPACE, ORG).expect("the second district is granted too");

    // An area this parser cannot read is an area no write can be shown to be inside, even
    // when another area of the same grant would admit it.
    let unreadable = Constraints {
        geo_areas: vec![CITY.to_owned(), "georel=near;maxDistance=500".to_owned()],
        ..grant()
    };
    assert_eq!(
        check(&entity(), &unreadable, SPACE, ORG),
        Err(Refusal::LocationOutsideGrant)
    );
}
