//! Each grant on its own (T-3530, EP-16): its types, id patterns, `q`, `scopeQ`, area and
//! attributes, as the evaluator builds the constraint set from them.
//!
//! The broker's view decides which entities and attributes a caller is served (ADR-N-050); the
//! gateway no longer judges an answer entity by entity (T-4755). What stays here is the attributes
//! a grant's own conditions read, which a write probe asks the broker for.

use jc_core::kinds::PolicySpec;
use std::collections::BTreeSet;

/// One grant as an entity is judged against it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GrantView {
    /// The types it names; empty is every type.
    pub types: BTreeSet<String>,
    /// What the space's models make of those names, filled where the decision knows the models
    /// (T-3473): one grant's IRIs, never another's.
    pub type_iris: BTreeSet<String>,
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
            type_iris: BTreeSet::new(),
            id_patterns: super::evaluator::id_patterns(policy),
            q: policy.q.clone(),
            scope_q: policy.scope_q.clone(),
            geo_q: policy.geo_q.clone(),
            attrs: super::evaluator::granted_attrs(&policy.information),
        }
    }
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
