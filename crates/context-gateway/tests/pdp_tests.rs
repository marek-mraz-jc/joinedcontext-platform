use chrono::{DateTime, Utc};
use context_gateway::pdp::evaluator::{evaluate, Request, Subject, Verdict};
use jc_core::kinds::{Operation, PolicySpec};
use std::collections::BTreeSet;

fn now() -> DateTime<Utc> {
    "2026-09-06T12:00:00Z"
        .parse()
        .expect("a fixed instant, so a verdict is reproducible")
}

fn policy(yaml: &str) -> PolicySpec {
    serde_norway::from_str(yaml).expect("the policy spec parses")
}

fn public_read() -> PolicySpec {
    policy(
        r#"contextSpaceRef: ovzdusie
assigner: did:web:banskabystrica.sk
assignee: { kind: role, id: public }
operations: [queryEntity, retrieveEntity]
information:
  - entities:
      - type: AirQualityObserved
        idPattern: "^urn:ngsi-ld:AirQualityObserved:banskabystrica\\.sk:ovzdusie:.*$"
    propertyNames: [pm10, pm25, dateObserved, location]
q: "pm10>=0"
scopeQ: "/geo/SK/BB"
"#,
    )
}

fn set(names: &[&str]) -> BTreeSet<String> {
    names.iter().map(|name| (*name).to_owned()).collect()
}

/// GW5, R5: nothing granted it, so it is refused. Not an empty result, not a guess.
#[test]
fn an_empty_policy_set_denies() {
    let verdict = evaluate(
        &Subject::anonymous(),
        Operation::QueryEntity,
        &Request::default(),
        "ovzdusie",
        &[],
        now(),
    );

    assert_eq!(verdict, Verdict::Deny);
    assert!(verdict.constraints().is_none());
}

/// GW22: anonymous is the role `public`, evaluated by exactly the same machinery.
#[test]
fn an_anonymous_caller_gets_the_public_grant_as_a_rewrite() {
    let verdict = evaluate(
        &Subject::anonymous(),
        Operation::QueryEntity,
        &Request::default(),
        "ovzdusie",
        &[public_read()],
        now(),
    );

    let constraints = verdict.constraints().expect("the public grant rewrites");
    assert_eq!(
        constraints.tenant, "ovzdusie",
        "the tenant is always pinned"
    );
    assert_eq!(constraints.types, set(&["AirQualityObserved"]));
    assert_eq!(
        constraints.attrs,
        set(&["dateObserved", "location", "pm10", "pm25"]),
        "asking for nothing yields the granted set, which is what the response is cut to"
    );
    // R13: the grant's scope travels inside its own q term, anchored, never as a
    // separate parameter that another policy's q could pair with.
    assert_eq!(
        constraints.q.as_deref(),
        Some(r#"(((pm10>=0);scope~="^/geo/SK/BB(/.*)?$"))"#)
    );
    assert_eq!(constraints.granted_scopes.as_deref(), Some("/geo/SK/BB"));
    assert!(constraints
        .id_patterns
        .iter()
        .all(|p| p.starts_with('^') && p.ends_with('$')));
}

/// GW3: an ordinary caller never gets a plain ALLOW. The floor is REWRITE, because the
/// tenant alone is pinned by the gateway.
#[test]
fn a_caller_the_policy_does_not_name_is_denied() {
    let stranger = Subject {
        user: Some("someone.else".to_owned()),
        ..Subject::default()
    };

    assert!(evaluate(
        &stranger,
        Operation::QueryEntity,
        &Request::default(),
        "ovzdusie",
        &[public_read()],
        now(),
    )
    .is_deny());
}

/// GW5: the grant lists the operations it grants, and nothing else. A read grant is not a
/// write grant, whatever else matches.
#[test]
fn an_operation_outside_the_grant_is_denied() {
    for operation in [
        Operation::CreateEntity,
        Operation::UpdateAttrs,
        Operation::DeleteEntity,
        Operation::QueryTemporal,
    ] {
        assert!(
            evaluate(
                &Subject::anonymous(),
                operation,
                &Request::default(),
                "ovzdusie",
                &[public_read()],
                now(),
            )
            .is_deny(),
            "{operation:?} is not in the grant"
        );
    }
}

/// R8: a grant may name a CIM 009 operation group, and it covers exactly the operations
/// Table 4.20-2 lists for it.
#[test]
fn an_operation_group_grant_covers_its_table_members() {
    let grant = policy(
        r#"contextSpaceRef: ovzdusie
assigner: did:web:banskabystrica.sk
assignee: { kind: role, id: public }
operations: [retrieveOps]
"#,
    );

    for covered in [Operation::RetrieveEntity, Operation::QueryEntity] {
        assert!(
            !evaluate(
                &Subject::anonymous(),
                covered,
                &Request::default(),
                "ovzdusie",
                std::slice::from_ref(&grant),
                now()
            )
            .is_deny(),
            "retrieveOps covers {covered:?}"
        );
    }
    for outside in [
        Operation::CreateEntity,
        Operation::QueryTemporal,
        Operation::QueryBatch,
    ] {
        assert!(
            evaluate(
                &Subject::anonymous(),
                outside,
                &Request::default(),
                "ovzdusie",
                std::slice::from_ref(&grant),
                now()
            )
            .is_deny(),
            "retrieveOps does not cover {outside:?}"
        );
    }
}

/// GW34: every one of the five groups grants exactly its members, through the PDP.
///
/// The expansion itself is held against Table 4.20-2 in `jc-core`'s `policy_tests`; what this
/// asks is the question a person asks of a grant — "so what may this caller actually do?" — of
/// all five groups at once, including the operations just outside each one. A group that leaked
/// one operation would be a silent widening of every policy that names it.
#[test]
fn every_operation_group_grants_its_members_and_nothing_beside_them() {
    use jc_core::kinds::policy::OperationGroup;

    // Every operation the vocabulary has, so "outside the group" means outside all of it.
    const EVERY: &[Operation] = &[
        Operation::CreateEntity,
        Operation::UpdateEntity,
        Operation::AppendAttrs,
        Operation::UpdateAttrs,
        Operation::DeleteAttrs,
        Operation::DeleteEntity,
        Operation::CreateBatch,
        Operation::UpsertBatch,
        Operation::UpdateBatch,
        Operation::DeleteBatch,
        Operation::UpsertTemporal,
        Operation::AppendAttrsTemporal,
        Operation::DeleteAttrsTemporal,
        Operation::UpdateAttrInstanceTemporal,
        Operation::DeleteAttrInstanceTemporal,
        Operation::DeleteTemporal,
        Operation::MergeEntity,
        Operation::ReplaceEntity,
        Operation::ReplaceAttrs,
        Operation::MergeBatch,
        Operation::PurgeEntity,
        Operation::RetrieveEntity,
        Operation::QueryEntity,
        Operation::QueryBatch,
        Operation::RetrieveTemporal,
        Operation::QueryTemporal,
        Operation::RetrieveEntityTypes,
        Operation::RetrieveEntityTypeDetails,
        Operation::RetrieveEntityTypeInfo,
        Operation::RetrieveAttrTypes,
        Operation::RetrieveAttrTypeDetails,
        Operation::RetrieveAttrTypeInfo,
        Operation::CreateSubscription,
        Operation::UpdateSubscription,
        Operation::RetrieveSubscription,
        Operation::QuerySubscription,
        Operation::DeleteSubscription,
        Operation::RetrieveEntityMap,
        Operation::UpdateEntityMap,
        Operation::DeleteEntityMap,
        Operation::CreateEntityMapQueryEntity,
        Operation::CreateEntityMapQueryTemporal,
        Operation::RetrieveContextSourceIdentity,
    ];

    for group in [
        OperationGroup::FederationOps,
        OperationGroup::AssociationOps,
        OperationGroup::UpdateOps,
        OperationGroup::RetrieveOps,
        OperationGroup::RedirectionOps,
    ] {
        let grant = policy(&format!(
            r#"contextSpaceRef: ovzdusie
assigner: did:web:banskabystrica.sk
assignee: {{ kind: role, id: public }}
operations: [{group}]
"#
        ));
        let members = group.operations();
        for operation in EVERY {
            let denied = evaluate(
                &Subject::anonymous(),
                *operation,
                &Request::default(),
                "ovzdusie",
                std::slice::from_ref(&grant),
                now(),
            )
            .is_deny();
            assert_eq!(
                denied,
                !members.contains(operation),
                "{group} and {operation}: the PDP and Table 4.20-2 disagree"
            );
        }
    }
}

/// GW4: a prohibition is evaluated first and ends the evaluation, whatever the caller's
/// other grants say.
#[test]
fn a_prohibition_overrides_a_permission_that_would_have_matched() {
    let prohibition = policy(
        r#"contextSpaceRef: ovzdusie
effect: prohibition
assigner: did:web:banskabystrica.sk
assignee: { kind: role, id: public }
operations: [queryEntity, retrieveEntity]
information:
  - entities:
      - type: AirQualityObserved
"#,
    );

    let allowed = evaluate(
        &Subject::anonymous(),
        Operation::QueryEntity,
        &Request::default(),
        "ovzdusie",
        &[public_read()],
        now(),
    );
    assert!(!allowed.is_deny(), "the permission alone rewrites");

    let denied = evaluate(
        &Subject::anonymous(),
        Operation::QueryEntity,
        &Request::default(),
        "ovzdusie",
        &[public_read(), prohibition],
        now(),
    );
    assert_eq!(denied, Verdict::Deny, "the prohibition wins");
}

/// GW7: a policy outside its validity window grants nothing, in either direction.
#[test]
fn a_policy_outside_its_validity_window_grants_nothing() {
    let expired = policy(
        r#"contextSpaceRef: ovzdusie
assigner: did:web:banskabystrica.sk
assignee: { kind: role, id: public }
operations: [queryEntity]
validity: { from: "2026-01-01T00:00:00Z", to: "2026-06-01T00:00:00Z" }
"#,
    );
    let future = policy(
        r#"contextSpaceRef: ovzdusie
assigner: did:web:banskabystrica.sk
assignee: { kind: role, id: public }
operations: [queryEntity]
validity: { from: "2027-01-01T00:00:00Z" }
"#,
    );

    for policy in [expired, future] {
        assert!(evaluate(
            &Subject::anonymous(),
            Operation::QueryEntity,
            &Request::default(),
            "ovzdusie",
            &[policy],
            now(),
        )
        .is_deny());
    }
}

/// GW11, per dimension: the request is intersected with the grant, never widened by it.
#[test]
fn asking_for_more_than_the_grant_covers_narrows_to_the_grant() {
    let asked = Request {
        types: set(&["AirQualityObserved", "ParkingSpot"]),
        attrs: set(&["pm10", "operatorPhone"]),
        q: Some("pm10>50".to_owned()),
        ..Request::default()
    };

    let verdict = evaluate(
        &Subject::anonymous(),
        Operation::QueryEntity,
        &asked,
        "ovzdusie",
        &[public_read()],
        now(),
    );
    let constraints = verdict.constraints().expect("a narrowed rewrite");

    assert_eq!(
        constraints.types,
        set(&["AirQualityObserved"]),
        "type narrowed"
    );
    assert_eq!(constraints.attrs, set(&["pm10"]), "attrs narrowed");
    assert_eq!(
        constraints.q.as_deref(),
        Some(r#"(pm10>50);(((pm10>=0);scope~="^/geo/SK/BB(/.*)?$"))"#),
        "the caller's filter is conjoined with the grant's, never replaced by it"
    );
    assert!(
        constraints.restricted,
        "the answer says it was narrowed (R22)"
    );
}

/// EP-16: a signed-in person on a public endpoint is never granted less than an anonymous
/// one. The steward's own grant names `status` only; beside the public grant, which names
/// no attribute, it must not narrow the read to `status`.
#[test]
fn a_grant_without_an_attribute_list_is_not_narrowed_by_a_sibling_with_one() {
    let public_everything = policy(
        r#"contextSpaceRef: helsinki
assigner: did:web:hel.fi
assignee: { kind: role, id: public }
operations: [queryEntity, retrieveEntity]
information:
  - entities:
      - type: BikeHireDockingStation
"#,
    );
    let steward_status = policy(
        r#"contextSpaceRef: helsinki
assigner: did:web:hel.fi
assignee: { kind: user, id: demo.steward@hel.fi }
operations: [queryEntity, retrieveEntity, updateAttrs]
information:
  - entities:
      - type: BikeHireDockingStation
    propertyNames: [status]
"#,
    );
    let steward = Subject {
        user: Some("demo.steward@hel.fi".to_owned()),
        roles: BTreeSet::from(["public".to_owned()]),
        ..Subject::default()
    };
    let policies = [public_everything, steward_status];

    let verdict = evaluate(
        &steward,
        Operation::QueryEntity,
        &Request::default(),
        "helsinki",
        &policies,
        now(),
    );
    let constraints = verdict.constraints().expect("a rewrite");
    assert!(
        constraints.attrs.is_empty(),
        "the public grant names every attribute; got {:?}",
        constraints.attrs
    );

    let asked = Request {
        attrs: set(&["availableBikeNumber"]),
        ..Request::default()
    };
    let verdict = evaluate(
        &steward,
        Operation::QueryEntity,
        &asked,
        "helsinki",
        &policies,
        now(),
    );
    assert_eq!(
        verdict.constraints().expect("a rewrite").attrs,
        set(&["availableBikeNumber"]),
        "what the public may read, the steward may read"
    );

    // Alone, the steward's grant still narrows to what it names.
    let verdict = evaluate(
        &steward,
        Operation::QueryEntity,
        &asked,
        "helsinki",
        &policies[1..],
        now(),
    );
    assert_ne!(
        verdict.constraints().map(|c| c.attrs.clone()),
        Some(set(&["availableBikeNumber"])),
        "without the public grant the list applies"
    );
}

/// ADR 006: the caller's filter must never be able to reach outside itself and regroup
/// what follows it. An unbalanced filter is dropped, leaving the grants alone in force.
#[test]
fn a_caller_filter_cannot_break_out_of_its_parentheses() {
    let injected = Request {
        // Closing the wrapping parenthesis early would turn the conjunction into a
        // disjunction and hand the caller everything the second operand matches.
        q: Some("pm10>50)|(pm10<0".to_owned()),
        ..Request::default()
    };

    let verdict = evaluate(
        &Subject::anonymous(),
        Operation::QueryEntity,
        &injected,
        "ovzdusie",
        &[public_read()],
        now(),
    );
    let q = verdict
        .constraints()
        .expect("a rewrite")
        .q
        .clone()
        .expect("the grant's own filter stays");

    assert_eq!(
        q, r#"(((pm10>=0);scope~="^/geo/SK/BB(/.*)?$"))"#,
        "the unbalanced filter was dropped, not conjoined"
    );
    assert!(!q.contains("pm10<0"));
}

/// GW10: grants are OR-ed with each other and AND-ed with the caller, so holding two
/// grants never lets one grant's filter apply to the other's types.
#[test]
fn several_grants_are_or_ed_with_each_other_and_and_ed_with_the_caller() {
    let second = policy(
        r#"contextSpaceRef: ovzdusie
assigner: did:web:banskabystrica.sk
assignee: { kind: role, id: public }
operations: [queryEntity]
information:
  - entities:
      - type: ParkingSpot
    propertyNames: [availableSpotNumber]
q: "availableSpotNumber>0"
"#,
    );

    let verdict = evaluate(
        &Subject::anonymous(),
        Operation::QueryEntity,
        &Request {
            q: Some("dateObserved>\"2026-09-01\"".to_owned()),
            ..Request::default()
        },
        "ovzdusie",
        &[public_read(), second],
        now(),
    );
    let constraints = verdict.constraints().expect("a rewrite");

    assert_eq!(
        constraints.q.as_deref(),
        Some(concat!(
            "(dateObserved>\"2026-09-01\");",
            r#"((((pm10>=0);scope~="^/geo/SK/BB(/.*)?$"))|((availableSpotNumber>0)))"#
        ))
    );
    assert_eq!(
        constraints.types,
        set(&["AirQualityObserved", "ParkingSpot"]),
        "the union of what the grants cover"
    );
}

/// The intersection must never widen, in any dimension. One negative case each: whatever
/// the caller sends, the grant's own constraint is still in the answer (GW11).
#[test]
fn no_dimension_can_be_widened_by_what_the_caller_sends() {
    let grant = policy(
        r#"contextSpaceRef: ovzdusie
assigner: did:web:banskabystrica.sk
assignee: { kind: role, id: public }
operations: [queryEntity]
information:
  - entities:
      - type: AirQualityObserved
    propertyNames: [pm10]
q: "pm10>=0"
scopeQ: "/geo/SK/BB"
geoQ: "georel=within;geometry=Polygon;coordinates=[[[19.1,48.7],[19.2,48.7],[19.2,48.76],[19.1,48.76],[19.1,48.7]]]"
temporalQ: "timerel=after;timeAt=P-1D"
"#,
    );

    // The caller asks for a different everything: another type, another attribute, a
    // wider filter, a wider scope, a wider area, a wider window.
    let greedy = Request {
        types: set(&["ParkingSpot"]),
        attrs: set(&["operatorPhone"]),
        q: Some("pm10<0".to_owned()),
        scope_q: Some("/geo/SK".to_owned()),
        geo_q: Some(
            "georel=within;geometry=Polygon;coordinates=[[[0,0],[90,0],[90,90],[0,90],[0,0]]]"
                .to_owned(),
        ),
        temporal_q: Some("timerel=after;timeAt=P-10Y".to_owned()),
        // What the caller filters on narrows the types the query considers, not what the
        // evaluator grants (T-1862); this case is about the grant.
        referenced: Default::default(),
    };

    let verdict = evaluate(
        &Subject::anonymous(),
        Operation::QueryEntity,
        &greedy,
        "ovzdusie",
        &[grant],
        now(),
    );
    let constraints = verdict.constraints().expect("a rewrite");

    assert!(
        constraints.types.is_empty(),
        "no granted type was asked for, so nothing is served"
    );
    assert!(
        constraints.empty,
        "no granted type or attribute was asked for, so nothing is served (T-0805)"
    );
    assert!(constraints.restricted);

    // The grant's last-day window, resolved against the same `now` as the decision: the
    // caller's ten years never reach the broker.
    let yesterday =
        (now() - chrono::Duration::days(1)).to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    for (dimension, value, granted) in [
        ("q", &constraints.q, "pm10>=0"),
        ("q", &constraints.q, r#"scope~="^/geo/SK/BB(/.*)?$""#),
        ("scopes", &constraints.granted_scopes, "/geo/SK/BB"),
        ("geoQ", &constraints.geo_q, "coordinates=[[[19.1,48.7]"),
        (
            "temporalQ",
            &constraints.temporal_q,
            &format!("timerel=after;timeAt={yesterday}"),
        ),
    ] {
        let value = value
            .as_deref()
            .unwrap_or_else(|| panic!("{dimension} survives"));
        assert!(
            value.contains(granted),
            "{dimension} lost the grant's own constraint: {value}"
        );
    }
    assert!(
        !constraints
            .q
            .as_deref()
            .unwrap_or_default()
            .contains("/geo/SK\""),
        "the caller's wider scope is not in the grant term"
    );
    assert!(!constraints
        .temporal_q
        .as_deref()
        .unwrap_or_default()
        .contains("P-10Y"));
}

/// T-0805: asking for an attribute the grant does not name is answered with nothing, never
/// with the whole entity. An empty `attrs` set means "no projection" downstream, so the
/// decision has to say `empty` itself, the way it does for a type nothing covers.
#[test]
fn an_attribute_outside_the_grant_empties_the_answer_instead_of_the_projection() {
    let request = Request {
        types: set(&["AirQualityObserved"]),
        attrs: set(&["reliability"]),
        ..Request::default()
    };
    let verdict = evaluate(
        &Subject::anonymous(),
        Operation::QueryEntity,
        &request,
        "ovzdusie",
        &[public_read()],
        now(),
    );
    let constraints = verdict.constraints().expect("a rewrite");
    assert!(constraints.empty, "nothing granted was asked for");
    assert!(
        constraints.empty || !constraints.attrs.is_empty(),
        "an empty attrs set would serve every attribute"
    );

    // One granted attribute among the asked ones: the answer is that one, projected.
    let mixed = Request {
        attrs: set(&["reliability", "pm10"]),
        ..request
    };
    let verdict = evaluate(
        &Subject::anonymous(),
        Operation::QueryEntity,
        &mixed,
        "ovzdusie",
        &[public_read()],
        now(),
    );
    let constraints = verdict.constraints().expect("a rewrite");
    assert!(!constraints.empty);
    assert_eq!(constraints.attrs, set(&["pm10"]));
}

/// T-0805, GW17: a write never narrows the grant's attribute set by a query parameter; the
/// write guard checks the body against what the grant names.
#[test]
fn a_write_keeps_the_grants_attributes_whatever_the_query_names() {
    let steward = policy(
        r#"contextSpaceRef: ovzdusie
assigner: did:web:banskabystrica.sk
assignee: { kind: role, id: steward }
operations: [updateAttrs]
information:
  - entities:
      - type: AirQualityObserved
    propertyNames: [pm10]
"#,
    );
    let request = Request {
        types: set(&["AirQualityObserved"]),
        attrs: set(&["reliability"]),
        ..Request::default()
    };
    let subject = Subject {
        roles: set(&["steward"]),
        ..Subject::anonymous()
    };
    let verdict = evaluate(
        &subject,
        Operation::UpdateAttrs,
        &request,
        "ovzdusie",
        &[steward],
        now(),
    );
    let constraints = verdict.constraints().expect("a rewrite");
    assert!(!constraints.empty);
    assert_eq!(
        constraints.attrs,
        set(&["pm10"]),
        "the grant's set, so a body naming reliability is refused by the guard"
    );
}

/// T-0805 edge cases: a batch write ignores `attrs` the same way; a caller naming no
/// attribute keeps the grant's set; a prohibition still ends the evaluation.
#[test]
fn attrs_edge_cases_batch_write_no_selection_and_prohibition() {
    let steward = policy(
        r#"contextSpaceRef: ovzdusie
assigner: did:web:banskabystrica.sk
assignee: { kind: role, id: steward }
operations: [upsertBatch, queryEntity]
information:
  - entities:
      - type: AirQualityObserved
    propertyNames: [pm10]
"#,
    );
    let subject = Subject {
        roles: set(&["steward"]),
        ..Subject::anonymous()
    };
    let greedy = Request {
        types: set(&["AirQualityObserved"]),
        attrs: set(&["reliability"]),
        ..Request::default()
    };
    let batch = evaluate(
        &subject,
        Operation::UpsertBatch,
        &greedy,
        "ovzdusie",
        std::slice::from_ref(&steward),
        now(),
    );
    let constraints = batch.constraints().expect("a rewrite");
    assert!(!constraints.empty);
    assert_eq!(constraints.attrs, set(&["pm10"]));

    let none = Request {
        types: set(&["AirQualityObserved"]),
        ..Request::default()
    };
    let read = evaluate(
        &subject,
        Operation::QueryEntity,
        &none,
        "ovzdusie",
        std::slice::from_ref(&steward),
        now(),
    );
    let constraints = read.constraints().expect("a rewrite");
    assert!(!constraints.empty);
    assert_eq!(
        constraints.attrs,
        set(&["pm10"]),
        "no selection means the grant's set"
    );

    let mut forbidden = steward.clone();
    forbidden.effect = jc_core::kinds::PolicyEffect::Prohibition;
    let denied = evaluate(
        &subject,
        Operation::QueryEntity,
        &none,
        "ovzdusie",
        &[steward, forbidden],
        now(),
    );
    assert_eq!(denied, Verdict::Deny);
}

/// T-0806, R9: a selector naming one entity by `id` grants that entity, not its whole type.
#[test]
fn an_exact_id_selector_compiles_into_a_pattern_that_matches_only_it() {
    let one = policy(
        r#"contextSpaceRef: ovzdusie
assigner: did:web:banskabystrica.sk
assignee: { kind: role, id: public }
operations: [queryEntity, retrieveEntity]
information:
  - entities:
      - type: AirQualityObserved
        id: urn:ngsi-ld:AirQualityObserved:banskabystrica.sk:ovzdusie:sever-01
"#,
    );
    let request = Request {
        types: set(&["AirQualityObserved"]),
        ..Request::default()
    };
    let verdict = evaluate(
        &Subject::anonymous(),
        Operation::QueryEntity,
        &request,
        "ovzdusie",
        &[one],
        now(),
    );
    let constraints = verdict.constraints().expect("a rewrite");
    let pattern = constraints
        .id_patterns
        .iter()
        .next()
        .expect("the id became a pattern");
    let compiled = regex::Regex::new(pattern).expect("anchored and escaped");
    assert!(compiled.is_match("urn:ngsi-ld:AirQualityObserved:banskabystrica.sk:ovzdusie:sever-01"));
    assert!(
        !compiled.is_match("urn:ngsi-ld:AirQualityObserved:banskabystrica.sk:ovzdusie:sever-012")
    );
    assert!(
        !compiled.is_match("urn:ngsi-ld:AirQualityObserved:banskabystricaXsk:ovzdusie:sever-01")
    );
}

/// R22, AG-13, T-1213: the narrowing signal describes the answer, not the request.
///
/// `ovzdusie`'s public grant whitelists four property names and deliberately withholds
/// `reliability` and `refDevice`. A caller who names no `attrs` — the ordinary read, and the
/// one every dashboard and every MCP tool makes — used to be told nothing: the flag compared
/// the granted attribute set against the request's, and a request that asked for nothing
/// cannot have had anything taken from it. The entities came back short and unannounced.
#[test]
fn a_whitelist_narrows_a_read_that_named_no_attribute() {
    let verdict = evaluate(
        &Subject::anonymous(),
        Operation::QueryEntity,
        &Request {
            types: set(&["AirQualityObserved"]),
            ..Request::default()
        },
        "ovzdusie",
        &[public_read()],
        now(),
    );
    let constraints = verdict.constraints().expect("the public grant rewrites");

    assert!(
        constraints.attrs.is_superset(&set(&["pm10"]))
            && !constraints.attrs.contains("reliability"),
        "the whitelist is what will be served: {:?}",
        constraints.attrs
    );
    assert!(
        constraints.restricted,
        "the answer is not everything the space holds, and the caller who asked is told so"
    );
}

/// The other half, so the flag stays worth reading: a grant that takes nothing away does not
/// claim to. Without this the rule could be satisfied by always answering `true`.
#[test]
fn a_grant_that_narrows_nothing_says_nothing() {
    let whole_space = policy(
        r#"contextSpaceRef: ovzdusie
assigner: did:web:banskabystrica.sk
assignee: { kind: role, id: public }
operations: [queryEntity, retrieveEntity]
information:
  - entities:
      - type: AirQualityObserved
"#,
    );
    let verdict = evaluate(
        &Subject::anonymous(),
        Operation::QueryEntity,
        &Request {
            types: set(&["AirQualityObserved"]),
            ..Request::default()
        },
        "ovzdusie",
        &[whole_space],
        now(),
    );
    let constraints = verdict.constraints().expect("the grant rewrites");

    assert!(
        constraints.attrs.is_empty(),
        "no whitelist means no projection, so the whole entity is served"
    );
    assert!(
        !constraints.restricted,
        "nothing was taken away, and a flag that is always true tells a reader nothing"
    );
}

/// DM-51, T-2241: a view endpoint's grant is written in the class the view serves, and the broker
/// answers in the class it stores. The query's `type` is inverted before it is sent, so the answer
/// has to be judged by the same pair — judging it by the target class dropped every entity a view
/// endpoint returned, and a view endpoint answered an empty list for a week's worth of commits.
#[test]
fn a_view_endpoints_constraints_read_the_brokers_own_class() {
    let verdict = evaluate(
        &Subject::anonymous(),
        Operation::QueryEntity,
        &Request::default(),
        "ovzdusie",
        &[public_read()],
        now(),
    );
    let constraints = verdict.constraints().expect("a rewrite");
    assert!(
        constraints.types.contains("AirQualityObserved"),
        "the grant names the class the view serves"
    );

    let upstream = constraints.in_source_model("AirQualityObserved", "WeatherObserved");
    assert!(
        upstream.types.contains("WeatherObserved"),
        "the broker's own class is what the answer is judged by"
    );
    assert!(
        !upstream.types.contains("AirQualityObserved"),
        "the target class is gone, or the check would admit an untranslated entity"
    );
    // Everything else is untouched: the inversion is about the class pair and nothing more.
    assert_eq!(upstream.attrs, constraints.attrs);
    assert_eq!(upstream.id_patterns, constraints.id_patterns);
    assert_eq!(upstream.q, constraints.q);

    // A class the mapping does not name is left where it is: a second grant is still a grant.
    let unrelated = constraints.in_source_model("TrafficFlowObserved", "TrafficSensor");
    assert_eq!(
        unrelated.types, constraints.types,
        "a mapping of another class pair changes nothing"
    );
}
