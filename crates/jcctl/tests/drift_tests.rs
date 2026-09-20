//! `jcctl drift` against a platform somebody changed behind Git's back (T-0132, CC-21,
//! CC-38, CC-67, UI-26).

mod common;

use common::*;
use jcctl::commands::drift::{detect, Kind};
use jcctl::platform::InMemory;
use jcctl::RawManifest;
use serde_json::json;
use std::process::{Command, Output};

/// The demo repository, live and converged: nothing has drifted yet.
fn converged() -> InMemory {
    InMemory::new()
        .with(manifest(ORG))
        .with(manifest(PROJECT))
        .with(manifest(SPACE))
        .with(manifest(ENDPOINT))
}

/// One manifest with a member changed, as an out-of-band edit leaves it.
fn edited(yaml: &str, path: &str, value: serde_json::Value) -> RawManifest {
    let mut edited = manifest(yaml);
    edited
        .spec
        .as_object_mut()
        .expect("a spec is an object")
        .insert(path.to_owned(), value);
    edited
}

fn run(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_jcctl"))
        .args(args)
        .output()
        .expect("jcctl runs")
}

#[test]
fn a_converged_platform_has_no_drift() {
    let dir = demo_repo("drift-clean");
    let report = detect(&load(&dir), &converged()).expect("drift runs");

    assert!(report.is_clean());
    assert_eq!(report.checked, 4);
    assert_eq!(report.to_json()["summary"]["drifted"], 0);
    assert_eq!(report.render(), "4 resources checked, no drift\n");

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn an_out_of_band_edit_is_drift_with_both_resolutions() {
    let dir = demo_repo("drift-edited");
    let platform = InMemory::new()
        .with(manifest(ORG))
        .with(manifest(PROJECT))
        .with(manifest(SPACE))
        .with(edited(ENDPOINT, "audience", json!("shared")));

    let report = detect(&load(&dir), &platform).expect("drift runs");

    assert!(!report.is_clean());
    assert_eq!(report.drifted.len(), 1);
    let drifted = &report.drifted[0];
    assert_eq!(drifted.kind, Kind::Modified);
    assert_eq!(drifted.id.kind, "Endpoint");

    // UI-26: exactly two buttons, and here both of them do something.
    let revert = drifted.revert.as_ref().expect("revert re-applies Git");
    assert_eq!(
        revert.spec["audience"],
        json!("public"),
        "Git still says public"
    );
    let adopt = drifted.adopt.as_ref().expect("adopt proposes live state");
    assert_eq!(
        adopt.spec["audience"],
        json!("shared"),
        "somebody narrowed it live"
    );
    assert_eq!(
        report.to_json()["drifted"][0]["resolutions"],
        json!(["revert", "adopt"])
    );
    assert_eq!(report.to_json()["drifted"][0]["unavailable"], json!(null));

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_resource_deleted_off_the_platform_can_only_be_reverted() {
    let dir = demo_repo("drift-missing");
    let mut platform = converged();
    {
        use jcctl::platform::Platform;
        let id = jcctl::ResourceId::from_manifest(&manifest(ENDPOINT));
        platform.delete(&id).expect("delete succeeds");
    }

    let report = detect(&load(&dir), &platform).expect("drift runs");

    let drifted = report
        .drifted
        .iter()
        .find(|d| d.id.kind == "Endpoint")
        .expect("the deleted endpoint drifted");
    assert_eq!(drifted.kind, Kind::Missing);
    assert!(drifted.revert.is_some(), "Git still declares it");
    assert!(drifted.adopt.is_none(), "there is no live state to adopt");
    assert!(Kind::Missing.unavailable().unwrap().contains("CC-19"));

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_resource_created_off_the_platform_can_only_be_adopted() {
    let dir = demo_repo("drift-unexpected");
    let extra = r#"apiVersion: joinedcontext.com/v1alpha1
kind: Endpoint
metadata:
  name: hand-made
  namespace: ovzdusie
spec:
  contextSpaceRef: ovzdusie
  slug: 3ubvq7zt5hnx9dm2kayr8wpc6f
  audience: shared
  enabledRepresentations: ["ngsi-ld"]
"#;
    let report = detect(&load(&dir), &converged().with(manifest(extra))).expect("drift runs");

    let drifted = report
        .drifted
        .iter()
        .find(|d| d.id.name == "hand-made")
        .expect("the hand-made endpoint drifted");
    assert_eq!(drifted.kind, Kind::Unexpected);
    assert!(drifted.adopt.is_some(), "the live state is committable");
    assert!(drifted.revert.is_none(), "Git declares nothing to re-apply");
    assert!(Kind::Unexpected.unavailable().unwrap().contains("CC-19"));

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn an_unmanaged_sandbox_space_is_never_drift() {
    // CC-67: a sandbox is created live through the green lane, carries a TTL and is never
    // backed by Git. Reporting it would report every sandbox on every run.
    let dir = demo_repo("drift-sandbox");
    let sandbox = r#"apiVersion: joinedcontext.com/v1alpha1
kind: ContextSpace
metadata:
  name: scratch
  namespace: ovzdusie
spec:
  isSandbox: true
  ttlDays: 1
"#;
    let inside = r#"apiVersion: joinedcontext.com/v1alpha1
kind: Endpoint
metadata:
  name: scratch-view
  namespace: ovzdusie
spec:
  contextSpaceRef: scratch
  slug: 9wkq2patz7xr4hs6ndl3ym8cvb
  audience: project
  enabledRepresentations: ["ngsi-ld"]
"#;
    let platform = converged().with(manifest(sandbox)).with(manifest(inside));

    let report = detect(&load(&dir), &platform).expect("drift runs");

    assert!(report.is_clean(), "{:?}", report.drifted);
    assert_eq!(report.sandboxed.len(), 2, "the space and what lives in it");
    assert_eq!(report.checked, 4, "the sandbox is not counted as checked");

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_space_git_declares_is_managed_however_its_sandbox_flag_reads() {
    // The other half of CC-67: `isSandbox` on a space the repository declares is a property
    // of managed configuration, not a licence to stop watching it.
    let dir = demo_repo("drift-declared-sandbox");
    common::write(
        &dir,
        "projects/ovzdusie/spaces/scratch/space.yaml",
        "apiVersion: joinedcontext.com/v1alpha1\nkind: ContextSpace\nmetadata:\n  name: scratch\n  namespace: ovzdusie\nspec:\n  isSandbox: true\n  ttlDays: 7\n",
    );
    let live = r#"apiVersion: joinedcontext.com/v1alpha1
kind: ContextSpace
metadata:
  name: scratch
  namespace: ovzdusie
spec:
  isSandbox: true
  ttlDays: 30
"#;
    let report = detect(&load(&dir), &converged().with(manifest(live))).expect("drift runs");

    assert!(report.sandboxed.is_empty());
    assert_eq!(
        report
            .drifted
            .iter()
            .map(|d| d.id.name.as_str())
            .collect::<Vec<_>>(),
        vec!["scratch"]
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn adopting_writes_the_live_state_at_the_path_its_kind_prescribes() {
    let dir = demo_repo("drift-adopt-write");
    let out = common::temp_dir("drift-adopt-out");
    let platform = InMemory::new()
        .with(manifest(ORG))
        .with(manifest(PROJECT))
        .with(manifest(SPACE))
        .with(edited(ENDPOINT, "audience", json!("shared")));

    let report = detect(&load(&dir), &platform).expect("drift runs");
    let written = jcctl::commands::drift::write_adoptions(&out, &report).expect("adoptions write");

    assert_eq!(written, 1);
    let path = out.join(ENDPOINT_PATH);
    let yaml = std::fs::read_to_string(&path).expect("the adopted manifest is at its path");
    assert!(yaml.contains("audience: shared"), "{yaml}");

    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_dir_all(&out);
}

#[test]
fn a_credential_the_platform_hands_back_never_reaches_an_adoption() {
    // MF-17: the live platform may answer with a literal secret; committing it is the one
    // thing configuration-as-code forbids, so adopt drops it and says which member went.
    let dir = demo_repo("drift-adopt-secret");
    // The credential rides along on a resource that drifted for another reason: an
    // undeclared member is not drift by itself (CC-69), and adopt still has to clean it.
    let mut live = edited(ENDPOINT, "audience", json!("shared"));
    live.spec
        .as_object_mut()
        .expect("a spec is an object")
        .insert("token".to_owned(), json!("ghp_live_secret_value"));
    let platform = InMemory::new()
        .with(manifest(ORG))
        .with(manifest(PROJECT))
        .with(manifest(SPACE))
        .with(live);

    let report = detect(&load(&dir), &platform).expect("drift runs");
    let drifted = &report.drifted[0];

    assert_eq!(drifted.redactions, vec!["token".to_owned()]);
    let adopt = drifted.adopt.as_ref().expect("adopt is still offered");
    assert!(adopt.spec.get("token").is_none(), "{:?}", adopt.spec);
    assert!(
        !serde_json::to_string(&report.to_json())
            .unwrap()
            .contains("ghp_live_secret_value"),
        "the report must not carry the secret either"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_command_needs_a_gateway_and_never_calls_an_unread_platform_clean() {
    // T-1216: it used to run the resource comparison against an empty in-process platform and
    // report every declared manifest as `missing` — on every run, on a platform where nothing
    // was wrong. Configuration cannot drift at all: every component reads it from the
    // repository (CC-72, T-0421), so what `drift` compares is the seed entities, and those are
    // read through the space surface.
    let dir = demo_repo("drift-cli-no-gateway");
    let output = run(&["drift", "--repo-dir", dir.to_str().unwrap()]);

    assert!(
        !output.status.success(),
        "a run that compared nothing exited as if it had found nothing"
    );
    let said = String::from_utf8_lossy(&output.stderr).to_string();
    assert!(said.contains("--gateway-url"), "{said}");
    let printed = String::from_utf8_lossy(&output.stdout).to_string();
    for kind in [
        "Organization",
        "Project",
        "ContextSpace",
        "DataModel",
        "missing",
    ] {
        assert!(
            !printed.contains(kind),
            "the report still names configuration, which cannot drift: {printed}"
        );
    }

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_adopt_dir_flag_says_it_has_nothing_to_write() {
    // A live entity is data, and data is not adopted into the repository (CC-38): a seed
    // entity's one resolution is revert. The flag stays for the day a live configuration store
    // exists — `detect` and `write_adoptions` are what adoption is built on — and until then it
    // says so instead of writing an empty directory and looking like it worked.
    let dir = demo_repo("drift-cli-adopt");
    let out = common::temp_dir("drift-cli-adopt-out");
    let output = run(&[
        "drift",
        "--repo-dir",
        dir.to_str().unwrap(),
        "--adopt-dir",
        out.to_str().unwrap(),
    ]);

    let said = String::from_utf8_lossy(&output.stderr).to_string();
    assert!(said.contains("nothing to write"), "{said}");

    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_dir_all(&out);
}
