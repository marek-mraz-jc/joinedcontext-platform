//! Edge cases of `pdp::projection::permitted` (T-1932, EP-26, MP-02).
//!
//! Contract, in one sentence: an entity may be returned only when the type it declares is one the
//! grants name — compacted or expanded to an IRI — and its id matches at least one of the grants'
//! id patterns, and an entity that declares neither, or declares something that is not a string,
//! is never returned under a grant that names either.
//!
//! This is the one door every read goes through (`app.rs`'s `retain`, the single-entity branch,
//! `egress::notifications`), because a query is narrowed upstream with `?type=` but a retrieve by
//! id carries no type at all and a broker may answer whatever it likes (T-2130, T-2131). So every
//! case below is written from the broker's side: what an answer has to look like to get past.

use context_gateway::pdp::evaluator::Constraints;
use context_gateway::pdp::projection::permitted;
use serde_json::{json, Value};
use std::collections::BTreeSet;

fn set(values: &[&str]) -> BTreeSet<String> {
    values.iter().map(|value| (*value).to_owned()).collect()
}

fn types(names: &[&str]) -> Constraints {
    Constraints {
        types: set(names),
        ..Constraints::default()
    }
}

fn ids(patterns: &[&str]) -> Constraints {
    Constraints {
        id_patterns: set(patterns),
        ..Constraints::default()
    }
}

const VEHICLE: &str = "urn:ngsi-ld:Vehicle:hel.fi:fleet:1";

fn entity(kind: Value) -> Value {
    json!({ "id": VEHICLE, "type": kind })
}

/// EP-26: an answer that declares no type cannot be judged against a type grant, so it is not
/// served. A broker that omits `type` must not be a way to read an entity of a type nobody
/// granted.
#[test]
fn an_answer_without_a_type_is_not_served_under_a_type_grant() {
    let grant = types(&["Vehicle"]);
    for answer in [
        json!({ "id": VEHICLE }),
        json!({ "id": VEHICLE, "type": null }),
        json!({ "id": VEHICLE, "type": 1 }),
        json!({ "id": VEHICLE, "type": [] }),
        json!({ "id": VEHICLE, "type": [1, 2] }),
        json!({ "id": VEHICLE, "type": { "value": "Vehicle" } }),
        json!({ "id": VEHICLE, "type": "" }),
        json!([]),
        Value::Null,
    ] {
        assert!(!permitted(&answer, &grant), "{answer}");
    }

    // And with no type grant at all, the same answers are not judged on their type.
    for answer in [
        json!({ "id": VEHICLE }),
        json!({ "id": VEHICLE, "type": null }),
    ] {
        assert!(permitted(&answer, &Constraints::default()), "{answer}");
    }
}

/// A broker may answer the expanded IRI where the grant wrote the term; comparing the strings as
/// they came would let the expanded form through unjudged.
#[test]
fn an_expanded_type_is_compared_by_its_term() {
    let grant = types(&["Vehicle"]);
    for declared in [
        "Vehicle",
        "https://uri.fiware.org/ns/data-models#Vehicle",
        "https://smartdatamodels.org/dataModel.Transportation/Vehicle",
        "http://example.org/ns/Vehicle",
    ] {
        assert!(permitted(&entity(json!(declared)), &grant), "{declared}");
    }
}

/// And a type that only looks like the granted one is another type.
#[test]
fn a_type_that_merely_resembles_the_granted_one_is_refused() {
    let grant = types(&["Vehicle"]);
    for declared in [
        "VehicleModel",
        "MegaVehicle",
        "vehicle",
        "VEHICLE",
        "Vehicle ",
        " Vehicle",
        "urn:ngsi-ld:Vehicle",
        "https://uri.fiware.org/ns/data-models#Vehicle/Trailer",
        "https://uri.fiware.org/ns/Vehicle#",
    ] {
        assert!(!permitted(&entity(json!(declared)), &grant), "{declared}");
    }
}

/// NGSI-LD multi-typing: a grant is a statement about a type, not about it being the only one, so
/// an entity is served when any of its types is granted.
#[test]
fn an_entity_of_several_types_is_served_when_any_one_is_granted() {
    let grant = types(&["Vehicle"]);
    assert!(permitted(&entity(json!(["Vehicle", "Depot"])), &grant));
    assert!(permitted(
        &entity(json!(["Depot", "https://hel.fi/ns/Vehicle"])),
        &grant
    ));
    assert!(!permitted(&entity(json!(["Depot", "Trailer"])), &grant));
    assert!(!permitted(
        &entity(json!([null, 1, { "type": "Vehicle" }])),
        &grant
    ));
}

/// `@type` is read when `type` is absent, because that is the same statement in expanded JSON-LD.
#[test]
fn at_type_is_read_when_type_is_absent() {
    let grant = types(&["Vehicle"]);
    assert!(permitted(
        &json!({ "id": VEHICLE, "@type": "Vehicle" }),
        &grant
    ));
    assert!(permitted(
        &json!({ "id": VEHICLE, "@type": ["Vehicle"] }),
        &grant
    ));
    assert!(!permitted(
        &json!({ "id": VEHICLE, "@type": "Depot" }),
        &grant
    ));
    // `type` wins when both are there: it is the compacted form the answer is encoded in.
    assert!(!permitted(
        &json!({ "id": VEHICLE, "type": "Depot", "@type": "Vehicle" }),
        &grant
    ));
}

/// R24: an exact `id` in a selector becomes an anchored pattern, so it grants that entity and not
/// every id that contains it.
#[test]
fn an_exact_id_grant_is_anchored_to_that_one_entity() {
    let grant = ids(&[&format!("^{}$", regex::escape(VEHICLE))]);

    assert!(permitted(&json!({ "id": VEHICLE }), &grant));
    for other in [
        "urn:ngsi-ld:Vehicle:hel.fi:fleet:11",
        "urn:ngsi-ld:Vehicle:hel.fi:fleet:1x",
        "xurn:ngsi-ld:Vehicle:hel.fi:fleet:1",
        "urn:ngsi-ld:Depot:hel.fi:fleet:urn:ngsi-ld:Vehicle:hel.fi:fleet:1",
        "urn:ngsi-ld:Vehicle:hel.fi:fleet:1 ",
        "URN:NGSI-LD:VEHICLE:HEL.FI:FLEET:1",
    ] {
        assert!(!permitted(&json!({ "id": other }), &grant), "{other}");
    }
}

/// An entity with no id, or an id that is not a string, cannot be matched against a pattern and
/// is not served.
#[test]
fn an_answer_without_a_readable_id_is_refused_under_an_id_grant() {
    let grant = ids(&["^urn:ngsi-ld:Vehicle:hel\\.fi:fleet:.*$"]);
    for answer in [
        json!({ "type": "Vehicle" }),
        json!({ "id": null, "type": "Vehicle" }),
        json!({ "id": 1 }),
        json!({ "id": ["urn:ngsi-ld:Vehicle:hel.fi:fleet:1"] }),
        json!({}),
    ] {
        assert!(!permitted(&answer, &grant), "{answer}");
    }
    assert!(
        permitted(&json!({ "@id": VEHICLE }), &grant),
        "@id is the same statement"
    );
}

/// R24, second line: a pattern that does not compile matches nothing. A grant the gateway cannot
/// evaluate must not become a grant that lets everything through — and it must not poison the
/// patterns beside it either.
#[test]
fn a_pattern_that_does_not_compile_matches_nothing_and_stops_nothing() {
    assert!(!permitted(&json!({ "id": VEHICLE }), &ids(&["^(unclosed"])));
    assert!(!permitted(&json!({ "id": VEHICLE }), &ids(&["["])));
    assert!(!permitted(&json!({ "id": VEHICLE }), &ids(&["*"])));

    let one_of_each = ids(&["^(unclosed", &format!("^{}$", regex::escape(VEHICLE))]);
    assert!(
        permitted(&json!({ "id": VEHICLE }), &one_of_each),
        "the readable pattern still grants what it names"
    );
    assert!(!permitted(
        &json!({ "id": "urn:ngsi-ld:Vehicle:hel.fi:fleet:2" }),
        &one_of_each
    ));
}

/// Several patterns are a union, and every one of them is asked.
#[test]
fn any_one_of_the_id_patterns_is_enough() {
    let grant = ids(&[
        "^urn:ngsi-ld:Vehicle:hel\\.fi:fleet:1$",
        "^urn:ngsi-ld:Depot:hel\\.fi:fleet:.*$",
    ]);

    assert!(permitted(&json!({ "id": VEHICLE }), &grant));
    assert!(permitted(
        &json!({ "id": "urn:ngsi-ld:Depot:hel.fi:fleet:north" }),
        &grant
    ));
    assert!(!permitted(
        &json!({ "id": "urn:ngsi-ld:Trailer:hel.fi:fleet:1" }),
        &grant
    ));
}

/// The two halves are an `and`: the right type with the wrong id is refused, and so is the right
/// id with the wrong type.
#[test]
fn the_type_and_the_id_both_have_to_hold() {
    let grant = Constraints {
        types: set(&["Vehicle"]),
        id_patterns: set(&[&format!("^{}$", regex::escape(VEHICLE))]),
        ..Constraints::default()
    };

    assert!(permitted(
        &json!({ "id": VEHICLE, "type": "Vehicle" }),
        &grant
    ));
    assert!(!permitted(
        &json!({ "id": VEHICLE, "type": "Depot" }),
        &grant
    ));
    assert!(!permitted(
        &json!({ "id": "urn:ngsi-ld:Vehicle:hel.fi:fleet:2", "type": "Vehicle" }),
        &grant
    ));
}

/// A grant that narrows neither half judges neither: that is an endpoint whose Policies restrict
/// by attribute or by area, and the entity is admitted here and filtered there.
#[test]
fn a_grant_that_names_no_type_and_no_pattern_admits_any_entity() {
    let grant = Constraints::default();
    assert!(permitted(&entity(json!("Vehicle")), &grant));
    assert!(permitted(&entity(json!("Depot")), &grant));
    assert!(permitted(&json!({}), &grant));
}

/// An unanchored pattern matches anywhere in the id. `PolicySpec::validate` refuses a pattern
/// that does not start with `^` and end with `$` (jc-core `policy.rs:695`), so this is the shape
/// the gateway would see only if that check were removed — and the answer here would then be as
/// wide as the pattern, which is why the check is where it is.
#[test]
fn an_unanchored_pattern_would_match_anywhere_which_is_why_validation_refuses_one() {
    let unanchored = ids(&["Vehicle"]);
    assert!(permitted(&json!({ "id": VEHICLE }), &unanchored));
    assert!(permitted(
        &json!({ "id": "urn:ngsi-ld:Depot:x:y:my-Vehicle-shed" }),
        &unanchored
    ));

    let spec: jc_core::kinds::PolicySpec = serde_norway::from_str(
        r#"
contextSpaceRef: { kind: ContextSpace, name: fleet }
assigner: did:web:hel.fi
assignee: { kind: role, id: public }
operations: [retrieveOps]
information:
  - entities: [{ type: Vehicle, idPattern: "Vehicle" }]
"#,
    )
    .expect("the manifest parses");
    assert!(
        spec.validate().is_err(),
        "an unanchored idPattern never reaches the gateway"
    );
}

/// Asking does not change the answer that was judged.
#[test]
fn asking_leaves_the_entity_untouched() {
    let answer = json!({ "id": VEHICLE, "type": "Vehicle", "weight": 1 });
    let before = answer.clone();
    assert!(permitted(&answer, &types(&["Vehicle"])));
    assert_eq!(answer, before);
}
