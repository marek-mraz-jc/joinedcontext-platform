//! T-0142: a `SyncSource` run polls, notices the source moved, and proposes the import
//! (MF-27…MF-31).
//!
//! The remote is a fake that answers one revision and writes a fixed set of manifests, which
//! is everything the loop asks of a transport. What the tests are about is the judgement in
//! front of it: when a run happens at all, what it proposes, and what it refuses to propose.

use jc_core::kinds::sync::SyncOrigin;
use jcctl::commands::plan::Action;
use jcctl::loader::RawManifest;
use jcctl::sync::{self, Phase, RemoteError, State, SyncRemote};
use serde_json::{json, Value};
use std::cell::RefCell;
use std::path::{Path, PathBuf};

const HOUR: u64 = 3_600;

/// A sandbox space: a green-lane create, so the lane in a test is the one the fixture chose.
const SANDBOX: &str = r#"apiVersion: joinedcontext.com/v1alpha1
kind: ContextSpace
metadata:
  name: regional-sandbox
  namespace: regional
  labels:
    joinedcontext.com/tier: standard
spec:
  isSandbox: true
"#;

/// The same source's second resource, without the label the selector asks for.
const OTHER: &str = r#"apiVersion: joinedcontext.com/v1alpha1
kind: ContextSpace
metadata:
  name: regional-internal
  namespace: regional
  labels:
    joinedcontext.com/tier: internal
spec:
  isSandbox: true
"#;

/// Identity and access are red whatever they do (CC-63), so this is how a proposal that
/// `autoMerge` must not merge is built.
const POLICY: &str = r#"apiVersion: joinedcontext.com/v1alpha1
kind: Policy
metadata:
  name: regional-read
  namespace: regional
spec:
  contextSpaceRef: { kind: ContextSpace, name: regional-sandbox }
  assigner: did:web:region.sk
  assignee: { kind: role, id: public }
  operations: [retrieveOps]
"#;

/// MF-24: a manifest carrying a secret instead of a reference is refused by the same gate an
/// interactive import is refused by.
const WITH_A_SECRET: &str = r#"apiVersion: joinedcontext.com/v1alpha1
kind: ContextSpace
metadata:
  name: regional-leaky
  namespace: regional
spec:
  isSandbox: true
  token: "glpat-0000000000000000000000"
"#;

/// A remote that answers one revision and writes the documents it was built with.
struct Fake {
    revision: String,
    documents: Vec<(&'static str, String)>,
    asked: RefCell<Vec<String>>,
}

impl Fake {
    fn at(revision: &str, documents: &[(&'static str, &str)]) -> Self {
        Self {
            revision: revision.to_owned(),
            documents: documents
                .iter()
                .map(|(name, body)| (*name, (*body).to_owned()))
                .collect(),
            asked: RefCell::new(Vec::new()),
        }
    }

    fn calls(&self) -> Vec<String> {
        self.asked.borrow().clone()
    }
}

impl SyncRemote for Fake {
    fn revision(&self, _origin: &SyncOrigin) -> Result<String, RemoteError> {
        self.asked.borrow_mut().push("revision".to_owned());
        Ok(self.revision.clone())
    }

    fn checkout(
        &self,
        _origin: &SyncOrigin,
        revision: &str,
        into: &Path,
    ) -> Result<(), RemoteError> {
        self.asked.borrow_mut().push(format!("checkout {revision}"));
        for (name, body) in &self.documents {
            std::fs::write(into.join(name), body)
                .map_err(|error| RemoteError::Unavailable(error.to_string()))?;
        }
        Ok(())
    }
}

/// The manifest under test, with `spec` members the caller wants to override.
fn source(overrides: Value) -> RawManifest {
    let mut spec = json!({
        "source": { "git": { "url": "https://git.region.sk/udp/models.git", "ref": "main" } },
        "schedule": { "interval": "30m" },
        "mode": "mirror",
        "conflictPolicy": "replace",
    });
    for (key, value) in overrides.as_object().expect("an object of overrides") {
        spec[key] = value.clone();
    }
    RawManifest {
        api_version: jc_core::API_VERSION.to_owned(),
        kind: "SyncSource".to_owned(),
        metadata: serde_json::from_value(json!({ "name": "regional", "namespace": "bb-doprava" }))
            .expect("metadata"),
        spec,
        status: None,
    }
}

/// An empty repository and a workspace beside it.
fn dirs(test: &str) -> (PathBuf, PathBuf) {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock after the epoch")
        .as_nanos();
    let root = std::env::temp_dir().join(format!("jcctl-sync-{test}-{now}"));
    let (repo, workspace) = (root.join("repo"), root.join("workspace"));
    std::fs::create_dir_all(&repo).expect("create the repository");
    std::fs::create_dir_all(&workspace).expect("create the workspace");
    (repo, workspace)
}

fn run(
    dirs: &(PathBuf, PathBuf),
    manifest: &RawManifest,
    state: &State,
    remote: &Fake,
) -> sync::Run {
    sync::poll(manifest, state, HOUR, &dirs.0, &dirs.1, remote).expect("the run is judged")
}

/// MF-27, MF-28: a moved source becomes one reviewable proposal carrying the import.
#[test]
fn a_moved_source_becomes_a_reviewable_proposal() {
    let dirs = dirs("proposed");
    let remote = Fake::at("9f1c0de", &[("space.yaml", SANDBOX)]);
    let outcome = run(&dirs, &source(json!({})), &State::default(), &remote);

    assert_eq!(outcome.phase, Phase::PendingApproval);
    let proposal = outcome.proposal.expect("a proposal");
    assert_eq!(proposal.name, "chg-sync-regional-9f1c0de");
    assert_eq!(proposal.namespace, "bb-doprava");
    assert!(proposal.rejected.is_empty(), "{:?}", proposal.rejected);

    let envelope = &proposal.envelope;
    assert_eq!(envelope["kind"], json!("Change"));
    assert_eq!(envelope["status"]["lane"], json!("green"));
    assert_eq!(envelope["status"]["plan"]["create"], json!(1));

    // The manifest lands in the project the SyncSource lives in, not the one the source
    // wrote (MF-22), and says which source put it there (MF-08, MF-27).
    let (path, manifest) = proposal.files.iter().next().expect("one file");
    assert_eq!(
        path,
        &PathBuf::from("projects/bb-doprava/spaces/regional-sandbox/space.yaml")
    );
    assert_eq!(manifest.metadata.namespace.as_deref(), Some("bb-doprava"));
    assert_eq!(
        manifest.metadata.rest["annotations"]["joinedcontext.com/sync-source"],
        json!("bb-doprava/regional")
    );

    // Nothing is imported until somebody merges it, so the revision stays unobserved and the
    // open proposal blocks the next run.
    assert_eq!(outcome.state.observed_revision, None);
    assert_eq!(
        outcome.state.open_proposal.as_deref(),
        Some("chg-sync-regional-9f1c0de")
    );
    assert_eq!(outcome.state.last_run_at, Some(HOUR));
}

/// A source that has not moved costs one cheap call and proposes nothing (CC-18).
#[test]
fn a_source_that_has_not_moved_is_not_fetched() {
    let dirs = dirs("unmoved");
    let remote = Fake::at("9f1c0de", &[("space.yaml", SANDBOX)]);
    let state = State {
        observed_revision: Some("9f1c0de".to_owned()),
        ..State::default()
    };
    let outcome = run(&dirs, &source(json!({})), &state, &remote);

    assert_eq!(outcome.phase, Phase::Synced);
    assert!(outcome.proposal.is_none());
    assert_eq!(remote.calls(), vec!["revision".to_owned()]);
}

/// MF-28: between runs the remote is not touched at all.
#[test]
fn a_run_that_is_not_due_asks_the_remote_nothing() {
    let dirs = dirs("not-due");
    let remote = Fake::at("9f1c0de", &[("space.yaml", SANDBOX)]);
    let state = State {
        last_run_at: Some(HOUR - 60),
        ..State::default()
    };
    let outcome = run(&dirs, &source(json!({})), &state, &remote);

    assert_eq!(outcome.phase, Phase::Synced);
    assert!(outcome.proposal.is_none());
    assert!(remote.calls().is_empty(), "{:?}", remote.calls());

    let schedule = jc_core::kinds::sync::Schedule::interval("30m");
    assert!(!sync::due(&schedule, &state, HOUR));
    assert!(sync::due(&schedule, &state, HOUR + 1_800));
    // A webhook schedule has no timer to be due on.
    assert!(!sync::due(
        &jc_core::kinds::sync::Schedule::webhook(),
        &State::default(),
        HOUR
    ));
}

/// Paused means paused, and an unreviewed proposal is not a reason to open a second one.
#[test]
fn a_paused_source_and_an_open_proposal_both_stop_the_run() {
    let dirs = dirs("stopped");
    let remote = Fake::at("9f1c0de", &[("space.yaml", SANDBOX)]);

    let paused = State {
        paused: true,
        ..State::default()
    };
    let outcome = run(&dirs, &source(json!({})), &paused, &remote);
    assert_eq!(outcome.phase, Phase::Paused);

    let waiting = State {
        open_proposal: Some("chg-sync-regional-9f1c0de".to_owned()),
        ..State::default()
    };
    let outcome = run(&dirs, &source(json!({})), &waiting, &remote);
    assert_eq!(outcome.phase, Phase::PendingApproval);
    assert!(outcome.proposal.is_none());

    assert!(remote.calls().is_empty(), "{:?}", remote.calls());
}

/// MF-10: the selector decides what a run proposes, and it decides it before the import gates
/// see the set, so a reference into what was left out is refused rather than missed.
#[test]
fn a_selector_narrows_what_the_run_proposes() {
    let dirs = dirs("selector");
    let remote = Fake::at("9f1c0de", &[("space.yaml", SANDBOX), ("other.yaml", OTHER)]);
    let manifest = source(json!({ "selector": { "joinedcontext.com/tier": "standard" } }));
    let outcome = run(&dirs, &manifest, &State::default(), &remote);

    let proposal = outcome.proposal.expect("a proposal");
    let names: Vec<&str> = proposal
        .files
        .values()
        .map(|m| m.metadata.name.as_str())
        .collect();
    assert_eq!(names, vec!["regional-sandbox"]);
}

/// MF-29, CC-70: `autoMerge` is a green-lane privilege. A red proposal carries the flag from
/// the manifest and still does not merge itself.
#[test]
fn auto_merge_never_reaches_past_the_green_lane() {
    let green_dirs = dirs("automerge");
    let manifest = source(json!({ "autoMerge": true }));

    let green = Fake::at("9f1c0de", &[("space.yaml", SANDBOX)]);
    let outcome = run(&green_dirs, &manifest, &State::default(), &green);
    let proposal = outcome.proposal.expect("a proposal");
    assert_eq!(proposal.envelope["status"]["lane"], json!("green"));
    assert!(proposal.auto_merge);

    let red_dirs = dirs("automerge-red");
    let remote = Fake::at(
        "abc1234",
        &[("space.yaml", SANDBOX), ("policy.yaml", POLICY)],
    );
    let outcome = run(&red_dirs, &manifest, &State::default(), &remote);
    let proposal = outcome.proposal.expect("a proposal");
    assert_eq!(proposal.envelope["status"]["lane"], json!("red"));
    assert_eq!(
        proposal.envelope["status"]["phase"],
        json!("PendingApproval")
    );
    assert!(!proposal.auto_merge, "a red proposal merged itself");
}

/// MF-24, MF-31: a run goes through the import gates, so a manifest carrying a literal
/// credential stops the whole run — and the revision stays unobserved, because a refused
/// revision is not an imported one.
#[test]
fn a_refused_resource_proposes_nothing_and_leaves_the_revision_alone() {
    let dirs = dirs("refused");
    let remote = Fake::at(
        "9f1c0de",
        &[("space.yaml", SANDBOX), ("leaky.yaml", WITH_A_SECRET)],
    );
    let outcome = run(&dirs, &source(json!({})), &State::default(), &remote);

    assert_eq!(outcome.phase, Phase::Error);
    assert_eq!(outcome.state.observed_revision, None);
    assert_eq!(outcome.state.open_proposal, None);

    let proposal = outcome
        .proposal
        .expect("a proposal that says why there is none");
    assert!(proposal.files.is_empty(), "a refused run wrote something");
    assert_eq!(proposal.rejected.len(), 1);
    assert!(
        proposal.rejected[0].contains("literal credential"),
        "{}",
        proposal.rejected[0]
    );
    // The value itself is not repeated back into the status.
    assert!(!proposal.rejected[0].contains("glpat-0000000000000000000000"));
}

/// CC-19: `prune` reaches what this source wrote and nothing else, and a deletion is red.
#[test]
fn prune_only_reaches_what_this_source_wrote() {
    let dirs = dirs("prune");
    let synced = dirs.0.join("projects/bb-doprava/spaces/gone");
    std::fs::create_dir_all(&synced).expect("create the space directory");
    std::fs::write(
        synced.join("space.yaml"),
        r#"apiVersion: joinedcontext.com/v1alpha1
kind: ContextSpace
metadata:
  name: gone
  namespace: bb-doprava
  annotations:
    joinedcontext.com/sync-source: bb-doprava/regional
spec:
  isSandbox: true
"#,
    )
    .expect("write the synced resource");
    let own = dirs.0.join("projects/bb-doprava/spaces/hand-authored");
    std::fs::create_dir_all(&own).expect("create the space directory");
    std::fs::write(
        own.join("space.yaml"),
        r#"apiVersion: joinedcontext.com/v1alpha1
kind: ContextSpace
metadata:
  name: hand-authored
  namespace: bb-doprava
spec:
  isSandbox: true
"#,
    )
    .expect("write the local resource");

    let remote = Fake::at("9f1c0de", &[("space.yaml", SANDBOX)]);
    let manifest = source(json!({ "prune": true }));
    let outcome = run(&dirs, &manifest, &State::default(), &remote);

    let proposal = outcome.proposal.expect("a proposal");
    assert_eq!(
        proposal.removed,
        vec![PathBuf::from("projects/bb-doprava/spaces/gone/space.yaml")],
        "prune reached past what the source owns"
    );
    assert_eq!(proposal.envelope["status"]["plan"]["delete"], json!(1));
    assert_eq!(proposal.envelope["status"]["lane"], json!("red"));
}

/// A source that moved but changes nothing records the revision rather than proposing an
/// empty merge request every half hour (CC-18).
#[test]
fn a_move_that_changes_nothing_is_recorded_and_not_proposed() {
    let dirs = dirs("no-op");
    let existing = dirs.0.join("projects/bb-doprava/spaces/regional-sandbox");
    std::fs::create_dir_all(&existing).expect("create the space directory");
    std::fs::write(
        existing.join("space.yaml"),
        SANDBOX.replace("namespace: regional", "namespace: bb-doprava"),
    )
    .expect("write the existing resource");

    let remote = Fake::at("9f1c0de", &[("space.yaml", SANDBOX)]);
    let manifest = source(json!({ "conflictPolicy": "skip" }));
    let outcome = run(&dirs, &manifest, &State::default(), &remote);

    assert_eq!(outcome.phase, Phase::Synced);
    assert!(outcome.proposal.is_none());
    assert_eq!(outcome.state.observed_revision.as_deref(), Some("9f1c0de"));
}

/// A `oneshot` source is copied once and then detached (Architecture/06 section 6).
#[test]
fn a_oneshot_source_detaches_after_the_run_that_reached_it() {
    let dirs = dirs("oneshot");
    let remote = Fake::at("9f1c0de", &[("space.yaml", SANDBOX)]);
    let manifest = source(json!({ "mode": "oneshot" }));
    let outcome = run(&dirs, &manifest, &State::default(), &remote);

    assert!(outcome.proposal.is_some());
    assert!(outcome.state.paused, "a oneshot stayed attached");
    assert!(!sync::due(
        &jc_core::kinds::sync::Schedule::interval("30m"),
        &outcome.state,
        HOUR * 100
    ));
}

/// CC-18: the same revision always produces the same proposal name, so a daemon that
/// restarted finds its own proposal instead of opening a second one for it.
#[test]
fn the_proposal_name_is_derived_from_the_revision() {
    assert_eq!(
        sync::proposal_name("regional", "9f1c0dedeadbeef"),
        "chg-sync-regional-9f1c0de"
    );
    assert_eq!(
        sync::proposal_name("regional", "9f1c0de"),
        "chg-sync-regional-9f1c0de"
    );
    assert_eq!(
        sync::owner_annotation("bb-doprava", "regional"),
        "bb-doprava/regional"
    );
}

/// The plan the reviewer reads is the one the lanes were computed from, so a create in it is
/// a create in the envelope.
#[test]
fn the_envelope_counts_what_the_proposal_carries() {
    let dirs = dirs("counts");
    let remote = Fake::at("9f1c0de", &[("space.yaml", SANDBOX), ("other.yaml", OTHER)]);
    let outcome = run(&dirs, &source(json!({})), &State::default(), &remote);
    let proposal = outcome.proposal.expect("a proposal");

    assert_eq!(proposal.files.len(), 2);
    assert_eq!(proposal.envelope["status"]["plan"]["create"], json!(2));
    assert_eq!(proposal.envelope["status"]["plan"]["update"], json!(0));
    assert_eq!(proposal.envelope["status"]["plan"]["delete"], json!(0));
    assert_eq!(Action::Create.as_str(), "CREATE");
}

/// MF-28: a source that runs on its origin's word runs when the caller says so, and on no
/// timer at all (T-0475).
#[test]
fn a_webhook_source_runs_when_it_is_asked_for_and_never_on_a_tick() {
    let dirs = dirs("asked");
    let remote = Fake::at("9f1c0de", &[("space.yaml", SANDBOX)]);
    let manifest = source(json!({ "schedule": { "webhook": true } }));

    // However long the timer runs, `poll` proposes nothing and the remote is never touched:
    // there is no interval to beat and there never will be.
    for now in [HOUR, HOUR * 24, HOUR * 24 * 365] {
        let outcome = sync::poll(&manifest, &State::default(), now, &dirs.0, &dirs.1, &remote)
            .expect("the tick is judged");
        assert_eq!(outcome.phase, Phase::Synced);
        assert!(outcome.proposal.is_none());
    }
    assert!(remote.calls().is_empty(), "{:?}", remote.calls());

    let outcome = sync::poll_now(
        &manifest,
        &State::default(),
        HOUR,
        &dirs.0,
        &dirs.1,
        &remote,
    )
    .expect("the asked-for run is judged");
    assert_eq!(outcome.phase, Phase::PendingApproval);
    let proposal = outcome.proposal.expect("a proposal");
    assert_eq!(proposal.name, "chg-sync-regional-9f1c0de");
    assert_eq!(proposal.files.len(), 1);
}

/// The button skips the wait and nothing else: the operator's switch and the reviewer's open
/// proposal both still stop the run (T-0475).
#[test]
fn an_asked_for_run_is_still_refused_by_the_switch_and_by_a_reviewer() {
    let dirs = dirs("asked-stopped");
    let remote = Fake::at("9f1c0de", &[("space.yaml", SANDBOX)]);
    let manifest = source(json!({ "schedule": { "webhook": true } }));

    let paused = State {
        paused: true,
        ..State::default()
    };
    let outcome = sync::poll_now(&manifest, &paused, HOUR, &dirs.0, &dirs.1, &remote)
        .expect("the asked-for run is judged");
    assert_eq!(outcome.phase, Phase::Paused);
    assert!(outcome.proposal.is_none());

    let waiting = State {
        open_proposal: Some("chg-sync-regional-9f1c0de".to_owned()),
        ..State::default()
    };
    let outcome = sync::poll_now(&manifest, &waiting, HOUR, &dirs.0, &dirs.1, &remote)
        .expect("the asked-for run is judged");
    assert_eq!(outcome.phase, Phase::PendingApproval);
    assert!(outcome.proposal.is_none());

    assert!(
        remote.calls().is_empty(),
        "neither refusal reaches the origin: {:?}",
        remote.calls()
    );
}

/// An asked-for run of a source that has not moved costs the cheap call and nothing more
/// (CC-18, T-0475).
#[test]
fn an_asked_for_run_of_an_unmoved_source_is_one_call_and_no_checkout() {
    let dirs = dirs("asked-unmoved");
    let remote = Fake::at("9f1c0de", &[("space.yaml", SANDBOX)]);
    let state = State {
        observed_revision: Some("9f1c0de".to_owned()),
        ..State::default()
    };
    let manifest = source(json!({ "schedule": { "webhook": true } }));

    let outcome = sync::poll_now(&manifest, &state, HOUR, &dirs.0, &dirs.1, &remote)
        .expect("the asked-for run is judged");

    assert_eq!(outcome.phase, Phase::Synced);
    assert!(outcome.proposal.is_none());
    assert_eq!(outcome.state.last_run_at, Some(HOUR));
    assert_eq!(remote.calls(), vec!["revision".to_owned()]);
}
