//! `jcctl sync`: the CLI half of the manifest loop Architecture/06 section 6 promises
//! (MF-27, MF-28, MF-34, T-0828).

mod common;

use common::*;
use jcctl::commands::sync::{self, Options};
use std::path::{Path, PathBuf};
use std::process::Command;

const SOURCE: &str = r#"apiVersion: joinedcontext.com/v1alpha1
kind: SyncSource
metadata:
  name: regional
  namespace: ovzdusie
spec:
  source:
    git:
      url: https://git.region.sk/udp/models.git
      ref: main
  schedule: { interval: 30m }
  mode: mirror
  conflictPolicy: replace
"#;

/// The origin as the caller materialised it: one manifest the sync would carry in.
const FOREIGN_SPACE: &str = r#"apiVersion: joinedcontext.com/v1alpha1
kind: ContextSpace
metadata:
  name: regional-air
  namespace: ovzdusie
spec:
  isSandbox: false
"#;

fn repo_with_source(name: &str) -> PathBuf {
    let dir = demo_repo(name);
    common::write(&dir, "projects/ovzdusie/sync/regional.yaml", SOURCE);
    dir
}

fn checkout(name: &str) -> PathBuf {
    let dir = temp_dir(name);
    common::write(&dir, "space.yaml", FOREIGN_SPACE);
    dir
}

fn options(checkout: &Path, state: &Path) -> Options {
    Options {
        source: "ovzdusie/regional".to_owned(),
        checkout: checkout.to_path_buf(),
        state: Some(state.to_path_buf()),
        once: true,
    }
}

#[test]
fn one_run_proposes_what_the_origin_carries_and_the_next_run_over_it_writes_nothing() {
    let repo = repo_with_source("sync-cmd-repo");
    let origin = checkout("sync-cmd-origin");
    // Inside the run's own temp directory: a state file at a stable path would carry the
    // open proposal of the previous run of this test into the next one.
    let state = origin.join("state.json");

    let first = sync::run(&repo, &options(&origin, &state)).expect("the first run decides");
    let proposal = first
        .proposal
        .expect("a proposal for what the origin carries");
    assert_eq!(
        proposal.envelope["kind"], "Change",
        "{:?}",
        proposal.envelope
    );
    assert!(
        proposal
            .files
            .keys()
            .any(|path| path.to_string_lossy().ends_with("regional-air/space.yaml")),
        "{:?}",
        proposal.files.keys().collect::<Vec<_>>()
    );

    // MF-28: the same origin at the same revision is a run that costs one revision call and
    // proposes nothing — the state file is what makes the second run recognise it.
    let second = sync::run(&repo, &options(&origin, &state)).expect("the second run decides");
    assert!(second.proposal.is_none(), "{:?}", second.reason);

    let _ = std::fs::remove_dir_all(&repo);
    let _ = std::fs::remove_dir_all(&origin);
    let _ = std::fs::remove_file(&state);
}

#[test]
fn a_source_the_repository_does_not_hold_is_named_not_guessed() {
    let repo = repo_with_source("sync-cmd-missing");
    let origin = checkout("sync-cmd-missing-origin");
    let state = origin.join("state.json");
    let mut options = options(&origin, &state);
    options.source = "ovzdusie/nowhere".to_owned();

    let error = sync::run(&repo, &options).expect_err("there is no such source");
    assert!(error.to_string().contains("ovzdusie/nowhere"), "{error}");

    let _ = std::fs::remove_dir_all(&repo);
    let _ = std::fs::remove_dir_all(&origin);
}

#[test]
fn the_command_prints_the_change_and_exits_two_when_a_proposal_is_open() {
    let repo = repo_with_source("sync-cmd-cli");
    let origin = checkout("sync-cmd-cli-origin");
    let state = origin.join("state.json");

    let output = Command::new(env!("CARGO_BIN_EXE_jcctl"))
        .args([
            "sync",
            "--repo-dir",
            repo.to_str().expect("path"),
            "--source",
            "ovzdusie/regional",
            "--checkout",
            origin.to_str().expect("path"),
            "--state",
            state.to_str().expect("path"),
            "--once",
            "--json",
        ])
        .output()
        .expect("jcctl runs");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(output.status.code(), Some(2), "{stdout}");
    assert!(stdout.contains("\"kind\": \"Change\""), "{stdout}");

    let _ = std::fs::remove_dir_all(&repo);
    let _ = std::fs::remove_dir_all(&origin);
    let _ = std::fs::remove_file(&state);
}
