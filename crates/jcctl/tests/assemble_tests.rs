//! Assembling one render from the organization repository and one repository per project
//! (T-2640, CC-85, CC-86, CC-88, PF-86).

mod common;

use common::{temp_dir, write, ENDPOINT, ORG, PROJECT, SPACE};
use jcctl::assemble::{assemble, AssembleError, Directories};
use jcctl::loader::Repository;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

const ENTRY: &str = r#"apiVersion: joinedcontext.com/v1alpha1
kind: Project
metadata:
  name: ovzdusie
  namespace: org
spec:
  organizationRef: banskabystrica
  repository: { name: ovzdusie }
  ref: main
"#;

/// The organization repository of layout 2: the organization, the layout, the registry.
fn organization(test: &str, entries: &[(&str, &str)]) -> PathBuf {
    let dir = temp_dir(&format!("{test}-org"));
    write(&dir, "org.yaml", ORG);
    write(&dir, ".jc/layout", "2\n");
    for (slug, entry) in entries {
        write(&dir, &format!("projects/{slug}.yaml"), entry);
    }
    dir
}

/// The project repository of the demo story: what `projects/ovzdusie/` holds in layout 1, at
/// its root.
fn project_repository(test: &str) -> PathBuf {
    let dir = temp_dir(&format!("{test}-project"));
    write(&dir, ".jc/layout", "2\n");
    write(&dir, "project.yaml", PROJECT);
    write(&dir, "spaces/ovzdusie/space.yaml", SPACE);
    write(&dir, "spaces/ovzdusie/endpoints/public-air.yaml", ENDPOINT);
    dir
}

fn checkouts(pairs: &[(&str, &Path)]) -> Directories {
    Directories(
        pairs
            .iter()
            .map(|(slug, dir)| ((*slug).to_owned(), dir.to_path_buf()))
            .collect(),
    )
}

fn resources(repository: &Repository) -> BTreeMap<String, (PathBuf, serde_json::Value)> {
    repository
        .iter()
        .map(|(id, loaded)| {
            (
                id.to_string(),
                (
                    loaded.path.clone(),
                    serde_json::to_value(&loaded.manifest).expect("a manifest serialises"),
                ),
            )
        })
        .collect()
}

/// CC-86: the demo organization renders the same from one repository (layout 1) as from the
/// organization repository and a project repository (layout 2): same ids, same paths, same
/// manifests.
#[test]
fn layout_2_renders_what_layout_1_renders() {
    let single = common::demo_repo("assemble-single");
    let one = assemble(&single, &Directories::default(), &temp_dir("unused"), None)
        .expect("layout 1 loads where it is");
    assert_eq!(one.layout, 1);

    let org = organization("same", &[("ovzdusie", ENTRY)]);
    let project = project_repository("same");
    let into = temp_dir("same-into").join("render");
    let two = assemble(&org, &checkouts(&[("ovzdusie", &project)]), &into, None)
        .expect("layout 2 assembles");
    assert_eq!(two.layout, 2);
    assert_eq!(two.entries.len(), 1);
    assert!(two.entries[0].rendered && two.entries[0].error.is_none());
    assert_eq!(resources(&two.repository), resources(&one.repository));
}

/// CC-86, PF-86: one repository under two registry slugs is two projects, each rendering its
/// own namespace; a slug unique in the organization, claimed by both, is refused by name.
#[test]
fn two_projects_on_one_repository_and_a_slug_they_both_claim() {
    let shared = temp_dir("two-slugs-project");
    write(&shared, ".jc/layout", "2\n");
    write(
        &shared,
        "spaces/ovzdusie/space.yaml",
        &SPACE
            .replace("namespace: ovzdusie", "namespace: \"{project}\"")
            .replace(
                "isSandbox: false",
                "isSandbox: false\n  urnSegment: \"{param:segment}\"",
            ),
    );
    write(
        &shared,
        "spaces/ovzdusie/endpoints/public-air.yaml",
        &ENDPOINT
            .replace("namespace: ovzdusie", "namespace: \"{project}\"")
            .replace("slug: zt4qm7ge2xdv6ksb3ncf5arw2y", "slug: \"{param:slug}\""),
    );
    write(
        &shared,
        "project.yaml",
        &(PROJECT.replace("name: ovzdusie", "name: \"{project}\"")
            + "  parameters:\n    slug: { type: string }\n    segment: { type: string }\n"),
    );
    let entry = |slug: &str, endpoint: &str, segment: &str| {
        ENTRY.replace(
            "name: ovzdusie\n  namespace",
            &format!("name: {slug}\n  namespace"),
        ) + &format!("  parameters: {{ slug: {endpoint}, segment: {segment} }}\n")
    };
    let air = entry("air", "zt4qm7ge2xdv6ksb3ncf5arw2y", "air");
    let staging = entry("air-staging", "q7w6e5r4t3y2uaiopazsxdcfgh", "air-staging");
    let org = organization("two-slugs", &[("air", &air), ("air-staging", &staging)]);
    let resolver = checkouts(&[("air", &shared), ("air-staging", &shared)]);

    let assembly = assemble(&org, &resolver, &temp_dir("two-slugs-into").join("r"), None)
        .expect("two projects on one repository assemble");
    let namespaces: Vec<String> = assembly
        .repository
        .iter()
        .filter(|(id, _)| id.kind == "Endpoint")
        .map(|(id, _)| id.namespace.clone().unwrap_or_default())
        .collect();
    assert_eq!(namespaces, ["air", "air-staging"]);

    let clash = entry("air-staging", "zt4qm7ge2xdv6ksb3ncf5arw2y", "air-staging");
    let org = organization("slug-clash", &[("air", &air), ("air-staging", &clash)]);
    let refused = assemble(
        &org,
        &resolver,
        &temp_dir("slug-clash-into").join("r"),
        None,
    )
    .expect_err("one slug claimed twice is refused");
    match refused {
        AssembleError::Conflict {
            value,
            first,
            second,
            ..
        } => {
            assert_eq!(value, "zt4qm7ge2xdv6ksb3ncf5arw2y");
            assert_eq!((first.as_str(), second.as_str()), ("air", "air-staging"));
        }
        other => panic!("expected a conflict, got {other}"),
    }
}

/// CC-86: a ref that cannot be fetched renders the project as the last assembly left it and
/// says so on its entry; a project never assembled is left out, with the same report.
#[test]
fn an_unfetchable_ref_keeps_the_last_checkout_and_reports_it() {
    let org = organization("unfetchable", &[("ovzdusie", ENTRY)]);
    let project = project_repository("unfetchable");
    let into = temp_dir("unfetchable-into").join("render");
    let first = assemble(&org, &checkouts(&[("ovzdusie", &project)]), &into, None)
        .expect("the first assembly");
    let before = resources(&first.repository);

    let again = assemble(&org, &Directories::default(), &into, None)
        .expect("an unfetchable project does not stop the organization");
    assert!(again.entries[0].rendered);
    assert!(again.entries[0]
        .error
        .as_deref()
        .is_some_and(|error| error.contains("ovzdusie")));
    assert_eq!(resources(&again.repository), before);

    let fresh = assemble(
        &org,
        &Directories::default(),
        &temp_dir("unfetchable-fresh").join("render"),
        None,
    )
    .expect("the organization assembles without the project");
    assert!(!fresh.entries[0].rendered);
    assert!(fresh
        .repository
        .iter()
        .all(|(id, _)| id.kind == "Organization"));
}

/// CC-88: the entry's parameter values are rendered into the project's manifests, a literal
/// where a parameter exists and a mapping reading an undeclared one are reported.
#[test]
fn parameters_render_and_literals_are_reported() {
    let project = project_repository("parameters");
    write(
        &project,
        "project.yaml",
        &(PROJECT.to_owned()
            + "  parameters:\n    audience: { type: string, default: public, enum: [public, organization] }\n    \
               feedHost: { type: string, default: data.banskabystrica.sk }\n"),
    );
    write(
        &project,
        "spaces/ovzdusie/endpoints/public-air.yaml",
        &ENDPOINT.replace("audience: public", "audience: \"{param:audience}\""),
    );
    write(
        &project,
        "pipelines/air/pipeline.yaml",
        "apiVersion: joinedcontext.com/v1alpha1\nkind: Pipeline\nmetadata:\n  name: air\n  \
         namespace: ovzdusie\nspec:\n  class: stream\n  description: data.banskabystrica.sk\n",
    );
    write(
        &project,
        "pipelines/air/bento.yaml",
        "pipeline:\n  processors:\n    - mapping: root.host = env(\"JC_PARAM_FEED_HOTS\")\n",
    );
    let entry = ENTRY.to_owned() + "  parameters: { audience: organization }\n";
    let org = organization("parameters", &[("ovzdusie", &entry)]);
    let assembly = assemble(
        &org,
        &checkouts(&[("ovzdusie", &project)]),
        &temp_dir("parameters-into").join("render"),
        None,
    )
    .expect("assembles");
    let endpoint = assembly
        .repository
        .iter()
        .find(|(id, _)| id.kind == "Endpoint")
        .map(|(_, loaded)| loaded.manifest.spec["audience"].clone())
        .expect("the endpoint is rendered");
    assert_eq!(endpoint, "organization");

    let messages: Vec<String> = assembly
        .findings
        .iter()
        .map(|finding| format!("{}: {}", finding.path.display(), finding.message))
        .collect();
    assert!(
        messages
            .iter()
            .any(|m| m.starts_with("pipelines/air/pipeline.yaml")
                && m.contains("spec.description")
                && m.contains("{param:feedHost}")),
        "{messages:?}"
    );
    assert!(
        messages
            .iter()
            .any(|m| m.starts_with("pipelines/air/bento.yaml") && m.contains("JC_PARAM_FEED_HOTS")),
        "{messages:?}"
    );
}

/// CC-85, PF-86: a layout 2 organization repository holding a project directory, a registry
/// entry whose file and name disagree, and a project repository without its layout are refused;
/// a refused assembly leaves the last one where it was.
#[test]
fn a_misplaced_project_is_refused_and_the_last_render_stays() {
    let org = organization("misplaced", &[("ovzdusie", ENTRY)]);
    let project = project_repository("misplaced");
    let into = temp_dir("misplaced-into").join("render");
    assemble(&org, &checkouts(&[("ovzdusie", &project)]), &into, None).expect("assembles");

    std::fs::remove_file(project.join(".jc/layout")).expect("remove the layout");
    let refused = assemble(&org, &checkouts(&[("ovzdusie", &project)]), &into, None)
        .expect_err("a project repository without .jc/layout");
    assert!(refused.to_string().contains(".jc/layout"), "{refused}");
    Repository::load(&into).expect("the last render still loads");

    let with_directory = organization("with-directory", &[("ovzdusie", ENTRY)]);
    write(&with_directory, "projects/transport/project.yaml", PROJECT);
    let refused = assemble(
        &with_directory,
        &Directories::default(),
        &temp_dir("with-directory-into").join("r"),
        None,
    )
    .expect_err("a project directory in layout 2");
    assert!(
        refused.to_string().contains("projects/transport"),
        "{refused}"
    );

    let misnamed = organization("misnamed", &[("air", ENTRY)]);
    let refused = assemble(
        &misnamed,
        &Directories::default(),
        &temp_dir("misnamed-into").join("r"),
        None,
    )
    .expect_err("an entry named apart from its file");
    assert!(
        refused.to_string().contains("projects/air.yaml"),
        "{refused}"
    );

    let unknown = organization("unknown-layout", &[]);
    write(&unknown, ".jc/layout", "3\n");
    assert!(assemble(
        &unknown,
        &Directories::default(),
        &temp_dir("u").join("r"),
        None
    )
    .is_err());
}
