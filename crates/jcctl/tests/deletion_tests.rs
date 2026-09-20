mod common;

use common::*;
use jcctl::commands::apply::{run, Options, Outcome};
use jcctl::commands::plan::Action;
use jcctl::platform::InMemory;

/// The whole point of CC-19: a partial checkout must not wipe live spaces. The endpoint is
/// gone from Git, and a plain `apply` leaves it alone.
#[test]
fn a_plain_apply_never_deletes() {
    let dir = demo_repo("delete-default");
    std::fs::remove_file(dir.join(ENDPOINT_PATH)).expect("drop the endpoint from Git");

    let mut platform = InMemory::new().with(manifest(ENDPOINT));
    let report = run(&load(&dir), &mut platform, Options::default()).expect("apply");

    assert_eq!(platform.len(), 4, "the live endpoint survived");
    let deletion = report
        .results
        .iter()
        .find(|r| r.action == Action::Delete)
        .expect("the deletion was reported");
    assert!(
        matches!(&deletion.outcome, Outcome::Skipped(why) if why.contains("--confirm-deletions")),
        "{:?}",
        deletion.outcome
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// One flag is not enough: deletion takes a second, deliberate confirmation (CC-19).
#[test]
fn one_flag_alone_does_not_delete() {
    for options in [
        Options {
            prune: true,
            confirm_deletions: false,
        },
        Options {
            prune: false,
            confirm_deletions: true,
        },
    ] {
        let dir = demo_repo("delete-one-flag");
        std::fs::remove_file(dir.join(ENDPOINT_PATH)).expect("drop the endpoint from Git");

        let mut platform = InMemory::new().with(manifest(ENDPOINT));
        run(&load(&dir), &mut platform, options).expect("apply");

        assert_eq!(platform.len(), 4, "{options:?} must not delete");
        assert!(!options.deletes());

        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[test]
fn prune_with_confirmation_removes_what_left_the_repository() {
    let dir = demo_repo("delete-prune");
    std::fs::remove_file(dir.join(ENDPOINT_PATH)).expect("drop the endpoint from Git");

    let mut platform = InMemory::new().with(manifest(ENDPOINT));
    let options = Options {
        prune: true,
        confirm_deletions: true,
    };
    let report = run(&load(&dir), &mut platform, options).expect("apply");

    assert!(report.is_successful());
    assert_eq!(platform.len(), 3);
    assert!(
        !platform.resources().any(|(id, _)| id.name == "public-air"),
        "the endpoint was removed"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// Deletion runs in descending wave order, so a space is emptied before it is removed.
#[test]
fn deletions_run_from_the_outermost_wave_inwards() {
    let dir = temp_dir("delete-order");
    write(&dir, "org.yaml", ORG);

    let mut platform = InMemory::new()
        .with(manifest(ORG))
        .with(manifest(PROJECT))
        .with(manifest(SPACE))
        .with(manifest(ENDPOINT));

    let options = Options {
        prune: true,
        confirm_deletions: true,
    };
    let report = run(&load(&dir), &mut platform, options).expect("apply");

    let deleted: Vec<&str> = report
        .results
        .iter()
        .filter(|r| r.action == Action::Delete)
        .map(|r| r.id.kind.as_str())
        .collect();
    assert_eq!(deleted, vec!["Endpoint", "ContextSpace", "Project"]);
    assert_eq!(platform.len(), 1, "only the organization is still declared");

    let _ = std::fs::remove_dir_all(&dir);
}
