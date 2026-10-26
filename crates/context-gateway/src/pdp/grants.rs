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

/// The read `read` split into one read per grant, each with that grant's own types, id patterns
/// and filter (F-PLT2-004, ADR 006). The union read pairs one grant's types with another's `q`:
/// with A {X, `a>1`} and B {Y, `b>1`}, an X entity with `b>1` passed. A grant whose types the
/// read does not ask is left out; an entity is readable when one of these reads finds it.
pub fn per_grant(read: &super::evaluator::Constraints) -> Vec<super::evaluator::Constraints> {
    read.grants
        .iter()
        .filter_map(|grant| {
            let types: BTreeSet<String> = if grant.types.is_empty() {
                read.types.clone()
            } else if read.types.is_empty() {
                grant.types.clone()
            } else {
                grant.types.intersection(&read.types).cloned().collect()
            };
            if types.is_empty() && !(grant.types.is_empty() && read.types.is_empty()) {
                return None;
            }
            let mut one = read.clone();
            one.types = types;
            one.id_patterns = grant.id_patterns.clone().into();
            one.q = super::evaluator::grant_filter(grant.q.as_deref(), grant.scope_q.as_deref())
                .map(|filter| format!("({filter})"));
            one.grants = vec![grant.clone()];
            Some(one)
        })
        .collect()
}

#[cfg(test)]
mod per_grant_tests {
    use super::*;

    /// F-PLT2-004: each read carries one grant's types with that grant's filter alone.
    #[test]
    fn each_grant_reads_its_own_types_with_its_own_filter() {
        let grant = |ty: &str, q: &str| GrantView {
            types: BTreeSet::from([ty.to_owned()]),
            q: Some(q.to_owned()),
            ..GrantView::default()
        };
        let read = super::super::evaluator::Constraints {
            types: BTreeSet::from(["X".to_owned(), "Y".to_owned()]),
            q: Some("(((a>1))|((b>1)))".to_owned()),
            grants: vec![grant("X", "a>1"), grant("Y", "b>1")],
            ..Default::default()
        };
        let reads = per_grant(&read);
        assert_eq!(reads.len(), 2);
        assert_eq!(reads[0].types, BTreeSet::from(["X".to_owned()]));
        assert_eq!(reads[0].q.as_deref(), Some("((a>1))"));
        assert_eq!(reads[1].types, BTreeSet::from(["Y".to_owned()]));
        assert_eq!(reads[1].q.as_deref(), Some("((b>1))"));
        let only_x = super::super::evaluator::Constraints {
            types: BTreeSet::from(["X".to_owned()]),
            ..read
        };
        let reads = per_grant(&only_x);
        assert_eq!(reads.len(), 1, "Y's grant reads nothing of an X target");
        assert_eq!(reads[0].q.as_deref(), Some("((a>1))"));
    }
}
