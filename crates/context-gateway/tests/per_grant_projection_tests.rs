//! T-3530: each entity is served with the attributes of the grants it matches, never with the
//! union of every grant the caller holds (EP-16, MP-02, K9, K10, K12).
//!
//! The broker is told the OR of the grants' conditions and the union of their lists, which is
//! all CIM 009 can say. A public depot one grant admits used to come back with the
//! `securityCode` only a district grant names: the answer was projected with one set for the
//! whole request. Each test here is the read surface's own sequence: decide, keep what
//! `permitted` keeps, project.

use chrono::{DateTime, Utc};
use context_gateway::pdp::drop_types_that_may_not_be_filtered;
use context_gateway::pdp::evaluator::{evaluate, Constraints, Request, Subject, Verdict};
use context_gateway::pdp::projection;
use context_gateway::query;
use jc_core::kinds::{Operation, PolicySpec};
use serde_json::{json, Value};
use std::collections::BTreeSet;

const DISTRICT: &str = "georel=within;geometry=Polygon;coordinates=[[[21.0,48.0],[21.5,48.0],[21.5,48.5],[21.0,48.5],[21.0,48.0]]]";

fn now() -> DateTime<Utc> {
    "2026-10-10T12:00:00Z".parse().expect("a fixed instant")
}

fn grant(conditions: &str, names: &str) -> PolicySpec {
    serde_norway::from_str(&format!(
        "contextSpaceRef: depots\n\
         assigner: did:web:hel.fi\n\
         assignee: {{ kind: role, id: public }}\n\
         operations: [queryEntity, retrieveEntity]\n\
         {conditions}\n\
         information:\n  \
         - entities:\n      \
         - type: Depot\n\
         {names}"
    ))
    .expect("the policy spec parses")
}

fn listing(names: &[&str]) -> String {
    format!("    propertyNames: [{}]\n", names.join(", "))
}

fn depot(id: &str, public: bool, scope: &str, at: [f64; 2]) -> Value {
    json!({
        "id": format!("urn:ngsi-ld:Depot:{id}"),
        "type": "Depot",
        "scope": scope,
        "public": { "type": "Property", "value": public },
        "name": { "type": "Property", "value": format!("Depot {id}") },
        "securityCode": { "type": "Property", "value": "4711" },
        "keeper": { "type": "Property", "value": "Ms Novak" },
        "location": { "type": "GeoProperty", "value": { "type": "Point", "coordinates": at } },
    })
}

/// Public, in Košice: the public grant admits it, the Banská Bystrica grant does not.
fn x() -> Value {
    depot("x", true, "/geo/SK/KE", [21.25, 48.7])
}

/// Private, in Banská Bystrica: only the district grant admits it.
fn y() -> Value {
    depot("y", false, "/geo/SK/BB", [21.25, 48.25])
}

/// Private, in Prešov: no grant admits it.
fn z() -> Value {
    depot("z", false, "/geo/SK/PO", [21.25, 49.0])
}

fn constraints(policies: &[PolicySpec], request: &Request) -> Constraints {
    let verdict = evaluate(
        &Subject::anonymous(),
        Operation::QueryEntity,
        request,
        "depots",
        policies,
        now(),
    );
    let Verdict::Rewrite(constraints) = verdict else {
        panic!("the grants permit the read");
    };
    match drop_types_that_may_not_be_filtered(*constraints, request) {
        Verdict::Rewrite(constraints) => *constraints,
        Verdict::Deny => panic!("the filter rule refuses nothing here"),
    }
}

/// What the read surface serves of a broker answer: what `permitted` keeps, projected.
fn served(policies: &[PolicySpec], request: &Request, answer: Vec<Value>) -> Vec<Value> {
    let constraints = constraints(policies, request);
    let mut body = Value::Array(answer);
    if let Value::Array(entities) = &mut body {
        entities.retain(|entity| projection::permitted(entity, &constraints));
    }
    projection::project_by_type(&mut body, &constraints);
    body.as_array().cloned().unwrap_or_default()
}

fn by_id<'a>(answer: &'a [Value], id: &str) -> Option<&'a Value> {
    answer
        .iter()
        .find(|entity| entity["id"] == format!("urn:ngsi-ld:Depot:{id}"))
}

fn members(entity: &Value) -> BTreeSet<String> {
    entity
        .as_object()
        .map(|members| members.keys().cloned().collect())
        .unwrap_or_default()
}

fn set(names: &[&str]) -> BTreeSet<String> {
    names.iter().map(|name| (*name).to_owned()).collect()
}

fn public_names() -> PolicySpec {
    grant("q: \"public==true\"", &listing(&["name"]))
}

fn district_codes() -> PolicySpec {
    grant(
        "scopeQ: \"/geo/SK/BB\"",
        &listing(&["name", "securityCode"]),
    )
}

#[test]
fn attribute_of_a_grant_the_entity_does_not_match_is_not_served() {
    let answer = served(
        &[public_names(), district_codes()],
        &Request::default(),
        vec![x(), y()],
    );

    let public = by_id(&answer, "x").expect("the public grant serves x");
    assert_eq!(
        members(public),
        set(&["id", "type", "scope", "name"]),
        "x is public, not in /geo/SK/BB: only the public grant's name ({public})"
    );
    let district = by_id(&answer, "y").expect("the district grant serves y");
    assert_eq!(
        members(district),
        set(&["id", "type", "scope", "name", "securityCode"])
    );
}

#[test]
fn geo_grant_attributes_only_inside_its_area() {
    let area = grant(
        &format!("geoQ: \"{DISTRICT}\""),
        &listing(&["name", "location", "keeper"]),
    );
    let answer = served(&[public_names(), area], &Request::default(), vec![x(), y()]);

    let outside = by_id(&answer, "x").expect("the public grant serves x outside the area");
    assert!(outside.get("keeper").is_none(), "{outside}");
    assert!(outside.get("location").is_none(), "{outside}");
    let inside = by_id(&answer, "y").expect("the area grant serves y");
    assert_eq!(inside["keeper"]["value"], json!("Ms Novak"));
}

#[test]
fn grant_without_list_serves_whole_only_what_it_matches() {
    let whole_public = grant("q: \"public==true\"", "");
    let answer = served(
        &[whole_public, district_codes()],
        &Request::default(),
        vec![x(), y()],
    );

    let public = by_id(&answer, "x").expect("x");
    assert!(
        public.get("securityCode").is_some() && public.get("keeper").is_some(),
        "the list-less grant serves the public depot whole: {public}"
    );
    let private = by_id(&answer, "y").expect("y");
    assert!(
        private.get("keeper").is_none(),
        "the private depot is the district grant's, with its list only: {private}"
    );
}

#[test]
fn entity_matching_two_grants_gets_the_union() {
    let public_codes_in_kosice = grant("scopeQ: \"/geo/SK/KE\"", &listing(&["securityCode"]));
    let answer = served(
        &[public_names(), public_codes_in_kosice],
        &Request::default(),
        vec![x()],
    );

    assert_eq!(
        members(by_id(&answer, "x").expect("x")),
        set(&["id", "type", "scope", "name", "securityCode"])
    );
}

#[test]
fn an_entity_no_grant_matches_is_not_served() {
    let answer = served(
        &[public_names(), district_codes()],
        &Request::default(),
        vec![x(), y(), z()],
    );

    assert!(by_id(&answer, "z").is_none(), "{answer:?}");
    assert_eq!(answer.len(), 2);
}

#[test]
fn a_grant_condition_that_does_not_parse_matches_nothing() {
    let broken = grant("q: \"public===\"", &listing(&["name"]));
    let answer = served(&[broken], &Request::default(), vec![x()]);

    assert!(answer.is_empty(), "{answer:?}");
}

/// The T-3528 oracle per entity: whether x matched `securityCode=="4711"` would say its code,
/// which no grant x matches shows. x is not considered; y, whose grant shows the code, is.
#[test]
fn a_filter_on_an_attribute_the_entity_does_not_show_drops_the_entity() {
    for (parameter, value) in [("q", "securityCode==\"4711\""), ("orderBy", "securityCode")] {
        let params = vec![(parameter.to_owned(), value.to_owned())];
        let request = Request {
            referenced: query::referenced_attributes(&params),
            ..Request::default()
        };
        let answer = served(
            &[public_names(), district_codes()],
            &request,
            vec![x(), y()],
        );

        assert!(by_id(&answer, "x").is_none(), "{parameter}: {answer:?}");
        assert!(by_id(&answer, "y").is_some(), "{parameter}: {answer:?}");
    }
}

/// `attrs` selects the entities that carry one of the names (CIM 009). The broker is asked for
/// the condition attributes beside them, so an entity that carries only those is not answered.
#[test]
fn the_broker_is_asked_for_what_the_conditions_read_and_attrs_still_selects() {
    let params = vec![("attrs".to_owned(), "securityCode".to_owned())];
    let request = Request {
        attrs: set(&["securityCode"]),
        referenced: query::referenced_attributes(&params),
        ..Request::default()
    };
    let constraints = constraints(&[public_names(), district_codes()], &request);
    let upstream = query::upstream(&params, &constraints, &[]);
    let asked = query::parse(&upstream);
    let attrs = query::first(&asked, "attrs").unwrap_or_default();
    assert!(
        attrs.split(',').any(|name| name == "public"),
        "the public grant's q reads `public`: {upstream}"
    );

    let mut without = y();
    if let Some(members) = without.as_object_mut() {
        members.remove("securityCode");
    }
    let answer = served(
        &[public_names(), district_codes()],
        &request,
        vec![without, y()],
    );
    assert_eq!(answer.len(), 1, "{answer:?}");
    assert_eq!(answer[0]["securityCode"]["value"], json!("4711"));
}

/// The temporal representation repeats instances; the grant's condition is judged on it all the
/// same, and the projection keeps the shown attributes' instances.
#[test]
fn a_temporal_answer_is_judged_per_grant_too() {
    let history = json!({
        "id": "urn:ngsi-ld:Depot:y",
        "type": "Depot",
        "scope": "/geo/SK/BB",
        "public": [{ "type": "Property", "value": false, "observedAt": "2026-10-01T00:00:00Z" }],
        "securityCode": [
            { "type": "Property", "value": "4711", "observedAt": "2026-10-01T00:00:00Z" },
            { "type": "Property", "value": "4712", "observedAt": "2026-10-02T00:00:00Z" }
        ],
    });
    let answer = served(
        &[public_names(), district_codes()],
        &Request::default(),
        vec![history],
    );

    assert_eq!(answer.len(), 1, "{answer:?}");
    assert_eq!(answer[0]["securityCode"].as_array().map(Vec::len), Some(2));
    assert!(answer[0].get("public").is_none());
}

/// A grant without an id pattern reaches every id, beside a grant that has one: the patterns are
/// each grant's own, not one union every entity has to fall into.
#[test]
fn a_grant_without_an_id_pattern_reaches_ids_beside_one_with_a_pattern() {
    let only_y = grant(
        "",
        "        idPattern: \"^urn:ngsi-ld:Depot:y$\"\n    propertyNames: [name, securityCode]\n",
    );
    let answer = served(
        &[public_names(), only_y],
        &Request::default(),
        vec![x(), y()],
    );

    assert_eq!(
        members(by_id(&answer, "x").expect("the public grant reaches x")),
        set(&["id", "type", "scope", "name"])
    );
    assert!(by_id(&answer, "y").expect("y")["securityCode"].is_object());
}
