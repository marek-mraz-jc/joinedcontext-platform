//! Edge cases of `federation::odrl_compiler::compile` (T-1903, EP-26, MP-02).
//!
//! Contract, in one sentence: an ODRL document that arrives from the other side of a data
//! space compiles into `Policy` manifests only when it is an `Agreement`, assigned to the DID
//! the negotiation recorded, carrying at least one permission, and reaching past nothing the
//! endpoint offered — and what comes out is assigned to that consumer, lives exactly as long
//! as the negotiated agreement, and keeps every constraint the offer or the document imposed
//! (DS-03, DS-04, DS-10, R8).
//!
//! Nothing in the document is trusted: it is written by the other participant's connector.
//! `odrl_compiler_tests.rs` owns the round trip and one case per refusal. These are the edges
//! around them — the envelope members missing or of the wrong type, the near-misses of a DID,
//! a rule with no action, one widening rule among several, and the two places where the
//! document could otherwise decide something the negotiation already decided.

use context_gateway::federation::odrl_compiler::{compile, CompileError};
use jc_core::kinds::{DataAgreementSpec, PolicyEffect, PolicySpec, PrincipalKind};
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

fn rule(actions: &[&str], attributes: &[&str]) -> Value {
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

fn compiled(document: &Value) -> Result<Vec<PolicySpec>, CompileError> {
    compile(document, &agreed(), &offer(), SPACE).map(|compiled| compiled.policies)
}

#[test]
fn only_a_document_that_says_it_is_an_agreement_compiles() {
    for kind in [
        json!("Offer"),
        json!("Policy"),
        json!("agreement"),
        json!("AGREEMENT"),
        json!(" Agreement"),
        json!("Agreement "),
        json!("odrl:Agreement"),
        json!(null),
        json!(7),
        json!(["Agreement"]),
    ] {
        let mut document = agreement(vec![rule(&["retrieveEntity"], &[])]);
        document["@type"] = kind.clone();
        assert!(
            matches!(compiled(&document), Err(CompileError::NotAnAgreement(_))),
            "{kind} is not an Agreement",
        );
    }

    let mut missing = agreement(vec![rule(&["retrieveEntity"], &[])]);
    missing.as_object_mut().expect("an object").remove("@type");
    assert!(matches!(
        compiled(&missing),
        Err(CompileError::NotAnAgreement(_)),
    ));
}

/// DS-04: a consumer is identified by a DID, and the check is on the scheme as it is written.
#[test]
fn a_consumer_that_is_not_a_did_is_refused_whatever_it_looks_like() {
    for assignee in [
        json!("https://kosice.sk"),
        json!("kosice.sk"),
        json!("DID:WEB:kosice.sk"),
        json!(" did:web:kosice.sk"),
        json!("urn:did:web:kosice.sk"),
        json!(""),
        json!(null),
        json!({ "@id": CONSUMER }),
        json!([CONSUMER]),
    ] {
        let mut document = agreement(vec![rule(&["retrieveEntity"], &[])]);
        document["assignee"] = assignee.clone();
        assert!(
            matches!(compiled(&document), Err(CompileError::NotADid(_))),
            "{assignee} is not a DID",
        );
    }
}

/// DS-10: the document may not choose who it is for. The negotiation recorded the participant,
/// and a document assigned to anyone else — however close the string — is refused.
#[test]
fn the_consumer_is_the_one_the_negotiation_recorded_and_no_near_miss_passes() {
    for other in [
        "did:web:kosice.sk ",
        "did:web:Kosice.sk",
        "did:web:kosice.sk.evil.example",
        "did:web:kosice.s",
        "did:web:kosice.sk/",
        "did:web:helsinki.fi",
    ] {
        let mut document = agreement(vec![rule(&["retrieveEntity"], &[])]);
        document["assignee"] = json!(other);
        assert!(
            matches!(compiled(&document), Err(CompileError::WrongConsumer { .. })),
            "{other:?} is not the participant the negotiation recorded",
        );
    }
}

#[test]
fn an_agreement_with_no_permission_at_all_grants_nothing_and_is_refused() {
    let mut empty = agreement(vec![]);
    assert!(matches!(compiled(&empty), Err(CompileError::GrantsNothing)));

    empty
        .as_object_mut()
        .expect("an object")
        .remove("permission");
    assert!(matches!(compiled(&empty), Err(CompileError::GrantsNothing)));

    // A document of prohibitions only takes access away from access nobody was given.
    let mut only_prohibition = agreement(vec![]);
    only_prohibition["prohibition"] = json!([rule(&["retrieveEntity"], &[])]);
    assert!(matches!(
        compiled(&only_prohibition),
        Err(CompileError::GrantsNothing),
    ));
}

/// R8: the verb vocabulary is closed, and it is closed against what the document writes rather
/// than against a list kept here.
#[test]
fn a_verb_the_vocabulary_does_not_have_is_refused() {
    for action in [
        "deleteEntity ",
        "ngsi-ld:dropEverything",
        "RetrieveEntity",
        "",
        "*",
    ] {
        let document = agreement(vec![json!({
            "assigner": PROVIDER,
            "assignee": CONSUMER,
            "action": [action],
            "target": { "uid": "AirQualityObserved" },
        })]);
        assert!(
            matches!(compiled(&document), Err(CompileError::UnknownOperation(_))),
            "{action:?} is not a CIM 009 operation",
        );
    }

    // The vocabulary is the CIM 009 one either way it is written: the compacted IRI the
    // exporter emits and the bare term both name the same operation.
    for action in ["ngsi-ld:retrieveEntity", "retrieveEntity"] {
        let document = agreement(vec![json!({
            "assigner": PROVIDER,
            "assignee": CONSUMER,
            "action": [action],
            "target": { "uid": "AirQualityObserved" },
        })]);
        let policies = compiled(&document).unwrap_or_else(|e| panic!("{action:?}: {e}"));
        assert_eq!(policies[0].operations.len(), 1, "{action:?}");
    }
}

/// A rule that names no action asks for no operation. It compiles — there is nothing to hold
/// to the ceiling — into a policy that permits nothing, because the PDP grants on a match in
/// the operations list and an empty list matches no operation.
#[test]
fn a_rule_with_no_action_compiles_into_a_policy_that_permits_no_operation() {
    let document = agreement(vec![json!({
        "assigner": PROVIDER,
        "assignee": CONSUMER,
        "action": [],
        "target": { "uid": "AirQualityObserved" },
    })]);

    match compiled(&document) {
        Ok(policies) => {
            assert_eq!(policies.len(), 1);
            assert!(
                policies[0].operations.is_empty(),
                "no operation is named, so none is granted",
            );
        }
        Err(CompileError::GrantsNothing) => {}
        Err(other) => panic!("a rule with no action is not an error of its own: {other}"),
    }
}

/// DS-03 across all four dimensions, and the property that matters more than any one of them:
/// a document is compiled whole or not at all, so one widening rule among three costs the
/// whole agreement rather than being dropped quietly.
#[test]
fn one_widening_rule_refuses_the_whole_agreement() {
    let document = agreement(vec![
        rule(&["retrieveEntity"], &["pm10"]),
        rule(&["queryEntity"], &["salary"]),
        rule(&["retrieveEntity"], &["pm25"]),
    ]);

    match compiled(&document) {
        Err(CompileError::Widens { dimension, asked }) => {
            assert_eq!(dimension, "the attribute");
            assert_eq!(asked, "salary");
        }
        other => panic!("the whole agreement is refused, not two thirds of it: {other:?}"),
    }
}

#[test]
fn a_type_the_offer_never_mentions_is_refused_however_it_is_spelled() {
    for entity_type in [
        "Vehicle",
        "airqualityobserved",
        "AirQualityObserved ",
        " AirQualityObserved",
        "AirQualityObserved2",
        "*",
        "",
    ] {
        let document = agreement(vec![json!({
            "assigner": PROVIDER,
            "assignee": CONSUMER,
            "action": ["ngsi-ld:retrieveEntity"],
            "target": { "uid": entity_type },
        })]);
        assert!(
            matches!(compiled(&document), Err(CompileError::Widens { .. })),
            "{entity_type:?} was never offered",
        );
    }
}

#[test]
fn an_operation_the_offer_does_not_grant_is_refused() {
    let document = agreement(vec![rule(&["createEntity"], &[])]);
    match compiled(&document) {
        Err(CompileError::Widens { dimension, asked }) => {
            assert_eq!(dimension, "the operation");
            assert_eq!(asked, "createEntity");
        }
        other => panic!("the offer grants two read verbs and nothing else: {other:?}"),
    }
}

/// DS-10, DS-12: the window comes from the negotiated `DataAgreement` and never from the
/// document, so a partner cannot write itself a longer agreement than the one that was signed.
#[test]
fn the_validity_is_the_negotiated_one_and_the_document_cannot_extend_it() {
    let mut document = agreement(vec![rule(&["retrieveEntity"], &["pm10"])]);
    document["permission"][0]["constraint"] = json!([{
        "leftOperand": "dateTime",
        "operator": "lteq",
        "rightOperand": "2099-01-01T00:00:00Z",
    }]);

    let policies = compiled(&document).expect("it compiles");
    let validity = policies[0].validity.clone().expect("a window");
    assert_eq!(validity.from, agreed().validity.from);
    assert_eq!(validity.to, agreed().validity.to);
}

/// The assignee of every compiled policy is the consumer's DID — not the document's assigner,
/// not a role, not a group — so the PDP matches the participant's own token and nobody else.
#[test]
fn every_compiled_policy_is_assigned_to_the_consumers_did_and_to_nothing_else() {
    let document = agreement(vec![
        rule(&["retrieveEntity"], &["pm10"]),
        rule(&["queryEntity"], &["pm25"]),
    ]);

    let policies = compiled(&document).expect("it compiles");
    assert_eq!(policies.len(), 2);
    for policy in &policies {
        assert_eq!(policy.assignee.kind, PrincipalKind::Did);
        assert_eq!(policy.assignee.id, CONSUMER);
        assert_eq!(policy.assigner, PROVIDER);
        assert_eq!(policy.effect, PolicyEffect::Permission);
    }
}

/// A `Policy` has to say who granted it. The rule's own assigner is used, the document's is
/// the fallback, and a document with neither is not representable as a policy.
#[test]
fn an_agreement_that_names_no_assigner_anywhere_is_not_representable() {
    let mut document = agreement(vec![rule(&["retrieveEntity"], &["pm10"])]);
    document
        .as_object_mut()
        .expect("an object")
        .remove("assigner");
    document["permission"][0]
        .as_object_mut()
        .expect("an object")
        .remove("assigner");

    assert!(matches!(
        compiled(&document),
        Err(CompileError::Unrepresentable(_)),
    ));
}

/// A target named by id has to be a URN a `Policy` can hold: anything else is refused rather
/// than written into a manifest that would then fail to load.
#[test]
fn a_target_id_that_is_not_a_urn_is_refused_rather_than_written_into_a_policy() {
    for id in [
        "station-7",
        "http://example.com/station",
        "urn:",
        "",
        "urn:ngsi-ld:",
    ] {
        let document = agreement(vec![json!({
            "assigner": PROVIDER,
            "assignee": CONSUMER,
            "action": ["ngsi-ld:retrieveEntity"],
            "target": { "uid": "AirQualityObserved", "@id": id },
        })]);
        // Either it is refused, or the id was not read as a target id at all — what must never
        // happen is a policy carrying an id a manifest cannot express.
        if let Ok(policies) = compiled(&document) {
            for policy in policies {
                for info in &policy.information {
                    for selector in &info.entities {
                        assert!(
                            selector.id.is_none(),
                            "{id:?} became a selector id: {selector:?}",
                        );
                    }
                }
            }
        }
    }
}

/// An agreement that names no attribute is asking for what the offer gives, which is the
/// offer's own whitelist and never "everything".
#[test]
fn an_agreement_naming_no_attribute_inherits_the_offers_whitelist_rather_than_everything() {
    let policies = compiled(&agreement(vec![rule(&["retrieveEntity"], &[])])).expect("compiles");
    let names = &policies[0].information[0].property_names;
    assert_eq!(
        names,
        &vec!["location".to_owned(), "pm10".to_owned(), "pm25".to_owned()]
    );
}

/// The offer's own filter is not the document's to drop: it survives into every compiled
/// policy whether or not the agreement mentions it (DS-03).
#[test]
fn the_offers_filter_survives_into_every_compiled_policy() {
    let policies = compiled(&agreement(vec![
        rule(&["retrieveEntity"], &["pm10"]),
        rule(&["queryEntity"], &["pm25"]),
    ]))
    .expect("compiles");

    for policy in &policies {
        assert!(
            policy
                .q
                .as_deref()
                .is_some_and(|q| q.contains("status==\"operational\"")),
            "the offer's filter is in {:?}",
            policy.q,
        );
    }
}

/// Compiling the same document twice is the same policies: nothing here reads a clock, a map
/// order or a counter, so what the reconcile writes does not churn between rounds.
#[test]
fn compiling_the_same_document_twice_is_the_same_policies() {
    let document = agreement(vec![
        rule(&["retrieveEntity"], &["pm10"]),
        rule(&["queryEntity"], &["pm25", "location"]),
    ]);

    assert_eq!(
        compiled(&document).expect("once"),
        compiled(&document).expect("twice")
    );
}
