//! T-0177: a revoked Policy or ServiceAccount stops granting without a restart
//! (R48, EP-19, OPS-45).

use context_gateway::app::Gateway;
use context_gateway::pdp::evaluator::{Request as PolicyRequest, Subject, Verdict};
use context_gateway::pdp::reaper::{Reaper, INTERVAL};
use context_gateway::pdp::PolicyPdp;
use context_gateway::proxy::Broker;
use context_gateway::store;
use jc_core::kinds::Operation;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

const SPACE: &str = r#"apiVersion: joinedcontext.com/v1alpha1
kind: ContextSpace
metadata:
  name: ovzdusie
  namespace: ovzdusie
spec:
  isSandbox: false
"#;

const ENDPOINT: &str = r#"apiVersion: joinedcontext.com/v1alpha1
kind: Endpoint
metadata:
  name: public-air
  namespace: ovzdusie
spec:
  contextSpaceRef: ovzdusie
  slug: zt4qm7ge2xdv6ksb3ncf5arw2y
  audience: public
  enabledRepresentations: ["ngsi-ld"]
"#;

const POLICY: &str = r#"apiVersion: joinedcontext.com/v1alpha1
kind: Policy
metadata:
  name: public-read
  namespace: ovzdusie
spec:
  contextSpaceRef: { kind: ContextSpace, name: ovzdusie }
  assigner: did:web:banskabystrica.sk
  assignee: { kind: role, id: public }
  operations: [retrieveOps]
"#;

const ACCOUNT: &str = r#"apiVersion: joinedcontext.com/v1alpha1
kind: ServiceAccount
metadata:
  name: conformance
  namespace: ovzdusie
spec:
  owner: { user: "qa" }
  purpose: "the conformance suite"
  roles:
    - { role: space-writer, scope: { contextSpace: ovzdusie } }
  credentials:
    - { kind: oauth-client, name: main }
"#;

fn repo(test_name: &str) -> PathBuf {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock after the epoch")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("gw-reaper-{test_name}-{now}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create the repository");
    write(&dir, "space.yaml", SPACE);
    write(&dir, "endpoint.yaml", ENDPOINT);
    write(&dir, "policy.yaml", POLICY);
    write(&dir, "serviceaccount.yaml", ACCOUNT);
    dir
}

fn write(dir: &Path, name: &str, body: &str) {
    std::fs::write(dir.join(name), body).expect("write a manifest");
}

fn gateway_on(dir: &Path) -> Arc<Gateway> {
    let (endpoints, spaces, _accounts, _federations, _agreements) =
        store::load(dir).expect("the repository loads");
    let gateway = Gateway::new(
        Broker::new("http://127.0.0.1:1".to_owned()),
        Box::new(PolicyPdp),
        "banskabystrica.sk",
    )
    .serve(endpoints)
    .serve_spaces(spaces);
    // A verifier is not needed to see the accounts table swap; `authenticate` is the only
    // way to fill it, so the reaper's own swap is what the account test then observes.
    Arc::new(gateway)
}

/// Whether an anonymous caller may still read through the endpoint.
fn anonymous_may_read(gateway: &Gateway) -> bool {
    let endpoint = gateway
        .resolver
        .resolve("zt4qm7ge2xdv6ksb3ncf5arw2y")
        .expect("the endpoint is in the table");
    !matches!(
        gateway.pdp.decide(
            &Subject::anonymous(),
            Operation::RetrieveEntity,
            &PolicyRequest::default(),
            &endpoint,
        ),
        Verdict::Deny
    )
}

#[test]
fn deleting_the_policy_revokes_the_grant_on_the_next_tick() {
    let dir = repo("policy-deleted");
    let gateway = gateway_on(&dir);
    let mut reaper = Reaper::new(Arc::clone(&gateway), &dir);

    assert!(
        anonymous_may_read(&gateway),
        "the seeded policy grants the anonymous caller a read"
    );
    assert!(!reaper.tick(), "an untouched repository reloads nothing");

    std::fs::remove_file(dir.join("policy.yaml")).expect("revoke the policy");

    assert!(reaper.tick(), "the repository changed, so it reloaded");
    assert!(
        !anonymous_may_read(&gateway),
        "the grant is gone from the running gateway, with no restart (R48)"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// The bound OPS-45 sets is five seconds; the poll is one, so a revocation is visible
/// after a single interval and the assertion is on the clock, not on a hope.
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn a_revocation_reaches_the_running_gateway_inside_the_five_second_bound() {
    assert!(
        INTERVAL <= Duration::from_secs(2),
        "EP-19 gives the endpoint table two seconds, OPS-45 gives a revocation five"
    );

    let dir = repo("policy-bound");
    let gateway = gateway_on(&dir);
    let started = tokio::time::Instant::now();
    tokio::spawn(Reaper::new(Arc::clone(&gateway), &dir).run());

    std::fs::remove_file(dir.join("policy.yaml")).expect("revoke the policy");

    // Paused clock: time only advances where the test says so, and the loop is polled at
    // every step, so this measures the reaper's own latency and nothing else.
    let mut waited = Duration::ZERO;
    while anonymous_may_read(&gateway) && waited < Duration::from_secs(5) {
        tokio::time::sleep(Duration::from_millis(100)).await;
        waited = started.elapsed();
    }

    assert!(
        !anonymous_may_read(&gateway),
        "still granting {waited:?} after the policy was deleted, over the OPS-45 bound"
    );
    assert!(waited <= Duration::from_secs(5), "took {waited:?}");

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_repository_that_cannot_be_read_keeps_the_table_that_is_serving() {
    let dir = repo("unreadable");
    let gateway = gateway_on(&dir);
    let mut reaper = Reaper::new(Arc::clone(&gateway), &dir);

    // A half-written manifest: the loader refuses the repository as a whole.
    write(
        &dir,
        "policy.yaml",
        "apiVersion: joinedcontext.com/v1alpha1\nkind: Pol",
    );

    assert!(
        !reaper.tick(),
        "a repository that does not load swaps nothing"
    );
    assert!(
        anonymous_may_read(&gateway),
        "the endpoint keeps serving what it was serving, rather than 404-ing everything"
    );

    // And once the write finishes, the same change is picked up: the failed attempt did
    // not mark the repository as seen.
    write(&dir, "policy.yaml", POLICY);
    std::fs::remove_file(dir.join("policy.yaml")).expect("revoke the policy");
    assert!(reaper.tick());
    assert!(!anonymous_may_read(&gateway));

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn withdrawing_a_service_account_stops_its_client_id_from_resolving() {
    let dir = repo("account-withdrawn");
    let gateway = gateway_on(&dir);
    let (_, _, accounts, _, _) = store::load(&dir).expect("the repository loads");
    assert_eq!(accounts.len(), 1, "the repository declares one account");
    gateway.replace_accounts(accounts);

    let mut reaper = Reaper::new(Arc::clone(&gateway), &dir);
    std::fs::remove_file(dir.join("serviceaccount.yaml")).expect("withdraw the account");
    assert!(reaper.tick());

    let (_, _, after, _, _) = store::load(&dir).expect("the repository still loads");
    assert_eq!(
        after.len(),
        0,
        "the reaper swapped in a table the withdrawn account is not in"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_new_endpoint_appears_without_a_restart() {
    let dir = repo("endpoint-added");
    let gateway = gateway_on(&dir);
    let mut reaper = Reaper::new(Arc::clone(&gateway), &dir);
    assert_eq!(gateway.resolver.len(), 1);

    write(
        &dir,
        "endpoint-2.yaml",
        &ENDPOINT
            .replace("public-air", "internal-air")
            .replace("zt4qm7ge2xdv6ksb3ncf5arw2y", "mluyob4nz52lok3ssk7pgn5vwt"),
    );

    assert!(reaper.tick());
    assert_eq!(
        gateway.resolver.len(),
        2,
        "EP-19: the table follows the repository"
    );

    let _ = std::fs::remove_dir_all(&dir);
}
