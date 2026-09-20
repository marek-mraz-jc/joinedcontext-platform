//! Edge cases of `pdp::evaluator::narrow` (T-1915, EP-26, MP-02).
//!
//! Contract, in one sentence: when the grants name anything at all, the result is a subset of what
//! they name — a caller can never widen an answer by naming a type or an attribute it was not
//! given — and the names are compared as bytes, so no spelling, casing or encoding of a granted
//! name gets past the comparison.
//!
//! The happy path (`asking for nothing yields the granted set`) is in `pdp_tests.rs`. These are
//! the inputs that try to widen the set.

use context_gateway::pdp::evaluator::narrow;
use std::collections::BTreeSet;

fn set(names: &[&str]) -> BTreeSet<String> {
    names.iter().map(|name| (*name).to_owned()).collect()
}

/// The one property of the function: nothing outside the grant survives it.
#[test]
fn nothing_the_grant_does_not_name_survives() {
    let granted = set(&["pm10", "pm25", "location"]);
    for requested in [
        set(&["secretPin"]),
        set(&["pm10", "secretPin"]),
        set(&["pm10", "pm25", "location", "secretPin", "age", "name"]),
        set(&["id", "type"]),
    ] {
        let narrowed = narrow(&requested, &granted);
        assert!(
            narrowed.is_subset(&granted),
            "{requested:?} widened the answer to {narrowed:?}"
        );
        assert!(!narrowed.contains("secretPin"), "{requested:?}");
    }
}

/// A name is a name: the comparison is exact, so no casing or encoding of it is a second spelling.
#[test]
fn no_other_spelling_of_a_granted_name_is_that_name() {
    let granted = set(&["pm10"]);
    for spelling in [
        "PM10",
        "Pm10",
        "pM10",
        "pm10 ",
        " pm10",
        "pm10/",
        "pm10.",
        "pm%3110",
        "%70m10",
        "%2570m10",
        "pm１０", // full-width digits
        "pm10\u{0}",
        "pm10\n",
        "pm10\r\n",
        "pm10\u{200b}", // zero-width space
        "рm10",         // Cyrillic er
    ] {
        let narrowed = narrow(&set(&[spelling]), &granted);
        assert!(narrowed.is_empty(), "{spelling:?} was taken for pm10");
    }
}

/// A grant that names nothing narrows nothing, which is the documented "whole entity" case: the
/// caller's own list stands. `intersect` is what pairs it with an empty request, and
/// `an_unwhitelisted_grant_beside_a_whitelisted_one_grants_everything` in `pdp_tests.rs` holds that
/// pairing; here it is only the arithmetic.
#[test]
fn an_empty_grant_leaves_the_request_as_it_is() {
    let empty = BTreeSet::new();
    assert_eq!(narrow(&set(&["pm10"]), &empty), set(&["pm10"]));
    assert_eq!(narrow(&empty, &empty), empty);
    assert_eq!(
        narrow(&set(&["a", "b", "c"]), &empty),
        set(&["a", "b", "c"]),
        "with no whitelist the caller's own selection is the only narrowing"
    );
}

/// A request that names nothing is every granted name, never nothing.
#[test]
fn an_empty_request_is_the_whole_grant() {
    let granted = set(&["pm10", "pm25"]);
    assert_eq!(narrow(&BTreeSet::new(), &granted), granted);
}

/// Two whitelists that share nothing leave nothing. Downstream reads an empty set as "no
/// projection", which is why `narrow_to_identity` (T-1916) exists; `narrow` itself must not
/// pretend the sets overlapped.
#[test]
fn disjoint_sets_leave_nothing() {
    assert!(narrow(&set(&["secretPin"]), &set(&["pm10"])).is_empty());
}

/// The bound and the bound plus one: a long list is still cut to the grant.
#[test]
fn a_list_of_one_and_a_list_of_many_are_both_cut_to_the_grant() {
    let granted = set(&["pm10"]);
    assert_eq!(narrow(&set(&["pm10"]), &granted), granted);

    let many: BTreeSet<String> = (0..257).map(|index| format!("attr{index}")).collect();
    assert!(
        narrow(&many, &granted).is_empty(),
        "257 names, none granted"
    );

    let with_one_granted: BTreeSet<String> =
        many.iter().cloned().chain(["pm10".to_owned()]).collect();
    assert_eq!(narrow(&with_one_granted, &granted), granted);
}

/// A duplicate is not a second name: the sets are sets before they reach the function.
#[test]
fn duplicates_cannot_multiply_a_name() {
    let requested: BTreeSet<String> = ["pm10", "pm10", "pm10"]
        .into_iter()
        .map(str::to_owned)
        .collect();
    assert_eq!(requested.len(), 1);
    assert_eq!(narrow(&requested, &set(&["pm10"])), set(&["pm10"]));
}

/// A name with a CR/LF or a NUL in it reaches a broker URL and a log line, so it may only survive
/// when the grant itself carries it, and then it is the grant's own name.
#[test]
fn a_control_character_only_survives_when_the_grant_itself_carries_it() {
    let hostile = "pm10\r\nX-Injected: 1";
    assert!(narrow(&set(&[hostile]), &set(&["pm10"])).is_empty());
    assert_eq!(
        narrow(&set(&[hostile]), &set(&[hostile])),
        set(&[hostile]),
        "a grant written with a control character is the grant's own problem, not a widening"
    );
}

/// The function is order-free and repeatable: the same pair of sets always narrows the same way,
/// which is what makes a verdict reproducible (GW13).
#[test]
fn the_same_two_sets_always_narrow_the_same_way() {
    let requested = set(&["pm25", "pm10", "location"]);
    let granted = set(&["location", "pm10"]);
    let once = narrow(&requested, &granted);
    for _ in 0..8 {
        assert_eq!(narrow(&requested, &granted), once);
    }
    assert_eq!(once, set(&["location", "pm10"]));
}

/// Neither argument is mutated, so a narrowing cannot be used to grow the grant set for the next
/// request that borrows it.
#[test]
fn narrowing_leaves_both_sets_untouched() {
    let requested = set(&["pm10", "secretPin"]);
    let granted = set(&["pm10"]);
    let before = (requested.clone(), granted.clone());

    let _ = narrow(&requested, &granted);

    assert_eq!((requested, granted), before);
}
