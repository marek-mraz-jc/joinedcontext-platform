//! T-0563: `jcctl validate` refuses a `ModelProjection` naming a class or a slot the referenced
//! DataModel version does not have, every offending name listed at once (MP-01).

use jcctl::commands::validate;
use std::path::{Path, PathBuf};

fn temp_repo(test_name: &str) -> PathBuf {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock after the epoch")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "jcctl-projection-{test_name}-{}-{now}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create temp repo");
    dir
}

fn write(dir: &Path, rel: &str, body: &str) {
    let path = dir.join(rel);
    std::fs::create_dir_all(path.parent().expect("relative path has a parent"))
        .expect("create parent");
    std::fs::write(path, body).expect("write manifest");
}

const PROJECTION_PATH: &str = "projects/helsinki/spaces/fleet/projections/partner-view.yaml";

const PROJECTION: &str = r#"apiVersion: joinedcontext.com/v1alpha1
kind: ModelProjection
metadata:
  name: partner-view
  namespace: helsinki
spec:
  contextSpaceRef: fleet
  dataModelRef: { kind: DataModel, name: fleet, version: "3" }
  classes:
    - name: Vehicle
      slots: [name, speed]
    - name: User
      slots: [name, age]
"#;

fn repo_with(test_name: &str, projection: &str) -> PathBuf {
    let dir = temp_repo(test_name);
    write(
        &dir,
        "org.yaml",
        r#"apiVersion: joinedcontext.com/v1alpha1
kind: Organization
metadata:
  name: hel
  namespace: org
spec:
  domain: hel.fi
  locales: ["en"]
  defaultLocale: en
"#,
    );
    write(
        &dir,
        "projects/helsinki/project.yaml",
        r#"apiVersion: joinedcontext.com/v1alpha1
kind: Project
metadata:
  name: helsinki
  namespace: org
spec:
  organizationRef: hel
"#,
    );
    write(
        &dir,
        "projects/helsinki/spaces/fleet/space.yaml",
        r#"apiVersion: joinedcontext.com/v1alpha1
kind: ContextSpace
metadata:
  name: fleet
  namespace: helsinki
spec:
  isSandbox: false
"#,
    );
    write(
        &dir,
        "projects/helsinki/spaces/fleet/datamodels/fleet.yaml",
        r#"apiVersion: joinedcontext.com/v1alpha1
kind: DataModel
metadata:
  name: fleet
  namespace: helsinki
spec:
  contextSpaceRef: fleet
  linkml: ./fleet.linkml.yaml
  version: 3.0.0
  lifecycle: draft
  classes: [Vehicle, User]
"#,
    );
    write(
        &dir,
        "projects/helsinki/spaces/fleet/datamodels/fleet.linkml.yaml",
        r#"id: https://hel.fi/models/fleet
name: fleet
classes:
  Vehicle:
    slots: [name, location, speed]
    attributes:
      odometer:
        range: integer
  User:
    slots: [name, age, email]
"#,
    );
    write(&dir, PROJECTION_PATH, projection);
    dir
}

#[test]
fn a_projection_inside_the_model_is_valid() {
    let dir = repo_with("valid", PROJECTION);
    let report = validate::run(&dir);
    assert_eq!(report.findings, vec![], "{:?}", report.findings);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn an_attribute_counts_as_a_slot() {
    let dir = repo_with(
        "attribute",
        &PROJECTION.replace("slots: [name, speed]", "slots: [name, odometer]"),
    );
    let report = validate::run(&dir);
    assert_eq!(report.findings, vec![], "{:?}", report.findings);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn every_unknown_class_and_slot_is_named_at_once() {
    let dir = repo_with(
        "unknown",
        &PROJECTION
            .replace("slots: [name, speed]", "slots: [name, colour]")
            .replace("name: User", "name: Driver"),
    );
    let report = validate::run(&dir);
    assert_eq!(report.findings.len(), 1, "{:?}", report.findings);
    let finding = &report.findings[0];
    assert_eq!(finding.path, Path::new(PROJECTION_PATH));
    assert!(
        finding.message.contains("Vehicle.colour"),
        "{}",
        finding.message
    );
    assert!(finding.message.contains("Driver"), "{}", finding.message);
    assert!(finding.message.contains("MP-01"), "{}", finding.message);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_projection_of_another_major_version_is_refused() {
    let dir = repo_with(
        "version",
        &PROJECTION.replace("version: \"3\"", "version: \"2\""),
    );
    let report = validate::run(&dir);
    assert_eq!(report.findings.len(), 1, "{:?}", report.findings);
    assert!(
        report.findings[0].message.contains("version 2"),
        "{}",
        report.findings[0].message
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_projection_of_a_model_nobody_declares_is_refused() {
    let dir = repo_with(
        "dangling",
        &PROJECTION.replace("name: fleet, version", "name: cars, version"),
    );
    let report = validate::run(&dir);
    assert_eq!(report.findings.len(), 1, "{:?}", report.findings);
    assert!(
        report.findings[0].message.contains("DataModel `cars`"),
        "{}",
        report.findings[0].message
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// CC-08: a model whose `linkml` climbs out of its folder is not opened to check a projection;
/// the projection is refused because the model's spec does not validate.
#[test]
fn a_model_source_outside_the_repository_is_not_read_for_a_projection() {
    let dir = repo_with("outside", PROJECTION);
    let outside = temp_repo("outside-source");
    let model = "projects/helsinki/spaces/fleet/datamodels";
    std::fs::rename(
        dir.join(model).join("fleet.linkml.yaml"),
        outside.join("fleet.linkml.yaml"),
    )
    .expect("move the source out");
    let manifest = std::fs::read_to_string(dir.join(model).join("fleet.yaml")).expect("model");
    write(
        &dir,
        &format!("{model}/fleet.yaml"),
        &manifest.replace(
            "linkml: ./fleet.linkml.yaml",
            &format!(
                "linkml: ../../../../../../../../..{}/fleet.linkml.yaml",
                outside.display()
            ),
        ),
    );
    let report = validate::run(&dir);
    assert!(
        report.findings.iter().any(
            |f| f.path == Path::new(PROJECTION_PATH) && f.message.contains("does not validate")
        ),
        "{:?}",
        report.findings
    );
    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_dir_all(&outside);
}
