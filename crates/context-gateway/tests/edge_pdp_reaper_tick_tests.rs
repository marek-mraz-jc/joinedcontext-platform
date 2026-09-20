//! Edge cases of `pdp::reaper::Reaper::tick` (T-1933, EP-26, MP-02).
//!
//! Contract, in one sentence: one tick swaps every table the enforcement point decides with —
//! endpoints, spaces, accounts, federations, agreements — exactly when the repository's
//! fingerprint changed **and** the whole repository loaded, and in every other case it changes
//! nothing and the tables that are serving keep serving.
//!
//! Both halves are a security property. A swap that does not happen leaves a revoked grant live
//! past OPS-45's five seconds; a swap on a half-written repository serves a policy set nobody
//! authored. `reaper_tests.rs` has the revocation itself; these are the repository states a
//! ConfigMap volume, a git-sync sidecar and a half-finished write produce.

use context_gateway::app::Gateway;
use context_gateway::pdp::reaper::Reaper;
use context_gateway::pdp::PolicyPdp;
use context_gateway::proxy::Broker;
use context_gateway::store;
use std::path::{Path, PathBuf};
use std::sync::Arc;

const SPACE: &str = r#"apiVersion: joinedcontext.com/v1alpha1
kind: ContextSpace
metadata:
  name: ovzdusie
  namespace: ovzdusie
spec:
  isSandbox: false
"#;

const SLUG: &str = "zt4qm7ge2xdv6ksb3ncf5arw2y";

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

/// A repository of its own per test, because a tick is about what the file system says.
fn repo(test_name: &str) -> PathBuf {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock after the epoch")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("gw-reaper-tick-{test_name}-{now}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create the repository");
    write(&dir, "space.yaml", SPACE);
    write(&dir, "endpoint.yaml", ENDPOINT);
    write(&dir, "policy.yaml", POLICY);
    dir
}

fn write(dir: &Path, name: &str, body: &str) {
    if let Some(parent) = dir.join(name).parent() {
        std::fs::create_dir_all(parent).expect("create the directory of a manifest");
    }
    std::fs::write(dir.join(name), body).expect("write a manifest");
}

fn gateway_on(dir: &Path) -> Arc<Gateway> {
    let (endpoints, spaces, _accounts, _federations, _agreements) =
        store::load(dir).expect("the repository loads");
    Arc::new(
        Gateway::new(
            Broker::new("http://127.0.0.1:1".to_owned()),
            Box::new(PolicyPdp),
            "banskabystrica.sk",
        )
        .serve(endpoints)
        .serve_spaces(spaces),
    )
}

/// EP-19: the repository going away is an outage of the volume, not a reason to stop serving.
/// Answering 404 for every endpoint because a ConfigMap is mid-swap would be the worse outage.
#[test]
fn a_repository_that_disappears_keeps_the_table_that_is_serving() {
    let dir = repo("vanished");
    let gateway = gateway_on(&dir);
    let mut reaper = Reaper::new(Arc::clone(&gateway), &dir);

    std::fs::remove_dir_all(&dir).expect("the volume goes away");

    assert!(!reaper.tick(), "there is nothing to swap in");
    assert!(!reaper.tick(), "and saying so again changes nothing");
    assert!(
        gateway.resolver.resolve(SLUG).is_some(),
        "the endpoint still serves"
    );

    // And when it comes back with one endpoint fewer, that is what gets served.
    std::fs::create_dir_all(&dir).expect("the volume comes back");
    write(&dir, "space.yaml", SPACE);
    assert!(reaper.tick());
    assert!(gateway.resolver.resolve(SLUG).is_none());

    let _ = std::fs::remove_dir_all(&dir);
}

/// A ConfigMap volume keeps its versions in `..data` and friends; the loader skips them, so the
/// fingerprint does too, or every projected update would swap the tables twice.
#[test]
fn a_dot_directory_is_not_part_of_the_fingerprint() {
    let dir = repo("dot-dir");
    let gateway = gateway_on(&dir);
    let mut reaper = Reaper::new(Arc::clone(&gateway), &dir);

    write(&dir, "..2026_09_18_08_00_00.123/policy.yaml", POLICY);
    write(&dir, ".hidden.yaml", POLICY);

    assert!(
        !reaper.tick(),
        "a dot entry is not a change to the repository"
    );
    assert!(gateway.resolver.resolve(SLUG).is_some());

    let _ = std::fs::remove_dir_all(&dir);
}

/// A manifest in a subdirectory is a manifest: a repository laid out per space must not be a
/// repository whose changes are invisible.
#[test]
fn a_manifest_in_a_subdirectory_is_seen() {
    let dir = repo("subdirectory");
    let gateway = gateway_on(&dir);
    let mut reaper = Reaper::new(Arc::clone(&gateway), &dir);
    assert_eq!(gateway.resolver.len(), 1);

    write(
        &dir,
        "ovzdusie/endpoint-2.yaml",
        &ENDPOINT
            .replace("public-air", "internal-air")
            .replace(SLUG, "mluyob4nz52lok3ssk7pgn5vwt"),
    );

    assert!(reaper.tick());
    assert_eq!(gateway.resolver.len(), 2);
    assert!(gateway
        .resolver
        .resolve("mluyob4nz52lok3ssk7pgn5vwt")
        .is_some());

    let _ = std::fs::remove_dir_all(&dir);
}

/// A file the loader ignores still changes the fingerprint, so it costs one reload and then the
/// repository settles: a tick that never settles would reload the tables every second.
#[test]
fn a_file_the_loader_ignores_costs_one_reload_and_then_settles() {
    let dir = repo("stray-file");
    let gateway = gateway_on(&dir);
    let mut reaper = Reaper::new(Arc::clone(&gateway), &dir);

    write(&dir, "README.md", "# the repository\n");

    assert!(reaper.tick(), "the directory changed");
    assert!(!reaper.tick(), "and it has not changed since");
    assert!(!reaper.tick());
    assert!(gateway.resolver.resolve(SLUG).is_some());

    let _ = std::fs::remove_dir_all(&dir);
}

/// A repository that does not load leaves the fingerprint unstored, so the same change is tried
/// again on every tick until it is whole — a half-written manifest does not have to be touched
/// twice to be picked up.
#[test]
fn a_failed_load_is_retried_on_every_tick_until_it_is_whole() {
    let dir = repo("half-written");
    let gateway = gateway_on(&dir);
    let mut reaper = Reaper::new(Arc::clone(&gateway), &dir);

    write(
        &dir,
        "policy.yaml",
        "apiVersion: joinedcontext.com/v1alpha1\nkind: Pol",
    );
    for attempt in 0..3 {
        assert!(
            !reaper.tick(),
            "attempt {attempt} must not swap anything in"
        );
        assert!(
            gateway.resolver.resolve(SLUG).is_some(),
            "attempt {attempt}"
        );
    }

    // The writer finishes, and nothing else has to happen for the change to arrive.
    write(&dir, "policy.yaml", POLICY);
    write(
        &dir,
        "endpoint.yaml",
        &ENDPOINT.replace(SLUG, "mluyob4nz52lok3ssk7pgn5vwt"),
    );
    assert!(reaper.tick());
    assert!(gateway
        .resolver
        .resolve("mluyob4nz52lok3ssk7pgn5vwt")
        .is_some());

    let _ = std::fs::remove_dir_all(&dir);
}

/// An unchanged repository is not reloaded, however often it is asked: the tables the PDP is
/// deciding with are not replaced under a request for nothing.
#[test]
fn an_unchanged_repository_is_never_swapped() {
    let dir = repo("unchanged");
    let gateway = gateway_on(&dir);
    let mut reaper = Reaper::new(Arc::clone(&gateway), &dir);

    for _ in 0..5 {
        assert!(!reaper.tick());
    }
    assert_eq!(gateway.resolver.len(), 1);

    let _ = std::fs::remove_dir_all(&dir);
}

/// A change and a change back are both changes: the reaper converges on what the repository says
/// now, not on what it has ever said.
#[test]
fn a_change_and_a_change_back_both_arrive() {
    let dir = repo("there-and-back");
    let gateway = gateway_on(&dir);
    let mut reaper = Reaper::new(Arc::clone(&gateway), &dir);

    std::fs::remove_file(dir.join("endpoint.yaml")).expect("withdraw the endpoint");
    assert!(reaper.tick());
    assert!(gateway.resolver.resolve(SLUG).is_none(), "withdrawn");

    write(&dir, "endpoint.yaml", ENDPOINT);
    assert!(reaper.tick());
    assert!(gateway.resolver.resolve(SLUG).is_some(), "published again");

    let _ = std::fs::remove_dir_all(&dir);
}

/// The fingerprint is paths, sizes and modification times. A rewrite that changes none of them is
/// not seen — written down here because it is a documented limit of the cheap check, and because
/// a `touch` is all it takes to be seen. The reconciler writes through a new ConfigMap version,
/// so it never produces this shape.
#[test]
fn a_rewrite_that_keeps_the_size_and_the_time_is_not_seen_until_it_is_touched() {
    let dir = repo("same-size");
    let gateway = gateway_on(&dir);
    let mut reaper = Reaper::new(Arc::clone(&gateway), &dir);

    let path = dir.join("endpoint.yaml");
    let before = std::fs::metadata(&path).expect("the manifest is there");
    let other_slug = "mluyob4nz52lok3ssk7pgn5vwt";
    assert_eq!(
        SLUG.len(),
        other_slug.len(),
        "the rewrite is byte for byte the same size"
    );
    std::fs::write(&path, ENDPOINT.replace(SLUG, other_slug)).expect("rewrite the manifest");
    std::fs::File::options()
        .write(true)
        .open(&path)
        .expect("open the manifest")
        .set_times(
            std::fs::FileTimes::new()
                .set_accessed(before.accessed().expect("an access time"))
                .set_modified(before.modified().expect("a modification time")),
        )
        .expect("put the modification time back");

    assert!(
        !reaper.tick(),
        "same path, same size, same mtime: nothing to see"
    );
    assert!(
        gateway.resolver.resolve(SLUG).is_some(),
        "the old slug is still served"
    );

    // A touch is all it takes.
    std::fs::File::options()
        .write(true)
        .open(&path)
        .expect("open the manifest")
        .set_times(std::fs::FileTimes::new().set_modified(std::time::SystemTime::now()))
        .expect("touch the manifest");

    assert!(reaper.tick());
    assert!(gateway.resolver.resolve(other_slug).is_some());

    let _ = std::fs::remove_dir_all(&dir);
}

/// R48: one tick swaps every table together. A grant withdrawn from the endpoint table and left
/// standing in the space table would still be honoured on the `/cs` surface.
#[test]
fn every_table_is_swapped_in_the_same_tick() {
    let dir = repo("all-tables");
    let gateway = gateway_on(&dir);
    let mut reaper = Reaper::new(Arc::clone(&gateway), &dir);
    assert_eq!(gateway.resolver.len(), 1);
    assert_eq!(gateway.resolver.spaces().len(), 1);

    std::fs::remove_file(dir.join("endpoint.yaml")).expect("withdraw the endpoint");
    std::fs::remove_file(dir.join("space.yaml")).expect("withdraw the space");

    assert!(reaper.tick());
    assert_eq!(gateway.resolver.len(), 0, "the endpoint table followed");
    assert_eq!(
        gateway.resolver.spaces().len(),
        0,
        "and the space table with it"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// A previews directory that cannot be read is not a reason to stop following the repository: a
/// workspace preview is an extra, a revocation is not.
#[test]
fn an_unreadable_previews_directory_does_not_stop_the_repository() {
    let dir = repo("previews-missing");
    let previews = dir.with_extension("previews-that-are-not-there");
    let gateway = gateway_on(&dir);
    let mut reaper = Reaper::new(Arc::clone(&gateway), &dir).with_previews(&previews);

    std::fs::remove_file(dir.join("endpoint.yaml")).expect("withdraw the endpoint");

    assert!(
        reaper.tick(),
        "the repository changed, previews or no previews"
    );
    assert!(gateway.resolver.resolve(SLUG).is_none());

    let _ = std::fs::remove_dir_all(&dir);
}

/// And a change under a previews directory is a change: a preview that was rendered again has to
/// reach the table it is served from.
#[test]
fn a_change_under_the_previews_directory_is_a_change() {
    let dir = repo("previews-change");
    let previews = dir.with_extension("previews");
    std::fs::create_dir_all(&previews).expect("create the previews directory");
    let gateway = gateway_on(&dir);
    let mut reaper = Reaper::new(Arc::clone(&gateway), &dir).with_previews(&previews);

    assert!(!reaper.tick(), "nothing has changed yet");
    write(&previews, "workspace-1/endpoint.yaml", ENDPOINT);

    assert!(reaper.tick(), "the previews directory changed");
    assert!(!reaper.tick());

    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_dir_all(&previews);
}
