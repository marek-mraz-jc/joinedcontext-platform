//! Edge cases of `resolver::Endpoint::admits` (T-1947, EP-14, EP-15, EP-26, MP-02).
//!
//! Contract, in one sentence: a public endpoint admits everyone including an anonymous caller, an
//! organization endpoint admits any caller the gateway could name a project for and no anonymous
//! one, and a project-list endpoint admits only its own project and the projects it lists,
//! compared exactly.
//!
//! This is the door before the PDP: `app.rs` calls it on the anonymous path, after a token is
//! resolved to an account and for a dataspace transfer token, and a `false` is a `404` that says
//! nothing about what is behind it. Widening it by one character would publish a space to a
//! project that was never listed.

use context_gateway::resolver::Endpoint;
use jc_core::kinds::{Audience, Representation};

const SLUG: &str = "mluyob4nz52lok3ssk7pgn5vwt";

fn endpoint(audience: Audience, allowed: &[&str]) -> Endpoint {
    Endpoint {
        slug: SLUG.to_owned(),
        title: std::collections::BTreeMap::new(),
        description: std::collections::BTreeMap::new(),
        space: "ovzdusie".to_owned(),
        project: "ovzdusie".to_owned(),
        audience,
        allowed_projects: allowed.iter().map(|name| (*name).to_owned()).collect(),
        representations: vec![Representation::NgsiLd],
        rate_limit: None,
        file_limits: None,
        hidden_attributes: std::collections::BTreeSet::new(),
        projection: None,
        base_path: format!("/api/endpoint/{SLUG}"),
        models: Vec::new(),
        view_mapping: None,
        policies: Vec::new(),
    }
}

/// EP-15: a project the list does not name is refused, and the comparison is exact — no case
/// folding, no trimming, no prefix, or a project could be published to by looking like another.
#[test]
fn a_project_that_is_not_named_exactly_is_refused() {
    let listed = endpoint(Audience::ProjectList, &["doprava", "energetika"]);
    for caller in [
        "Doprava",
        "DOPRAVA",
        "doprava ",
        " doprava",
        "doprava/",
        "dopravа", // the last letter is Cyrillic a
        "doprav",
        "dopravax",
        "energetika2",
        "ovzdusie2",
        "",
        "*",
        "%64oprava",
    ] {
        assert!(!listed.admits(Some(caller)), "{caller:?}");
    }

    for caller in ["doprava", "energetika", "ovzdusie"] {
        assert!(listed.admits(Some(caller)), "{caller:?}");
    }
}

/// EP-14: a public endpoint is the one that serves a caller with no token at all.
#[test]
fn only_a_public_endpoint_admits_an_anonymous_caller() {
    assert!(endpoint(Audience::Public, &[]).admits(None));
    assert!(!endpoint(Audience::Organization, &[]).admits(None));
    assert!(!endpoint(Audience::ProjectList, &["ovzdusie"]).admits(None));
    assert!(
        !endpoint(Audience::ProjectList, &[]).admits(None),
        "an empty list is not an invitation"
    );
}

/// A public endpoint admits every caller, named or not: it is published to the world, and the
/// Policy is what decides what that world may read.
#[test]
fn a_public_endpoint_admits_every_caller() {
    let public = endpoint(Audience::Public, &[]);
    for caller in [
        None,
        Some("ovzdusie"),
        Some("doprava"),
        Some(""),
        Some("../etc"),
    ] {
        assert!(public.admits(caller), "{caller:?}");
    }
}

/// An organization endpoint asks only whether the caller has a project at all, because every
/// project of this instance belongs to the organization.
#[test]
fn an_organization_endpoint_admits_any_named_project() {
    let internal = endpoint(Audience::Organization, &[]);
    for caller in ["ovzdusie", "doprava", "a-project-nobody-listed"] {
        assert!(internal.admits(Some(caller)), "{caller}");
    }
    assert!(!internal.admits(None));
}

/// The owning project does not have to list itself: an endpoint always serves the space it
/// belongs to, and a steward who has to list their own project forgets to.
#[test]
fn the_owning_project_is_admitted_without_being_listed() {
    let listed = endpoint(Audience::ProjectList, &["doprava"]);
    assert!(
        listed.admits(Some("ovzdusie")),
        "the endpoint's own project"
    );
    assert!(listed.admits(Some("doprava")));
    assert!(!listed.admits(Some("energetika")));
}

/// An empty list narrows to the owning project alone rather than opening the endpoint up.
#[test]
fn an_empty_allowed_list_leaves_only_the_owning_project() {
    let alone = endpoint(Audience::ProjectList, &[]);
    assert!(alone.admits(Some("ovzdusie")));
    for caller in ["doprava", "energetika", ""] {
        assert!(!alone.admits(Some(caller)), "{caller:?}");
    }
}

/// A repeated entry is one entry, and it does not change the answer for anything else.
#[test]
fn a_duplicated_entry_changes_nothing() {
    let repeated = endpoint(Audience::ProjectList, &["doprava", "doprava", "doprava"]);
    assert!(repeated.admits(Some("doprava")));
    assert!(!repeated.admits(Some("energetika")));
}

/// Every entry is asked, including the last: a long list is not searched half way.
#[test]
fn every_entry_of_a_long_list_is_asked() {
    let many: Vec<String> = (0..256)
        .map(|index| format!("project-{index:03}"))
        .collect();
    let names: Vec<&str> = many.iter().map(String::as_str).collect();
    let listed = endpoint(Audience::ProjectList, &names);

    assert!(listed.admits(Some("project-000")));
    assert!(listed.admits(Some("project-255")), "the last entry counts");
    assert!(!listed.admits(Some("project-256")));
}

/// A caller name the gateway could not resolve to a project arrives as an empty string on an
/// organization endpoint, and is admitted as "some caller". Struck as unreachable: every project
/// the enforcement point passes comes from a manifest namespace (`store.rs`'s
/// `id.namespace.clone().unwrap_or_default()`) and the reconciler namespaces every manifest, so
/// the empty string is a shape that no request produces. Written down because the audience check
/// would not catch it if that ever changed.
#[test]
fn an_empty_project_name_counts_as_a_caller_on_an_organization_endpoint() {
    assert!(endpoint(Audience::Organization, &[]).admits(Some("")));
    assert!(
        !endpoint(Audience::ProjectList, &["doprava"]).admits(Some("")),
        "a project list still compares it to the names, and no name is empty"
    );
}

/// Asking twice answers the same, and asking does not change the endpoint.
#[test]
fn the_answer_does_not_depend_on_the_order_of_the_questions() {
    let listed = endpoint(Audience::ProjectList, &["doprava"]);
    for _ in 0..3 {
        assert!(listed.admits(Some("doprava")));
        assert!(!listed.admits(Some("energetika")));
        assert!(!listed.admits(None));
        assert!(listed.admits(Some("ovzdusie")));
    }
}
