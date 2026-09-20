mod common;

use common::*;
use jcctl::commands::apply::{run, Options};
use jcctl::commands::plan::{compute, Action};
use jcctl::platform::{InMemory, Platform};
use jcctl::service_accounts::owner_policies;
use serde_json::json;

const SERVICE_ACCOUNT: &str = r#"apiVersion: joinedcontext.com/v1alpha1
kind: ServiceAccount
metadata:
  name: vendorx-parking-push
  namespace: ovzdusie
spec:
  owner: { user: "jana.k@bb" }
  purpose: "VendorX cloud pushes ParkingSpot updates every 10 s"
  roles:
    - role: space-writer
      scope: { contextSpace: ovzdusie }
      types: [ParkingSpot]
      operations: [createEntity, updateAttrs]
  credentials:
    - kind: oauth-client
      name: main
"#;

fn with_service_account(test_name: &str) -> std::path::PathBuf {
    let dir = demo_repo(test_name);
    write(
        &dir,
        "projects/ovzdusie/access/serviceaccounts/vendorx-parking-push.yaml",
        SERVICE_ACCOUNT,
    );
    dir
}

/// CC-61: the grant is not a second grammar. A role binding compiles to a Policy the PDP
/// evaluates like any other, with the account as the assignee.
#[test]
fn a_role_binding_compiles_to_one_policy_manifest() {
    let policies = owner_policies(&manifest(SERVICE_ACCOUNT), "banskabystrica.sk");

    assert_eq!(policies.len(), 1);
    let policy = &policies[0];
    assert_eq!(policy.kind, "Policy");
    assert_eq!(
        policy.metadata.name,
        "sa-vendorx-parking-push-space-writer-ovzdusie"
    );
    assert_eq!(policy.metadata.namespace.as_deref(), Some("ovzdusie"));
    assert_eq!(policy.spec["contextSpaceRef"], json!("ovzdusie"));
    assert_eq!(policy.spec["assigner"], json!("did:web:banskabystrica.sk"));
    assert_eq!(
        policy.spec["assignee"],
        json!({ "kind": "serviceAccount", "id": "vendorx-parking-push" })
    );
    assert_eq!(
        policy.spec["operations"],
        json!(["createEntity", "updateAttrs"])
    );
    assert_eq!(
        policy.spec["information"],
        json!([{ "entities": [{ "type": "ParkingSpot" }] }])
    );
    assert_eq!(
        policy.metadata.rest["annotations"]["joinedcontext.com/generated-by"],
        json!("jcctl/service-accounts")
    );
}

/// The policy the reconciler generates has to satisfy the same rules a hand-written one
/// does, or it would be rejected the moment somebody exported and re-imported it.
#[test]
fn the_generated_policy_is_a_valid_policy_manifest() {
    let policies = owner_policies(&manifest(SERVICE_ACCOUNT), "banskabystrica.sk");
    let yaml = serde_norway::to_string(&policies[0]).expect("the generated policy serializes");

    jc_core::registry::validate_yaml("Policy", &yaml)
        .expect("Policy is a catalogued kind")
        .expect("the generated policy validates");
}

/// The whole point of CC-61: applying a ServiceAccount provisions its governing policy in
/// the same run, without anybody writing it by hand.
#[test]
fn applying_a_service_account_provisions_its_owner_policy() {
    let dir = with_service_account("sa-apply");
    let mut platform = InMemory::new();

    let report = run(&load(&dir), &mut platform, Options::default()).expect("apply");
    assert!(report.is_successful());

    let policies = platform
        .list("ovzdusie", "policies")
        .expect("the platform lists policies");
    assert_eq!(policies.len(), 1);
    assert_eq!(
        policies[0].metadata.name,
        "sa-vendorx-parking-push-space-writer-ovzdusie"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// A generated resource must converge like any other, or the second run would keep
/// rewriting it and drift detection would never be quiet (CC-18).
#[test]
fn the_generated_policy_converges_and_stays_converged() {
    let dir = with_service_account("sa-idempotent");
    let repo = load(&dir);
    let mut platform = InMemory::new();

    let first = compute(&repo, &platform).expect("plan");
    assert_eq!(
        first.count(Action::Create),
        6,
        "four manifests plus the account and its policy"
    );

    run(&repo, &mut platform, Options::default()).expect("first apply");
    let second = run(&repo, &mut platform, Options::default()).expect("second apply");

    assert_eq!(second.applied(), 0);
    assert!(compute(&repo, &platform).expect("plan").is_clean());

    let _ = std::fs::remove_dir_all(&dir);
}

/// A role that is not scoped to a context space has no `contextSpaceRef` to point at. It
/// is left to that project's own policies rather than widened into a space grant.
#[test]
fn a_project_wide_role_generates_no_space_policy() {
    let project_wide = SERVICE_ACCOUNT.replace(
        "      scope: { contextSpace: ovzdusie }",
        "      scope: { project: ovzdusie }",
    );

    assert!(owner_policies(&manifest(&project_wide), "banskabystrica.sk").is_empty());
}

/// A hand-written policy of the same identity is the one that counts: generation must not
/// overwrite what somebody deliberately committed.
#[test]
fn a_hand_written_policy_of_the_same_name_wins() {
    let dir = with_service_account("sa-override");
    write(
        &dir,
        "projects/ovzdusie/spaces/ovzdusie/policies/sa-vendorx-parking-push-space-writer-ovzdusie.yaml",
        r#"apiVersion: joinedcontext.com/v1alpha1
kind: Policy
metadata:
  name: sa-vendorx-parking-push-space-writer-ovzdusie
  namespace: ovzdusie
spec:
  contextSpaceRef: ovzdusie
  assigner: did:web:banskabystrica.sk
  assignee: { kind: serviceAccount, id: vendorx-parking-push }
  operations: [retrieveEntity]
"#,
    );

    let mut platform = InMemory::new();
    run(&load(&dir), &mut platform, Options::default()).expect("apply");

    let policies = platform.list("ovzdusie", "policies").expect("policies");
    assert_eq!(policies.len(), 1);
    assert_eq!(
        policies[0].spec["operations"],
        json!(["retrieveEntity"]),
        "the committed manifest wins over the generated one"
    );

    let _ = std::fs::remove_dir_all(&dir);
}
