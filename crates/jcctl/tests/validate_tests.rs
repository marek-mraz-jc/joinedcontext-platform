use jcctl::commands::validate;
use std::path::{Path, PathBuf};

fn temp_repo(test_name: &str) -> PathBuf {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock after the epoch")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "jcctl-validate-{test_name}-{}-{now}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create temp repo");
    dir
}

fn write(dir: &Path, rel: &str, body: &str) {
    let path = dir.join(rel);
    std::fs::create_dir_all(path.parent().expect("relative path has a parent"))
        .expect("create parent directory");
    std::fs::write(path, body).expect("write manifest");
}

/// The demo repository: an organization, its project, one space and one endpoint, each at
/// the path its kind prescribes (MF-06).
fn valid_repo(test_name: &str) -> PathBuf {
    let dir = temp_repo(test_name);
    write(
        &dir,
        "org.yaml",
        r#"apiVersion: joinedcontext.com/v1alpha1
kind: Organization
metadata:
  name: banskabystrica
  namespace: org
spec:
  domain: banskabystrica.sk
  locales: ["sk"]
  defaultLocale: sk
"#,
    );
    write(
        &dir,
        "projects/ovzdusie/project.yaml",
        r#"apiVersion: joinedcontext.com/v1alpha1
kind: Project
metadata:
  name: ovzdusie
  namespace: org
spec:
  organizationRef: banskabystrica
"#,
    );
    write(
        &dir,
        "projects/ovzdusie/spaces/ovzdusie/space.yaml",
        r#"apiVersion: joinedcontext.com/v1alpha1
kind: ContextSpace
metadata:
  name: ovzdusie
  namespace: ovzdusie
spec:
  isSandbox: false
"#,
    );
    write(&dir, ENDPOINT_PATH, ENDPOINT);
    write(&dir, "users/roles/pipeline-developer.yaml", ROLE);
    write(&dir, "users/groups/air-quality-team.yaml", GROUP);
    write(&dir, ROLE_BINDING_PATH, ROLE_BINDING);
    dir
}

/// The group the binding names: a binding to a group no manifest declares matches nobody (PF-64).
const GROUP: &str = r#"apiVersion: joinedcontext.com/v1alpha1
kind: Group
metadata:
  name: air-quality-team
  namespace: org
spec:
  description: The air quality domain, measurement and modelling
  members:
    - { user: jana.kovacova@banskabystrica.sk }
"#;

const ROLE: &str = r#"apiVersion: joinedcontext.com/v1alpha1
kind: Role
metadata:
  name: pipeline-developer
  namespace: org
spec:
  rules:
    - kinds: [Pipeline, DataSource, Mapping]
      verbs: [propose]
    - kinds: [Endpoint]
      verbs: [propose]
      constraints:
        - { field: spec.audience, notIn: [public] }
"#;

const ROLE_BINDING_PATH: &str = "users/assignments/ovzdusie-developers.yaml";

const ROLE_BINDING: &str = r#"apiVersion: joinedcontext.com/v1alpha1
kind: RoleBinding
metadata:
  name: ovzdusie-developers
  namespace: org
spec:
  subjects: [{ group: air-quality-team }, { user: jana.kovacova@banskabystrica.sk }]
  role: pipeline-developer
  scope: { project: ovzdusie }
  validity: { notAfter: "2026-12-31T23:59:59Z" }
"#;

/// PF-49: `users/` is validated like everything else; a binding with two scopes is refused
/// with the file that carries it.
#[test]
fn a_binding_with_two_scopes_is_refused_from_users() {
    let dir = valid_repo("two-scopes");
    write(
        &dir,
        ROLE_BINDING_PATH,
        &ROLE_BINDING.replace(
            "scope: { project: ovzdusie }",
            "scope: { project: ovzdusie, organization: banskabystrica }",
        ),
    );

    let report = validate::run(&dir);
    assert_eq!(report.findings.len(), 1, "{:?}", report.findings);
    assert_eq!(report.findings[0].path, Path::new(ROLE_BINDING_PATH));
    assert!(
        report.findings[0].message.contains("exactly one"),
        "{}",
        report.findings[0].message
    );

    let _ = std::fs::remove_dir_all(&dir);
}

const ENDPOINT_PATH: &str = "projects/ovzdusie/spaces/ovzdusie/endpoints/public-air.yaml";

const ENDPOINT: &str = r#"apiVersion: joinedcontext.com/v1alpha1
kind: Endpoint
metadata:
  name: public-air
  namespace: ovzdusie
spec:
  contextSpaceRef: ovzdusie
  slug: zt4qm7ge2xdv6ksb3ncf5arw2y
  audience: public
  enabledRepresentations: ["ngsi-ld", "geojson"]
"#;

#[test]
fn a_valid_repository_has_no_findings() {
    let dir = valid_repo("valid");

    let report = validate::run(&dir);
    assert_eq!(report.findings, vec![]);
    assert_eq!(report.checked, 7);
    assert!(report.is_valid());

    let _ = std::fs::remove_dir_all(&dir);
}

/// An invariant no JSON Schema can express: a public endpoint that also lists projects
/// would silently narrow its own audience (EP-14, EP-15).
#[test]
fn a_cross_field_invariant_is_reported_with_the_file_that_broke_it() {
    let dir = valid_repo("invariant");
    write(
        &dir,
        ENDPOINT_PATH,
        &ENDPOINT.replace(
            "  audience: public\n",
            "  audience: public\n  allowedProjects: [\"bb-doprava\"]\n",
        ),
    );

    let report = validate::run(&dir);
    assert_eq!(report.checked, 6);
    assert_eq!(report.findings.len(), 1);

    let finding = &report.findings[0];
    assert_eq!(finding.path, Path::new(ENDPOINT_PATH));
    assert_eq!(finding.line, 1);
    assert!(
        finding.message.contains("allowedProjects"),
        "{}",
        finding.message
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// `deny_unknown_fields` is the control that keeps a credential out of Git: a token
/// written beside the `secretRef` that should carry it fails to parse at all.
#[test]
fn a_field_that_is_not_in_the_kind_is_refused() {
    let dir = valid_repo("unknown-field");
    write(
        &dir,
        ENDPOINT_PATH,
        &ENDPOINT.replace(
            "  audience: public\n",
            "  audience: public\n  token: hunter2\n",
        ),
    );

    let report = validate::run(&dir);
    assert_eq!(report.checked, 6);
    assert_eq!(report.findings.len(), 1);
    assert!(
        report.findings[0].message.contains("token"),
        "{}",
        report.findings[0].message
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// A manifest that parses but sits in the wrong folder is still a defect: the path is the
/// resource's address in the repository (MF-06).
#[test]
fn a_manifest_at_the_wrong_path_is_reported() {
    let dir = valid_repo("misplaced");
    std::fs::remove_file(dir.join(ENDPOINT_PATH)).expect("move the endpoint");
    write(&dir, "projects/ovzdusie/public-air.yaml", ENDPOINT);

    let report = validate::run(&dir);
    assert_eq!(report.checked, 7);
    assert_eq!(report.findings.len(), 1);
    assert!(
        report.findings[0].message.contains(ENDPOINT_PATH),
        "{}",
        report.findings[0].message
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// A document that is not a manifest at all stops the walk, and the finding still points
/// at the file, document and line.
#[test]
fn a_malformed_document_reports_its_line() {
    let dir = valid_repo("malformed");
    write(
        &dir,
        "projects/ovzdusie/spaces/ovzdusie/space.yaml",
        "apiVersion: joinedcontext.com/v1alpha1\nkind: ContextSpace\nmetadata: [not, a, map]\nspec: {}\n",
    );

    let report = validate::run(&dir);
    assert!(!report.is_valid());
    assert_eq!(report.checked, 0);
    assert_eq!(report.findings.len(), 1);
    assert_eq!(report.findings[0].document, 1);

    let _ = std::fs::remove_dir_all(&dir);
}

/// A project directory with no `project.yaml` (MF-01, PF-05): the spaces under it load, so
/// nothing else says the project itself was never declared (T-0902).
#[test]
fn a_project_directory_without_its_manifest_is_reported() {
    let dir = valid_repo("no-project-manifest");
    write(
        &dir,
        "projects/mobilita/spaces/mobilita/space.yaml",
        r#"apiVersion: joinedcontext.com/v1alpha1
kind: ContextSpace
metadata:
  name: mobilita
  namespace: mobilita
spec:
  isSandbox: true
"#,
    );

    let report = validate::run(&dir);
    let finding = report
        .findings
        .iter()
        .find(|f| f.path == Path::new("projects/mobilita"))
        .unwrap_or_else(|| panic!("the directory is named: {:?}", report.findings));
    assert!(
        finding.message.contains("projects/mobilita/project.yaml"),
        "{}",
        finding.message
    );
    assert!(!report.is_valid());

    let _ = std::fs::remove_dir_all(&dir);
}

/// A binding to a group no manifest declares (PF-62, PF-64): it matches nobody, and nothing but
/// this says so.
#[test]
fn a_binding_to_an_undeclared_group_is_refused_and_a_declared_one_is_not() {
    let dir = valid_repo("undeclared-group");
    write(
        &dir,
        "users/roles/pipeline-developer.yaml",
        r#"apiVersion: joinedcontext.com/v1alpha1
kind: Role
metadata:
  name: pipeline-developer
  namespace: org
spec:
  rules:
    - kinds: [Pipeline]
      verbs: [propose]
"#,
    );
    write(
        &dir,
        "users/assignments/ovzdusie-developers.yaml",
        r#"apiVersion: joinedcontext.com/v1alpha1
kind: RoleBinding
metadata:
  name: ovzdusie-developers
  namespace: org
spec:
  subjects:
    - group: nobody
  role: pipeline-developer
  scope: { project: ovzdusie }
"#,
    );

    let report = validate::run(&dir);
    let finding = report
        .findings
        .iter()
        .find(|f| f.message.contains("group `nobody`"))
        .unwrap_or_else(|| panic!("the group is named: {:?}", report.findings));
    assert!(
        finding.message.contains("users/groups/nobody.yaml"),
        "{}",
        finding.message
    );

    write(
        &dir,
        "users/groups/nobody.yaml",
        r#"apiVersion: joinedcontext.com/v1alpha1
kind: Group
metadata:
  name: nobody
  namespace: org
spec:
  description: The team that maintains the air quality pipelines
  members:
    - { user: jana.kovacova@banskabystrica.sk }
"#,
    );
    let report = validate::run(&dir);
    assert_eq!(report.findings, vec![], "the declared group is accepted");

    let _ = std::fs::remove_dir_all(&dir);
}
