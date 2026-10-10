//! Each entity judged by the grants it matches, not by the union of every grant (T-3530, EP-16,
//! K9, K10, K12).
//!
//! The broker is told one filter, the OR of the grants' conditions, and one attribute list, the
//! union of the grants' lists: CIM 009 has no way to say "these attributes for the entities this
//! condition selects". So an entity one grant admits came back carrying an attribute only another
//! grant names, under a condition the entity does not meet: a public depot showed the
//! `securityCode` a district grant gives for its own depots. Here every entity is held against
//! each grant's own types, id patterns, `q`, `scopeQ` and area, and it shows the union of the
//! lists of the grants it matches; an entity no grant matches is not served at all.

use super::geo::Areas;
use super::projection::{id_permitted, type_granted};
use antares_jsonld::{expand_entity, Context, ExpandOpts};
use jc_core::kinds::PolicySpec;
use serde_json::Value;
use std::collections::BTreeSet;
use std::sync::OnceLock;

/// One grant as an entity is judged against it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GrantView {
    /// The types it names; empty is every type.
    pub types: BTreeSet<String>,
    /// Its anchored id patterns; empty is every id.
    pub id_patterns: BTreeSet<String>,
    /// Its `q`, in the NGSI-LD query language.
    pub q: Option<String>,
    /// Its `scopeQ`.
    pub scope_q: Option<String>,
    /// Its area, as a `geoQ`.
    pub geo_q: Option<String>,
    /// The attributes it shows; empty is the whole entity (EP-16).
    pub attrs: BTreeSet<String>,
}

impl GrantView {
    /// The view of one policy: its own selectors, conditions and whitelist, and nobody else's.
    pub fn of(policy: &PolicySpec) -> Self {
        GrantView {
            types: super::evaluator::granted_types(&policy.information),
            id_patterns: super::evaluator::id_patterns(policy),
            q: policy.q.clone(),
            scope_q: policy.scope_q.clone(),
            geo_q: policy.geo_q.clone(),
            attrs: super::evaluator::granted_attrs(&policy.information),
        }
    }

    /// Whether the entity, as the broker answered it, meets every condition of this grant.
    ///
    /// Each condition fails closed: a `q` that does not parse, an area that is not a polygon
    /// and an entity that cannot be placed or read all answer no.
    pub fn matches(&self, entity: &Value) -> bool {
        type_granted(entity, &self.types)
            && id_permitted(entity, &self.id_patterns)
            && self
                .scope_q
                .as_deref()
                .is_none_or(|scope_q| antares_ql::scope::scope_matches(scope_q, entity))
            && self.geo_q.as_deref().is_none_or(|geo_q| {
                Areas::of(&[geo_q.to_owned()], None).is_some_and(|areas| areas.admits(entity))
            })
            && self.q.as_deref().is_none_or(|q| q_holds(q, entity))
    }
}

/// What one entity may show by the grants it matches.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Shown {
    /// A matched grant names no list: the whole entity (EP-16).
    Whole,
    /// The union of the matched grants' lists.
    Only(BTreeSet<String>),
}

impl Shown {
    /// Whether an attribute of this name may be shown.
    pub fn shows(&self, name: &str) -> bool {
        match self {
            Shown::Whole => true,
            Shown::Only(names) => names.contains(name),
        }
    }
}

/// What `entity` may show, or `None` when no grant matches it and it is not served.
///
/// No grants at all is a constraint set the evaluator did not build from policies (a delivery
/// judged by its stored subscription alone, a hand-built set); it narrows nothing here, and the
/// set's own `attrs` still do.
pub fn shown(entity: &Value, grants: &[GrantView]) -> Option<Shown> {
    if grants.is_empty() {
        return Some(Shown::Whole);
    }
    // ponytail: every grant's conditions are evaluated per entity, and an entity is expanded
    // once per grant with a q; cache the expansion per entity if a page of many grants is slow.
    let mut shown: Option<Shown> = None;
    for grant in grants.iter().filter(|grant| grant.matches(entity)) {
        shown = Some(match (shown, grant.attrs.is_empty()) {
            (_, true) | (Some(Shown::Whole), _) => Shown::Whole,
            (Some(Shown::Only(mut names)), false) => {
                names.extend(grant.attrs.iter().cloned());
                Shown::Only(names)
            }
            (None, false) => Shown::Only(grant.attrs.clone()),
        });
    }
    shown
}

/// The attributes the grants' own conditions read, which the broker has to answer with for the
/// conditions to be judged here: each `q`'s top-level names, and `location` under an area.
///
/// They are asked for beside the shown ones and stripped again by the projection unless a grant
/// the entity matches shows them.
pub fn condition_attributes(grants: &[GrantView]) -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    for grant in grants {
        if let Some(node) = grant.q.as_deref().and_then(|q| antares_ql::parse_q(q).ok()) {
            names.extend(node.attribute_paths().into_iter().map(str::to_owned));
        }
        if grant.geo_q.is_some() {
            names.insert("location".to_owned());
        }
    }
    names
}

/// The core `@context`, by which both the grant's terms and the answer's members are expanded,
/// so a term is compared with the same term.
fn core() -> &'static Context {
    static CORE: OnceLock<Context> = OnceLock::new();
    CORE.get_or_init(antares_jsonld::core_context)
}

/// Whether the grant's `q` holds for the entity, by the broker's own evaluator.
///
/// The answer is compacted and the evaluator reads the broker's expanded form, so the entity is
/// expanded first. A linked-entity term (`attr{…}`) cannot be followed without the store and
/// does not match.
fn q_holds(q: &str, entity: &Value) -> bool {
    let Ok(node) = antares_ql::parse_q(q) else {
        return false;
    };
    let Some(members) = entity.as_object() else {
        return false;
    };
    let options = ExpandOpts {
        // A temporal answer repeats instances of one attribute; a current one is unaffected.
        temporal: true,
        ..ExpandOpts::default()
    };
    let Ok(expanded) = expand_entity(members, core(), options) else {
        return false;
    };
    antares_ql::eval::eval_q(&node, &expanded, core(), &|_| None)
}
