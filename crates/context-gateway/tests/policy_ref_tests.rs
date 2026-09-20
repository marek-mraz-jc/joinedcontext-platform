//! An Endpoint with `spec.policyRef` evaluates that one Policy of its space and no other, so
//! four endpoints over one space can each publish a different slice of it (EP-14, GW8).

use context_gateway::store;
use jc_core::kinds::{Operation, Principal, PrincipalKind};
use std::path::{Path, PathBuf};

const SPACE: &str = r#"apiVersion: joinedcontext.com/v1alpha1
kind: ContextSpace
metadata:
  name: helsinki
  namespace: helsinki
spec:
  isSandbox: false
  urnSegment: helsinki
"#;

const ALL: &str = r#"apiVersion: joinedcontext.com/v1alpha1
kind: Endpoint
metadata:
  name: helsinki-all
  namespace: helsinki
spec:
  contextSpaceRef: helsinki
  slug: aaaaaaaaaaaaaaaaaaaaaaaaaa
  audience: public
  enabledRepresentations: ["ngsi-ld"]
"#;

const BIKES: &str = r#"apiVersion: joinedcontext.com/v1alpha1
kind: Endpoint
metadata:
  name: helsinki-bikes
  namespace: helsinki
spec:
  contextSpaceRef: helsinki
  slug: bbbbbbbbbbbbbbbbbbbbbbbbbb
  audience: public
  policyRef: urn:ngsi-ld:Policy:hel.fi:helsinki:public-bikes
  enabledRepresentations: ["ngsi-ld"]
"#;

const DANGLING: &str = r#"apiVersion: joinedcontext.com/v1alpha1
kind: Endpoint
metadata:
  name: helsinki-nothing
  namespace: helsinki
spec:
  contextSpaceRef: helsinki
  slug: cccccccccccccccccccccccccc
  audience: public
  policyRef: urn:ngsi-ld:Policy:hel.fi:elsewhere:public-bikes
  enabledRepresentations: ["ngsi-ld"]
"#;

fn policy(name: &str, entity_type: &str) -> String {
    format!(
        r#"apiVersion: joinedcontext.com/v1alpha1
kind: Policy
metadata:
  name: {name}
  namespace: helsinki
spec:
  contextSpaceRef: {{ kind: ContextSpace, name: helsinki }}
  assigner: did:web:hel.fi
  assignee: {{ kind: role, id: public }}
  operations: [retrieveOps]
  information:
    - entities:
        - type: {entity_type}
"#
    )
}

const WRITER: &str = r#"apiVersion: joinedcontext.com/v1alpha1
kind: Policy
metadata:
  name: pipelines-write
  namespace: helsinki
spec:
  contextSpaceRef: { kind: ContextSpace, name: helsinki }
  assigner: did:web:hel.fi
  assignee: { kind: serviceAccount, id: pipelines }
  operations: [upsertBatch]
"#;

fn repo(test_name: &str) -> PathBuf {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock after the epoch")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("gw-policy-ref-{test_name}-{now}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create the repository");
    write(&dir, "space.yaml", SPACE);
    write(&dir, "all.yaml", ALL);
    write(&dir, "bikes.yaml", BIKES);
    write(&dir, "nothing.yaml", DANGLING);
    write(
        &dir,
        "public-bikes.yaml",
        &policy("public-bikes", "BikeHireDockingStation"),
    );
    write(
        &dir,
        "public-events.yaml",
        &policy("public-events", "Event"),
    );
    write(&dir, "pipelines-write.yaml", WRITER);
    dir
}

fn write(dir: &Path, name: &str, body: &str) {
    std::fs::write(dir.join(name), body).expect("write a manifest");
}

fn types_of(policies: &[jc_core::kinds::PolicySpec]) -> Vec<String> {
    let mut types: Vec<String> = policies
        .iter()
        .flat_map(|policy| policy.information.iter())
        .flat_map(|info| info.entities.iter())
        .map(|entity| entity.entity_type.clone())
        .collect();
    types.sort();
    types
}

#[test]
fn an_endpoint_without_policy_ref_evaluates_every_policy_of_its_space() {
    let dir = repo("all");
    let (endpoints, spaces, ..) = store::load(&dir).expect("the repository loads");
    let all = endpoints
        .iter()
        .find(|endpoint| endpoint.slug == "aaaaaaaaaaaaaaaaaaaaaaaaaa")
        .expect("the unbound endpoint");
    assert_eq!(all.policies.len(), 3, "both public slices and the writer");
    assert_eq!(types_of(&all.policies), ["BikeHireDockingStation", "Event"]);
    assert!(all.policies.iter().any(|policy| {
        policy.assignee
            == Principal {
                kind: PrincipalKind::ServiceAccount,
                id: "pipelines".to_owned(),
            }
            && policy.operations.iter().any(|op| {
                matches!(
                    op,
                    jc_core::kinds::OperationRef::Single(Operation::UpsertBatch)
                )
            })
    }));
    // The canonical space surface is the whole space too.
    assert_eq!(spaces[0].endpoint.policies.len(), 3);
}

#[test]
fn an_endpoint_with_policy_ref_evaluates_that_policy_alone() {
    let dir = repo("bikes");
    let (endpoints, ..) = store::load(&dir).expect("the repository loads");
    let bikes = endpoints
        .iter()
        .find(|endpoint| endpoint.slug == "bbbbbbbbbbbbbbbbbbbbbbbbbb")
        .expect("the bound endpoint");
    assert_eq!(bikes.policies.len(), 1);
    assert_eq!(types_of(&bikes.policies), ["BikeHireDockingStation"]);
}

#[test]
fn a_policy_ref_into_another_space_binds_nothing_rather_than_everything() {
    let dir = repo("dangling");
    let (endpoints, ..) = store::load(&dir).expect("the repository loads");
    let nothing = endpoints
        .iter()
        .find(|endpoint| endpoint.slug == "cccccccccccccccccccccccccc")
        .expect("the dangling endpoint is still in the table");
    assert!(
        nothing.policies.is_empty(),
        "{:?}",
        types_of(&nothing.policies)
    );
}
