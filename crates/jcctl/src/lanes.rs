//! Risk lanes of a change proposal (T-0143, CC-63, CC-64, CC-70, AG-10).
//!
//! Every proposed change lands in one of three lanes: *green* is auto-approved by the
//! policy bot, *yellow* needs one domain approver, *red* needs the full chain
//! (Architecture/06 section 4). The lane is derived here, from what the change actually
//! does — never from what the author says about it, which is why nothing in this module
//! reads a lane out of the manifest: a caller who could declare `green` on a deletion
//! would have found the way around the approval chain (CC-63, AG-11).
//!
//! A proposal takes the lane of its strictest change, so one deletion buried in twenty
//! additions still needs the red chain.

use crate::commands::plan::{Action, ChangeSet, ResourceChange};
use serde_json::{json, Value};

/// The approval lane of a change (CC-63).
///
/// Ordered: `Green < Yellow < Red`, so the lane of a set is the maximum of its parts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Lane {
    /// Auto-approved by the policy bot; still a committed, revertible repository path (CC-64).
    Green,
    /// One domain approver (CC-34).
    Yellow,
    /// The full approval chain: cross-domain, public exposure, federation, deletion (CC-63).
    Red,
}

impl Lane {
    /// The wire name of the lane, as the Change envelope and the Portal carry it.
    pub const fn as_str(self) -> &'static str {
        match self {
            Lane::Green => "green",
            Lane::Yellow => "yellow",
            Lane::Red => "red",
        }
    }
}

/// Kinds whose change is a federation edge or an identity decision: red whatever it does
/// (CC-63, Architecture/06 section 4). `Organization` carries the settings tiers, and lane
/// policy itself is always red (CC-70).
const ALWAYS_RED: &[&str] = &[
    "Organization",
    "Policy",
    "ScopeDefinition",
    "ServiceAccount",
    "SharedSpaceReference",
    "DataOffer",
    "DataAgreement",
    "DataSpaceParticipant",
    "ContextSourceRegistration",
    "Role",
    "RoleBinding",
];

/// Why a change is in its lane, in one clause, for the proposal body and the audit trail.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaneVerdict {
    /// The lane the change has to go through.
    pub lane: Lane,
    /// The rule that put it there.
    pub reason: &'static str,
}

/// The lane of one resource change (CC-63).
pub fn lane_of(change: &ResourceChange) -> LaneVerdict {
    // A plan lists what it would not touch as well, and a resource nobody changes is not a
    // change: deciding this first is what keeps one untouched Policy from turning a
    // sandbox proposal red.
    if change.action == Action::Unchanged {
        return LaneVerdict {
            lane: Lane::Green,
            reason: "nothing changes",
        };
    }
    if change.action == Action::Delete {
        // CC-63 says "any deletion", and CC-19 makes deletion explicit anyway.
        return LaneVerdict {
            lane: Lane::Red,
            reason: "a deletion is always red (CC-63, CC-19)",
        };
    }
    if ALWAYS_RED.contains(&change.id.kind.as_str()) {
        return LaneVerdict {
            lane: Lane::Red,
            reason: "identity, access, federation and lane policy are red (CC-63, CC-70)",
        };
    }
    if change.id.kind == "Endpoint" && touches_public_audience(change) {
        return LaneVerdict {
            lane: Lane::Red,
            reason: "public exposure of an endpoint is red (CC-63)",
        };
    }
    if change.id.kind == "AgentProfile" {
        return lane_of_agent_profile(change);
    }
    if let Some(reason) = green_reason(change) {
        return LaneVerdict {
            lane: Lane::Green,
            reason,
        };
    }
    LaneVerdict {
        lane: Lane::Yellow,
        reason: "an additive change to published configuration needs one domain approver (CC-63)",
    }
}

fn lane_of_agent_profile(change: &ResourceChange) -> LaneVerdict {
    if change.action == Action::Update {
        let touches_egress = change
            .diff
            .iter()
            .any(|d| d.path.starts_with("spec.egress"));
        let touches_limits = change
            .diff
            .iter()
            .any(|d| d.path.starts_with("spec.limits"));
        if touches_egress || touches_limits {
            return LaneVerdict {
                lane: Lane::Red,
                reason:
                    "raising limits or widening egress of an agent profile is red (CC-70, AG-47)",
            };
        }
    }
    LaneVerdict {
        lane: Lane::Yellow,
        reason: "agent profile changes require domain approval (CC-63, AG-47)",
    }
}

/// Whether the change publishes an endpoint, or moves its audience at all: both the
/// declared value and the member the diff touches count, so `public → internal` is red too.
fn touches_public_audience(change: &ResourceChange) -> bool {
    let declared_public = change
        .declared
        .as_ref()
        .and_then(|manifest| manifest.spec.get("audience"))
        .and_then(Value::as_str)
        == Some("public");
    declared_public || change.diff.iter().any(|d| d.path == "spec.audience")
}

/// The self-service cases (CC-63 green column, CC-67): an ephemeral sandbox space, and a
/// dashboard nobody outside the project can see.
fn green_reason(change: &ResourceChange) -> Option<&'static str> {
    let spec = change.declared.as_ref().map(|manifest| &manifest.spec)?;
    match change.id.kind.as_str() {
        "ContextSpace" if spec.get("isSandbox").and_then(Value::as_bool) == Some(true) => {
            Some("an unmanaged sandbox space is self-service (CC-67)")
        }
        "Dashboard" => match spec.get("visibility").and_then(Value::as_str) {
            // The default of an undeclared visibility is the private one, so an absent
            // member is green as well; publishing one is what leaves the lane.
            None | Some("private") | Some("project") => {
                Some("a dashboard nobody outside the project sees is self-service (CC-63)")
            }
            _ => None,
        },
        _ => None,
    }
}

/// The lane of a whole proposal: the strictest lane of its changes, and red as soon as it
/// spans two namespaces, which is what "cross-domain" means in a repository (CC-63).
pub fn lane_of_changeset(changes: &ChangeSet) -> LaneVerdict {
    let mut verdict = LaneVerdict {
        lane: Lane::Green,
        reason: "nothing changes",
    };
    let mut namespaces = std::collections::BTreeSet::new();
    for change in changes.waves.iter().flat_map(|(_, list)| list) {
        if change.action != Action::Unchanged {
            namespaces.insert(change.id.namespace.clone());
        }
        let candidate = lane_of(change);
        if candidate.lane > verdict.lane {
            verdict = candidate;
        }
    }
    if namespaces.len() > 1 && verdict.lane < Lane::Red {
        return LaneVerdict {
            lane: Lane::Red,
            reason: "a proposal that crosses domains is red (CC-63)",
        };
    }
    verdict
}

/// The `kind: Change` envelope of a proposal, as API/01 section 3 documents it.
///
/// `name` is the proposal's own name (`chg-…`), `namespace` the project it belongs to, and
/// `merge_request` the forge URL once the branch is pushed. The phase follows the lane:
/// a green proposal is auto-approved and goes straight to `Deploying` (CC-63, CC-65),
/// everything else waits for a human in `PendingApproval` (CC-34).
pub fn change_envelope(
    changes: &ChangeSet,
    name: &str,
    namespace: &str,
    merge_request: Option<&str>,
) -> Value {
    let verdict = lane_of_changeset(changes);
    let (mut create, mut update, mut delete) = (0, 0, 0);
    for change in changes.waves.iter().flat_map(|(_, list)| list) {
        match change.action {
            Action::Create => create += 1,
            Action::Update => update += 1,
            Action::Delete => delete += 1,
            Action::Unchanged => {}
        }
    }
    let mut status = json!({
        "lane": verdict.lane.as_str(),
        "reason": verdict.reason,
        "plan": { "create": create, "update": update, "delete": delete },
        "phase": if verdict.lane == Lane::Green { "Deploying" } else { "PendingApproval" },
    });
    if let Some(url) = merge_request {
        status["mergeRequest"] = json!(url);
    }
    json!({
        "apiVersion": jc_core::API_VERSION,
        "kind": "Change",
        "metadata": { "name": name, "namespace": namespace },
        "status": status,
    })
}
