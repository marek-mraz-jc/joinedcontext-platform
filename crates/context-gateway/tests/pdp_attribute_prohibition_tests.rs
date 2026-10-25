//! GW4, GW8, EP-47 (T-5586): a prohibition that names attributes takes back only those, as the
//! schema, the access document and the assistant already read it; one naming none still refuses
//! the whole type.

use chrono::{DateTime, Utc};
use context_gateway::auth::subject::Subject;
use context_gateway::pdp::evaluator::{evaluate, Constraints, Request, Verdict};
use context_gateway::pdp::projection::project_entity;
use context_gateway::pdp::{drop_types_that_may_not_be_filtered, write_guard};
use jc_core::kinds::{Operation, PolicySpec};
use serde_json::json;
use std::collections::BTreeSet;

fn now() -> DateTime<Utc> {
    "2026-10-25T12:00:00Z".parse().expect("a fixed instant")
}

fn policy(yaml: &str) -> PolicySpec {
    serde_norway::from_str(yaml).expect("the policy spec parses")
}

fn set(names: &[&str]) -> BTreeSet<String> {
    names.iter().map(|name| (*name).to_owned()).collect()
}

fn editor() -> Subject {
    Subject {
        roles: set(&["editor"]),
        ..Subject::anonymous()
    }
}

fn sensors() -> Request {
    Request {
        types: set(&["Sensor"]),
        ..Request::default()
    }
}

/// A grant of `operations` on Sensor, naming `attrs` when given, and a prohibition of the same
/// operations naming `prohibited` (no attribute: the whole type).
fn policies(operations: &str, attrs: &str, prohibited: &str) -> Vec<PolicySpec> {
    let names = |list: &str| {
        if list.is_empty() {
            String::new()
        } else {
            format!("    propertyNames: [{list}]\n")
        }
    };
    let body = |effect: &str, list: &str| {
        format!(
            "contextSpaceRef: sensors\n{effect}assigner: did:web:example.org\n\
             assignee: {{ kind: role, id: editor }}\noperations: [{operations}]\n\
             information:\n  - entities:\n      - type: Sensor\n{}",
            names(list)
        )
    };
    vec![
        policy(&body("", attrs)),
        policy(&body("effect: prohibition\n", prohibited)),
    ]
}

fn decide(operation: Operation, request: &Request, policies: &[PolicySpec]) -> Verdict {
    evaluate(&editor(), operation, request, "sensors", policies, now())
}

fn rewrite(verdict: &Verdict) -> &Constraints {
    verdict.constraints().expect("a rewrite, not a deny")
}

#[test]
fn a_write_prohibition_on_status_refuses_a_patch_of_status_and_lets_level_through() {
    for granted in ["", "level, note, status"] {
        let policies = policies("updateAttrs", granted, "status");
        let verdict = decide(Operation::UpdateAttrs, &sensors(), &policies);
        let constraints = rewrite(&verdict);
        assert_eq!(constraints.hidden, set(&["status"]), "grant [{granted}]");
        assert!(
            write_guard::check_fragment(
                &json!({"level": {"type": "Property", "value": 3}}),
                constraints
            )
            .is_ok(),
            "level is granted and not prohibited (grant [{granted}])"
        );
        assert_eq!(
            write_guard::check_fragment(
                &json!({"level": {"type": "Property", "value": 3},
                        "status": {"type": "Property", "value": "ok"}}),
                constraints
            ),
            Err(write_guard::Refusal::AttributeOutsideGrant(
                "status".to_owned()
            )),
            "a write touching the prohibited attribute is refused whole (grant [{granted}])"
        );
    }
}

#[test]
fn a_read_prohibition_on_owner_email_drops_it_from_answers() {
    let policies = policies("queryEntity", "", "ownerEmail");
    let verdict = decide(Operation::QueryEntity, &sensors(), &policies);
    let constraints = rewrite(&verdict);
    assert!(
        constraints.restricted,
        "the answer says it is narrowed (R22)"
    );
    let mut entity = json!({"id": "urn:ngsi-ld:Sensor:s1", "type": "Sensor",
        "level": {"type": "Property", "value": 3},
        "ownerEmail": {"type": "Property", "value": "a@example.org"}});
    project_entity(&mut entity, &constraints.attrs, &constraints.hidden);
    assert_eq!(entity.get("ownerEmail"), None);
    assert!(
        entity.get("level").is_some(),
        "the rest of the entity is served"
    );

    // Asking for the withheld attribute alone selects nothing: `attrs` selects the entities
    // carrying it, which would tell which carry it.
    let asked = Request {
        attrs: set(&["ownerEmail"]),
        ..sensors()
    };
    let verdict = decide(Operation::QueryEntity, &asked, &policies);
    assert!(rewrite(&verdict).empty);
}

#[test]
fn a_filter_on_a_read_prohibited_attribute_answers_nothing() {
    let policies = policies("queryEntity", "", "ownerEmail");
    let request = Request {
        q: Some("ownerEmail==\"a@example.org\"".to_owned()),
        referenced: set(&["ownerEmail"]),
        ..sensors()
    };
    let Verdict::Rewrite(constraints) = decide(Operation::QueryEntity, &request, &policies) else {
        panic!("the prohibition names an attribute, so the read is narrowed, not refused");
    };
    let filtered = drop_types_that_may_not_be_filtered(*constraints, &request);
    assert!(
        filtered.constraints().is_some_and(|c| c.empty),
        "the filter is an oracle on the withheld value (T-1862)"
    );

    let other = Request {
        q: Some("level>2".to_owned()),
        referenced: set(&["level"]),
        ..sensors()
    };
    let Verdict::Rewrite(constraints) = decide(Operation::QueryEntity, &other, &policies) else {
        panic!("a rewrite");
    };
    let filtered = drop_types_that_may_not_be_filtered(*constraints, &other);
    assert!(filtered.constraints().is_some_and(|c| !c.empty));
}

#[test]
fn a_prohibition_naming_no_attribute_still_refuses_the_whole_type() {
    let policies = policies("updateAttrs, queryEntity", "", "");
    assert_eq!(
        decide(Operation::UpdateAttrs, &sensors(), &policies),
        Verdict::Deny
    );
    assert_eq!(
        decide(Operation::QueryEntity, &Request::default(), &policies),
        Verdict::Deny
    );
}

/// The grant named only what the prohibition takes back: nothing is left, and an empty
/// whitelist must not read as the whole entity.
#[test]
fn a_prohibition_taking_back_every_granted_attribute_refuses() {
    let policies = policies("updateAttrs", "status", "status");
    assert_eq!(
        decide(Operation::UpdateAttrs, &sensors(), &policies),
        Verdict::Deny
    );
}

/// One entry naming an attribute never softens another entry's whole-type prohibition.
#[test]
fn an_attribute_entry_beside_a_whole_type_entry_still_refuses() {
    let mut policies = policies("updateAttrs", "", "status");
    let prohibition = policy(
        "contextSpaceRef: sensors\neffect: prohibition\nassigner: did:web:example.org\n\
         assignee: { kind: role, id: editor }\noperations: [updateAttrs]\n\
         information:\n  - entities:\n      - type: Sensor\n    propertyNames: [status]\n\
         \x20\x20- entities:\n      - type: Sensor\n",
    );
    policies[1] = prohibition;
    assert_eq!(
        decide(Operation::UpdateAttrs, &sensors(), &policies),
        Verdict::Deny
    );
}

/// A prohibition on another type takes nothing from this request.
#[test]
fn an_attribute_prohibition_on_another_type_withholds_nothing() {
    let policies = policies("updateAttrs", "", "status");
    let vehicles = Request {
        types: set(&["Vehicle", "Sensor"]),
        ..Request::default()
    };
    let verdict = decide(Operation::UpdateAttrs, &vehicles, &policies);
    assert_eq!(rewrite(&verdict).hidden, set(&["status"]));
    let lamps = Request {
        types: set(&["Lamp"]),
        ..Request::default()
    };
    let verdict = decide(Operation::UpdateAttrs, &lamps, &policies);
    assert!(rewrite(&verdict).hidden.is_empty());
}
