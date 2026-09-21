//! The loader resolves an `endpointRef` to the slug this environment minted, and never to a
//! slug the referring project may not use (EP-77, EP-15, T-1448).

use jcctl::commands::validate;
use jcctl::loader::{Repository, ResourceId};
use std::path::{Path, PathBuf};

const SLUG: &str = "scsd2eehkx42n53z2zyd6vshfh7s7irf";

fn write(dir: &Path, rel: &str, body: &str) {
    let path = dir.join(rel);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, body).unwrap();
}

/// Project `source` publishes `bikes` with `audience`; project `user` refers to it by name.
fn repo(test: &str, source: &str, user: &str, audience: &str, reference: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("jcctl-shared-ref-{test}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    write(&dir, "org.yaml", "apiVersion: joinedcontext.com/v1alpha1\nkind: Organization\nmetadata:\n  name: helsinki\n  namespace: org\nspec:\n  domain: hel.fi\n  locales: [\"en\"]\n  defaultLocale: en\n");
    for project in [source, user] {
        write(&dir, &format!("projects/{project}/project.yaml"), &format!("apiVersion: joinedcontext.com/v1alpha1\nkind: Project\nmetadata:\n  name: {project}\n  namespace: org\nspec:\n  organizationRef: helsinki\n"));
    }
    write(&dir, &format!("projects/{source}/spaces/hub/space.yaml"), &format!("apiVersion: joinedcontext.com/v1alpha1\nkind: ContextSpace\nmetadata:\n  name: hub\n  namespace: {source}\nspec:\n  isSandbox: false\n"));
    write(&dir, &format!("projects/{source}/spaces/hub/endpoints/bikes.yaml"), &format!("apiVersion: joinedcontext.com/v1alpha1\nkind: Endpoint\nmetadata:\n  name: bikes\n  namespace: {source}\nspec:\n  slug: {SLUG}\n  contextSpaceRef: hub\n  audience: {audience}\n  enabledRepresentations: [ngsi-ld]\n"));
    write(&dir, &format!("projects/{user}/shared/city-bikes.yaml"), &format!("apiVersion: joinedcontext.com/v1alpha1\nkind: SharedSpaceReference\nmetadata:\n  name: city-bikes\n  namespace: {user}\nspec:\n  endpointRef: {reference}\n  alias: city-bikes\n"));
    dir
}

fn resolved(dir: &Path, user: &str) -> serde_json::Value {
    let repo = Repository::load(dir).expect("the repository loads");
    let id = ResourceId {
        group: "joinedcontext.com".to_owned(),
        kind: "SharedSpaceReference".to_owned(),
        namespace: Some(user.to_owned()),
        name: "city-bikes".to_owned(),
    };
    repo.get(&id)
        .expect("the reference loaded")
        .manifest
        .spec
        .clone()
}

#[test]
fn a_ref_resolves_to_the_slug_of_this_environment() {
    let dir = repo(
        "resolves",
        "helsinki",
        "helsinki-mobility",
        "organization",
        "{ project: helsinki, name: bikes }",
    );
    let spec = resolved(&dir, "helsinki-mobility");
    assert_eq!(spec["endpointSlug"], SLUG);
    assert!(spec.get("endpointRef").is_none());
    assert!(validate::run(&dir)
        .findings
        .iter()
        .all(|f| !f.message.contains("SharedSpaceReference")));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_pair_copied_under_two_new_names_resolves_with_no_edit_but_the_ref_project() {
    // A bundle imported under new names carries its reference as a name, so only the
    // project the import renamed changes; the slug is whatever this environment minted.
    let dir = repo(
        "renamed",
        "espoo",
        "espoo-mobility",
        "project-list\n  allowedProjects: [espoo-mobility]",
        "{ project: espoo, name: bikes }",
    );
    assert_eq!(resolved(&dir, "espoo-mobility")["endpointSlug"], SLUG);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_ref_to_a_missing_endpoint_is_a_finding_naming_it() {
    let dir = repo(
        "missing",
        "helsinki",
        "helsinki-mobility",
        "organization",
        "{ project: helsinki, name: trams }",
    );
    let report = validate::run(&dir);
    let finding = report
        .findings
        .iter()
        .find(|f| f.message.contains("SharedSpaceReference city-bikes"))
        .unwrap_or_else(|| panic!("{:?}", report.findings));
    assert!(
        finding.message.contains("helsinki/trams"),
        "{}",
        finding.message
    );
    assert_eq!(
        finding.path,
        PathBuf::from("projects/helsinki-mobility/shared/city-bikes.yaml")
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn an_endpoint_that_does_not_admit_the_project_is_refused_and_its_slug_never_leaks() {
    let dir = repo(
        "refused",
        "helsinki",
        "helsinki-mobility",
        "project-list\n  allowedProjects: [helsinki-kpi]",
        "{ project: helsinki, name: bikes }",
    );
    let spec = resolved(&dir, "helsinki-mobility");
    assert!(spec.get("endpointSlug").is_none(), "{spec}");
    assert!(!spec.to_string().contains(SLUG));
    let report = validate::run(&dir);
    assert!(
        report.findings.iter().any(|f| f
            .message
            .contains("not shared with project helsinki-mobility")),
        "{:?}",
        report.findings
    );
    let _ = std::fs::remove_dir_all(&dir);
}

// --- T-2532: the edge cases of `resolve_endpoint_refs` (EP-15, EP-77) ---

/// The slug `user`'s reference resolved to, or `None` when it did not resolve or the repository
/// refused to load at all: either way the referring project got no slug.
fn slug_for(dir: &Path, user: &str) -> Option<String> {
    let repo = Repository::load(dir).ok()?;
    let id = ResourceId {
        group: "joinedcontext.com".to_owned(),
        kind: "SharedSpaceReference".to_owned(),
        namespace: Some(user.to_owned()),
        name: "city-bikes".to_owned(),
    };
    let spec = &repo.get(&id)?.manifest.spec;
    spec["endpointSlug"].as_str().map(str::to_owned)
}

/// `repo` with the endpoint's spec lines after `slug:` replaced by `rest`.
fn repo_with_endpoint(test: &str, user: &str, rest: &str, reference: &str) -> PathBuf {
    let dir = repo(test, "helsinki", user, "organization", reference);
    write(&dir, "projects/helsinki/spaces/hub/endpoints/bikes.yaml", &format!("apiVersion: joinedcontext.com/v1alpha1\nkind: Endpoint\nmetadata:\n  name: bikes\n  namespace: helsinki\nspec:\n  slug: {SLUG}\n  contextSpaceRef: hub\n{rest}  enabledRepresentations: [ngsi-ld]\n"));
    dir
}

/// EP-15: an endpoint that names no audience is not shared with anyone else.
#[test]
fn an_endpoint_with_no_audience_field_is_unresolved_without_an_explicit_allow() {
    let dir = repo_with_endpoint(
        "no-audience",
        "helsinki-mobility",
        "",
        "{ project: helsinki, name: bikes }",
    );
    assert_eq!(slug_for(&dir, "helsinki-mobility"), None);
    let _ = std::fs::remove_dir_all(&dir);
}

/// EP-15: only the exact words `public` and `organization` open an endpoint to every project.
#[test]
fn an_audience_value_of_unexpected_case_is_treated_as_restricted_not_public() {
    for (n, audience) in ["Public", "PUBLIC", "Organization", " public", "everyone"]
        .iter()
        .enumerate()
    {
        let dir = repo_with_endpoint(
            &format!("case-{n}"),
            "helsinki-mobility",
            &format!("  audience: \"{audience}\"\n"),
            "{ project: helsinki, name: bikes }",
        );
        assert_eq!(slug_for(&dir, "helsinki-mobility"), None, "{audience:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}

/// EP-15: an allow-list entry that is not a project name matches nobody.
#[test]
fn allowed_projects_containing_a_non_string_value_does_not_panic_and_does_not_match() {
    let dir = repo_with_endpoint(
        "non-string-allowed",
        "helsinki-mobility",
        "  audience: project-list\n  allowedProjects: [42, {name: helsinki-mobility}, null]\n",
        "{ project: helsinki, name: bikes }",
    );
    assert_eq!(slug_for(&dir, "helsinki-mobility"), None);
    let _ = std::fs::remove_dir_all(&dir);
}

/// EP-77: a reference that names no project or no endpoint resolves to nothing.
#[test]
fn an_endpoint_ref_missing_project_or_name_is_unresolved() {
    for (n, reference) in [
        "{ name: bikes }",
        "{ project: helsinki }",
        "{}",
        "{ project: \"\", name: \"\" }",
    ]
    .iter()
    .enumerate()
    {
        let dir = repo(
            &format!("partial-ref-{n}"),
            "helsinki",
            "helsinki-mobility",
            "public",
            reference,
        );
        assert_eq!(slug_for(&dir, "helsinki-mobility"), None, "{reference}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}

/// EP-14: a public endpoint is shared with every project, allow-list or not.
#[test]
fn an_audience_of_public_resolves_for_any_referring_project() {
    let dir = repo(
        "public",
        "helsinki",
        "helsinki-mobility",
        "public",
        "{ project: helsinki, name: bikes }",
    );
    assert_eq!(slug_for(&dir, "helsinki-mobility").as_deref(), Some(SLUG));
    let _ = std::fs::remove_dir_all(&dir);
}

/// EP-14: an organization endpoint admits every project of the repository's organization.
#[test]
fn an_audience_of_organization_admits_every_project_of_the_organization() {
    for (n, user) in ["helsinki-mobility", "helsinki-kpi"].iter().enumerate() {
        let dir = repo(
            &format!("org-{n}"),
            "helsinki",
            user,
            "organization",
            "{ project: helsinki, name: bikes }",
        );
        assert_eq!(slug_for(&dir, user).as_deref(), Some(SLUG), "{user}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}

/// EP-15: the allow-list is what shares a project-list endpoint, and only with the listed ones.
#[test]
fn a_project_named_in_allowed_projects_resolves_and_one_not_named_does_not() {
    let listed = repo(
        "listed",
        "helsinki",
        "helsinki-mobility",
        "project-list\n  allowedProjects: [helsinki-mobility]",
        "{ project: helsinki, name: bikes }",
    );
    assert_eq!(
        slug_for(&listed, "helsinki-mobility").as_deref(),
        Some(SLUG)
    );
    let _ = std::fs::remove_dir_all(&listed);
    let near = repo(
        "near-miss",
        "helsinki",
        "helsinki-mobility",
        "project-list\n  allowedProjects: [helsinki-mobilit, Helsinki-Mobility]",
        "{ project: helsinki, name: bikes }",
    );
    assert_eq!(slug_for(&near, "helsinki-mobility"), None);
    let _ = std::fs::remove_dir_all(&near);
}

/// EP-77: an endpoint with no slug resolves to nothing, even for a project it admits.
#[test]
fn an_endpoint_that_passes_the_audience_check_but_carries_no_slug_is_unresolved() {
    let dir = repo(
        "no-slug",
        "helsinki",
        "helsinki-mobility",
        "public",
        "{ project: helsinki, name: bikes }",
    );
    write(&dir, "projects/helsinki/spaces/hub/endpoints/bikes.yaml", "apiVersion: joinedcontext.com/v1alpha1\nkind: Endpoint\nmetadata:\n  name: bikes\n  namespace: helsinki\nspec:\n  contextSpaceRef: hub\n  audience: public\n  enabledRepresentations: [ngsi-ld]\n");
    assert_eq!(slug_for(&dir, "helsinki-mobility"), None);
    let _ = std::fs::remove_dir_all(&dir);
}

/// EP-77: a resolved reference carries the slug and no longer the name it was written with.
#[test]
fn resolving_removes_endpointref_and_writes_endpointslug_exactly_once() {
    let dir = repo(
        "once",
        "helsinki",
        "helsinki-mobility",
        "public",
        "{ project: helsinki, name: bikes }",
    );
    let spec = resolved(&dir, "helsinki-mobility");
    assert!(spec.get("endpointRef").is_none(), "{spec}");
    assert_eq!(spec.to_string().matches(SLUG).count(), 1, "{spec}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// EP-77: two projects referring to one endpoint each get it, and one's refusal is not the other's.
#[test]
fn two_shared_space_references_naming_the_same_endpoint_resolve_independently() {
    let dir = repo(
        "two-refs",
        "helsinki",
        "helsinki-mobility",
        "project-list\n  allowedProjects: [helsinki-mobility]",
        "{ project: helsinki, name: bikes }",
    );
    write(&dir, "projects/helsinki-kpi/project.yaml", "apiVersion: joinedcontext.com/v1alpha1\nkind: Project\nmetadata:\n  name: helsinki-kpi\n  namespace: org\nspec:\n  organizationRef: helsinki\n");
    write(&dir, "projects/helsinki-kpi/shared/city-bikes.yaml", "apiVersion: joinedcontext.com/v1alpha1\nkind: SharedSpaceReference\nmetadata:\n  name: city-bikes\n  namespace: helsinki-kpi\nspec:\n  endpointRef: { project: helsinki, name: bikes }\n  alias: city-bikes\n");
    assert_eq!(slug_for(&dir, "helsinki-mobility").as_deref(), Some(SLUG));
    assert_eq!(
        slug_for(&dir, "helsinki-kpi"),
        None,
        "the kpi project is not on the list"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
