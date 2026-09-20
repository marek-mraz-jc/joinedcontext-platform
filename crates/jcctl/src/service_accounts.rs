//! The owner Policy a ServiceAccount cannot exist without (T-0141, CC-61, PF-34, PF-35).
//!
//! A service account is a principal of the same authorization model as a person, so its
//! roles are not a second grammar: each role binding compiles to one `Policy` manifest
//! that the PDP evaluates like any other. Generating them here is what makes CC-61 hold
//! by construction — no writer can reach live state without a policy governing it,
//! because the reconciler creates both in the same run.
//!
//! Only a role scoped to a context space compiles: a project- or organization-wide grant
//! has no `contextSpaceRef` to point at, and is left to the space policies of that
//! project rather than silently widened into one.

use crate::loader::RawManifest;
use jc_core::annotations::GENERATED_BY;
use jc_core::kinds::ServiceAccountSpec;
use jc_core::API_VERSION;
use serde_json::{json, Value};

/// The tool the generated manifests name as their origin (MF-08).
pub const GENERATOR: &str = "jcctl/service-accounts";

/// The owner policies of one ServiceAccount manifest, in role order (CC-61).
///
/// `org_domain` is the organization's domain, which becomes the policy assigner
/// (`did:web:{domain}`). A manifest that is not a ServiceAccount, or whose spec does not
/// parse, yields nothing: `validate` is what reports a broken manifest, not this.
pub fn owner_policies(manifest: &RawManifest, org_domain: &str) -> Vec<RawManifest> {
    if manifest.kind != "ServiceAccount" {
        return Vec::new();
    }
    let Ok(spec) = serde_json::from_value::<ServiceAccountSpec>(manifest.spec.clone()) else {
        return Vec::new();
    };

    spec.roles
        .iter()
        .filter_map(|binding| {
            let space = binding.scope.context_space.as_deref()?;
            let policy = policy_spec(&manifest.metadata.name, space, org_domain, binding);
            Some(RawManifest {
                api_version: API_VERSION.to_owned(),
                kind: "Policy".to_owned(),
                metadata: crate::loader::RawMetadata {
                    name: policy_name(&manifest.metadata.name, &binding.role, space),
                    namespace: manifest.metadata.namespace.clone(),
                    rest: annotations(),
                },
                spec: policy,
                status: None,
            })
        })
        .collect()
}

/// `sa-{account}-{role}-{space}`: stable across runs, so a second `apply` is a no-op, and
/// unique per binding, so two roles of one account do not collide.
fn policy_name(account: &str, role: &str, space: &str) -> String {
    format!("sa-{account}-{role}-{space}")
}

fn annotations() -> serde_json::Map<String, Value> {
    let mut rest = serde_json::Map::new();
    rest.insert("annotations".to_owned(), json!({ GENERATED_BY: GENERATOR }));
    rest
}

fn policy_spec(
    account: &str,
    space: &str,
    org_domain: &str,
    binding: &jc_core::kinds::RoleBinding,
) -> Value {
    let mut spec = json!({
        "contextSpaceRef": space,
        "assigner": format!("did:web:{org_domain}"),
        "assignee": { "kind": "serviceAccount", "id": account },
        "operations": binding.operations,
    });

    // The types whitelist becomes the policy's entity selector; without one the grant
    // covers every type in the space, which is what an unrestricted role binding means.
    if !binding.types.is_empty() {
        spec["information"] = json!([{
            "entities": binding
                .types
                .iter()
                .map(|entity_type| json!({ "type": entity_type }))
                .collect::<Vec<_>>(),
        }]);
    }
    spec
}
