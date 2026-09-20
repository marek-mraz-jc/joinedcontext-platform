//! Edge cases of `pdp::projection::project_entity` (T-1929, EP-26, MP-02).
//!
//! Contract, in one sentence: after it returns, the entity carries its identity and the members
//! the broker itself generates, plus exactly those of its own members that the grant's whitelist
//! names and the endpoint does not hide — and nothing is compared loosely, so a member survives
//! only under the name the grant wrote.
//!
//! An attribute outside the grant is not a display preference but data the caller has no right to
//! (R9), and the endpoint's `hiddenAttributes` narrow further and may not be widened by an empty
//! whitelist (EP-61). `projection_tests.rs` holds the ordinary cases; these are the names and the
//! shapes that try to slip past the comparison.

use context_gateway::pdp::projection::project_entity;
use serde_json::{json, Value};
use std::collections::BTreeSet;

fn names(names: &[&str]) -> BTreeSet<String> {
    names.iter().map(|name| (*name).to_owned()).collect()
}

fn nothing() -> BTreeSet<String> {
    BTreeSet::new()
}

fn station() -> Value {
    json!({
        "id": "urn:ngsi-ld:AirQualityObserved:bb.sk:ovzdusie:station-01",
        "type": "AirQualityObserved",
        "@context": "https://uri.etsi.org/ngsi-ld/v1/ngsi-ld-core-context-v1.8.jsonld",
        "createdAt": "2026-09-18T08:00:00Z",
        "modifiedAt": "2026-09-18T09:00:00Z",
        "pm10": { "type": "Property", "value": 34.2 },
        "operatorPhone": { "type": "Property", "value": "+421 900 000 000" }
    })
}

/// R9: a name is a name. A whitelist for `pm10` does not serve `PM10`, `pm10 ` or `pm 10`, and an
/// attribute whose name only looks like a granted one stays behind.
#[test]
fn an_attribute_name_is_matched_exactly_and_never_loosely() {
    let mut entity = json!({
        "id": "urn:ngsi-ld:AirQualityObserved:bb.sk:ovzdusie:station-01",
        "type": "AirQualityObserved",
        "pm10": { "value": 1 },
        "PM10": { "value": 2 },
        "pm10 ": { "value": 3 },
        " pm10": { "value": 4 },
        "pm100": { "value": 5 },
        "pm1": { "value": 6 },
        "pm10%20": { "value": 7 },
        "pm%31%30": { "value": 8 },
        "pm１0": { "value": 9 },
        "pm10\n": { "value": 10 }
    });
    project_entity(&mut entity, &names(&["pm10"]), &nothing());

    let members: Vec<&String> = entity.as_object().expect("an entity").keys().collect();
    assert_eq!(members, vec!["id", "pm10", "type"], "{entity}");
}

/// EP-61: the endpoint's denial survives the "empty whitelist means everything" rule, and it wins
/// over a grant that names the attribute.
#[test]
fn a_hidden_attribute_goes_whether_or_not_the_grant_names_a_whitelist() {
    for granted in [nothing(), names(&["pm10", "operatorPhone"])] {
        let mut entity = station();
        project_entity(&mut entity, &granted, &names(&["operatorPhone"]));

        assert!(entity.get("operatorPhone").is_none(), "{granted:?}");
        assert_eq!(entity["pm10"]["value"], json!(34.2));
    }
}

/// Identity and the broker's own members survive any whitelist: without them the answer is not an
/// NGSI-LD entity, and provenance is what a federated read is judged by (EP-71, CIM 009 4.8).
#[test]
fn the_structural_and_system_members_survive_a_whitelist_that_names_none_of_them() {
    let mut entity = station();
    entity["scope"] = json!("/ovzdusie/bb");
    entity["@type"] = json!("AirQualityObserved");
    entity["expiresAt"] = json!("2027-01-01T00:00:00Z");
    entity["deletedAt"] = json!("2026-09-19T00:00:00Z");

    project_entity(&mut entity, &names(&["pm10"]), &nothing());

    for kept in [
        "id",
        "type",
        "@context",
        "@type",
        "scope",
        "createdAt",
        "modifiedAt",
        "expiresAt",
        "deletedAt",
    ] {
        assert!(
            entity.get(kept).is_some(),
            "{kept} is not the grant's to remove"
        );
    }
    assert!(entity.get("operatorPhone").is_none());
}

/// A member that merely looks structural is an ordinary attribute and goes with the rest.
#[test]
fn a_member_that_only_looks_structural_is_stripped() {
    let mut entity = json!({
        "id": "urn:ngsi-ld:Vehicle:hel.fi:fleet:1",
        "type": "Vehicle",
        "ID": 1, "Id": 2, "id ": 3, "@Context": 4, "@contexts": 5,
        "scopes": 6, "@graph": 7, "createdat": 8, "created_at": 9, "type\u{0}": 10
    });
    project_entity(&mut entity, &names(&["weight"]), &nothing());

    let members: Vec<&String> = entity.as_object().expect("an entity").keys().collect();
    assert_eq!(members, vec!["id", "type"]);
}

/// An endpoint cannot hide identity or provenance: those members are not attributes, and the
/// answer would stop being an entity. T-2334 tracks refusing such a name at validation instead of
/// accepting a list that hides nothing.
#[test]
fn hiding_an_identity_or_system_member_is_not_what_hidden_attributes_do() {
    let mut entity = station();
    project_entity(
        &mut entity,
        &nothing(),
        &names(&["id", "type", "@context", "createdAt", "modifiedAt"]),
    );

    for kept in ["id", "type", "@context", "createdAt", "modifiedAt"] {
        assert!(entity.get(kept).is_some(), "{kept} is still served");
    }
}

/// Neither set naming anything is no projection at all, and the entity comes back byte for byte.
#[test]
fn an_empty_grant_and_an_empty_hidden_set_change_nothing() {
    let mut entity = station();
    let before = entity.clone();
    project_entity(&mut entity, &nothing(), &nothing());
    assert_eq!(entity, before);
}

/// A denial with no whitelist beside it strips only what it names.
#[test]
fn a_denial_alone_strips_only_what_it_names() {
    let mut entity = station();
    project_entity(&mut entity, &nothing(), &names(&["operatorPhone"]));

    assert!(entity.get("operatorPhone").is_none());
    assert_eq!(entity["pm10"]["value"], json!(34.2));
    assert_eq!(entity["type"], json!("AirQualityObserved"));
}

/// A value that is not an entity is left as it is, so a projection never turns one shape of
/// answer into another; the callers decide what may be in an answer at all.
#[test]
fn a_value_that_is_not_an_object_is_left_alone() {
    for mut value in [json!([]), json!("pm10"), json!(0), Value::Null, json!(true)] {
        let before = value.clone();
        project_entity(&mut value, &names(&["pm10"]), &names(&["operatorPhone"]));
        assert_eq!(value, before);
    }
}

/// The whitelist covers the top-level member; what that member carries comes with it, because an
/// attribute's own metadata is part of the attribute (CIM 009 4.5).
#[test]
fn a_granted_attribute_keeps_its_metadata_and_sub_properties() {
    let mut entity = json!({
        "id": "urn:ngsi-ld:AirQualityObserved:bb.sk:ovzdusie:station-01",
        "type": "AirQualityObserved",
        "pm10": {
            "type": "Property",
            "value": 34.2,
            "observedAt": "2026-09-18T09:00:00Z",
            "unitCode": "GQ",
            "qualityOfMeasurement": { "type": "Property", "value": "good" }
        }
    });
    project_entity(&mut entity, &names(&["pm10"]), &names(&["unitCode"]));

    assert_eq!(
        entity["pm10"]["unitCode"],
        json!("GQ"),
        "hiding is per attribute, not per path"
    );
    assert_eq!(
        entity["pm10"]["qualityOfMeasurement"]["value"],
        json!("good")
    );
}

/// Unicode and empty names are compared like any other, with no normalisation of either side.
#[test]
fn unicode_and_empty_names_are_compared_as_written() {
    let mut entity = json!({
        "id": "urn:ngsi-ld:Vehicle:hel.fi:fleet:1",
        "type": "Vehicle",
        "teplota": 1,
        "teplotá": 2,
        "": 3,
        "váha": 4
    });
    project_entity(&mut entity, &names(&["teplota", ""]), &nothing());

    let members: Vec<&String> = entity.as_object().expect("an entity").keys().collect();
    assert_eq!(members, vec!["", "id", "teplota", "type"]);
}

/// Projecting an already projected entity changes nothing further: the operation is idempotent,
/// which is what lets a surface project twice without a second rule.
#[test]
fn projecting_twice_is_projecting_once() {
    let mut once = station();
    project_entity(&mut once, &names(&["pm10"]), &names(&["operatorPhone"]));
    let mut twice = once.clone();
    project_entity(&mut twice, &names(&["pm10"]), &names(&["operatorPhone"]));

    assert_eq!(once, twice);
}

/// A whitelist naming attributes the entity does not carry leaves an entity that is still an
/// entity: identity, provenance, nothing else.
#[test]
fn a_whitelist_that_matches_nothing_leaves_the_identity_and_nothing_else() {
    let mut entity = station();
    project_entity(&mut entity, &names(&["no10", "nothing"]), &nothing());

    let members: Vec<&String> = entity.as_object().expect("an entity").keys().collect();
    assert_eq!(
        members,
        vec!["@context", "createdAt", "id", "modifiedAt", "type"]
    );
    assert!(entity.get("pm10").is_none(), "an attribute nobody granted");
}
