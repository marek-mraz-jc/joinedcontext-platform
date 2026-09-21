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

/// MF-02, T-1482: an account, a role and a space of 63 characters each are each a valid name, so
/// the Policy compiled from them must be one too: at most 63 characters, a DNS-1123 label, the
/// same on every run, and different for a second binding that differs only past the cut.
#[test]
fn a_long_account_role_and_space_still_give_a_valid_policy_name() {
    let account = "a".repeat(63);
    let long = |role: &str, space: &str| {
        SERVICE_ACCOUNT
            .replace("vendorx-parking-push", &account)
            .replace("space-writer", role)
            .replace("contextSpace: ovzdusie", &format!("contextSpace: {space}"))
    };
    let role = format!("{}-x", "r".repeat(60));
    let space = format!("{}-1", "s".repeat(61));
    let name_of = |yaml: &str| {
        let policies = owner_policies(&manifest(yaml), "banskabystrica.sk");
        assert_eq!(policies.len(), 1, "one binding, one policy");
        policies[0].metadata.name.clone()
    };

    let name = name_of(&long(&role, &space));
    assert!(name.len() <= 63, "{name} is {} characters", name.len());
    jc_core::names::validate_dns1123_label(&name).unwrap_or_else(|err| panic!("{name}: {err}"));
    assert!(name.starts_with("sa-aaaa"), "{name}");
    assert_eq!(name, name_of(&long(&role, &space)), "stable across runs");

    let other_space = format!("{}-2", "s".repeat(61));
    assert_ne!(
        name,
        name_of(&long(&role, &other_space)),
        "two bindings that differ past the cut keep two names",
    );
}
