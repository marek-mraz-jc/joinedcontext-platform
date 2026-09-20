//! Edge cases of `pdp::evaluator::evaluate` and `Verdict::constraints` (T-1913, T-1914; EP-26,
//! MP-02, GW1..GW10).
//!
//! Contract, in one sentence: every path that cannot decide ends in DENY, a matching prohibition
//! ends the evaluation whatever the permissions say, and a rewrite carries the union of the
//! grants intersected with the request and never anything the caller asked for beyond them.
//!
//! `pdp_tests.rs` covers the shape of that intersection in detail. These are the decision's own
//! boundaries: the validity window's two ends, who a policy is about, and the accessor a caller
//! of the verdict reads it through — which is the only way out of a `Verdict`, so a `Deny` that
//! handed back constraints would be a grant nobody wrote.

use chrono::{DateTime, Duration, Utc};
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

/// A grant to one principal, optionally inside a validity window.
fn grant_to(kind: &str, id: &str, validity: &str) -> PolicySpec {
    policy(&format!(
        "contextSpaceRef: ovzdusie\n\
         assigner: did:web:banskabystrica.sk\n\
         assignee: {{ kind: {kind}, id: {id} }}\n\
         operations: [queryEntity, retrieveEntity]\n\
         {validity}\
         information:\n\
         \x20 - entities:\n\
         \x20     - type: AirQualityObserved\n"
    ))
}

fn public_read() -> PolicySpec {
    grant_to("role", "public", "")
}

fn set(names: &[&str]) -> BTreeSet<String> {
    names.iter().map(|name| (*name).to_owned()).collect()
}

fn verdict(subject: &Subject, policies: &[PolicySpec], at: DateTime<Utc>) -> Verdict {
    evaluate(
        subject,
        Operation::QueryEntity,
        &Request::default(),
        "ovzdusie",
        policies,
        at,
    )
}

// --- Verdict::constraints (T-1913) ---------------------------------------------------------

#[test]
fn the_only_way_to_read_a_rewrite_hands_nothing_back_on_a_deny() {
    // Every caller of the PDP reads the decision through this accessor. A `Deny` that answered
    // `Some` would be a constraint set nobody granted, injected into a broker call.
    let denied = verdict(&Subject::anonymous(), &[], now());
    assert!(denied.is_deny());
    assert!(denied.constraints().is_none());

    let rewritten = verdict(&Subject::anonymous(), &[public_read()], now());
    assert!(!rewritten.is_deny());
    let constraints = rewritten
        .constraints()
        .expect("a rewrite carries its constraints");
    assert_eq!(
        constraints.tenant, "ovzdusie",
        "the tenant is the gateway's, on every rewrite"
    );
}

#[test]
fn is_deny_and_constraints_never_disagree() {
    // Two readings of one decision; a caller that trusted the wrong one would either refuse a
    // granted call or forward an ungranted one.
    for policies in [Vec::new(), vec![public_read()]] {
        let decision = verdict(&Subject::anonymous(), &policies, now());
        assert_eq!(
            decision.is_deny(),
            decision.constraints().is_none(),
            "{decision:?}"
        );
    }
}

// --- evaluate (T-1914) ---------------------------------------------------------------------

#[test]
fn a_caller_with_no_identity_and_no_public_grant_is_denied() {
    // GW5, R5: the floor is DENY, not an empty answer and not a guess.
    let to_a_user = grant_to("user", "jana", "");
    assert!(verdict(&Subject::anonymous(), &[to_a_user], now()).is_deny());
}

#[test]
fn a_policy_is_about_exactly_one_kind_of_principal() {
    // A name that matches in the wrong dimension is a grant to somebody else: `jana` the user is
    // not `jana` the group, and a service account is not a role.
    let subject = Subject {
        user: Some("jana".to_owned()),
        groups: set(&["stewards"]),
        roles: set(&["public"]),
        service_account: Some("collector".to_owned()),
        ..Subject::default()
    };

    for (kind, id, granted) in [
        ("user", "jana", true),
        ("group", "jana", false),
        ("role", "jana", false),
        ("serviceAccount", "jana", false),
        ("group", "stewards", true),
        ("user", "stewards", false),
        ("serviceAccount", "collector", true),
        ("role", "collector", false),
    ] {
        let decision = verdict(&subject, &[grant_to(kind, id, "")], now());
        assert_eq!(
            !decision.is_deny(),
            granted,
            "a grant to {kind} {id} decided the wrong way"
        );
    }
}

#[test]
fn the_validity_window_opens_on_its_from_and_closes_before_its_to() {
    // GW7. The two ends are not the same: `from` is inclusive and `to` is exclusive, so a policy
    // valid "until noon" grants nothing at noon. Getting either end wrong grants a second of
    // access nobody wrote, or refuses one they did.
    let timed = grant_to(
        "role",
        "public",
        "validity: { from: 2026-09-06T12:00:00Z, to: 2026-09-07T12:00:00Z }\n",
    );
    let from: DateTime<Utc> = "2026-09-06T12:00:00Z".parse().expect("an instant");
    let to: DateTime<Utc> = "2026-09-07T12:00:00Z".parse().expect("an instant");

    assert!(verdict(
        &Subject::anonymous(),
        std::slice::from_ref(&timed),
        from - Duration::seconds(1)
    )
    .is_deny());
    assert!(
        !verdict(&Subject::anonymous(), std::slice::from_ref(&timed), from).is_deny(),
        "from is inclusive"
    );
    assert!(!verdict(
        &Subject::anonymous(),
        std::slice::from_ref(&timed),
        to - Duration::seconds(1)
    )
    .is_deny());
    assert!(
        verdict(&Subject::anonymous(), &[timed], to).is_deny(),
        "to is exclusive"
    );
}

#[test]
fn a_window_with_only_one_end_is_open_at_the_other() {
    let opens = grant_to(
        "role",
        "public",
        "validity: { from: 2026-09-06T12:00:00Z }\n",
    );
    let closes = grant_to("role", "public", "validity: { to: 2026-09-06T12:00:00Z }\n");
    let far_future: DateTime<Utc> = "2099-01-01T00:00:00Z".parse().expect("an instant");
    let long_ago: DateTime<Utc> = "2000-01-01T00:00:00Z".parse().expect("an instant");

    assert!(!verdict(
        &Subject::anonymous(),
        std::slice::from_ref(&opens),
        far_future
    )
    .is_deny());
    assert!(verdict(&Subject::anonymous(), &[opens], long_ago).is_deny());
    assert!(!verdict(
        &Subject::anonymous(),
        std::slice::from_ref(&closes),
        long_ago
    )
    .is_deny());
    assert!(verdict(&Subject::anonymous(), &[closes], far_future).is_deny());
}

#[test]
fn a_prohibition_outside_its_window_prohibits_nothing() {
    // A prohibition is evaluated by the same window as a permission. One that were always in
    // force would keep refusing after the day it was written for, which is a different bug from
    // the one that matters, but the opposite reading is the dangerous one: a prohibition that
    // applies while it is not yet in force, and one that stops while it still is.
    let forbid = policy(
        "contextSpaceRef: ovzdusie\n\
         assigner: did:web:banskabystrica.sk\n\
         assignee: { kind: role, id: public }\n\
         effect: prohibition\n\
         operations: [queryEntity]\n\
         validity: { from: 2026-09-07T00:00:00Z }\n\
         information:\n\
         \x20 - entities:\n\
         \x20     - type: AirQualityObserved\n",
    );
    let policies = vec![public_read(), forbid];

    let before: DateTime<Utc> = "2026-09-06T12:00:00Z".parse().expect("an instant");
    let after: DateTime<Utc> = "2026-09-07T00:00:00Z".parse().expect("an instant");
    assert!(!verdict(&Subject::anonymous(), &policies, before).is_deny());
    assert!(
        verdict(&Subject::anonymous(), &policies, after).is_deny(),
        "the prohibition comes into force and ends the evaluation"
    );
}

#[test]
fn a_prohibition_that_names_types_still_covers_a_request_that_named_none() {
    // A caller who asks for no type is asking for every type the grants cover, which includes
    // the prohibited one; reading "no type named" as "not the prohibited type" would serve it.
    let forbid = policy(
        "contextSpaceRef: ovzdusie\n\
         assigner: did:web:banskabystrica.sk\n\
         assignee: { kind: role, id: public }\n\
         effect: prohibition\n\
         operations: [queryEntity]\n\
         information:\n\
         \x20 - entities:\n\
         \x20     - type: AirQualityObserved\n",
    );
    let wide = policy(
        "contextSpaceRef: ovzdusie\n\
         assigner: did:web:banskabystrica.sk\n\
         assignee: { kind: role, id: public }\n\
         operations: [queryEntity]\n\
         information:\n\
         \x20 - entities:\n\
         \x20     - type: AirQualityObserved\n\
         \x20     - type: WeatherObserved\n",
    );
    let policies = [wide, forbid];

    let asked_nothing = evaluate(
        &Subject::anonymous(),
        Operation::QueryEntity,
        &Request::default(),
        "ovzdusie",
        &policies,
        now(),
    );
    assert!(
        asked_nothing.is_deny(),
        "an unfiltered query would have included the prohibited type"
    );

    let asked_elsewhere = evaluate(
        &Subject::anonymous(),
        Operation::QueryEntity,
        &Request {
            types: set(&["WeatherObserved"]),
            ..Request::default()
        },
        "ovzdusie",
        &policies,
        now(),
    );
    assert!(
        !asked_elsewhere.is_deny(),
        "a query that cannot reach the prohibited type is not refused by it"
    );
}

#[test]
fn a_prohibition_on_another_operation_does_not_reach_this_one() {
    let forbid_writes = policy(
        "contextSpaceRef: ovzdusie\n\
         assigner: did:web:banskabystrica.sk\n\
         assignee: { kind: role, id: public }\n\
         effect: prohibition\n\
         operations: [deleteEntity]\n\
         information:\n\
         \x20 - entities:\n\
         \x20     - type: AirQualityObserved\n",
    );
    assert!(!verdict(
        &Subject::anonymous(),
        &[public_read(), forbid_writes],
        now()
    )
    .is_deny());
}

#[test]
fn a_prohibition_to_somebody_else_does_not_reach_this_caller() {
    let forbid_jana = policy(
        "contextSpaceRef: ovzdusie\n\
         assigner: did:web:banskabystrica.sk\n\
         assignee: { kind: user, id: jana }\n\
         effect: prohibition\n\
         operations: [queryEntity]\n\
         information:\n\
         \x20 - entities:\n\
         \x20     - type: AirQualityObserved\n",
    );
    assert!(!verdict(
        &Subject::anonymous(),
        &[public_read(), forbid_jana.clone()],
        now()
    )
    .is_deny());

    let jana = Subject {
        user: Some("jana".to_owned()),
        roles: set(&["public"]),
        ..Subject::default()
    };
    assert!(verdict(&jana, &[public_read(), forbid_jana], now()).is_deny());
}

#[test]
fn the_decision_is_the_same_however_the_policies_are_ordered() {
    // A caller reading "prohibitions first" as an ordering of the list rather than of the
    // evaluation would get a different answer depending on how the reconciler happened to sort
    // them, which is a grant that depends on the weather.
    let forbid = policy(
        "contextSpaceRef: ovzdusie\n\
         assigner: did:web:banskabystrica.sk\n\
         assignee: { kind: role, id: public }\n\
         effect: prohibition\n\
         operations: [queryEntity]\n\
         information:\n\
         \x20 - entities:\n\
         \x20     - type: AirQualityObserved\n",
    );
    let forwards = verdict(
        &Subject::anonymous(),
        &[public_read(), forbid.clone()],
        now(),
    );
    let backwards = verdict(&Subject::anonymous(), &[forbid, public_read()], now());
    assert_eq!(forwards, backwards);
    assert!(forwards.is_deny());
}

#[test]
fn the_same_inputs_always_give_the_same_verdict() {
    // GW13: `now` is a parameter so the dry-run endpoint can be honest. A decision that moved
    // between two calls with the same arguments would make that endpoint a guess.
    let subject = Subject {
        user: Some("jana".to_owned()),
        groups: set(&["stewards"]),
        roles: set(&["public"]),
        ..Subject::default()
    };
    let policies = [public_read(), grant_to("group", "stewards", "")];
    let first = verdict(&subject, &policies, now());
    for _ in 0..5 {
        assert_eq!(verdict(&subject, &policies, now()), first);
    }
}

#[test]
fn a_grant_to_a_role_the_caller_does_not_hold_grants_nothing_even_when_they_hold_others() {
    let subject = Subject {
        roles: set(&["public", "viewer"]),
        ..Subject::default()
    };
    assert!(verdict(&subject, &[grant_to("role", "steward", "")], now()).is_deny());
    assert!(!verdict(&subject, &[grant_to("role", "viewer", "")], now()).is_deny());
}

#[test]
fn an_anonymous_caller_holds_the_public_role_and_nothing_else() {
    // GW22. Anything else it appeared to hold would be a grant written for a signed-in person
    // being handed to everybody with the address.
    let anonymous = Subject::anonymous();
    assert_eq!(anonymous.roles, set(&["public"]));
    assert!(anonymous.user.is_none());
    assert!(anonymous.service_account.is_none());
    assert!(anonymous.groups.is_empty());
    assert!(anonymous.did.is_none());
}
