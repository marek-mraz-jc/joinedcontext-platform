use jcctl::loader::Repository;
use jcctl::waves::{plan, wave_of, WAVE_ACCESS, WAVE_EXPOSURE, WAVE_ROOTS, WAVE_RUNTIME};
use std::path::PathBuf;

fn temp_repo(test_name: &str) -> PathBuf {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock after the epoch")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "jcctl-wave-{test_name}-{}-{now}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create temp repo");
    dir
}

fn write(dir: &std::path::Path, rel: &str, body: &str) {
    let path = dir.join(rel);
    std::fs::create_dir_all(path.parent().expect("relative path has a parent"))
        .expect("create parent directory");
    std::fs::write(path, body).expect("write manifest");
}

const ORG: &str = r#"apiVersion: joinedcontext.com/v1alpha1
kind: Organization
metadata:
  name: my-city
  namespace: org
spec:
  domain: banskabystrica.sk
  locales: ["sk"]
  defaultLocale: sk
"#;

const PROJECT: &str = r#"apiVersion: joinedcontext.com/v1alpha1
kind: Project
metadata:
  name: doprava
  namespace: org
spec:
  organizationRef: my-city
"#;

const SPACE: &str = r#"apiVersion: joinedcontext.com/v1alpha1
kind: ContextSpace
metadata:
  name: senzory
  namespace: doprava
spec:
  isSandbox: false
"#;

const ENDPOINT: &str = r#"apiVersion: joinedcontext.com/v1alpha1
kind: Endpoint
metadata:
  name: stream
  namespace: doprava
spec:
  contextSpaceRef: senzory
  slug: abcdefghijklmnopqrstuvwxyz
  audience: public
  enabledRepresentations: ["ngsi-ld"]
"#;

/// Architecture/06 section 3 assigns every reconciled kind a wave; a new kind must be
/// placed there deliberately, or listed as an artifact the reconciler never converges.
#[test]
fn every_catalogued_kind_has_a_wave_or_is_a_known_artifact() {
    // Kinds the reconciler never converges: a Bundle exists only for a download, a
    // Blueprint is expanded at authoring time - what reaches the broker is the manifests
    // it rendered, each of which has a wave of its own (CC-25) - and a UiSchema has no
    // counterpart outside the repository, because the Portal reads the arrangement and
    // draws the form itself (UI-02).
    let artifacts = ["Bundle", "Blueprint", "UiSchema", "Environment"];

    for info in jc_core::registry::KINDS {
        match wave_of(info.kind) {
            Some(wave) => assert!(
                wave <= WAVE_RUNTIME && !artifacts.contains(&info.kind),
                "kind `{}` has wave {wave}",
                info.kind
            ),
            None => assert!(
                artifacts.contains(&info.kind),
                "catalogued kind `{}` has no sync wave",
                info.kind
            ),
        }
    }
}

#[test]
fn waves_are_ascending_and_hold_the_documented_kinds() {
    let dir = temp_repo("ascending");
    write(&dir, "org.yaml", ORG);
    write(&dir, "projects/doprava/project.yaml", PROJECT);
    write(&dir, "projects/doprava/spaces/senzory/space.yaml", SPACE);
    write(
        &dir,
        "projects/doprava/spaces/senzory/endpoints/stream.yaml",
        ENDPOINT,
    );

    let repo = Repository::load(&dir).expect("loads repo");
    let convergence = plan(&repo);

    assert_eq!(convergence.len(), 4);
    let numbers: Vec<u8> = convergence.waves().iter().map(|(n, _)| *n).collect();
    assert_eq!(numbers, vec![WAVE_ROOTS, 1, WAVE_EXPOSURE]);

    let order: Vec<&str> = convergence.iter().map(|id| id.name.as_str()).collect();
    assert_eq!(order, vec!["my-city", "doprava", "senzory", "stream"]);

    let _ = std::fs::remove_dir_all(&dir);
}

/// A Mapping compiles against its DataModel and a Policy names its ScopeDefinition;
/// both pairs share a wave, so the order inside the wave carries the dependency.
#[test]
fn kinds_inside_a_wave_follow_the_documented_order() {
    let dir = temp_repo("intra-wave");
    write(
        &dir,
        "projects/doprava/spaces/senzory/datamodels/mappings/traffic.yaml",
        r#"apiVersion: joinedcontext.com/v1alpha1
kind: Mapping
metadata:
  name: traffic
  namespace: doprava
spec:
  contextSpaceRef: senzory
"#,
    );
    write(
        &dir,
        "projects/doprava/spaces/senzory/datamodels/traffic.yaml",
        r#"apiVersion: joinedcontext.com/v1alpha1
kind: DataModel
metadata:
  name: traffic
  namespace: doprava
spec:
  contextSpaceRef: senzory
"#,
    );
    write(
        &dir,
        "projects/doprava/spaces/senzory/policies/allow-geo.yaml",
        r#"apiVersion: joinedcontext.com/v1alpha1
kind: Policy
metadata:
  name: allow-geo
  namespace: doprava
spec:
  contextSpaceRef: senzory
"#,
    );
    write(
        &dir,
        "projects/doprava/policies/geo-bb.yaml",
        r#"apiVersion: joinedcontext.com/v1alpha1
kind: ScopeDefinition
metadata:
  name: geo-bb
  namespace: doprava
spec:
  scopeString: "/geo/SK/BB"
"#,
    );

    let convergence = plan(&Repository::load(&dir).expect("loads repo"));
    let order: Vec<&str> = convergence.iter().map(|id| id.kind.as_str()).collect();
    assert_eq!(
        order,
        vec!["DataModel", "Mapping", "ScopeDefinition", "Policy"]
    );

    let access = convergence
        .waves()
        .iter()
        .find(|(n, _)| *n == WAVE_ACCESS)
        .expect("an access wave");
    assert_eq!(access.1.len(), 2);

    let _ = std::fs::remove_dir_all(&dir);
}

/// A Bundle is an export artifact (CC-22): it loads, but the reconciler never converges
/// it, and it must not displace the resources around it.
#[test]
fn a_bundle_in_the_repository_is_not_planned() {
    let dir = temp_repo("bundle");
    write(&dir, "org.yaml", ORG);
    write(
        &dir,
        "bundle.yaml",
        r#"apiVersion: joinedcontext.com/v1alpha1
kind: Bundle
metadata:
  name: export-2026-09
spec:
  exportedAt: "2026-09-06T10:00:00Z"
  exportedBy: platform
  sourceInstance: https://dev.joinedcontext.com
  items: []
"#,
    );

    let repo = Repository::load(&dir).expect("loads repo");
    assert_eq!(repo.len(), 2);

    let convergence = plan(&repo);
    assert_eq!(convergence.len(), 1);
    assert_eq!(
        convergence
            .iter()
            .map(|id| id.kind.as_str())
            .collect::<Vec<_>>(),
        vec!["Organization"]
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn planning_the_same_repository_twice_gives_the_same_plan() {
    let dir = temp_repo("deterministic");
    write(&dir, "org.yaml", ORG);
    write(&dir, "projects/doprava/project.yaml", PROJECT);
    write(&dir, "projects/doprava/spaces/senzory/space.yaml", SPACE);

    let first = plan(&Repository::load(&dir).expect("loads repo"));
    let second = plan(&Repository::load(&dir).expect("loads repo again"));
    assert_eq!(first, second);

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn empty_repository_yields_empty_plan() {
    let dir = temp_repo("empty");

    let repo = Repository::load(&dir).expect("loads empty repo");
    assert!(repo.is_empty());

    let convergence = plan(&repo);
    assert!(convergence.is_empty());
    assert_eq!(convergence.len(), 0);
    assert!(convergence.waves().is_empty());

    let _ = std::fs::remove_dir_all(&dir);
}
