//! Sync-wave ordering of a repository (T-0126, CC-18, Architecture/06 section 3).
//!
//! The order comes from the kind alone. Research verdict P4 rejects a free-form
//! dependency DAG, so a plan is the repository bucketed into the six waves of
//! Architecture/06 section 3 and nothing more: no reference graph, no cycles, no
//! deadlock to detect. Within a wave the kinds converge in the documented order, which
//! puts a `DataModel` before the `Mapping` that targets it and a `ScopeDefinition`
//! before the `Policy` that names it.

use crate::loader::{Repository, ResourceId};
use std::collections::BTreeMap;

/// Wave 0: the repository roots everything else hangs from.
pub const WAVE_ROOTS: u8 = 0;
/// Wave 1: context spaces with their models, mappings and identities.
pub const WAVE_SPACES: u8 = 1;
/// Wave 2: access control, converged before anything is exposed.
pub const WAVE_ACCESS: u8 = 2;
/// Wave 3: the endpoints and gateway routes that expose a space (EP-01).
pub const WAVE_EXPOSURE: u8 = 3;
/// Wave 4: cross-space and data-space sharing on top of those endpoints (DS-07).
pub const WAVE_FEDERATION: u8 = 4;
/// Wave 5: the runtime that streams into the endpoints (MF-27).
pub const WAVE_RUNTIME: u8 = 5;

/// Every reconciled kind in convergence order, the table of Architecture/06 section 3.
/// A kind missing here is not reconciled: `Bundle` is an export artifact (CC-22).
const ORDER: &[(u8, &str)] = &[
    (WAVE_ROOTS, "Organization"),
    // Who may change configuration converges before anything they may change (PF-49), and the
    // group a binding names before the binding that names it (PF-62, PF-63).
    (WAVE_ROOTS, "Group"),
    (WAVE_ROOTS, "Role"),
    (WAVE_ROOTS, "RoleBinding"),
    (WAVE_ROOTS, "Project"),
    (WAVE_ROOTS, "DataSpaceParticipant"),
    (WAVE_ROOTS, "AgentProfile"),
    (WAVE_SPACES, "ContextSpace"),
    (WAVE_SPACES, "DataModel"),
    // A projection is checked against its model, so the model converges first (MP-01).
    (WAVE_SPACES, "ModelProjection"),
    (WAVE_SPACES, "Mapping"),
    (WAVE_SPACES, "ServiceAccount"),
    (WAVE_ACCESS, "ScopeDefinition"),
    (WAVE_ACCESS, "Policy"),
    // The catalogue connection and its API token resolve before an Endpoint's
    // `spec.publish.ckan` block can be acted on (EP-62, EP-67).
    (WAVE_EXPOSURE, "CkanInstance"),
    (WAVE_EXPOSURE, "Endpoint"),
    // A subscription watches a space through its endpoints, so both stand before it
    // converges (Architecture/06, Wave 4).
    (WAVE_FEDERATION, "Subscription"),
    (WAVE_FEDERATION, "SharedSpaceReference"),
    (WAVE_FEDERATION, "ContextSourceRegistration"),
    (WAVE_FEDERATION, "DataOffer"),
    (WAVE_FEDERATION, "DataAgreement"),
    // A DataSource converges before the pipelines that render their input from it.
    (WAVE_RUNTIME, "DataSource"),
    (WAVE_RUNTIME, "Pipeline"),
    (WAVE_RUNTIME, "App"),
    (WAVE_RUNTIME, "SyncSource"),
    (WAVE_RUNTIME, "Dashboard"),
    (WAVE_RUNTIME, "Layer"),
];

/// Position of a kind in the convergence order, `None` if it is not reconciled.
fn rank_of(kind: &str) -> Option<usize> {
    ORDER.iter().position(|(_, k)| *k == kind)
}

/// The sync wave a kind reconciles in, `None` if the reconciler never converges it
/// (CC-18).
pub fn wave_of(kind: &str) -> Option<u8> {
    rank_of(kind).map(|i| ORDER[i].0)
}

/// The reconciliation order of one repository, grouped into sync waves (CC-18).
///
/// Wave `n` is applied and health-checked before wave `n + 1` starts; resources inside
/// one wave may be applied concurrently.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    waves: Vec<(u8, Vec<ResourceId>)>,
}

impl Plan {
    /// The non-empty waves in ascending order, each with its wave number (CC-18).
    pub fn waves(&self) -> &[(u8, Vec<ResourceId>)] {
        &self.waves
    }

    /// Every planned resource, in the order the reconciler applies it (CC-18).
    pub fn iter(&self) -> impl Iterator<Item = &ResourceId> {
        self.waves.iter().flat_map(|(_, ids)| ids)
    }

    /// The number of resources the plan converges.
    pub fn len(&self) -> usize {
        self.waves.iter().map(|(_, ids)| ids.len()).sum()
    }

    /// Whether the plan converges nothing.
    pub fn is_empty(&self) -> bool {
        self.waves.is_empty()
    }
}

/// Orders a loaded repository into sync waves (CC-18, PF-07).
///
/// Deterministic: the repository is already indexed by [`ResourceId`], so two runs on
/// the same commit produce the identical plan (CC-27). A kind with no wave is left
/// out: it carries no live state to converge.
pub fn plan(repo: &Repository) -> Plan {
    let mut buckets: BTreeMap<(u8, usize), Vec<ResourceId>> = BTreeMap::new();
    for (id, _) in repo.iter() {
        if let Some(rank) = rank_of(&id.kind) {
            buckets
                .entry((ORDER[rank].0, rank))
                .or_default()
                .push(id.clone());
        }
    }

    let mut waves: Vec<(u8, Vec<ResourceId>)> = Vec::new();
    for ((wave, _), ids) in buckets {
        match waves.last_mut() {
            Some((last, resources)) if *last == wave => resources.extend(ids),
            _ => waves.push((wave, ids)),
        }
    }
    Plan { waves }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Kinds the reconciler never converges: a `Bundle` exists only for a download, a
    /// `Blueprint` is expanded at authoring time, a `UiSchema` has no counterpart outside
    /// the repository at all — the Portal reads the arrangement and draws the form itself —
    /// and an `Environment` is what the load renders *with*, merged over every manifest before
    /// anything is converged (CC-22, CC-25, UI-02, CC-73).
    const ARTIFACTS: &[&str] = &["Bundle", "Blueprint", "UiSchema", "Environment"];

    /// A kind added to the catalogue without a wave never reconciles, and the repository
    /// it appears in converges silently short of what Git says.
    ///
    /// The same invariant is asserted in `tests/waves_tests.rs`, but that target runs in
    /// `ci-full` and this one runs in the fast lane, which is where a new kind is added.
    #[test]
    fn every_catalogued_kind_has_a_wave_or_is_a_known_artifact() {
        for info in jc_core::registry::KINDS {
            match wave_of(info.kind) {
                Some(wave) => assert!(
                    wave <= WAVE_RUNTIME && !ARTIFACTS.contains(&info.kind),
                    "kind `{}` has wave {wave}",
                    info.kind
                ),
                None => assert!(
                    ARTIFACTS.contains(&info.kind),
                    "catalogued kind `{}` has no sync wave: add it to ORDER and to the \
                     wave table in Architecture/06 section 3, or list it as an artifact",
                    info.kind
                ),
            }
        }
    }

    /// The table is the order, so a kind listed twice would silently take the first wave.
    #[test]
    fn no_kind_is_listed_in_two_waves() {
        let mut seen = std::collections::BTreeSet::new();
        for (_, kind) in ORDER {
            assert!(seen.insert(*kind), "kind `{kind}` is listed twice");
        }
    }
}
