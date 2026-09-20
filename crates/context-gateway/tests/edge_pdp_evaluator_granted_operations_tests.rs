//! Edge cases of `pdp::evaluator::granted_operations` (T-1918, EP-26, MP-02).
//!
//! Contract, in one sentence: it answers exactly the CIM 009 Table 4.20-2 names the policy grants,
//! with a group expanded to its own members and nothing beside them — a group must never carry an
//! operation the table does not put in it, and an empty grant must answer nothing rather than
//! everything.
//!
//! The access surface prints this list, and the write guard reads the same expansion, so an extra
//! name here is an operation a caller is told it holds.

use context_gateway::pdp::evaluator::granted_operations;
use jc_core::kinds::{Operation, OperationGroup, PolicySpec};
use std::collections::BTreeSet;

fn policy(operations: &str) -> PolicySpec {
    serde_norway::from_str(&format!(
        r#"contextSpaceRef: ovzdusie
assigner: did:web:banskabystrica.sk
assignee: {{ kind: role, id: public }}
operations: {operations}
"#
    ))
    .expect("the policy spec parses")
}

fn names(of: &[Operation]) -> BTreeSet<&'static str> {
    of.iter().map(Operation::as_str).collect()
}

/// The first case, and the one an expansion bug would show: each group is its own table row and
/// nothing more.
#[test]
fn each_group_expands_to_exactly_its_own_operations() {
    for group in [
        OperationGroup::RetrieveOps,
        OperationGroup::UpdateOps,
        OperationGroup::AssociationOps,
        OperationGroup::FederationOps,
        OperationGroup::RedirectionOps,
    ] {
        let granted = granted_operations(&policy(&format!("[{}]", group.as_str())));

        assert_eq!(
            granted,
            names(group.operations()),
            "{} expanded to something else",
            group.as_str()
        );
    }
}

/// The consumption groups carry no write at all: this is the property the write guard leans on.
#[test]
fn a_read_group_carries_no_write() {
    for group in [
        OperationGroup::RetrieveOps,
        OperationGroup::AssociationOps,
        OperationGroup::FederationOps,
    ] {
        let granted = granted_operations(&policy(&format!("[{}]", group.as_str())));

        for write in [
            "createEntity",
            "updateEntity",
            "appendAttrs",
            "updateAttrs",
            "deleteAttrs",
            "deleteEntity",
            "mergeEntity",
            "replaceEntity",
            "replaceAttrs",
            "purgeEntity",
        ] {
            assert!(
                !granted.contains(write),
                "{} granted {write}",
                group.as_str()
            );
        }
    }
}

/// `retrieveOps` is the smallest group, and it is exactly two operations: it is what a public grant
/// is written with, so anything extra in it is public.
#[test]
fn retrieve_ops_is_two_reads_and_nothing_else() {
    assert_eq!(
        granted_operations(&policy("[retrieveOps]")),
        BTreeSet::from(["queryEntity", "retrieveEntity"])
    );
}

/// A single operation is itself, by the wire name, and nothing near it.
#[test]
fn a_single_operation_is_itself() {
    let granted = granted_operations(&policy("[retrieveEntity]"));

    assert_eq!(granted, BTreeSet::from(["retrieveEntity"]));
    assert!(
        !granted.contains("queryEntity"),
        "the neighbour in its group is not granted"
    );
    assert!(!granted.contains("retrieveEntityTypes"));
}

/// Singles and groups in one policy are the union, with the duplicate counted once.
#[test]
fn singles_and_groups_are_the_union_with_no_duplicate() {
    let granted = granted_operations(&policy("[retrieveEntity, retrieveOps, retrieveEntity]"));

    assert_eq!(granted, BTreeSet::from(["queryEntity", "retrieveEntity"]));
}

/// A policy that grants nothing answers nothing. `grants_operation` then finds no match and
/// `evaluate` denies, which is the GW5 floor.
#[test]
fn an_empty_operation_list_grants_nothing() {
    assert!(granted_operations(&policy("[]")).is_empty());
}

/// Every name is a CIM 009 name: lower camel case, no group name among them, no name the
/// vocabulary does not know.
#[test]
fn every_name_is_a_cim_009_operation_name() {
    let granted = granted_operations(&policy("[federationOps, redirectionOps, updateOps]"));
    let known: BTreeSet<&str> = Operation::ALL.iter().map(Operation::as_str).collect();

    for name in &granted {
        assert!(known.contains(name), "{name} is not a CIM 009 operation");
        assert!(
            !name.ends_with("Ops"),
            "{name} is a group name, not an operation"
        );
        assert!(
            name.chars().next().is_some_and(char::is_lowercase),
            "{name} is not written the way the table writes it"
        );
    }
}

/// `redirectionOps` forwards everything, which is the widest grant a policy can carry, and it is
/// still only what the table lists: the subscription operations are not in it.
#[test]
fn the_widest_group_is_still_only_its_own_row() {
    let granted = granted_operations(&policy("[redirectionOps]"));

    assert!(granted.contains("deleteEntity"), "it does forward writes");
    for subscription in [
        "createSubscription",
        "updateSubscription",
        "retrieveSubscription",
        "querySubscription",
        "deleteSubscription",
    ] {
        assert!(
            !granted.contains(subscription),
            "redirectionOps granted {subscription}, which Table 4.20-2 does not put in it"
        );
    }
}

/// The answer is a set, so the order the policy wrote its operations in cannot change it, and the
/// same policy always answers the same list.
#[test]
fn the_answer_is_order_free_and_repeatable() {
    let one = granted_operations(&policy("[updateOps, retrieveEntity]"));
    let other = granted_operations(&policy("[retrieveEntity, updateOps]"));

    assert_eq!(one, other);
    for _ in 0..8 {
        assert_eq!(
            granted_operations(&policy("[updateOps, retrieveEntity]")),
            one
        );
    }
}

/// A policy's effect is not part of this answer: the same expansion is used to say what a
/// prohibition takes away, so a prohibition of `retrieveOps` has to expand the same way.
#[test]
fn a_prohibition_expands_exactly_like_a_permission() {
    let prohibition: PolicySpec = serde_norway::from_str(
        r#"contextSpaceRef: ovzdusie
effect: prohibition
assigner: did:web:banskabystrica.sk
assignee: { kind: role, id: public }
operations: [retrieveOps]
"#,
    )
    .expect("the policy spec parses");

    assert_eq!(
        granted_operations(&prohibition),
        granted_operations(&policy("[retrieveOps]"))
    );
}
