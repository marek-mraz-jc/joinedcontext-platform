//! Edge cases of `pdp::evaluator::granted_attrs` (T-1919, EP-26, MP-02).
//!
//! Contract, in one sentence: it answers the union of the property and relationship names the
//! policy's registration information whitelists, exactly as written — and an information block
//! that whitelists nothing answers the empty set, which every caller of this function has to read
//! as "the whole entity" and not as "nothing" (R9, T-0805).
//!
//! That empty-set meaning is the sharp edge: `intersect` handles it by dropping every other
//! grant's whitelist when one grant has none, and `no_whitelist_anywhere_means_the_whole_entity`
//! below pins the arithmetic this function contributes to it.

use context_gateway::pdp::evaluator::{granted_attrs, granted_types};
use jc_core::kinds::{PolicySpec, RegistrationInfo};
use std::collections::BTreeSet;

fn info(yaml: &str) -> Vec<RegistrationInfo> {
    let policy: PolicySpec = serde_norway::from_str(&format!(
        r#"contextSpaceRef: ovzdusie
assigner: did:web:banskabystrica.sk
assignee: {{ kind: role, id: public }}
operations: [queryEntity]
information:
{yaml}"#
    ))
    .expect("the policy spec parses");
    policy.information
}

fn set(names: &[&str]) -> BTreeSet<String> {
    names.iter().map(|name| (*name).to_owned()).collect()
}

/// A relationship is as much of a whitelist as a property: a grant that names only relationships
/// still narrows, and forgetting them would serve every property of the entity.
#[test]
fn properties_and_relationships_are_one_whitelist() {
    let information = info(
        r#"  - entities:
      - type: Vehicle
    propertyNames: [name, weight]
    relationshipNames: [refDepot]
"#,
    );

    assert_eq!(
        granted_attrs(&information),
        set(&["name", "refDepot", "weight"])
    );
}

/// A grant that names only a relationship is not an empty whitelist.
#[test]
fn a_relationship_only_grant_is_still_a_whitelist() {
    let information = info(
        r#"  - entities:
      - type: Vehicle
    relationshipNames: [refDepot]
"#,
    );

    let granted = granted_attrs(&information);

    assert_eq!(granted, set(&["refDepot"]));
    assert!(
        !granted.is_empty(),
        "an empty set would mean the whole entity"
    );
}

/// The case every caller of this function has to get right: nothing whitelisted is the empty set,
/// and the empty set means the whole entity. It is written down here so a change that makes it
/// answer something else is caught in this file rather than in a leak.
#[test]
fn no_whitelist_anywhere_means_the_whole_entity() {
    for information in [
        info("  - entities:\n      - type: Vehicle\n"),
        info("  - entities:\n      - type: Vehicle\n    propertyNames: []\n"),
        info("  - entities:\n      - type: Vehicle\n    relationshipNames: []\n"),
        info("  - entities:\n      - type: Vehicle\n    propertyNames: []\n    relationshipNames: []\n"),
        Vec::new(),
    ] {
        assert!(
            granted_attrs(&information).is_empty(),
            "{information:?} answered a whitelist it does not have"
        );
    }
}

/// Several information blocks are one union, and a duplicate across them is one name.
#[test]
fn several_blocks_are_one_union_with_no_duplicate() {
    let information = info(
        r#"  - entities:
      - type: User
    propertyNames: [name, age]
  - entities:
      - type: Vehicle
    propertyNames: [name, weight]
    relationshipNames: [refDepot]
"#,
    );

    assert_eq!(
        granted_attrs(&information),
        set(&["age", "name", "refDepot", "weight"])
    );
}

/// The union is over the whole policy, not per type: this function cannot say which attribute
/// belongs to which type, and a caller that needs that reads `attrs_by_type` from the projection
/// instead (MP-02, T-1862). Named here because it is the defect the leak probe found.
#[test]
fn the_union_says_nothing_about_which_type_carries_which_attribute() {
    let information = info(
        r#"  - entities:
      - type: User
    propertyNames: [age]
  - entities:
      - type: Vehicle
    propertyNames: [weight]
"#,
    );

    assert_eq!(granted_attrs(&information), set(&["age", "weight"]));
    assert_eq!(granted_types(&information), set(&["User", "Vehicle"]));
    // The pair above is exactly why a per-type map exists: this set alone would serve a Vehicle
    // the age of a User. `model_projection_tests.rs` holds the per-type narrowing.
}

/// A name is kept exactly as the manifest wrote it: no trimming, no case folding, no decoding.
/// Anything else would make two grants of one name, or let a hostile spelling match a real one.
#[test]
fn a_name_is_kept_exactly_as_written() {
    let information = info(
        r#"  - entities:
      - type: User
    propertyNames: ["Age", "age", " age", "age ", "%61ge", "aGe"]
"#,
    );

    assert_eq!(
        granted_attrs(&information),
        set(&["%61ge", " age", "Age", "aGe", "age", "age "]),
        "six spellings are six names"
    );
}

/// An attribute list of one and of many both come back whole: there is no cap here, and the cap
/// that does exist belongs to the request parser.
#[test]
fn a_list_of_one_and_a_list_of_many_both_come_back_whole() {
    let one = info("  - entities:\n      - type: User\n    propertyNames: [age]\n");
    assert_eq!(granted_attrs(&one).len(), 1);

    let names: Vec<String> = (0..257).map(|index| format!("attr{index}")).collect();
    let many = info(&format!(
        "  - entities:\n      - type: User\n    propertyNames: [{}]\n",
        names.join(", ")
    ));
    assert_eq!(granted_attrs(&many).len(), 257);
}

/// A block with no entity selector at all still contributes its whitelist: the types and the
/// attributes are two independent questions, and a grant that names attributes without a type is
/// a grant over every type it covers.
#[test]
fn a_block_with_no_entity_selector_still_carries_its_whitelist() {
    let information = info("  - entities: []\n    propertyNames: [age]\n");

    assert_eq!(granted_attrs(&information), set(&["age"]));
    assert!(granted_types(&information).is_empty());
}

/// The identity names are not implied: `granted_attrs` answers what the policy wrote, and the
/// identity is added downstream by `narrow_to_identity` (T-1916) where it is needed.
#[test]
fn the_identity_is_not_implied() {
    let information = info("  - entities:\n      - type: User\n    propertyNames: [age]\n");
    let granted = granted_attrs(&information);

    assert!(!granted.contains("id"));
    assert!(!granted.contains("type"));
}

/// The same information always answers the same set, whatever order it is written in.
#[test]
fn the_answer_is_order_free_and_repeatable() {
    let one = info("  - entities:\n      - type: User\n    propertyNames: [age, name]\n");
    let other = info("  - entities:\n      - type: User\n    propertyNames: [name, age]\n");

    assert_eq!(granted_attrs(&one), granted_attrs(&other));
    let once = granted_attrs(&one);
    for _ in 0..8 {
        assert_eq!(granted_attrs(&one), once);
    }
}
