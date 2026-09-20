//! Edge cases of `auth::accounts::Account::roles_in` (T-1891, EP-26, MP-02).
//!
//! Contract, in one sentence: a role reaches a call only when it is scoped to the whole
//! organization, to the project it is asked about, or to the space segment it is asked about —
//! and the names are compared exactly, so nothing reaches a neighbouring space or project because
//! its name merely looks similar.
//!
//! The one caller (`src/app.rs`, `roles_on(endpoint.audience, account.roles_in(&account.project,
//! &endpoint.space))`) asks about the account's OWN project and the called endpoint's space, behind
//! `endpoint.admits(Some(&account.project))`. So a role scoped to a third project can never be
//! asked for in production; it is still tested here, because this function is public and the day a
//! second caller passes the endpoint's project instead, the answer must stay narrow.

use context_gateway::auth::accounts::{Account, ScopedRole};
use std::collections::BTreeSet;

fn role(role: &str, space: Option<&str>, project: Option<&str>, organization: bool) -> ScopedRole {
    ScopedRole {
        role: role.to_owned(),
        context_space: space.map(str::to_owned),
        project: project.map(str::to_owned),
        organization,
    }
}

fn account(roles: Vec<ScopedRole>) -> Account {
    Account {
        name: "collector".to_owned(),
        project: "helsinki".to_owned(),
        roles,
    }
}

fn set(names: &[&str]) -> BTreeSet<String> {
    names.iter().map(|name| (*name).to_owned()).collect()
}

#[test]
fn an_account_with_no_roles_has_no_roles_here() {
    assert_eq!(
        account(vec![]).roles_in("helsinki", "mobility"),
        BTreeSet::new()
    );
}

#[test]
fn an_organization_role_reaches_every_project_and_space() {
    let a = account(vec![role("reader", None, None, true)]);
    assert_eq!(a.roles_in("helsinki", "mobility"), set(&["reader"]));
    assert_eq!(a.roles_in("espoo", "waste"), set(&["reader"]));
    // Including the empty names, which is what an unnamespaced repository resolves to.
    assert_eq!(a.roles_in("", ""), set(&["reader"]));
}

#[test]
fn a_role_scoped_to_another_space_is_not_a_role_here() {
    let a = account(vec![role("writer", Some("waste"), None, false)]);
    assert_eq!(a.roles_in("helsinki", "mobility"), BTreeSet::new());
    assert_eq!(a.roles_in("helsinki", "waste"), set(&["writer"]));
}

#[test]
fn a_role_scoped_to_another_project_is_not_a_role_here() {
    let a = account(vec![role("writer", None, Some("espoo"), false)]);
    assert_eq!(a.roles_in("helsinki", "mobility"), BTreeSet::new());
}

#[test]
fn names_are_compared_exactly_so_nothing_neighbouring_gets_in() {
    // Case, a trailing slash, a trailing space, a percent-encoded letter, an encoded one twice and
    // a prefix of the real name: none of these is the space that was asked about.
    for near in [
        "Mobility",
        "mobility/",
        "mobility ",
        " mobility",
        "mobilit",
        "mobilityy",
        "%6dobility",
        "%256dobility",
        "mobility\0",
        "mobility\n",
    ] {
        let a = account(vec![role("writer", Some(near), None, false)]);
        assert_eq!(
            a.roles_in("helsinki", "mobility"),
            BTreeSet::new(),
            "a role scoped to {near:?} must not reach the space `mobility`",
        );
    }
    // And the same for the project.
    for near in [
        "Helsinki",
        "helsinki/",
        "helsinki ",
        "helsink",
        "%68elsinki",
    ] {
        let a = account(vec![role("writer", None, Some(near), false)]);
        assert_eq!(
            a.roles_in("helsinki", "mobility"),
            BTreeSet::new(),
            "project {near:?}"
        );
    }
}

#[test]
fn an_empty_scope_name_matches_only_an_empty_name() {
    let a = account(vec![role("writer", Some(""), None, false)]);
    assert_eq!(a.roles_in("helsinki", "mobility"), BTreeSet::new());
    assert_eq!(a.roles_in("helsinki", ""), set(&["writer"]));

    let unnamespaced = account(vec![role("writer", None, Some(""), false)]);
    assert_eq!(
        unnamespaced.roles_in("helsinki", "mobility"),
        BTreeSet::new()
    );
    // A repository without namespaces resolves every project to "", and then a project-scoped role
    // does reach it: that is the same project, not a wider one.
    assert_eq!(unnamespaced.roles_in("", "mobility"), set(&["writer"]));
}

#[test]
fn one_role_named_twice_is_one_role() {
    let a = account(vec![
        role("reader", Some("mobility"), None, false),
        role("reader", None, Some("helsinki"), false),
        role("reader", None, None, true),
    ]);
    assert_eq!(a.roles_in("helsinki", "mobility"), set(&["reader"]));
}

#[test]
fn a_role_out_of_scope_does_not_carry_one_in_scope_with_it() {
    let a = account(vec![
        role("reader", Some("mobility"), None, false),
        role("owner", Some("waste"), None, false),
        role("admin", None, Some("espoo"), false),
    ]);
    assert_eq!(a.roles_in("helsinki", "mobility"), set(&["reader"]));
}

#[test]
fn a_scope_naming_a_space_and_another_project_reaches_the_space() {
    // Either scope is enough, by the `||` of the filter: this pins the behaviour so a later reading
    // of the code cannot quietly turn it into "both must match" or "the wider one wins".
    let a = account(vec![role("reader", Some("mobility"), Some("espoo"), false)]);
    assert_eq!(a.roles_in("helsinki", "mobility"), set(&["reader"]));
    assert_eq!(a.roles_in("espoo", "waste"), set(&["reader"]));
    assert_eq!(a.roles_in("helsinki", "waste"), BTreeSet::new());
}

#[test]
fn a_role_name_is_returned_as_written_and_nothing_is_invented() {
    // Whatever the manifest called the role is what the PDP is asked about: this function neither
    // trims, lower-cases nor expands anything, and it never adds a role nobody bound.
    let odd = "Reader-With Space\tand\ttabs";
    let a = account(vec![role(odd, None, None, true)]);
    let answer = a.roles_in("helsinki", "mobility");
    assert_eq!(answer, set(&[odd]));
    assert!(
        !answer.contains("reader"),
        "no role is invented from another's spelling"
    );
}

#[test]
fn a_hundred_bindings_answer_only_the_ones_in_scope() {
    let mut roles: Vec<ScopedRole> = (0..100)
        .map(|n| {
            role(
                &format!("role-{n}"),
                Some(&format!("space-{n}")),
                None,
                false,
            )
        })
        .collect();
    roles.push(role("reader", Some("mobility"), None, false));
    let a = account(roles);
    assert_eq!(a.roles_in("helsinki", "mobility"), set(&["reader"]));
}
