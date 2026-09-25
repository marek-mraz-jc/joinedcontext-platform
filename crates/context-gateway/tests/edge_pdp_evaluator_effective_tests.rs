//! Edge cases of `pdp::evaluator::effective` (T-1917, EP-26, MP-02).
//!
//! Contract, in one sentence: it answers the policies whose assignee IS this caller and which are
//! in force at `now`, and nothing else — a policy of another user, another group, another kind of
//! principal, or one outside its validity window is never in the answer, whatever it grants.
//!
//! This is what the access surface (`/access`, EP-55) is built from, so a policy that leaks in here
//! is a policy a caller is told about without holding it.

use chrono::{DateTime, Utc};
use context_gateway::pdp::evaluator::{effective, Subject};
use jc_core::kinds::{PolicySpec, Principal, PrincipalKind};
use std::collections::BTreeSet;

fn now() -> DateTime<Utc> {
    "2026-09-06T12:00:00Z".parse().expect("a fixed instant")
}

fn policy(yaml: &str) -> PolicySpec {
    serde_norway::from_str(yaml).expect("the policy spec parses")
}

/// A grant to one principal, written the way a Policy manifest writes it.
fn to(kind: &str, id: &str) -> PolicySpec {
    policy(&format!(
        r#"contextSpaceRef: ovzdusie
assigner: did:web:banskabystrica.sk
assignee: {{ kind: {kind}, id: {id} }}
operations: [queryEntity]
information:
  - entities:
      - type: AirQualityObserved
"#
    ))
}

/// The same grant with a validity window.
fn between(from: Option<&str>, to_: Option<&str>) -> PolicySpec {
    let mut window = String::from("validity:\n");
    if let Some(from) = from {
        window.push_str(&format!("  from: {from}\n"));
    }
    if let Some(to_) = to_ {
        window.push_str(&format!("  to: {to_}\n"));
    }
    policy(&format!(
        r#"contextSpaceRef: ovzdusie
assigner: did:web:banskabystrica.sk
assignee: {{ kind: role, id: public }}
operations: [queryEntity]
{window}"#
    ))
}

fn jana() -> Subject {
    Subject {
        user: Some("jana.kovacova".to_owned()),
        groups: BTreeSet::from(["ovzdusie-stewards".to_owned()]),
        roles: BTreeSet::from(["portal-viewer".to_owned()]),
        service_account: Some("pipeline-runner".to_owned()),
        did: Some("did:web:banskabystrica.sk:agents:kpi".to_owned()),
        agreement: None,
        via: None,
    }
}

/// The first case: nothing of another caller is in the answer, in any of the five principal kinds.
#[test]
fn no_policy_of_another_principal_is_effective() {
    let others = [
        to("user", "matej.hric"),
        to("group", "doprava-stewards"),
        to("role", "portal-admin"),
        to("serviceAccount", "kpi-runner"),
        to("did", "did:web:doprava.sk:agents:kpi"),
    ];

    assert!(effective(&jana(), &others, now()).is_empty(), "{others:?}");
}

/// Each of the five kinds matches on its own field, and only on that field: a name in `user` is not
/// a service account of the same name, and a group is not a role.
#[test]
fn a_principal_kind_matches_only_its_own_field() {
    let subject = Subject {
        user: Some("shared-name".to_owned()),
        groups: BTreeSet::from(["shared-name".to_owned()]),
        roles: BTreeSet::new(),
        service_account: None,
        did: None,
        agreement: None,
        // The account a delegated call came through is never a principal of it (AG-95).
        via: Some("shared-name".to_owned()),
    };

    assert_eq!(
        effective(&subject, &[to("user", "shared-name")], now()).len(),
        1
    );
    assert_eq!(
        effective(&subject, &[to("group", "shared-name")], now()).len(),
        1
    );
    for kind in ["role", "serviceAccount", "did"] {
        assert!(
            effective(&subject, &[to(kind, "shared-name")], now()).is_empty(),
            "a {kind} grant matched a name the caller holds as a user and a group"
        );
    }
}

/// An identifier is compared exactly: no casing, spacing or encoding of it is a second spelling.
#[test]
fn no_other_spelling_of_an_identifier_is_that_identifier() {
    for spelling in [
        "Jana.Kovacova",
        "JANA.KOVACOVA",
        "jana.kovacova ",
        " jana.kovacova",
        "jana%2Ekovacova",
        "jana.kovacova\u{0}",
        "jana.kovacova\n",
        "jana.kovacova\r\n",
        "jana.kovacova\u{200b}", // zero-width space
        "jаna.kovacova",         // Cyrillic а
    ] {
        // The assignee is built rather than written as YAML: a control character in a flow mapping
        // is a parse error, and what is under test is the comparison, not the parser.
        let mut grant = to("user", "nobody");
        grant.assignee = Principal::new(PrincipalKind::User, spelling);
        let grants = [grant];

        assert!(
            effective(&jana(), &grants, now()).is_empty(),
            "{spelling:?} was taken for the caller"
        );
    }
}

/// The window is closed at the start and open at the end, which is the rule `in_force` states:
/// `now >= from` and `now < to`.
#[test]
fn the_validity_window_includes_its_start_and_excludes_its_end() {
    let public = Subject::anonymous();
    let table = [
        (None, None, true, "no window at all is always in force"),
        (
            Some("2026-09-06T12:00:00Z"),
            None,
            true,
            "from == now is in force",
        ),
        (
            Some("2026-09-06T12:00:01Z"),
            None,
            false,
            "one second in the future is not",
        ),
        (
            None,
            Some("2026-09-06T12:00:01Z"),
            true,
            "to one second away is in force",
        ),
        (
            None,
            Some("2026-09-06T12:00:00Z"),
            false,
            "to == now has expired",
        ),
        (
            Some("2026-09-06T11:59:59Z"),
            Some("2026-09-06T12:00:01Z"),
            true,
            "inside both bounds",
        ),
        (
            Some("2026-09-06T12:00:01Z"),
            Some("2026-09-06T12:00:00Z"),
            false,
            "a window that ends before it starts holds nothing",
        ),
    ];

    for (from, to_, expected, why) in table {
        let grants = [between(from, to_)];
        assert_eq!(
            !effective(&public, &grants, now()).is_empty(),
            expected,
            "{why}"
        );
    }
}

/// An anonymous caller holds exactly `public`: not a user, not a group, not every role.
#[test]
fn an_anonymous_caller_holds_only_the_public_role() {
    let anonymous = Subject::anonymous();

    assert_eq!(
        effective(&anonymous, &[to("role", "public")], now()).len(),
        1
    );
    for other in [
        to("role", "portal-viewer"),
        to("user", "public"),
        to("group", "public"),
        to("serviceAccount", "public"),
        to("did", "public"),
    ] {
        assert!(
            effective(&anonymous, std::slice::from_ref(&other), now()).is_empty(),
            "anonymous matched {other:?}"
        );
    }
}

/// A caller with no identity at all matches nothing, rather than everything: a subject built from
/// `Default` (no user, no role) is the shape a PEP that failed to authenticate would hand over.
#[test]
fn a_subject_with_nothing_in_it_matches_nothing() {
    let nobody = Subject::default();
    let grants = [
        to("role", "public"),
        to("user", "jana.kovacova"),
        to("group", "ovzdusie-stewards"),
    ];

    assert!(effective(&nobody, &grants, now()).is_empty());
}

/// A prohibition is a policy the caller holds too: the access surface has to see it, because what
/// a caller may do is the permissions minus the prohibitions (GW4).
#[test]
fn a_prohibition_that_names_the_caller_is_effective() {
    let prohibition = policy(
        r#"contextSpaceRef: ovzdusie
effect: prohibition
assigner: did:web:banskabystrica.sk
assignee: { kind: user, id: jana.kovacova }
operations: [queryEntity]
information:
  - entities:
      - type: PersonRecord
"#,
    );

    let policies = [prohibition];
    let held = effective(&jana(), &policies, now());

    assert_eq!(
        held.len(),
        1,
        "a prohibition of this caller is one of its policies"
    );
    assert!(held[0].effect.is_prohibition());
}

/// The answer keeps the order of the policy set and holds no duplicates of its own: the surface
/// renders it, and a doubled row would read as two grants.
#[test]
fn the_answer_is_the_policy_set_in_its_own_order() {
    let grants = [
        to("user", "jana.kovacova"),
        to("group", "doprava-stewards"),
        to("role", "portal-viewer"),
    ];

    let held = effective(&jana(), &grants, now());

    assert_eq!(held.len(), 2, "the doprava group is not this caller's");
    assert_eq!(held[0], &grants[0]);
    assert_eq!(held[1], &grants[2]);
}

/// An empty policy set answers nothing, and the answer is repeatable for the same instant.
#[test]
fn an_empty_set_answers_nothing_and_the_answer_is_reproducible() {
    assert!(effective(&jana(), &[], now()).is_empty());

    let grants = [to("user", "jana.kovacova")];
    let once = effective(&jana(), &grants, now());
    for _ in 0..8 {
        assert_eq!(effective(&jana(), &grants, now()), once);
    }
}

/// An operation is not part of this question: `effective` answers the policy set, and a policy that
/// grants an operation the caller is not asking for is still one of the caller's policies (EP-55).
/// The operation filter is `applies`, which `evaluate` uses, and `pdp_tests.rs` holds it.
#[test]
fn the_operation_a_policy_grants_does_not_narrow_this_answer() {
    let write_only = policy(
        r#"contextSpaceRef: ovzdusie
assigner: did:web:banskabystrica.sk
assignee: { kind: user, id: jana.kovacova }
operations: [deleteEntity]
information:
  - entities:
      - type: AirQualityObserved
"#,
    );

    assert_eq!(effective(&jana(), &[write_only], now()).len(), 1);
}
