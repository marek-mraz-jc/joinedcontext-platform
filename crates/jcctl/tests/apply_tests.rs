mod common;

use common::*;
use jcctl::commands::apply::{run, Options, Outcome};
use jcctl::commands::plan::{compute, Action};
use jcctl::loader::{RawManifest, ResourceId};
use jcctl::platform::{InMemory, Platform, PlatformError};

/// TS-21 and CC-18: the second run from the same commit must find nothing to do.
#[test]
fn applying_twice_leaves_an_empty_plan() {
    let dir = demo_repo("apply-idempotent");
    let repo = load(&dir);
    let mut platform = InMemory::new();

    let first = run(&repo, &mut platform, Options::default()).expect("first apply");
    assert!(first.is_successful());
    assert_eq!(first.applied(), 4);
    assert_eq!(platform.len(), 4);

    let second = run(&repo, &mut platform, Options::default()).expect("second apply");
    assert!(second.is_successful());
    assert_eq!(second.applied(), 0, "a converged platform is written again");
    assert!(second
        .results
        .iter()
        .all(|r| r.outcome == Outcome::Unchanged));

    assert!(compute(&repo, &platform).expect("plan computes").is_clean());

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn resources_converge_in_ascending_wave_order() {
    let dir = demo_repo("apply-order");
    let mut platform = InMemory::new();

    let report = run(&load(&dir), &mut platform, Options::default()).expect("apply");

    assert_eq!(
        report
            .results
            .iter()
            .map(|r| r.id.kind.as_str())
            .collect::<Vec<_>>(),
        vec!["Organization", "Project", "ContextSpace", "Endpoint"]
    );
    assert!(report.results.iter().all(|r| r.action == Action::Create));

    let _ = std::fs::remove_dir_all(&dir);
}

/// CC-20: the run records the commit the repository was on, when it is a checkout.
#[test]
fn a_repository_that_is_not_a_checkout_records_no_revision() {
    let dir = demo_repo("apply-revision");
    let mut platform = InMemory::new();

    let report = run(&load(&dir), &mut platform, Options::default()).expect("apply");
    assert_eq!(report.revision, None);

    let _ = std::fs::remove_dir_all(&dir);
}

/// A platform that refuses one kind. CC-18: the run keeps going inside the wave, and
/// stops before the waves that depend on the one that failed.
#[derive(Default)]
struct RefusesSpaces {
    inner: InMemory,
}

impl Platform for RefusesSpaces {
    fn list(&self, project: &str, plural: &str) -> Result<Vec<RawManifest>, PlatformError> {
        self.inner.list(project, plural)
    }

    fn put(&mut self, manifest: &RawManifest) -> Result<(), PlatformError> {
        if manifest.kind == "ContextSpace" {
            return Err(PlatformError::Rejected {
                id: Box::new(ResourceId::from_manifest(manifest)),
                message: "tenant quota exhausted".to_owned(),
            });
        }
        self.inner.put(manifest)
    }

    fn delete(&mut self, id: &ResourceId) -> Result<(), PlatformError> {
        self.inner.delete(id)
    }
}

#[test]
fn a_failed_wave_stops_the_waves_that_depend_on_it_and_the_run_is_re_runnable() {
    let dir = demo_repo("apply-failure");
    let repo = load(&dir);
    let mut platform = RefusesSpaces::default();

    let report = run(&repo, &mut platform, Options::default()).expect("apply runs");

    assert!(!report.is_successful());
    assert_eq!(report.failures().count(), 1);
    assert_eq!(report.applied(), 2, "wave 0 still converged");

    let endpoint = report
        .results
        .iter()
        .find(|r| r.id.kind == "Endpoint")
        .expect("the endpoint was reported");
    assert_eq!(
        endpoint.outcome,
        Outcome::Skipped("wave 1 did not converge".to_owned())
    );

    // The failure left a re-runnable state: what did converge is there, the rest is still
    // pending (CC-18).
    let pending = compute(&repo, &platform).expect("plan computes");
    assert_eq!(pending.count(Action::Create), 2);
    assert_eq!(pending.count(Action::Unchanged), 2);

    let _ = std::fs::remove_dir_all(&dir);
}
