//! Edge cases of `pdp::write_guard::{check, check_granted_id, check_identifier}` (T-1939,
//! T-1940, T-1941; EP-26, MP-02, GW11, GW16, GW17, GW19, GW28, GW29, PF-10, PF-42, R24).
//!
//! Contracts, one sentence each:
//!
//! - `check`: a payload passes only when every part of it — its id, its type, its attributes, its
//!   scope and its own coordinates — is inside the grant, and it is refused whole rather than
//!   trimmed.
//! - `check_granted_id`: an identifier alone is held to the grant's types and patterns, so a path
//!   id and a bare URN in a batch delete get the same answer as a payload.
//! - `check_identifier`: an id is an NGSI-LD URN whose type is the type the payload declares;
//!   nothing else of it is read, because the space is the Endpoint's (ADR-N-041, PF-42, PF-43).
//!
//! `write_guard_tests.rs` covers the golden grant's happy path and one refusal of each kind.
//! These are the shapes around them, where a wrong answer is a write landing somewhere it was
//! never granted.

use context_gateway::pdp::evaluator::Constraints;
use context_gateway::pdp::write_guard::{check, check_granted_id, check_identifier, Refusal};
use serde_json::{json, Value};
use std::collections::BTreeSet;

const SPACE: &str = "ovzdusie";
const ID: &str = "urn:ngsi-ld:AirQualityObserved:banskabystrica.sk:ovzdusie:station-01";
const CITY: &str = "georel=within;geometry=Polygon;coordinates=[[[19.10,48.70],[19.20,48.70],[19.20,48.76],[19.10,48.76],[19.10,48.70]]]";

fn names(names: &[&str]) -> BTreeSet<String> {
    names.iter().map(|name| (*name).to_owned()).collect()
}

fn grant() -> Constraints {
    Constraints {
        tenant: SPACE.to_owned(),
        types: names(&["AirQualityObserved"]),
        attrs: names(&["pm10", "pm25", "dateObserved", "location"]),
        granted_scopes: Some("/geo/SK/BB".to_owned()),
        geo_q: Some(CITY.to_owned()),
        geo_areas: vec![CITY.to_owned()],
        ..Constraints::default()
    }
}

/// A grant that narrows nothing: no type, no attribute, no scope, no area.
fn wide() -> Constraints {
    Constraints {
        tenant: SPACE.to_owned(),
        ..Constraints::default()
    }
}

fn entity() -> Value {
    json!({
        "id": ID,
        "type": "AirQualityObserved",
        "pm10": { "type": "Property", "value": 34.2 },
        "scope": "/geo/SK/BB/Radvan",
        "location": {
            "type": "GeoProperty",
            "value": { "type": "Point", "coordinates": [19.15, 48.73] }
        }
    })
}

// --- check_identifier (T-1941) --------------------------------------------------------------

#[test]
fn a_urn_naming_another_organization_or_space_is_an_id_and_reaches_nothing_it_names() {
    // ADR-N-041, PF-42: identity is the space and the URN. The Endpoint pinned the space, so a
    // URN whose segments name another organization or space is an id like any other, written
    // into this space under this space's grant, and the copy of an entity keeps its URN.
    for named_elsewhere in [
        "urn:ngsi-ld:AirQualityObserved:bbsk.sk:ovzdusie:station-01",
        "urn:ngsi-ld:AirQualityObserved:banskabystrica.sk:doprava:station-01",
        "urn:ngsi-ld:AirQualityObserved:Helsinki-001",
    ] {
        check_identifier(named_elsewhere, Some("AirQualityObserved"))
            .unwrap_or_else(|refusal| panic!("{named_elsewhere} refused: {refusal}"));
        let mut payload = entity();
        payload["id"] = json!(named_elsewhere);
        check(&payload, &grant()).unwrap_or_else(|refusal| panic!("{named_elsewhere}: {refusal}"));
    }
}

#[test]
fn an_id_that_is_no_ngsi_ld_urn_is_refused_rather_than_guessed_at() {
    // PF-43. Each of these is a shape a client might send, and none is an NGSI-LD URN.
    for malformed in [
        "",
        "station-01",
        "urn:ngsi-ld:AirQualityObserved",
        "urn:ngsi-ld:AirQualityObserved:",
        "urn:ngsi-ld:airQualityObserved:station-01",
        "urn:ngsi-ld:AirQualityObserved:station 01",
        "http://example.org/entities/station-01",
        "urn:example:AirQualityObserved:banskabystrica.sk:ovzdusie:station-01",
        "  urn:ngsi-ld:AirQualityObserved:banskabystrica.sk:ovzdusie:station-01",
    ] {
        assert!(
            matches!(
                check_identifier(malformed, None),
                Err(Refusal::MalformedId(_))
            ),
            "{malformed:?} was read as a usable id"
        );
    }
}

#[test]
fn the_type_in_the_urn_and_the_type_in_the_payload_have_to_agree() {
    // PF-43: the URN carries the type, so a payload declaring another one would be stored under
    // an id that says it is something else — and a later read by type would never find it.
    check_identifier(ID, Some("AirQualityObserved")).expect("they agree");
    assert!(matches!(
        check_identifier(ID, Some("WeatherObserved")),
        Err(Refusal::MalformedId(_))
    ));
    assert!(
        matches!(
            check_identifier(ID, Some("airqualityobserved")),
            Err(Refusal::MalformedId(_))
        ),
        "a type is compared as written, not case-folded"
    );
    check_identifier(ID, None).expect("a payload that declares none is not checked");
}

#[test]
fn a_grant_that_names_ids_still_bounds_a_write_whatever_its_urn_says() {
    // GW16: an `idPattern` is a target the steward wrote, read against the URN as a string. An
    // upper-case spelling of the granted prefix is a different string and falls outside it.
    let mut narrowed = grant();
    narrowed.id_patterns = BTreeSet::from([
        "^urn:ngsi-ld:AirQualityObserved:banskabystrica\\.sk:ovzdusie:.*$".to_owned(),
    ]);
    check_granted_id(ID, &narrowed).expect("inside the pattern");
    for outside in [
        "urn:ngsi-ld:AirQualityObserved:BANSKABYSTRICA.SK:ovzdusie:station-01",
        "urn:ngsi-ld:AirQualityObserved:banskabystrica.sk:OVZDUSIE:station-01",
        "urn:ngsi-ld:AirQualityObserved:Helsinki-001",
    ] {
        assert!(
            matches!(
                check_granted_id(outside, &narrowed),
                Err(Refusal::IdOutsideGrant(_))
            ),
            "{outside} passed a grant that names other ids"
        );
    }
}

// --- check_granted_id (T-1940) --------------------------------------------------------------

#[test]
fn an_identifier_alone_is_held_to_the_grants_types() {
    // A batch delete names its entities as bare URNs and an addressed write carries its id in the
    // path; both must be checked as strictly as a payload, or a delete would be the way around
    // every type grant.
    check_granted_id(ID, &grant()).expect("the granted type");
    let other = "urn:ngsi-ld:WeatherObserved:banskabystrica.sk:ovzdusie:station-01";
    assert!(matches!(
        check_granted_id(other, &grant()),
        Err(Refusal::TypeOutsideGrant(name)) if name == "WeatherObserved"
    ));
}

#[test]
fn a_grant_that_names_no_type_holds_an_identifier_to_none() {
    // An empty `types` set is "every granted type", not "no type"; reading it the other way
    // would refuse every write under a grant that narrows by attribute alone.
    check_granted_id(ID, &wide()).expect("nothing narrows the type");
    check_granted_id(
        "urn:ngsi-ld:WeatherObserved:banskabystrica.sk:ovzdusie:x",
        &wide(),
    )
    .expect("nothing narrows the type");
}

#[test]
fn an_identifier_outside_every_granted_pattern_is_refused() {
    let narrowed = Constraints {
        id_patterns: names(&[
            "^urn:ngsi-ld:AirQualityObserved:banskabystrica\\.sk:ovzdusie:radvan-.*$",
        ]),
        ..grant()
    };
    check_granted_id(
        "urn:ngsi-ld:AirQualityObserved:banskabystrica.sk:ovzdusie:radvan-01",
        &narrowed,
    )
    .expect("inside the pattern");
    assert!(matches!(
        check_granted_id(ID, &narrowed),
        Err(Refusal::IdOutsideGrant(_))
    ));
}

#[test]
fn an_identifier_is_inside_the_grant_when_any_one_pattern_admits_it() {
    // Several grants each bring their own pattern and they are a union, not an intersection: a
    // caller holding two grants may write what either of them covers.
    let two = Constraints {
        id_patterns: names(&[
            "^urn:ngsi-ld:AirQualityObserved:banskabystrica\\.sk:ovzdusie:radvan-.*$",
            "^urn:ngsi-ld:AirQualityObserved:banskabystrica\\.sk:ovzdusie:station-.*$",
        ]),
        ..grant()
    };
    check_granted_id(ID, &two).expect("the second pattern admits it");
}

#[test]
fn an_identifier_that_is_not_a_urn_is_refused_before_any_pattern_is_tried() {
    assert!(matches!(
        check_granted_id("station-01", &grant()),
        Err(Refusal::MalformedId(_))
    ));
}

// --- check (T-1939) ---------------------------------------------------------------------------

#[test]
fn a_payload_with_no_id_at_all_is_refused() {
    // GW16: there is nothing to place, so there is nothing to permit.
    let mut nameless = entity();
    nameless.as_object_mut().expect("an object").remove("id");
    assert!(matches!(
        check(&nameless, &grant()),
        Err(Refusal::MalformedId(_))
    ));
}

#[test]
fn a_payload_that_is_not_an_object_is_refused() {
    // A caller can send anything; `check` is the gate and must answer rather than pass through.
    for payload in [json!(null), json!([]), json!(7), json!("text"), json!(true)] {
        assert!(
            check(&payload, &grant()).is_err(),
            "{payload} was accepted as an entity"
        );
    }
}

#[test]
fn access_control_smuggled_into_the_data_is_refused_whatever_else_is_right() {
    // GW28, GW29: policy belongs in a `Policy` entity. An entity carrying its own `acl` would be
    // a grant nobody proposed, sitting inside the data it purports to guard.
    for smuggled in [
        "owner",
        "acl",
        "allowedRoles",
        "visibility",
        "permissions",
        "policy",
    ] {
        let mut entity = entity();
        entity
            .as_object_mut()
            .expect("an object")
            .insert(smuggled.to_owned(), json!("anything"));
        assert!(
            matches!(
                check(&entity, &grant()),
                Err(Refusal::SmuggledPolicyAttribute(name)) if name == smuggled
            ),
            "{smuggled} was written into an entity"
        );
    }
}

#[test]
fn the_smuggled_policy_check_runs_before_anything_that_could_pass_it() {
    // It is the first thing `check` does, so a payload that is wrong in two ways still reports
    // the one that matters most, and an entity whose id is fine cannot slip an `acl` past.
    let mut entity = entity();
    let object = entity.as_object_mut().expect("an object");
    object.insert("acl".to_owned(), json!(["everyone"]));
    object.insert("id".to_owned(), json!("not a urn"));
    assert!(matches!(
        check(&entity, &grant()),
        Err(Refusal::SmuggledPolicyAttribute(_))
    ));
}

#[test]
fn a_payload_declaring_no_type_is_refused_by_a_grant_that_names_one() {
    // The URN carries the type, but the payload's own `type` is what the broker stores, so a
    // payload without one under a type grant cannot be shown to be inside it.
    let mut typeless = entity();
    typeless.as_object_mut().expect("an object").remove("type");
    assert!(matches!(
        check(&typeless, &grant()),
        Err(Refusal::TypeOutsideGrant(name)) if name.is_empty()
    ));
    // And under a grant that narrows no type it is fine: the id still places the entity.
    check(&typeless, &wide()).expect("nothing narrows the type");
}

#[test]
fn one_ungranted_attribute_refuses_the_whole_write_and_names_it() {
    // GW17: never trimmed. A write half-applied is a write the caller cannot reason about, and
    // silently dropping an attribute is how data goes missing without an error.
    let mut entity = entity();
    entity.as_object_mut().expect("an object").insert(
        "stewardNote".to_owned(),
        json!({ "type": "Property", "value": "hello" }),
    );
    assert!(matches!(
        check(&entity, &grant()),
        Err(Refusal::AttributeOutsideGrant(name)) if name == "stewardNote"
    ));
}

#[test]
fn a_grant_that_names_no_attribute_admits_every_attribute() {
    // An empty `attrs` set is "no projection", which for a write means the whole entity. Reading
    // it as an empty whitelist would refuse every write under a grant narrowed only by type.
    let mut entity = entity();
    entity.as_object_mut().expect("an object").insert(
        "anythingAtAll".to_owned(),
        json!({ "type": "Property", "value": 1 }),
    );
    check(&entity, &wide()).expect("nothing narrows the attributes");
}

#[test]
fn a_scope_outside_the_granted_tree_is_refused_and_a_sibling_prefix_is_not_a_parent() {
    // R13, R29, R30: `/geo/SK/BB` covers `/geo/SK/BB/Sasova` and does not cover `/geo/SK/BBB`,
    // which shares its first characters and is a different district entirely.
    for (scope, granted) in [
        ("/geo/SK/BB", true),
        ("/geo/SK/BB/Sasova", true),
        ("/geo/SK/BB/Sasova/Podlavice", true),
        ("/geo/SK/BBB", false),
        ("/geo/SK/BBB/Somewhere", false),
        ("/geo/SK", false),
        ("/geo/SK/ZA", false),
    ] {
        let mut entity = entity();
        entity.as_object_mut().expect("an object")["scope"] = json!(scope);
        assert_eq!(
            check(&entity, &grant()).is_ok(),
            granted,
            "{scope} was decided the wrong way"
        );
    }
}

#[test]
fn every_scope_of_a_list_has_to_be_granted_and_not_merely_one_of_them() {
    // An entity may carry several scopes. One of them outside the tree is data landing where the
    // caller may not write, however many of the others are inside it.
    let mut entity = entity();
    entity.as_object_mut().expect("an object")["scope"] =
        json!(["/geo/SK/BB/Radvan", "/geo/SK/ZA"]);
    assert!(matches!(
        check(&entity, &grant()),
        Err(Refusal::ScopeOutsideGrant(_))
    ));

    entity.as_object_mut().expect("an object")["scope"] =
        json!(["/geo/SK/BB/Radvan", "/geo/SK/BB/Sasova"]);
    check(&entity, &grant()).expect("both are inside the tree");
}

#[test]
fn a_scope_that_is_not_a_string_or_a_list_of_them_is_refused() {
    for odd in [json!(7), json!({"path": "/geo/SK/BB"}), json!(true)] {
        let mut entity = entity();
        entity.as_object_mut().expect("an object")["scope"] = odd.clone();
        assert!(
            matches!(check(&entity, &grant()), Err(Refusal::ScopeOutsideGrant(_))),
            "{odd} was accepted as a scope"
        );
    }
}

#[test]
fn an_entity_that_says_nothing_about_where_it_is_is_not_outside_the_area() {
    // GW16 is a statement about the entity's own coordinates. An entity with no location cannot
    // be placed outside the grant, so refusing it would refuse a legitimate write.
    let mut nowhere = entity();
    nowhere
        .as_object_mut()
        .expect("an object")
        .remove("location");
    check(&nowhere, &grant()).expect("no location is not a location outside the area");
}

#[test]
fn a_location_the_parser_cannot_read_is_refused_rather_than_passed() {
    // A location that is there but unreadable must not pass an area check that never ran: that
    // is the difference between "nothing to check" and "could not check".
    for unreadable in [
        json!({ "type": "GeoProperty", "value": { "type": "Point" } }),
        json!({ "type": "GeoProperty", "value": "19.15,48.73" }),
        json!({ "type": "GeoProperty", "value": { "type": "Point", "coordinates": [] } }),
        json!({ "type": "GeoProperty", "value": { "type": "Point", "coordinates": ["a", "b"] } }),
        json!("somewhere"),
    ] {
        let mut entity = entity();
        entity.as_object_mut().expect("an object")["location"] = unreadable.clone();
        assert!(
            matches!(check(&entity, &grant()), Err(Refusal::LocationOutsideGrant)),
            "{unreadable} passed the area check"
        );
    }
}

#[test]
fn one_unreadable_granted_area_refuses_the_write_rather_than_leaving_the_rest_to_decide() {
    // An area the parser cannot read is an area the write cannot be shown to be outside of
    // either. Letting the readable ones decide would grant whatever they happen to cover.
    let broken = Constraints {
        geo_areas: vec![CITY.to_owned(), "georel=within;geometry=Polygon".to_owned()],
        ..grant()
    };
    assert!(matches!(
        check(&entity(), &broken),
        Err(Refusal::LocationOutsideGrant)
    ));
}

#[test]
fn a_refusal_never_says_what_the_rule_was() {
    // R20: the caller learns that the write was refused, never the shape of the grant that
    // refused it, or a probe would read the policy by writing.
    let mut outside = entity();
    outside.as_object_mut().expect("an object")["location"] = json!({
        "type": "GeoProperty",
        "value": { "type": "Point", "coordinates": [21.25, 48.73] }
    });
    let refusal = check(&outside, &grant()).expect_err("outside the area");
    let said = refusal.to_string();
    assert!(!said.contains("coordinates"), "{said}");
    assert!(!said.contains("19.1"), "{said}");
    assert!(!said.contains("Polygon"), "{said}");
}
