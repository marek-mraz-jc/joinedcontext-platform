use context_gateway::pdp::evaluator::Constraints;
use context_gateway::pdp::projection::{permitted, project, project_entity, ungranted};
use serde_json::{json, Value};
use std::collections::BTreeSet;

fn granted(names: &[&str]) -> BTreeSet<String> {
    names.iter().map(|name| (*name).to_owned()).collect()
}

/// No endpoint-level narrowing: the grant alone decides (EP-61).
fn nothing() -> BTreeSet<String> {
    BTreeSet::new()
}

fn station() -> Value {
    json!({
        "id": "urn:ngsi-ld:AirQualityObserved:banskabystrica.sk:ovzdusie:station-01",
        "type": "AirQualityObserved",
        "@context": "https://uri.etsi.org/ngsi-ld/v1/ngsi-ld-core-context-v1.8.jsonld",
        "pm10": { "type": "Property", "value": 34.2 },
        "pm25": { "type": "Property", "value": 12.0 },
        "operatorPhone": { "type": "Property", "value": "+421 900 000 000" },
        "refDistrict": { "type": "Relationship", "object": "urn:ngsi-ld:District:bb:sasova" },
        "location": { "type": "GeoProperty", "value": { "type": "Point", "coordinates": [19.15, 48.73] } }
    })
}

/// R9: an attribute the grant does not name never reaches the wire.
#[test]
fn ungranted_attributes_are_stripped_and_the_entity_stays_valid_ngsi_ld() {
    let mut entity = station();
    project_entity(&mut entity, &granted(&["pm10", "location"]), &nothing());

    assert_eq!(entity["pm10"]["value"], json!(34.2));
    assert_eq!(entity["location"]["value"]["type"], json!("Point"));
    assert!(entity.get("pm25").is_none());
    assert!(
        entity.get("operatorPhone").is_none(),
        "the phone number is not public"
    );
    assert!(entity.get("refDistrict").is_none());

    // Still an entity: without these it is not NGSI-LD at all.
    assert_eq!(entity["type"], json!("AirQualityObserved"));
    assert!(entity["id"].as_str().is_some());
    assert!(entity["@context"].as_str().is_some());
}

/// A relationship is projected by the same rule as a property: the grant lists both, and
/// what it does not list goes (R6, R9).
#[test]
fn a_granted_relationship_survives_and_an_ungranted_one_does_not() {
    let mut entity = station();
    project_entity(&mut entity, &granted(&["refDistrict"]), &nothing());

    assert_eq!(
        entity["refDistrict"]["object"],
        json!("urn:ngsi-ld:District:bb:sasova")
    );
    assert!(entity.get("pm10").is_none());
}

#[test]
fn an_empty_grant_list_is_a_grant_over_the_whole_entity() {
    let mut entity = station();
    let before = entity.clone();
    project_entity(&mut entity, &BTreeSet::new(), &nothing());

    assert_eq!(entity, before, "no whitelist means nothing to strip");
}

#[test]
fn a_whole_query_answer_is_projected_entity_by_entity() {
    let mut answer = json!([station(), station()]);
    project(&mut answer, &granted(&["pm25"]), &nothing());

    for entity in answer.as_array().expect("an array of entities") {
        assert!(entity.get("pm25").is_some());
        assert!(entity.get("pm10").is_none());
        assert!(entity.get("operatorPhone").is_none());
    }
}

/// GW17: a write is refused whole when it touches an ungranted attribute, so the caller
/// needs to know which attributes those are, not just that something was wrong.
#[test]
fn ungranted_lists_exactly_the_attributes_outside_the_grant() {
    let entity = station();
    let outside = ungranted(&entity, &granted(&["pm10", "location"]));

    assert_eq!(outside, vec!["operatorPhone", "pm25", "refDistrict"]);
    assert!(ungranted(&entity, &BTreeSet::new()).is_empty());
    assert!(ungranted(
        &entity,
        &granted(&["pm10", "pm25", "operatorPhone", "refDistrict", "location"])
    )
    .is_empty());
}

/// EP-61: what the endpoint hides is gone whether or not the grant names a whitelist, so
/// publishing the same space twice with different detail needs no second Policy.
#[test]
fn an_endpoint_can_hide_an_attribute_the_grant_allows() {
    let mut whitelisted = station();
    project_entity(
        &mut whitelisted,
        &granted(&["pm10", "operatorPhone"]),
        &granted(&["operatorPhone"]),
    );
    assert_eq!(whitelisted["pm10"]["value"], json!(34.2));
    assert!(
        whitelisted.get("operatorPhone").is_none(),
        "an endpoint that hides an attribute hides it from a caller the policy allows it to"
    );

    // The dangerous case: no whitelist at all means "every granted attribute", and the
    // denial has to survive that, not be swallowed by an empty set.
    let mut everything = station();
    project_entity(
        &mut everything,
        &BTreeSet::new(),
        &granted(&["operatorPhone"]),
    );
    assert!(everything.get("operatorPhone").is_none());
    assert_eq!(everything["pm25"]["value"], json!(12.0));
    assert_eq!(everything["type"], json!("AirQualityObserved"));
}

/// T-2335, T-2131: only an object is an entity. Under a grant that narrows neither the type nor
/// the id, `type_granted` and `id_permitted` both say yes to any value, and `project_entity_to`
/// returns early on anything that is not an object — so an element that is not an entity would
/// reach the caller with every attribute it carries.
#[test]
fn nothing_but_an_object_is_an_entity_a_read_may_serve() {
    let wide = Constraints {
        tenant: "ovzdusie".to_owned(),
        ..Constraints::default()
    };
    assert!(wide.types.is_empty() && wide.id_patterns.is_empty());
    assert!(permitted(&station(), &wide), "an entity is still served");

    for not_an_entity in [
        json!([station()]),
        json!([]),
        json!("urn:ngsi-ld:AirQualityObserved:banskabystrica.sk:ovzdusie:station-01"),
        json!(7),
        json!(true),
        json!(null),
    ] {
        assert!(
            !permitted(&not_an_entity, &wide),
            "{not_an_entity} was served as an entity"
        );
    }
}

/// The same under a grant that does narrow the type, which is what every Policy writes: an
/// `EntitySelector` requires `type` (`jc-core/src/kinds/policy.rs:525`), so this is the shape the
/// read path really sees and the one the guard above is the second line behind.
#[test]
fn a_type_grant_already_refuses_what_carries_no_type() {
    let narrowed = Constraints {
        tenant: "ovzdusie".to_owned(),
        types: granted(&["AirQualityObserved"]),
        ..Constraints::default()
    };
    assert!(permitted(&station(), &narrowed));
    assert!(!permitted(&json!([station()]), &narrowed));
}
