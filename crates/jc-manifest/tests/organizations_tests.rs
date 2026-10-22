//! Several Organizations in one repository (PF-121, PF-122, ADR-N-058).

use jc_manifest::validate;
use std::path::{Path, PathBuf};

fn temp_repo(test_name: &str) -> PathBuf {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock after the epoch")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "jcctl-organizations-{test_name}-{}-{now}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).expect("create temp repo");
    dir
}

fn write(dir: &Path, rel: &str, body: &str) {
    let path = dir.join(rel);
    std::fs::create_dir_all(path.parent().expect("relative path has a parent"))
        .expect("create parent directory");
    std::fs::write(path, body).expect("write manifest");
}

fn organization(name: &str, domain: &str, installation: bool) -> String {
    let claim = if installation {
        "  installation: true\n"
    } else {
        ""
    };
    format!(
        "apiVersion: joinedcontext.com/v1alpha1\nkind: Organization\nmetadata:\n  name: {name}\n  \
         namespace: org\nspec:\n  domain: {domain}\n  locales: [sk, en]\n  defaultLocale: sk\n{claim}"
    )
}

fn project(name: &str, organization: &str, domain: Option<&str>) -> String {
    let domain = domain
        .map(|d| format!("  domain: {d}\n"))
        .unwrap_or_default();
    format!(
        "apiVersion: joinedcontext.com/v1alpha1\nkind: Project\nmetadata:\n  name: {name}\n  \
         namespace: org\nspec:\n  organizationRef: {organization}\n{domain}"
    )
}

/// Helsinki is the installation's, Žilina an Organization of its own with its project.
fn two_organizations(test_name: &str, zilina_domain: Option<&str>) -> PathBuf {
    let dir = temp_repo(test_name);
    write(
        &dir,
        "organizations/hel.yaml",
        &organization("hel", "hel.fi", true),
    );
    write(
        &dir,
        "organizations/zilina.yaml",
        &organization("zilina", "zilina.sk", false),
    );
    write(
        &dir,
        "projects/praha/project.yaml",
        &project("praha", "hel", Some("praha.eu")),
    );
    write(
        &dir,
        "projects/zilina/project.yaml",
        &project("zilina", "zilina", zilina_domain),
    );
    dir
}

fn messages(findings: &[validate::Finding]) -> Vec<String> {
    findings.iter().map(|f| f.message.clone()).collect()
}

#[test]
fn two_organization_files_load_and_the_installation_names_the_domain_pf121() {
    let dir = two_organizations("two", Some("zilina.sk"));
    let report = validate::run(&dir);
    assert_eq!(messages(&report.findings), Vec::<String>::new());
    assert_eq!(report.checked, 4);

    let repo = jc_manifest::loader::Repository::load(&dir).expect("loads");
    let (installation, _) = repo.installation_organization().expect("one installation");
    assert_eq!(installation.name, "hel");
    assert_eq!(repo.org_domain(), Some("hel.fi"));
}

#[test]
fn a_subdomain_of_its_organization_is_allowed_pf122() {
    let report = validate::run(&two_organizations("subdomain", Some("opendata.zilina.sk")));
    assert_eq!(messages(&report.findings), Vec::<String>::new());
}

#[test]
fn a_project_naming_a_missing_organization_is_refused_with_the_member_pf122() {
    let dir = two_organizations("missing", None);
    write(
        &dir,
        "projects/bb/project.yaml",
        &project("bb", "bystrica", None),
    );
    let found = messages(&validate::run(&dir).findings);
    assert_eq!(found.len(), 1, "{found:?}");
    assert!(
        found[0].contains("`spec.organizationRef` names `bystrica`"),
        "{}",
        found[0]
    );
    assert!(found[0].contains("hel, zilina"), "{}", found[0]);
}

#[test]
fn a_project_of_another_organization_under_a_foreign_domain_is_refused_pf122() {
    for domain in ["hel.fi", "notzilina.sk"] {
        let found = messages(&validate::run(&two_organizations("foreign", Some(domain))).findings);
        assert_eq!(found.len(), 1, "{domain}: {found:?}");
        assert!(found[0].contains("`spec.domain`"), "{}", found[0]);
        assert!(found[0].contains(&format!("`{domain}`")), "{}", found[0]);
    }
}

#[test]
fn several_organizations_need_exactly_one_installation_pf121() {
    let none = temp_repo("none");
    write(
        &none,
        "organizations/hel.yaml",
        &organization("hel", "hel.fi", false),
    );
    write(
        &none,
        "organizations/zilina.yaml",
        &organization("zilina", "zilina.sk", false),
    );
    let found = validate::run(&none).findings;
    assert_eq!(found.len(), 2, "{:?}", messages(&found));
    assert!(found
        .iter()
        .all(|f| f.message.contains("none is the installation's")));

    let two = temp_repo("two-claims");
    write(&two, "org.yaml", &organization("hel", "hel.fi", false));
    write(
        &two,
        "organizations/zilina.yaml",
        &organization("zilina", "zilina.sk", true),
    );
    let found = messages(&validate::run(&two).findings);
    assert_eq!(found.len(), 2, "{found:?}");
    assert!(
        found[0].contains("`org.yaml`, `organizations/zilina.yaml` all claim it"),
        "{}",
        found[0]
    );
}

#[test]
fn the_legacy_org_yaml_still_loads_with_a_deprecation_warning_pf121() {
    let dir = temp_repo("legacy");
    write(&dir, "org.yaml", &organization("hel", "hel.fi", false));
    write(
        &dir,
        "projects/praha/project.yaml",
        &project("praha", "hel", None),
    );
    let report = validate::run(&dir);
    assert_eq!(messages(&report.findings), Vec::<String>::new());
    let warned: Vec<_> = messages(&report.warnings)
        .into_iter()
        .filter(|m| m.contains("legacy"))
        .collect();
    assert_eq!(warned.len(), 1, "{warned:?}");
    assert!(
        warned[0].contains("move it to `organizations/hel.yaml`"),
        "{}",
        warned[0]
    );

    let moved = temp_repo("moved");
    write(
        &moved,
        "organizations/hel.yaml",
        &organization("hel", "hel.fi", false),
    );
    assert!(!messages(&validate::run(&moved).warnings)
        .iter()
        .any(|m| m.contains("legacy")));
}
