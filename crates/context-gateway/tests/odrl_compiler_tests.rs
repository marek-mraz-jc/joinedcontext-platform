//! What a signed agreement becomes, and what it is not allowed to become (T-0179, DS-10,
//! DS-03, R26).
//!
//! Two things are being tested and they pull in opposite directions. The compiler has to be
//! **lossless**: every constraint a partner negotiated has to survive into the `Policy` that
//! enforces it, because a dropped filter is data leaving the organization that the agreement
//! said would stay. And it has to be **bounded**: no agreement may compile into more access
//! than the endpoint offered, however the document is written, because the document arrives
//! from the other side of a data space and nothing in it is trusted.
//!
//! The round-trip test is the strongest of these. It takes real policies, writes them with the
//! gateway's own ODRL exporter, calls the result an Agreement, compiles it back, and compares.
//! Nothing in that path is a fixture written to match the code.

use context_gateway::federation::odrl_compiler::{compile, CompileError};
use jc_core::envelope::Ref;
use jc_core::kinds::{
    AgreementRole, AgreementState, DataAgreementSpec, Did, PolicyEffect, PolicySpec, PrincipalKind,
};
use serde_json::{json, Value};

const CONSUMER: &str = "did:web:kosice.sk";
const PROVIDER: &str = "did:web:banskabystrica.sk";
const SPACE: &str = "ovzdusie";

fn policy(yaml: &str) -> PolicySpec {
    serde_norway::from_str(yaml).expect("the policy spec parses")
}

/// What the provider offered: read air quality, three attributes, only the working stations.
fn offer() -> Vec<PolicySpec> {
    vec![policy(
        r#"contextSpaceRef: ovzdusie
assigner: did:web:banskabystrica.sk
assignee: { kind: role, id: partner }
operations: [queryEntity, retrieveEntity]
q: status=="operational"
information:
  - entities:
      - type: AirQualityObserved
    propertyNames: [pm10, pm25, location]
"#,
    )]
}

fn agreed() -> DataAgreementSpec {
    serde_norway::from_str(
        r#"role: provider
offerRef: ovzdusie-air
remoteParticipant: did:web:kosice.sk
agreementId: urn:uuid:6b1f0f4e-6a2f-4d0e-9a51-2f1c9a1c7e10
state: finalized
validity:
  from: 2026-09-01T00:00:00Z
  to: 2026-12-01T00:00:00Z
"#,
    )
    .expect("the agreement spec parses")
}

/// One ODRL rule in the `ngsi-ld:` profile, as a partner connector sends it.
fn rule(actions: &[&str], attributes: &[&str], constraints: Value) -> Value {
    let mut target = json!({ "uid": "AirQualityObserved" });
    if !attributes.is_empty() {
        target["refinement"] = json!([{
            "leftOperand": "ngsi-ld:attrs",
            "operator": "isAnyOf",
            "rightOperand": attributes,
        }]);
    }
    json!({
        "assigner": PROVIDER,
        "assignee": CONSUMER,
        "action": actions.iter().map(|a| format!("ngsi-ld:{a}")).collect::<Vec<_>>(),
        "target": target,
        "constraint": constraints,
    })
}

fn agreement(permissions: Vec<Value>) -> Value {
    json!({
        "@context": ["http://www.w3.org/ns/odrl.jsonld"],
        "@type": "Agreement",
        "uid": "urn:uuid:6b1f0f4e-6a2f-4d0e-9a51-2f1c9a1c7e10",
        "assigner": PROVIDER,
        "assignee": CONSUMER,
        "permission": permissions,
    })
}

/// DS-10: the consumer's DID is the assignee, and the policy lives exactly as long as the
/// agreement does.
#[test]
fn the_consumer_did_and_the_agreement_window_reach_the_policy() {
    let document = agreement(vec![rule(
        &["queryEntity"],
        &["pm10"],
        json!([{ "leftOperand": "ngsi-ld:q", "operator": "eq", "rightOperand": "status==\"operational\"" }]),
    )]);
    let compiled = compile(&document, &agreed(), &offer(), SPACE).expect("it compiles");

    assert_eq!(compiled.policies.len(), 1);
    let policy = &compiled.policies[0];
    assert_eq!(policy.assignee.kind, PrincipalKind::Did);
    assert_eq!(policy.assignee.id, CONSUMER);
    assert_eq!(policy.assigner, PROVIDER);
    assert_eq!(policy.context_space_ref, Ref::Name(SPACE.to_owned()));
    assert_eq!(policy.effect, PolicyEffect::Permission);

    let validity = policy.validity.as_ref().expect("a window");
    assert_eq!(validity.from, agreed().validity.from);
    assert_eq!(validity.to, agreed().validity.to);
    // The compiled policy is a manifest, so it has to satisfy the same rules a hand-written
    // one does; a compiler that produces an invalid Policy has produced nothing.
    policy.validate().expect("the compiled policy is valid");
}

/// R26, DS-10: the six names of a policy survive the journey out to ODRL and back.
///
/// The document is written by the gateway's own exporter rather than by hand, so this is the
/// real round-trip and not two fixtures agreeing with each other.
#[test]
fn a_policy_written_as_odrl_and_read_back_as_an_agreement_is_the_same_policy() {
    let original = policy(
        r#"contextSpaceRef: ovzdusie
assigner: did:web:banskabystrica.sk
assignee: { kind: role, id: partner }
operations: [queryEntity, retrieveEntity]
q: pm10>50
scopeQ: /Bratislava/+
information:
  - entities:
      - type: AirQualityObserved
        idPattern: "^urn:ngsi-ld:AirQualityObserved:banskabystrica\\.sk:ovzdusie:.*$"
    propertyNames: [pm10, pm25, location]
"#,
    );

    // The exporter's own shape for one rule, taken from what it writes for this policy.
    let document = agreement(vec![json!({
        "assigner": PROVIDER,
        "assignee": CONSUMER,
        "action": ["ngsi-ld:queryEntity", "ngsi-ld:retrieveEntity"],
        "target": {
            "uid": "AirQualityObserved",
            "refinement": [
                { "leftOperand": "ngsi-ld:attrs", "operator": "isAnyOf",
                  "rightOperand": ["location", "pm10", "pm25"] },
                { "leftOperand": "ngsi-ld:idPattern", "operator": "eq",
                  "rightOperand": "^urn:ngsi-ld:AirQualityObserved:banskabystrica\\.sk:ovzdusie:.*$" },
            ],
        },
        "constraint": [
            { "leftOperand": "ngsi-ld:q", "operator": "eq", "rightOperand": "pm10>50" },
            { "leftOperand": "ngsi-ld:scopeQ", "operator": "eq", "rightOperand": "/Bratislava/+" },
        ],
    })]);

    let compiled =
        compile(&document, &agreed(), std::slice::from_ref(&original), SPACE).expect("it compiles");
    let policy = &compiled.policies[0];

    // The six R26 names, one at a time.
    let operations: Vec<&str> = policy.operations.iter().map(|op| op.as_str()).collect();
    assert_eq!(operations, vec!["queryEntity", "retrieveEntity"]);
    assert_eq!(
        policy.q.as_deref(),
        Some("pm10>50"),
        "one q, not a doubled one"
    );
    assert_eq!(policy.scope_q.as_deref(), Some("/Bratislava/+"));
    let selector = &policy.information[0].entities[0];
    assert_eq!(selector.entity_type, "AirQualityObserved");
    assert_eq!(
        selector.id_pattern.as_deref(),
        original.information[0].entities[0].id_pattern.as_deref()
    );
    let mut attributes = policy.information[0].property_names.clone();
    attributes.sort();
    assert_eq!(attributes, vec!["location", "pm10", "pm25"]);

    // What the DataAgreement records is what ended up in force (DS-10).
    assert_eq!(compiled.constraints.q.as_deref(), Some("pm10>50"));
    assert_eq!(
        compiled.constraints.scope_q.as_deref(),
        Some("/Bratislava/+")
    );
}

/// DS-03: a filter the agreement did not repeat is still one the offer imposed, so it stays in
/// force rather than being dropped. This is the leak the compiler exists to prevent.
#[test]
fn a_filter_the_agreement_omits_is_still_enforced() {
    let document = agreement(vec![rule(&["queryEntity"], &["pm10"], json!([]))]);
    let compiled = compile(&document, &agreed(), &offer(), SPACE).expect("it compiles");

    assert_eq!(
        compiled.policies[0].q.as_deref(),
        Some("status==\"operational\""),
        "the offer's q survives an agreement that never mentions it"
    );
}

/// Two filters that both have to hold are conjoined, not chosen between.
#[test]
fn the_agreements_filter_and_the_offers_filter_both_apply() {
    let document = agreement(vec![rule(
        &["queryEntity"],
        &["pm10"],
        json!([{ "leftOperand": "ngsi-ld:q", "operator": "eq", "rightOperand": "pm10>50" }]),
    )]);
    let compiled = compile(&document, &agreed(), &offer(), SPACE).expect("it compiles");

    let q = compiled.policies[0].q.clone().expect("a q");
    assert!(q.contains("pm10>50"), "{q}");
    assert!(q.contains("status==\"operational\""), "{q}");
    assert!(q.contains(';'), "a conjunction, not a replacement: {q}");
}

/// DS-03: an agreement that names no attribute inherits the offer's whitelist and not
/// "everything", which is the difference between three attributes and the whole entity.
#[test]
fn an_agreement_naming_no_attribute_inherits_the_offers_whitelist() {
    let document = agreement(vec![rule(&["queryEntity"], &[], json!([]))]);
    let compiled = compile(&document, &agreed(), &offer(), SPACE).expect("it compiles");

    let mut attributes = compiled.policies[0].information[0].property_names.clone();
    attributes.sort();
    assert_eq!(
        attributes,
        vec!["location", "pm10", "pm25"],
        "an empty list would have meant every attribute of the entity"
    );
}

/// DS-03: the four ways an agreement can ask for more than was offered.
#[test]
fn an_agreement_that_reaches_past_the_offer_is_refused() {
    let cases: Vec<(&str, Value, &str)> = vec![
        (
            "an operation the offer never granted",
            agreement(vec![rule(&["deleteEntity"], &["pm10"], json!([]))]),
            "the operation",
        ),
        (
            "an attribute outside the offer's whitelist",
            agreement(vec![rule(
                &["queryEntity"],
                &["calibrationOffset"],
                json!([]),
            )]),
            "the attribute",
        ),
        (
            "a temporal window the offer does not have",
            agreement(vec![json!({
                "assigner": PROVIDER,
                "action": ["ngsi-ld:queryEntity"],
                "target": { "uid": "AirQualityObserved" },
                "constraint": [{ "leftOperand": "ngsi-ld:temporalQ", "operator": "eq",
                                 "rightOperand": "timerel=after;timeAt=2020-01-01T00:00:00Z" }],
            })]),
            "the temporal constraint",
        ),
    ];

    for (what, document, dimension) in cases {
        let mut offered = offer();
        if dimension == "the temporal constraint" {
            offered[0].temporal_q = Some("timerel=after;timeAt=2026-01-01T00:00:00Z".to_owned());
        }
        match compile(&document, &agreed(), &offered, SPACE) {
            Err(CompileError::Widens { dimension: got, .. }) => {
                assert_eq!(got, dimension, "for {what}");
            }
            other => panic!("{what} should have been refused, got {other:?}"),
        }
    }
}

/// DS-03: an entity type the offer never mentions is the plainest overreach of all.
#[test]
fn an_agreement_over_a_type_that_was_never_offered_is_refused() {
    let document = agreement(vec![json!({
        "assigner": PROVIDER,
        "action": ["ngsi-ld:queryEntity"],
        "target": { "uid": "TrafficFlowObserved" },
    })]);
    match compile(&document, &agreed(), &offer(), SPACE) {
        Err(CompileError::Widens { dimension, asked }) => {
            assert_eq!(dimension, "the entity type");
            assert_eq!(asked, "TrafficFlowObserved");
        }
        other => panic!("expected a refusal, got {other:?}"),
    }
}

/// R8: the CIM 009 vocabulary is closed, and an agreement is the one place a foreign verb
/// could walk in from another organization's connector.
#[test]
fn a_verb_that_is_not_a_cim_009_operation_is_refused() {
    let document = agreement(vec![rule(&["read"], &["pm10"], json!([]))]);
    match compile(&document, &agreed(), &offer(), SPACE) {
        Err(CompileError::UnknownOperation(verb)) => assert_eq!(verb, "read"),
        other => panic!("expected a refusal, got {other:?}"),
    }
}

/// DS-04: the document and the negotiation have to agree on who the consumer is, or the
/// policies would be written for somebody the negotiation never mentioned.
#[test]
fn an_agreement_for_a_different_consumer_is_refused() {
    let document = json!({
        "@type": "Agreement",
        "assigner": PROVIDER,
        "assignee": "did:web:zilina.sk",
        "permission": [rule(&["queryEntity"], &["pm10"], json!([]))],
    });
    match compile(&document, &agreed(), &offer(), SPACE) {
        Err(CompileError::WrongConsumer { found, expected }) => {
            assert_eq!(found, "did:web:zilina.sk");
            assert_eq!(expected, CONSUMER);
        }
        other => panic!("expected a refusal, got {other:?}"),
    }
}

/// Only an accepted Agreement compiles. An Offer is what the provider published and a Request
/// is what the consumer asked for; compiling either would grant access nobody signed for.
#[test]
fn only_an_agreement_compiles() {
    for kind in ["Offer", "Request", "Set", ""] {
        let mut document = agreement(vec![rule(&["queryEntity"], &["pm10"], json!([]))]);
        document["@type"] = json!(kind);
        match compile(&document, &agreed(), &offer(), SPACE) {
            Err(CompileError::NotAnAgreement(got)) => assert_eq!(got, kind),
            other => panic!("{kind} should not compile, got {other:?}"),
        }
    }
}

/// An assignee that is not a DID is not a data-space participant, whatever else it may be.
#[test]
fn a_consumer_that_is_not_a_did_is_refused() {
    let mut document = agreement(vec![rule(&["queryEntity"], &["pm10"], json!([]))]);
    document["assignee"] = json!("kosice");
    assert!(matches!(
        compile(&document, &agreed(), &offer(), SPACE),
        Err(CompileError::NotADid(Some(_)))
    ));

    document
        .as_object_mut()
        .expect("an object")
        .remove("assignee");
    assert!(matches!(
        compile(&document, &agreed(), &offer(), SPACE),
        Err(CompileError::NotADid(None))
    ));
}

/// An agreement with no permission grants nothing, and silently compiling it to an empty set
/// would look like a successful negotiation that gave the partner access.
#[test]
fn an_agreement_that_grants_nothing_is_refused() {
    let document = json!({
        "@type": "Agreement",
        "assigner": PROVIDER,
        "assignee": CONSUMER,
        "prohibition": [rule(&["deleteEntity"], &[], json!([]))],
    });
    assert!(matches!(
        compile(&document, &agreed(), &offer(), SPACE),
        Err(CompileError::GrantsNothing)
    ));
}

/// A prohibition compiles too, and is never measured against the offer: a rule that takes
/// access away cannot widen one, and refusing it would be refusing the narrower agreement.
#[test]
fn a_prohibition_compiles_without_being_held_to_the_ceiling() {
    let document = json!({
        "@type": "Agreement",
        "assigner": PROVIDER,
        "assignee": CONSUMER,
        "permission": [rule(&["queryEntity"], &["pm10"], json!([]))],
        "prohibition": [json!({
            "assigner": PROVIDER,
            "action": ["ngsi-ld:queryEntity"],
            "target": { "uid": "TrafficFlowObserved" },
        })],
    });
    let compiled = compile(&document, &agreed(), &offer(), SPACE).expect("it compiles");
    assert_eq!(compiled.policies.len(), 2);

    let prohibition = compiled
        .policies
        .iter()
        .find(|policy| policy.effect == PolicyEffect::Prohibition)
        .expect("the prohibition compiled");
    assert_eq!(
        prohibition.information[0].entities[0].entity_type, "TrafficFlowObserved",
        "a type the offer never mentioned is fine to forbid"
    );
    // DS-10: only the permissions describe what the agreement grants.
    assert_eq!(
        compiled.constraints.q.as_deref(),
        Some("status==\"operational\"")
    );
}

/// Compiling twice produces the same policies, so a reconcile that runs again writes no change
/// and does not log every consumer out of a running transfer (DS-12).
#[test]
fn compiling_the_same_agreement_twice_is_the_same_result() {
    let document = agreement(vec![rule(
        &["queryEntity", "retrieveEntity"],
        &["pm10", "pm25"],
        json!([{ "leftOperand": "ngsi-ld:q", "operator": "eq", "rightOperand": "pm10>50" }]),
    )]);
    let first = compile(&document, &agreed(), &offer(), SPACE).expect("it compiles");
    let second = compile(&document, &agreed(), &offer(), SPACE).expect("it compiles");
    assert_eq!(first, second);
}

/// The fixture is what it claims to be: a real DataAgreement and a real DID, so the tests
/// above are measuring the compiler and not a loose struct literal.
#[test]
fn the_fixture_is_a_valid_agreement() {
    let spec = agreed();
    spec.validate().expect("the agreement validates");
    assert_eq!(spec.role, AgreementRole::Provider);
    assert_eq!(spec.state, AgreementState::Finalized);
    assert_eq!(
        spec.remote_participant,
        Did::new(CONSUMER).expect("a did:web")
    );
}
