//! T-0527: the bindings under users/ compiled into CODEOWNERS, roles.json and the gate input
//! (PF-51, PF-52, CC-41, CC-42).

mod common;

use common::*;
use jcctl::roles::{self, RolesError};
use std::path::Path;
use std::process::Command;

const ROLE_DEVELOPER: &str = r#"apiVersion: joinedcontext.com/v1alpha1
kind: Role
metadata: { name: pipeline-developer, namespace: org }
spec:
  rules:
    - kinds: [Pipeline, DataSource, Mapping]
      verbs: [propose]
    - kinds: [Endpoint]
      verbs: [propose]
      constraints:
        - { field: spec.audience, notIn: [public] }
"#;

const ROLE_ADMIN: &str = r#"apiVersion: joinedcontext.com/v1alpha1
kind: Role
metadata: { name: org-admin, namespace: org }
spec:
  rules:
    - kinds: [Endpoint, Pipeline, Role, RoleBinding]
      verbs: [propose, approve, delete]
"#;

const BINDING_DEVELOPERS: &str = r#"apiVersion: joinedcontext.com/v1alpha1
kind: RoleBinding
metadata: { name: ovzdusie-developers, namespace: org }
spec:
  subjects: [{ group: air-quality-team }, { user: jana.kovacova@banskabystrica.sk }]
  role: pipeline-developer
  scope: { project: ovzdusie }
"#;

const BINDING_ADMINS: &str = r#"apiVersion: joinedcontext.com/v1alpha1
kind: RoleBinding
metadata: { name: admins, namespace: org }
spec:
  subjects: [{ user: admin }, { group: platform-admins }]
  role: org-admin
  scope: { organization: banskabystrica }
"#;

const BINDING_SPACE_REVIEWERS: &str = r#"apiVersion: joinedcontext.com/v1alpha1
kind: RoleBinding
metadata: { name: parking-reviewers, namespace: org }
spec:
  subjects: [{ user: peter }]
  role: org-admin
  scope: { contextSpace: parking }
"#;

fn users_repo(test_name: &str) -> std::path::PathBuf {
    let dir = demo_repo(test_name);
    write(&dir, "users/roles/pipeline-developer.yaml", ROLE_DEVELOPER);
    write(&dir, "users/roles/org-admin.yaml", ROLE_ADMIN);
    write(
        &dir,
        "users/assignments/ovzdusie-developers.yaml",
        BINDING_DEVELOPERS,
    );
    write(&dir, "users/assignments/admins.yaml", BINDING_ADMINS);
    write(
        &dir,
        "users/assignments/parking-reviewers.yaml",
        BINDING_SPACE_REVIEWERS,
    );
    dir
}

#[test]
fn codeowners_gives_the_rule_setting_paths_to_organization_approvers_only() {
    let dir = users_repo("codeowners");
    let compiled = roles::compile(&load(&dir)).expect("compiles");
    let owners = &compiled.codeowners;

    // Organization scope owns everything and alone owns what sets the rules (CC-42, CC-70).
    for path in [
        "/CODEOWNERS",
        "/users/",
        "/platform/",
        "/policies/",
        "/.gitea/",
        "/",
    ] {
        assert!(
            owners.contains(&format!("{path} @admin @banskabystrica/platform-admins\n")),
            "{path} is owned by the organization approvers:\n{owners}"
        );
    }
    // A developer proposes but never approves: no line of theirs, on any path.
    assert!(!owners.contains("air-quality-team"), "{owners}");
    assert!(!owners.contains("jana"), "{owners}");
    // A context space binding that approves owns the space under whichever project holds it.
    assert!(
        owners.contains("/projects/*/spaces/parking/ @peter\n"),
        "{owners}"
    );
    assert!(owners.starts_with("# Written by `jcctl roles render`"));

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn roles_json_carries_the_bindings_as_the_gate_reads_them() {
    let dir = users_repo("roles-json");
    let compiled = roles::compile(&load(&dir)).expect("compiles");
    let data: serde_json::Value = serde_json::from_str(&compiled.roles_json).expect("json");

    assert_eq!(
        data["roles"]["pipeline-developer"]["rules"][1]["constraints"][0]["notIn"][0],
        "public"
    );
    let bindings = data["bindings"].as_array().expect("bindings");
    assert_eq!(bindings.len(), 3);
    assert_eq!(bindings[0]["name"], "admins", "sorted by name");
    assert_eq!(bindings[0]["scope"]["organization"], "banskabystrica");
    assert_eq!(bindings[1]["subjects"][0]["group"], "air-quality-team");
    assert_eq!(bindings[2]["scope"]["contextSpace"], "parking");
    assert!(compiled.roles_json.ends_with('\n'));

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_binding_to_a_role_the_repository_lacks_is_refused() {
    let dir = demo_repo("missing-role");
    write(&dir, "users/assignments/admins.yaml", BINDING_ADMINS);
    let err = roles::compile(&load(&dir)).expect_err("no such role");
    assert!(
        matches!(err, RolesError::MissingRole { ref role, .. } if role == "org-admin"),
        "{err}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn render_writes_the_five_files_and_is_idempotent() {
    let dir = users_repo("render");
    let written = roles::render(&dir).expect("renders");
    assert_eq!(written.len(), 5);
    for rel in [
        roles::CODEOWNERS,
        roles::ROLES_JSON,
        roles::ROLES_REGO,
        roles::ROLES_TEST_REGO,
        roles::WORKFLOW,
    ] {
        assert!(dir.join(rel).is_file(), "{rel} exists");
    }
    let first = std::fs::read_to_string(dir.join(roles::CODEOWNERS)).expect("read");
    roles::render(&dir).expect("renders again");
    assert_eq!(
        first,
        std::fs::read_to_string(dir.join(roles::CODEOWNERS)).expect("read")
    );
    let workflow = std::fs::read_to_string(dir.join(roles::WORKFLOW)).expect("workflow");
    assert!(
        workflow.contains("validate --repo-dir /repo"),
        "jcctl validate runs at the repository root"
    );
    assert!(workflow.contains("conftest test /tmp/input.json -p policies -d policies/roles.json"));

    let _ = std::fs::remove_dir_all(&dir);
}

const PUBLIC_ENDPOINT: &str = r#"apiVersion: joinedcontext.com/v1alpha1
kind: Endpoint
metadata:
  name: air
  namespace: ovzdusie
spec:
  contextSpaceRef: ovzdusie
  slug: mluyob4nz52lok3ssk7pgn5vwt
  audience: public
  enabledRepresentations: ["ngsi-ld"]
"#;

const PIPELINE: &str = r#"apiVersion: joinedcontext.com/v1alpha1
kind: Pipeline
metadata:
  name: aq
  namespace: ovzdusie
spec:
  class: resident
  source: { dataSourceRef: { kind: DataSource, name: mqtt-mesto } }
  compute: { kind: bloblang, bloblang: "root = this" }
  targetEndpoint: urn:ngsi-ld:Endpoint:banskabystrica.sk:ovzdusie:public-air
"#;

/// The diff of a merge request as `git diff --name-status base...HEAD` lists it.
const NAME_STATUS: &str = "A\tprojects/ovzdusie/spaces/ovzdusie/endpoints/air.yaml\nD\tprojects/ovzdusie/pipelines/aq/pipeline.yaml\nM\tREADME.md\nM\tprojects/ovzdusie/spaces/ovzdusie/datamodels/x.linkml.yaml\n";

fn merge_request(test_name: &str) -> (std::path::PathBuf, std::path::PathBuf) {
    let head = users_repo(test_name);
    write(
        &head,
        "projects/ovzdusie/spaces/ovzdusie/endpoints/air.yaml",
        PUBLIC_ENDPOINT,
    );
    write(&head, "README.md", "# repo\n");
    let base = temp_dir(&format!("{test_name}-base"));
    write(
        &base,
        "projects/ovzdusie/pipelines/aq/pipeline.yaml",
        PIPELINE,
    );
    (head, base)
}

#[test]
fn input_lists_added_manifests_from_head_and_deleted_ones_from_base() {
    let (head, base) = merge_request("input");
    let input = roles::input(
        &head,
        &base,
        NAME_STATUS,
        "jana",
        Some("jana.kovacova@banskabystrica.sk"),
        &["air-quality-team".to_owned()],
    )
    .expect("input");
    assert_eq!(input.author, "jana");
    assert_eq!(input.groups, vec!["air-quality-team"]);
    assert_eq!(
        input.changes.len(),
        2,
        "README and the LinkML source are not manifests: {:?}",
        input.changes
    );
    let endpoint = &input.changes[0];
    assert_eq!(
        (
            endpoint.action,
            endpoint.kind.as_str(),
            endpoint.name.as_str()
        ),
        ("propose", "Endpoint", "air")
    );
    assert_eq!(endpoint.project.as_deref(), Some("ovzdusie"));
    assert_eq!(endpoint.manifest["spec"]["audience"], "public");
    let pipeline = &input.changes[1];
    assert_eq!(
        (pipeline.action, pipeline.kind.as_str()),
        ("delete", "Pipeline")
    );

    let _ = std::fs::remove_dir_all(&head);
    let _ = std::fs::remove_dir_all(&base);
}

/// The whole gate, when conftest is installed (CI installs it; a sandbox may not).
#[test]
fn conftest_denies_a_developers_public_endpoint_and_lets_an_admin_through() {
    let Ok(conftest) = which("conftest") else {
        eprintln!("conftest is not installed: skipping the gate run");
        return;
    };
    let (head, base) = merge_request("gate");
    roles::render(&head).expect("renders");
    let verify = Command::new(&conftest)
        .current_dir(&head)
        .args(["verify", "-p", "policies"])
        .output()
        .expect("conftest verify");
    assert!(
        verify.status.success(),
        "{}",
        String::from_utf8_lossy(&verify.stdout)
    );

    let run = |author: &str, groups: &[String]| {
        let input = roles::input(&head, &base, NAME_STATUS, author, None, groups).expect("input");
        let file = head.join("input.json");
        std::fs::write(&file, serde_json::to_string(&input).expect("json")).expect("write");
        Command::new(&conftest)
            .current_dir(&head)
            .args([
                "test",
                "input.json",
                "-p",
                "policies",
                "-d",
                "policies/roles.json",
            ])
            .output()
            .expect("conftest test")
    };
    let developer = run("jana", &["air-quality-team".to_owned()]);
    assert!(
        !developer.status.success(),
        "a developer's public endpoint is red"
    );
    let out = String::from_utf8_lossy(&developer.stdout);
    assert!(
        out.contains("Endpoint") && out.contains("Pipeline"),
        "both the public endpoint and the deletion are named:\n{out}"
    );
    let admin = run("admin", &[]);
    assert!(
        admin.status.success(),
        "{}",
        String::from_utf8_lossy(&admin.stdout)
    );

    let _ = std::fs::remove_dir_all(&head);
    let _ = std::fs::remove_dir_all(&base);
}

fn which(binary: &str) -> Result<std::path::PathBuf, ()> {
    std::env::var_os("PATH")
        .into_iter()
        .flat_map(|paths| std::env::split_paths(&paths).collect::<Vec<_>>())
        .map(|dir| dir.join(binary))
        .find(|candidate| Path::new(candidate).is_file())
        .ok_or(())
}

#[test]
fn a_repository_without_users_gets_no_files() {
    let dir = demo_repo("roles_no_users");
    assert!(roles::files(&load(&dir)).expect("compiles").is_none());
    assert!(roles::render(&dir).expect("renders").is_empty());
    assert!(!dir.join(roles::CODEOWNERS).exists());
}

// ---------------------------------------------------------------------------
// A role of a project, bound inside that project and nowhere else (PF-68, PF-69, T-0871)
// ---------------------------------------------------------------------------

const PROJECT_ROLE_ANALYST: &str = r#"apiVersion: joinedcontext.com/v1alpha1
kind: Role
metadata: { name: air-analyst, namespace: ovzdusie }
spec:
  rules:
    - kinds: [DataSource, Mapping]
      verbs: [propose]
"#;

const BINDING_ANALYSTS: &str = r#"apiVersion: joinedcontext.com/v1alpha1
kind: RoleBinding
metadata: { name: ovzdusie-analysts, namespace: org }
spec:
  subjects: [{ user: peter }]
  role: air-analyst
  scope: { project: ovzdusie }
"#;

#[test]
fn a_project_role_is_bound_in_its_own_project_and_carries_its_namespace() {
    let dir = users_repo("project-role");
    write(
        &dir,
        "projects/ovzdusie/roles/air-analyst.yaml",
        PROJECT_ROLE_ANALYST,
    );
    write(
        &dir,
        "users/assignments/ovzdusie-analysts.yaml",
        BINDING_ANALYSTS,
    );

    let compiled = roles::compile(&load(&dir)).expect("compiles");
    let data: serde_json::Value = serde_json::from_str(&compiled.roles_json).expect("json");

    // The organization's roles and the project's stay apart, so the gate knows which is which.
    assert!(data["roles"]["air-analyst"].is_null(), "{}", data["roles"]);
    assert_eq!(
        data["projectRoles"]["ovzdusie"]["air-analyst"]["rules"][0]["kinds"][0],
        "DataSource"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_project_role_bound_at_organization_scope_or_in_another_project_is_refused() {
    let at_org = r#"apiVersion: joinedcontext.com/v1alpha1
kind: RoleBinding
metadata: { name: everyone-analysts, namespace: org }
spec:
  subjects: [{ user: peter }]
  role: air-analyst
  scope: { organization: banskabystrica }
"#;
    let dir = demo_repo("project-role-at-org");
    write(
        &dir,
        "projects/ovzdusie/roles/air-analyst.yaml",
        PROJECT_ROLE_ANALYST,
    );
    write(&dir, "users/assignments/everyone-analysts.yaml", at_org);
    let err = roles::compile(&load(&dir)).expect_err("out of reach");
    assert!(
        matches!(&err, RolesError::RoleOutOfReach { role, role_project, .. }
            if role == "air-analyst" && role_project == "ovzdusie"),
        "{err}"
    );
    assert!(err.to_string().contains("organization scope"), "{err}");
    let _ = std::fs::remove_dir_all(&dir);

    let elsewhere = r#"apiVersion: joinedcontext.com/v1alpha1
kind: RoleBinding
metadata: { name: doprava-analysts, namespace: org }
spec:
  subjects: [{ user: peter }]
  role: air-analyst
  scope: { project: doprava }
"#;
    let dir = demo_repo("project-role-elsewhere");
    write(
        &dir,
        "projects/ovzdusie/roles/air-analyst.yaml",
        PROJECT_ROLE_ANALYST,
    );
    write(&dir, "users/assignments/doprava-analysts.yaml", elsewhere);
    let err = roles::compile(&load(&dir)).expect_err("out of reach");
    assert!(
        matches!(&err, RolesError::RoleOutOfReach { scope, .. } if scope.contains("doprava")),
        "{err}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_same_role_name_in_users_and_in_a_project_is_refused() {
    let clash = r#"apiVersion: joinedcontext.com/v1alpha1
kind: Role
metadata: { name: pipeline-developer, namespace: ovzdusie }
spec:
  rules:
    - kinds: [DataSource]
      verbs: [propose]
"#;
    let dir = users_repo("role-name-clash");
    write(
        &dir,
        "projects/ovzdusie/roles/pipeline-developer.yaml",
        clash,
    );
    let err = roles::compile(&load(&dir)).expect_err("a name in two places");
    assert!(
        matches!(&err, RolesError::RoleNameClash { name, project }
            if name == "pipeline-developer" && project == "ovzdusie"),
        "{err}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

// ---- T-2536: the edge cases of `roles::input`, the permission gate's view of a change ----

/// The gate input for one name-status listing over a fresh head and base checkout.
fn gate(test_name: &str, name_status: &str) -> Result<roles::GateInput, RolesError> {
    let (head, base) = merge_request(test_name);
    roles::input(&head, &base, name_status, "jana", None, &[])
}

/// What the gate saw: `(action, kind, name, project)` per change.
fn seen(input: &roles::GateInput) -> Vec<(&'static str, String, String, Option<String>)> {
    input
        .changes
        .iter()
        .map(|c| (c.action, c.kind.clone(), c.name.clone(), c.project.clone()))
        .collect()
}

/// A checkout path outside both trees, holding a manifest the gate must never read.
fn outside_manifest(test_name: &str) -> std::path::PathBuf {
    let outside = temp_dir(test_name);
    write(&outside, "endpoint.yaml", PUBLIC_ENDPOINT);
    outside.join("endpoint.yaml")
}

/// PF-52, CC-41: a path that climbs out of the checkout is refused, never read.
#[test]
fn a_path_containing_dot_dot_segments_does_not_read_a_file_outside_repo_dir_or_base_dir() {
    let target = outside_manifest("roles-dotdot-target");
    let climb = format!("../../../../../../../..{}", target.display());
    for status in ["A", "M", "D"] {
        let refused = gate("roles-dotdot", &format!("{status}\t{climb}\n"));
        assert!(
            matches!(refused, Err(RolesError::ChangePath { .. })),
            "{status}: {refused:?}"
        );
    }
}

/// PF-52, CC-41: an absolute path does not replace the checkout it is joined to.
#[test]
fn an_absolute_path_in_the_name_status_line_does_not_replace_repo_dir() {
    let target = outside_manifest("roles-absolute-target");
    for status in ["A", "D"] {
        let refused = gate(
            "roles-absolute",
            &format!("{status}\t{}\n", target.display()),
        );
        assert!(
            matches!(refused, Err(RolesError::ChangePath { .. })),
            "{status}: {refused:?}"
        );
    }
}

/// PF-52: git quotes a path with a byte outside printable ASCII (`core.quotePath`); the gate
/// reads the file it names instead of passing it as not a manifest.
#[test]
fn a_path_git_quotes_is_read_as_the_file_it_names() {
    let (head, base) = merge_request("roles-quoted");
    write(
        &head,
        "projects/ovzdusie/spaces/ovzdusie/endpoints/verejn\u{fd}.yaml",
        PUBLIC_ENDPOINT,
    );
    write(
        &head,
        "projects/ovzdusie/spaces/ovzdusie/endpoints/a\tb.yaml",
        PUBLIC_ENDPOINT,
    );
    let listing = "A\t\"projects/ovzdusie/spaces/ovzdusie/endpoints/verejn\\303\\275.yaml\"\n\
                   A\t\"projects/ovzdusie/spaces/ovzdusie/endpoints/a\\tb.yaml\"\n";
    let input = roles::input(&head, &base, listing, "jana", None, &[]).expect("input");
    assert_eq!(input.changes.len(), 2, "{:?}", seen(&input));
    assert!(input.changes.iter().all(|c| c.kind == "Endpoint"));

    for broken in [
        "A\t\"projects/x.yaml",
        "A\t\"projects/\\q.yaml\"",
        "A\t\"a\\3\"",
    ] {
        let refused = roles::input(&head, &base, broken, "jana", None, &[]);
        assert!(
            matches!(refused, Err(RolesError::ChangePath { .. })),
            "{broken}: {refused:?}"
        );
    }
}

/// PF-52: a YAML file that is not a manifest is skipped by name: the loader refuses it in
/// `jcctl validate`, which runs before the gate (MF-09), so the gate has nothing to weigh.
#[test]
fn a_file_that_does_not_parse_as_a_manifest_is_reported_not_silently_skipped_or_the_skip_is_named()
{
    let (head, base) = merge_request("roles-not-manifest");
    write(&head, "projects/ovzdusie/notes.yaml", "just: [text\n");
    let input = roles::input(
        &head,
        &base,
        "A\tprojects/ovzdusie/notes.yaml\n",
        "jana",
        None,
        &[],
    )
    .expect("input");
    assert!(input.changes.is_empty(), "{:?}", seen(&input));
}

/// PF-52: the manifest's own namespace names the project; the path only stands in when the
/// manifest has none (or is at organization level).
#[test]
fn the_project_field_prefers_the_manifests_own_namespace_over_the_path_derived_one() {
    let (head, base) = merge_request("roles-namespace");
    write(
        &head,
        "projects/doprava/spaces/ovzdusie/endpoints/air.yaml",
        PUBLIC_ENDPOINT,
    );
    let input = roles::input(
        &head,
        &base,
        "A\tprojects/doprava/spaces/ovzdusie/endpoints/air.yaml\n",
        "jana",
        None,
        &[],
    )
    .expect("input");
    assert_eq!(input.changes[0].project.as_deref(), Some("ovzdusie"));
}

/// PF-52, CC-41: a rename proposes the new manifest and deletes the old one, so moving a
/// resource out of a project needs the right to delete it there.
#[test]
fn a_rename_line_with_three_columns_uses_the_new_path_not_the_old() {
    let (head, base) = merge_request("roles-rename");
    write(
        &head,
        "projects/ovzdusie/pipelines/aq2/pipeline.yaml",
        &PIPELINE.replace("name: aq", "name: aq2"),
    );
    let input = roles::input(
        &head,
        &base,
        "R087\tprojects/ovzdusie/pipelines/aq/pipeline.yaml\tprojects/ovzdusie/pipelines/aq2/pipeline.yaml\n",
        "jana",
        None,
        &[],
    )
    .expect("input");
    let changes = seen(&input);
    assert_eq!(
        changes
            .iter()
            .map(|c| (c.0, c.2.as_str()))
            .collect::<Vec<_>>(),
        vec![("propose", "aq2"), ("delete", "aq")],
        "{changes:?}"
    );
    assert_eq!(
        input.changes[0].path,
        "projects/ovzdusie/pipelines/aq2/pipeline.yaml"
    );
}

/// PF-52: a line without a tab, a blank line and a status alone are not changes.
#[test]
fn a_line_with_no_tab_at_all_is_skipped_rather_than_erroring() {
    for listing in ["", "\n", "A projects/x.yaml\n", "M\n", "garbage"] {
        let input = gate("roles-no-tab", listing).expect("input");
        assert!(input.changes.is_empty(), "{listing:?}");
    }
}

/// PF-52: added and modified files are read from the head, deleted ones from the base.
#[test]
fn a_status_of_m_or_a_reads_from_repo_dir_and_d_reads_from_base_dir() {
    let endpoint = "projects/ovzdusie/spaces/ovzdusie/endpoints/air.yaml";
    let pipeline = "projects/ovzdusie/pipelines/aq/pipeline.yaml";
    for (listing, expected) in [
        (format!("A\t{endpoint}\n"), vec![("propose", "Endpoint")]),
        (format!("M\t{endpoint}\n"), vec![("propose", "Endpoint")]),
        (format!("T\t{endpoint}\n"), vec![("propose", "Endpoint")]),
        (format!("D\t{pipeline}\n"), vec![("delete", "Pipeline")]),
        // The endpoint is only in the head and the pipeline only in the base.
        (format!("D\t{endpoint}\nA\t{pipeline}\n"), vec![]),
    ] {
        let input = gate("roles-sides", &listing).expect("input");
        let got: Vec<(&str, &str)> = input
            .changes
            .iter()
            .map(|c| (c.action, c.kind.as_str()))
            .collect();
        assert_eq!(got, expected, "{listing}");
    }
}

/// PF-52: a path that is not on the side it is read from is not a change to weigh.
#[test]
fn a_path_that_no_longer_exists_at_the_expected_side_is_skipped_not_erred() {
    let input = gate(
        "roles-missing",
        "M\tprojects/ovzdusie/gone.yaml\nD\tprojects/ovzdusie/gone.yaml\n",
    )
    .expect("input");
    assert!(input.changes.is_empty(), "{:?}", seen(&input));
}

/// PF-52: a LinkML source is the authoring source of a model, not a manifest.
#[test]
fn a_linkml_yaml_file_in_the_diff_is_skipped_like_the_loader_skips_it() {
    let (head, base) = merge_request("roles-linkml");
    write(
        &head,
        "projects/ovzdusie/spaces/ovzdusie/datamodels/x.linkml.yaml",
        PUBLIC_ENDPOINT,
    );
    let input = roles::input(
        &head,
        &base,
        "A\tprojects/ovzdusie/spaces/ovzdusie/datamodels/x.linkml.yaml\n",
        "jana",
        None,
        &[],
    )
    .expect("input");
    assert!(input.changes.is_empty(), "{:?}", seen(&input));
}

/// PF-52: every document of a multi-document file is its own change.
#[test]
fn a_multi_document_yaml_file_contributes_one_change_per_document() {
    let (head, base) = merge_request("roles-multi");
    write(
        &head,
        "projects/ovzdusie/two.yaml",
        &format!(
            "{PUBLIC_ENDPOINT}---\n{}",
            PUBLIC_ENDPOINT.replace("name: air", "name: water")
        ),
    );
    let input = roles::input(
        &head,
        &base,
        "A\tprojects/ovzdusie/two.yaml\n",
        "jana",
        None,
        &[],
    )
    .expect("input");
    assert_eq!(
        input
            .changes
            .iter()
            .map(|c| c.name.as_str())
            .collect::<Vec<_>>(),
        vec!["air", "water"]
    );
}
