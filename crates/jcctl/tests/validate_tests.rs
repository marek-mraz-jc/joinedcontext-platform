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

// --- T-2527: the edge cases of `run` and its reference checks (PL-39, PF-49, PF-68, PF-69) ---

/// A pipeline of `project` whose source is `reference` (YAML), at `projects/{project}/…`.
fn pipeline(dir: &Path, project: &str, reference: &str) {
    write(
        dir,
        &format!("projects/{project}/pipelines/ingest.yaml"),
        &format!(
            "apiVersion: joinedcontext.com/v1alpha1\nkind: Pipeline\nmetadata:\n  name: ingest\n  namespace: {project}\nspec:\n  source:\n    dataSourceRef: {reference}\n"
        ),
    );
}

/// A DataSource named `name` in `project`.
fn data_source(dir: &Path, project: &str, name: &str) {
    write(
        dir,
        &format!("projects/{project}/datasources/{name}.yaml"),
        &format!(
            "apiVersion: joinedcontext.com/v1alpha1\nkind: DataSource\nmetadata:\n  name: {name}\n  namespace: {project}\nspec:\n  type: http\n  http:\n    url: https://opendata.banskabystrica.sk/aq.json\n"
        ),
    );
}

fn with_pl39(report: &validate::Report) -> Vec<&str> {
    report
        .findings
        .iter()
        .filter(|f| f.message.contains("PL-39"))
        .map(|f| f.message.as_str())
        .collect()
}

/// PL-39: a connection belongs to the team holding its credentials, so a typed reference is
/// looked up in the pipeline's own project, whatever namespace it names.
#[test]
fn a_typed_data_source_ref_is_scoped_to_the_pipelines_own_project() {
    let dir = valid_repo("pl39-foreign");
    write(&dir, "projects/doprava/project.yaml", "apiVersion: joinedcontext.com/v1alpha1\nkind: Project\nmetadata:\n  name: doprava\n  namespace: org\nspec:\n  organizationRef: banskabystrica\n");
    data_source(&dir, "doprava", "aq-feed");
    pipeline(
        &dir,
        "ovzdusie",
        "{ kind: DataSource, name: aq-feed, namespace: doprava }",
    );
    let report = validate::run(&dir);
    let found = with_pl39(&report);
    assert_eq!(found.len(), 1, "{:?}", report.findings);
    assert!(found[0].contains("aq-feed"), "{}", found[0]);
}

/// PL-39: a reference to a DataSource of the same project is not a finding, bare or typed.
#[test]
fn a_data_source_of_the_same_project_resolves_bare_or_typed() {
    for (n, reference) in ["aq-feed", "{ kind: DataSource, name: aq-feed }"]
        .iter()
        .enumerate()
    {
        let dir = valid_repo(&format!("pl39-same-{n}"));
        data_source(&dir, "ovzdusie", "aq-feed");
        pipeline(&dir, "ovzdusie", reference);
        let report = validate::run(&dir);
        assert!(
            with_pl39(&report).is_empty(),
            "{reference}: {:?}",
            report.findings
        );
    }
}

/// PL-39: a reference to a DataSource nobody declares is a finding at the pipeline, naming it.
#[test]
fn a_pipeline_referencing_no_such_data_source_is_reported_at_the_pipeline() {
    let dir = valid_repo("pl39-missing");
    pipeline(&dir, "ovzdusie", "no-such-feed");
    let report = validate::run(&dir);
    let finding = report
        .findings
        .iter()
        .find(|f| f.message.contains("PL-39"))
        .unwrap_or_else(|| panic!("{:?}", report.findings));
    assert!(
        finding.message.contains("no-such-feed"),
        "{}",
        finding.message
    );
    assert_eq!(
        finding.path,
        PathBuf::from("projects/ovzdusie/pipelines/ingest.yaml")
    );
}

/// PL-39: a reference that is neither a name nor an object is the kind's finding, not a panic
/// and not a second finding.
#[test]
fn a_data_source_ref_of_the_wrong_shape_is_one_finding_and_no_panic() {
    let dir = valid_repo("pl39-shape");
    pipeline(&dir, "ovzdusie", "42");
    let report = validate::run(&dir);
    assert!(with_pl39(&report).is_empty(), "{:?}", report.findings);
    assert!(
        report
            .findings
            .iter()
            .any(|f| f.path == Path::new("projects/ovzdusie/pipelines/ingest.yaml")),
        "{:?}",
        report.findings
    );
}

/// PF-49: a binding naming a role nobody declares is reported at the binding's own file.
#[test]
fn a_role_binding_naming_a_missing_role_is_reported_at_the_binding() {
    let dir = valid_repo("missing-role");
    write(
        &dir,
        ROLE_BINDING_PATH,
        &ROLE_BINDING.replace("role: pipeline-developer", "role: no-such-role"),
    );
    let report = validate::run(&dir);
    let finding = report
        .findings
        .iter()
        .find(|f| f.message.contains("no-such-role"))
        .unwrap_or_else(|| panic!("{:?}", report.findings));
    assert_eq!(finding.path, PathBuf::from(ROLE_BINDING_PATH));
}

/// PF-68: one role name in `users/` and in a project is refused at the project's role.
#[test]
fn a_role_name_declared_twice_is_reported_at_the_project_role() {
    let dir = valid_repo("role-clash");
    let project_role = "projects/ovzdusie/roles/pipeline-developer.yaml";
    write(
        &dir,
        project_role,
        &ROLE.replace("namespace: org", "namespace: ovzdusie"),
    );
    let report = validate::run(&dir);
    let clash: Vec<_> = report
        .findings
        .iter()
        .filter(|f| f.message.contains("pipeline-developer"))
        .collect();
    assert!(!clash.is_empty(), "{:?}", report.findings);
    assert!(
        clash
            .iter()
            .any(|f| f.path == Path::new(project_role) || f.path == Path::new("users")),
        "{clash:?}"
    );
}

/// PF-49: a repository without roles and bindings has no role finding, and no `users/` folder
/// is needed for that.
#[test]
fn no_role_or_rolebinding_manifest_means_no_role_finding() {
    let dir = valid_repo("no-roles");
    std::fs::remove_dir_all(dir.join("users")).expect("remove users");
    let report = validate::run(&dir);
    assert!(
        report
            .findings
            .iter()
            .all(|f| !f.message.to_lowercase().contains("role")),
        "{:?}",
        report.findings
    );
}

/// CC-12: a repository that does not load is exactly one finding, the one that stopped it.
#[test]
fn a_repository_that_fails_to_load_yields_exactly_one_finding() {
    let dir = valid_repo("unloadable");
    write(
        &dir,
        "projects/ovzdusie/broken.yaml",
        "apiVersion: joinedcontext.com/v1alpha1\nkind: [\n",
    );
    pipeline(&dir, "ovzdusie", "no-such-feed");
    let report = validate::run(&dir);
    assert_eq!(report.findings.len(), 1, "{:?}", report.findings);
    assert_eq!(report.checked, 0);
}

/// CC-12: every class of finding is reported in one run, none stops the others.
#[test]
fn run_reports_every_class_of_finding_at_once() {
    let dir = valid_repo("everything");
    pipeline(&dir, "ovzdusie", "no-such-feed");
    write(
        &dir,
        ROLE_BINDING_PATH,
        &ROLE_BINDING.replace("{ group: air-quality-team }", "{ group: nobody-declared }"),
    );
    std::fs::create_dir_all(dir.join("projects/orphan"))
        .expect("a project folder with no manifest");
    let report = validate::run(&dir);
    let said = |needle: &str| report.findings.iter().any(|f| f.message.contains(needle));
    assert!(said("PL-39"), "{:?}", report.findings);
    assert!(said("nobody-declared"), "{:?}", report.findings);
    assert!(said("orphan"), "{:?}", report.findings);
}

mod federation_topology {
    //! CC-13: `jcctl validate` checks the federation topology the registrations declare.
    use super::*;

    fn space(dir: &Path, name: &str) {
        write(
            dir,
            &format!("projects/ovzdusie/spaces/{name}/space.yaml"),
            &format!("apiVersion: joinedcontext.com/v1alpha1\nkind: ContextSpace\nmetadata:\n  name: {name}\n  namespace: ovzdusie\nspec:\n  isSandbox: false\n"),
        );
    }

    fn endpoint(dir: &Path, name: &str, space: &str, slug: &str) {
        write(
            dir,
            &format!("projects/ovzdusie/spaces/{space}/endpoints/{name}.yaml"),
            &format!("apiVersion: joinedcontext.com/v1alpha1\nkind: Endpoint\nmetadata:\n  name: {name}\n  namespace: ovzdusie\nspec:\n  contextSpaceRef: {space}\n  slug: {slug}\n  audience: organization\n  enabledRepresentations: [\"ngsi-ld\"]\n"),
        );
    }

    fn registration(dir: &Path, name: &str, hub: &str, member: &str, id_pattern: Option<&str>) {
        let pattern = id_pattern
            .map(|p| format!("\n          idPattern: \"{p}\""))
            .unwrap_or_default();
        write(
            dir,
            &format!("projects/ovzdusie/spaces/{hub}/registrations/{name}.yaml"),
            &format!("apiVersion: joinedcontext.com/v1alpha1\nkind: ContextSourceRegistration\nmetadata:\n  name: {name}\n  namespace: ovzdusie\nspec:\n  contextSpaceRef: {hub}\n  endpointRef: {{ kind: Endpoint, name: {member} }}\n  information:\n    - entities:\n        - type: Vehicle{pattern}\n  federation:\n    identity: caller\n"),
        );
    }

    /// A hub, served by its own Endpoint, over one member space.
    fn hub_repo(test: &str) -> PathBuf {
        let dir = valid_repo(test);
        space(&dir, "hub");
        space(&dir, "transport");
        endpoint(&dir, "hub-read", "hub", "a2bcdefghijklmnopqrstuvwxy");
        endpoint(
            &dir,
            "transport-read",
            "transport",
            "b2bcdefghijklmnopqrstuvwxy",
        );
        registration(&dir, "transport", "hub", "transport-read", None);
        dir
    }

    fn messages(dir: &Path) -> Vec<String> {
        validate::run(dir)
            .findings
            .into_iter()
            .map(|f| f.message)
            .collect()
    }

    /// CC-13: a hub over a member, served by its own Endpoint, is a topology with no finding.
    #[test]
    fn a_served_hub_over_a_declared_member_is_clean() {
        let dir = hub_repo("cc13-clean");
        assert_eq!(messages(&dir), Vec::<String>::new());
    }

    /// A test name, what breaks the topology, and the words its finding carries.
    type Breakage = (&'static str, fn(&Path), &'static str);

    /// CC-13: every broken edge is named where it is declared, one finding each.
    #[test]
    fn every_broken_edge_of_the_topology_is_a_finding() {
        let cases: [Breakage; 5] = [
            (
                "cc13-dangling",
                |dir| registration(dir, "ghost", "hub", "no-such-endpoint", None),
                "a dangling peer",
            ),
            (
                "cc13-no-hub",
                |dir| registration(dir, "lost", "nowhere", "transport-read", None),
                "which no ContextSpace of this project declares",
            ),
            (
                "cc13-unserved",
                |dir| {
                    space(dir, "silent");
                    registration(dir, "unread", "silent", "transport-read", None);
                },
                "no Endpoint of this project serves it",
            ),
            (
                "cc13-loop",
                |dir| registration(dir, "back", "transport", "hub-read", None),
                "a federation loop",
            ),
            (
                "cc13-unanchored",
                |dir| {
                    registration(
                        dir,
                        "wide",
                        "hub",
                        "transport-read",
                        Some("urn:ngsi-ld:Vehicle:.*"),
                    )
                },
                "anchored",
            ),
        ];
        for (test, break_it, expected) in cases {
            let dir = hub_repo(test);
            break_it(&dir);
            let said = messages(&dir);
            assert!(
                said.iter().any(|m| m.contains(expected)),
                "{test}: {said:?}"
            );
        }
    }
}

// --- T-2591: the roles of an App (AP-91, AP-98, PF-64) ---

const ROLES_APP_PATH: &str = "projects/ovzdusie/apps/alerts/app.yaml";

/// An App of ovzdusie whose editors are `members` and whose note write is gated by `gate`.
fn roles_app(members: &str, gate: &str) -> String {
    format!(
        r#"apiVersion: joinedcontext.com/v1alpha1
kind: App
metadata:
  name: alerts
  namespace: ovzdusie
spec:
  kind: static
  source: {{ path: ./src }}
  build: {{ node: "22" }}
  visibility: roles
  roles:
    - {{ name: editor, description: "Writes the note" }}
  access:
    - {{ role: editor, subjects: [{members}] }}
  dataNeeds:
    - contextSpaceRef: {{ kind: ContextSpace, name: ovzdusie }}
      types: [Alert]
      operations: [queryEntity, updateAttrs]
{gate}"#
    )
}

fn messages(findings: &[validate::Finding]) -> Vec<&str> {
    findings.iter().map(|f| f.message.as_str()).collect()
}

/// AP-91, PF-64: an App role's group names a Group manifest, as a binding's does; a declared
/// group and an address of the organization pass.
#[test]
fn an_app_role_naming_an_undeclared_group_is_refused_and_a_declared_one_is_not() {
    let dir = valid_repo("app-role-groups");
    let gate = "      roles: [editor]\n";
    write(
        &dir,
        ROLES_APP_PATH,
        &roles_app(
            "{ group: air-quality-team }, { user: jana@banskabystrica.sk }",
            gate,
        ),
    );
    let report = validate::run(&dir);
    assert_eq!(
        report.findings,
        vec![],
        "a declared group and an address of the organization"
    );

    write(
        &dir,
        ROLES_APP_PATH,
        &roles_app("{ group: nobody-declared }", gate),
    );
    let report = validate::run(&dir);
    let found = messages(&report.findings);
    assert!(
        found
            .iter()
            .any(|m| m.contains("group `nobody-declared`") && m.contains("PF-64")),
        "{found:?}"
    );
    assert!(
        report
            .findings
            .iter()
            .all(|f| f.path.ends_with(ROLES_APP_PATH)),
        "{found:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// AP-91, PF-41: a role member outside the organization's domain is refused; a subdomain of it
/// is the organization's.
#[test]
fn an_app_role_member_outside_the_organizations_domain_is_refused() {
    let dir = valid_repo("app-role-domain");
    let gate = "      roles: [editor]\n";
    for (member, outside) in [
        ("jana@mesto.banskabystrica.sk", false),
        ("jana@example.org", true),
        ("jana@banskabystrica.sk.evil.org", true),
        ("jana@notbanskabystrica.sk", true),
    ] {
        write(
            &dir,
            ROLES_APP_PATH,
            &roles_app(&format!("{{ user: {member} }}"), gate),
        );
        let report = validate::run(&dir);
        let found = messages(&report.findings);
        let said = found
            .iter()
            .any(|m| m.contains(member) && m.contains("AP-91"));
        assert_eq!(said, outside, "{member}: {found:?}");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// AP-98: a write every person who can open the app may make is valid and is said as a warning,
/// naming the app, the operation and the type; gating it by a role silences it.
#[test]
fn a_write_open_to_everyone_is_a_warning_and_a_role_gated_one_is_not() {
    let dir = valid_repo("app-role-writes");
    write(
        &dir,
        ROLES_APP_PATH,
        &roles_app("{ group: air-quality-team }", ""),
    );
    let report = validate::run(&dir);
    assert_eq!(report.findings, vec![]);
    let warned = messages(&report.warnings);
    assert!(
        warned.iter().any(
            |m| m.contains("everyone who can open alerts can updateAttrs Alert")
                && m.contains("AP-98")
        ),
        "{warned:?}"
    );

    write(
        &dir,
        ROLES_APP_PATH,
        &roles_app("{ group: air-quality-team }", "      roles: [editor]\n"),
    );
    let report = validate::run(&dir);
    assert!(
        !messages(&report.warnings)
            .iter()
            .any(|m| m.contains("AP-98")),
        "{:?}",
        report.warnings
    );
    let _ = std::fs::remove_dir_all(&dir);
}

fn project(dir: &Path, name: &str) {
    write(
        dir,
        &format!("projects/{name}/project.yaml"),
        &format!(
            "apiVersion: joinedcontext.com/v1alpha1\nkind: Project\nmetadata:\n  name: {name}\n  \
             namespace: org\nspec:\n  organizationRef: banskabystrica\n"
        ),
    );
}

fn space(dir: &Path, project: &str, name: &str, pin: Option<&str>) {
    let pin = pin.map_or(String::new(), |pin| format!("  urnSegment: {pin}\n"));
    write(
        dir,
        &format!("projects/{project}/spaces/{name}/space.yaml"),
        &format!(
            "apiVersion: joinedcontext.com/v1alpha1\nkind: ContextSpace\nmetadata:\n  name: \
             {name}\n  namespace: {project}\nspec:\n  isSandbox: false\n{pin}"
        ),
    );
}

fn segment_clashes(dir: &Path) -> Vec<String> {
    validate::run(dir)
        .findings
        .into_iter()
        .map(|f| f.message)
        .filter(|m| m.contains("PF-44"))
        .collect()
}

/// PF-44, AP-115: two projects' spaces rendering one id segment are refused at each manifest,
/// naming both projects: `a-b`/`c` and `a`/`b-c` both render `a-b-c`, and so do two equal pins.
#[test]
fn two_projects_spaces_rendering_one_id_segment_are_refused_naming_both() {
    let dir = valid_repo("space-segment-clash");
    for name in ["mesto", "mesto-doprava"] {
        project(&dir, name);
    }
    space(&dir, "mesto-doprava", "linky", None);
    space(&dir, "mesto", "doprava-linky", None);
    let found = segment_clashes(&dir);
    assert_eq!(found.len(), 2, "each manifest is located: {found:?}");
    for message in &found {
        assert!(
            message.contains("'mesto-doprava-linky'")
                && message.contains("mesto and mesto-doprava"),
            "names both: {message}"
        );
    }

    // A pin moves one of them off the segment; pinning the other's segment clashes again.
    space(&dir, "mesto", "doprava-linky", Some("mesto-linky"));
    assert!(
        segment_clashes(&dir).is_empty(),
        "{:?}",
        segment_clashes(&dir)
    );
    space(&dir, "ovzdusie", "ovzdusie", Some("mesto-linky"));
    let found = segment_clashes(&dir);
    assert_eq!(found.len(), 2, "{found:?}");
    assert!(found[0].contains("mesto and ovzdusie"), "{found:?}");

    let _ = std::fs::remove_dir_all(&dir);
}
