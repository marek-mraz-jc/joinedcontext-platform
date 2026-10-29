//! Edge cases of `pdp::evaluator::narrow_to_identity` (T-1916, EP-26, MP-02).
//!
//! Contract, in one sentence: it is `narrow`, except that two non-empty whitelists sharing nothing
//! answer `{id, type}` instead of the empty set — because an empty set downstream means "no
//! projection", which would serve the whole entity (T-0812) — and `{id, type}` is all it may ever
//! add: never a requested name, never a granted one.

use context_gateway::pdp::evaluator::{narrow, narrow_to_identity};
use std::collections::BTreeSet;

fn set(names: &[&str]) -> BTreeSet<String> {
    names.iter().map(|name| (*name).to_owned()).collect()
}

fn identity() -> BTreeSet<String> {
    set(&["id", "type"])
}

/// The case the function exists for, and the one that would leak if it were the empty set: a
/// caller asking for an attribute the grant does not whitelist must not fall through to "no
/// projection at all".
#[test]
fn a_request_sharing_nothing_with_the_grant_keeps_the_identity_and_nothing_else() {
    let narrowed = narrow_to_identity(&set(&["secretPin"]), &set(&["pm10"]));

    assert_eq!(narrowed, identity());
    assert!(
        !narrowed.contains("secretPin"),
        "the asked-for name is not the answer"
    );
    assert!(!narrowed.contains("pm10"), "nor is the granted one");
    assert!(
        !narrowed.is_empty(),
        "an empty set is read as no projection, which is the whole point of this function"
    );
}

/// Every disjoint pair answers the same two names, whatever was asked for.
#[test]
fn every_disjoint_pair_answers_the_same_two_names() {
    for (requested, granted) in [
        (set(&["secretPin"]), set(&["pm10"])),
        (set(&["a", "b", "c"]), set(&["x", "y"])),
        (set(&["id "]), set(&["id"])), // a trailing space is another name
        (set(&["ID", "TYPE"]), set(&["id"])), // and so is upper case
        (set(&["location"]), set(&["Location"])),
    ] {
        assert_eq!(
            narrow_to_identity(&requested, &granted),
            identity(),
            "{requested:?} against {granted:?}"
        );
    }
}

/// When the sets do overlap it is `narrow` exactly: no identity is bolted on, because the caller
/// asked for a projection and gets the one it asked for.
#[test]
fn an_overlap_is_narrow_itself_with_nothing_added() {
    let requested = set(&["pm10", "secretPin"]);
    let granted = set(&["pm10", "pm25"]);

    let narrowed = narrow_to_identity(&requested, &granted);

    assert_eq!(narrowed, set(&["pm10"]));
    assert_eq!(narrowed, narrow(&requested, &granted));
    assert!(
        !narrowed.contains("id"),
        "nothing is added to a projection that survived"
    );
}

/// An empty side is the documented "nothing to narrow" case and keeps `narrow`'s answer, because
/// there is no disjointness to hide: one of the two sets says "everything".
#[test]
fn an_empty_side_is_narrow_and_not_the_identity() {
    let empty = BTreeSet::new();

    assert_eq!(narrow_to_identity(&empty, &set(&["pm10"])), set(&["pm10"]));
    assert_eq!(narrow_to_identity(&set(&["pm10"]), &empty), set(&["pm10"]));
    assert_eq!(narrow_to_identity(&empty, &empty), empty);
    assert_eq!(
        narrow_to_identity(&set(&["secretPin"]), &empty),
        set(&["secretPin"]),
        "with no whitelist there is nothing this function can decide; the grant is unprojected"
    );
}

/// `id` and `type` are never a way into an attribute: a grant that whitelists exactly them still
/// narrows to them, and a request for them against a grant that has neither is the same answer.
#[test]
fn the_identity_names_are_not_a_channel() {
    assert_eq!(
        narrow_to_identity(&set(&["id", "type"]), &set(&["id", "type"])),
        identity()
    );
    assert_eq!(
        narrow_to_identity(&set(&["id"]), &set(&["pm10"])),
        identity()
    );
    assert_eq!(
        narrow_to_identity(&set(&["secretPin"]), &set(&["id", "type"])),
        identity(),
        "asking for a hidden attribute against an identity-only grant answers the identity"
    );
}

/// The answer is always inside the grant, or the two identity names; there is no third outcome.
#[test]
fn the_answer_is_the_grant_or_the_identity() {
    let granted = set(&["pm10", "pm25"]);
    for requested in [
        set(&["pm10"]),
        set(&["pm10", "pm25"]),
        set(&["pm10", "secretPin"]),
        set(&["secretPin"]),
        set(&["secretPin", "age", "name", "weight"]),
        BTreeSet::new(),
    ] {
        let narrowed = narrow_to_identity(&requested, &granted);
        let allowed = narrowed.is_subset(&granted) || narrowed == identity();
        assert!(allowed, "{requested:?} answered {narrowed:?}");
    }
}

/// A long request that shares nothing is still the two names, not a page of them.
#[test]
fn a_request_of_many_names_that_shares_nothing_is_still_two_names() {
    let many: BTreeSet<String> = (0..257).map(|index| format!("attr{index}")).collect();

    assert_eq!(narrow_to_identity(&many, &set(&["pm10"])), identity());
}

/// The same inputs always answer the same way, and neither set is touched.
#[test]
fn it_is_repeatable_and_leaves_its_inputs_alone() {
    let requested = set(&["secretPin"]);
    let granted = set(&["pm10"]);
    let before = (requested.clone(), granted.clone());

    for _ in 0..8 {
        assert_eq!(narrow_to_identity(&requested, &granted), identity());
    }
    assert_eq!((requested, granted), before);
}

/// A control character or an encoding of a granted name does not overlap it, so the answer is the
/// identity rather than the hostile name.
#[test]
fn a_hostile_spelling_answers_the_identity_and_never_itself() {
    for hostile in ["pm10\r\nX-Injected: 1", "%70m10", "pm10\u{0}", "PM10"] {
        let narrowed = narrow_to_identity(&set(&[hostile]), &set(&["pm10"]));
        assert_eq!(narrowed, identity(), "{hostile:?}");
        assert!(
            !narrowed.contains(hostile),
            "{hostile:?} reached the projection"
        );
    }
}
