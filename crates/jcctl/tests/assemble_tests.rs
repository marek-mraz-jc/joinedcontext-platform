//! Assembling one render from the organization repository and one repository per project
//! (T-2640, CC-85, CC-86, CC-88, PF-86).

mod common;

use common::{temp_dir, write, ENDPOINT, ORG, PROJECT, SPACE};
use jcctl::assemble::{assemble, assemble_reusing, AssembleError, Assembly, Directories, Renders};
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
    assert_eq!(
        again.repository,
        Repository::load_for(&into, None).expect("the last render loads")
    );

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

/// T-4443: only a link is resolved on the way, so the check that no file of a checkout leads out
/// of it must still hold for a link to a file outside, and for a link reached through a linked
/// directory inside; a linked directory that stays inside is copied.
#[cfg(unix)]
#[test]
fn a_checkout_that_links_out_is_refused_and_one_that_links_inside_is_copied() {
    use std::os::unix::fs::symlink;
    let org = organization("links", &[("ovzdusie", ENTRY)]);
    let outside = temp_dir("links-outside");
    write(&outside, "secret.yaml", "not: for the render\n");

    let inside = project_repository("links-inside");
    write(&inside, "docs/README.md", "notes\n");
    symlink(inside.join("docs"), inside.join("notes")).expect("a link");
    let into = temp_dir("links-into").join("render");
    assemble(&org, &checkouts(&[("ovzdusie", &inside)]), &into, None)
        .expect("a link that stays inside is followed");
    assert!(into.join("projects/ovzdusie/notes/README.md").is_file());

    let out = project_repository("links-out");
    symlink(
        outside.join("secret.yaml"),
        out.join("spaces/ovzdusie/leak.yaml"),
    )
    .expect("a link");
    let refused = assemble(&org, &checkouts(&[("ovzdusie", &out)]), &into, None)
        .expect_err("a link out is refused");
    assert!(
        refused.to_string().contains("links out of the repository"),
        "{refused}"
    );

    let nested = project_repository("links-nested");
    std::fs::create_dir_all(nested.join("extra")).expect("a directory");
    symlink(outside.join("secret.yaml"), nested.join("extra/leak.yaml")).expect("a link");
    symlink(nested.join("extra"), nested.join("spaces/more")).expect("a link");
    let refused = assemble(&org, &checkouts(&[("ovzdusie", &nested)]), &into, None)
        .expect_err("a link out behind a linked directory is refused");
    assert!(
        refused.to_string().contains("links out of the repository"),
        "{refused}"
    );
}

/// T-4457: the assembly keeps each manifest as it rendered it instead of reading the tree back,
/// and what it keeps is what reading the tree back gives: same paths, documents, lines,
/// manifests, rendered parameters and `{orgDomain}`.
#[test]
fn the_assembly_in_memory_is_what_the_written_render_loads() {
    let project = project_repository("in-memory");
    write(
        &project,
        "project.yaml",
        &(PROJECT.to_owned()
            + "  parameters:\n    audience: { type: string, default: public, enum: [public, organization] }\n"),
    );
    write(
        &project,
        "spaces/ovzdusie/endpoints/public-air.yaml",
        &ENDPOINT.replace("audience: public", "audience: \"{param:audience}\""),
    );
    // A leading `---`, an empty document and a comment: each moves the index or the line of
    // the documents after it.
    write(
        &project,
        "spaces/ovzdusie/more.yaml",
        &format!(
            "---\n{}---\n# nothing here\n---\n{}",
            SPACE.replace("name: ovzdusie\n  namespace", "name: second\n  namespace"),
            ENDPOINT
                .replace("public-air", "second-air")
                .replace("contextSpaceRef: ovzdusie", "contextSpaceRef: second")
                .replace("zt4qm7ge2xdv6ksb3ncf5arw2y", "qp3mx7kd2wbv5nsc4jgf6rta2e")
                .replace("public\n", "\"{orgDomain}\"\n")
        ),
    );
    let entry = ENTRY.to_owned() + "  parameters: { audience: organization }\n";
    let org = organization("in-memory", &[("ovzdusie", &entry)]);
    let into = temp_dir("in-memory-into").join("render");
    let assembly =
        assemble(&org, &checkouts(&[("ovzdusie", &project)]), &into, None).expect("assembles");
    let read_back = Repository::load_for(&into, None).expect("the written render loads");
    assert_eq!(assembly.repository, read_back);
    let second = assembly
        .repository
        .iter()
        .find(|(id, _)| id.name == "second-air")
        .map(|(_, loaded)| loaded)
        .expect("the third document");
    assert_eq!((second.document, second.line), (2, 8));
}

/// `slug` under `links` pointed at `tree`, as `jcctl checkouts` leaves a checkout: a link to a
/// tree named by its commit, swapped by one rename.
fn link(links: &Path, slug: &str, tree: &Path) -> Directories {
    std::fs::create_dir_all(links).expect("the links");
    let at = links.join(slug);
    let next = links.join(format!(".{slug}.next"));
    std::os::unix::fs::symlink(tree, &next).expect("linked");
    std::fs::rename(&next, &at).expect("swapped in");
    checkouts(&[(slug, &at)])
}

fn representations(assembly: &Assembly) -> serde_json::Value {
    let (_, endpoint) = assembly
        .repository
        .iter()
        .find(|(id, _)| id.kind == "Endpoint")
        .expect("the Endpoint");
    endpoint.manifest.spec["enabledRepresentations"].clone()
}

const ONE: &str = r#"enabledRepresentations: ["ngsi-ld"]"#;
const BOTH: &str = r#"enabledRepresentations: ["ngsi-ld", "geojson"]"#;

/// T-4460: a linked project whose tree did not move is taken from the last assembly, not
/// rendered again, and stays on disk; relinked to another tree, it renders that tree.
#[test]
fn a_linked_project_renders_again_only_when_its_link_moves() {
    let org = organization("reuse", &[("ovzdusie", ENTRY)]);
    let c1 = project_repository("reuse-c1");
    let links = temp_dir("reuse-links");
    let into = temp_dir("reuse-into").join("r");
    let mut renders = Renders::default();
    let first = assemble_reusing(
        &org,
        &link(&links, "ovzdusie", &c1),
        &into,
        None,
        &mut renders,
    )
    .expect("assembles");
    assert_eq!(
        representations(&first),
        serde_json::json!(["ngsi-ld", "geojson"])
    );
    assert!(!first.entries[0].reused, "nothing was kept yet");

    // A tree is written once; one rewritten in place is not read again while the link holds.
    let endpoint = c1.join("spaces/ovzdusie/endpoints/public-air.yaml");
    write(
        &c1,
        "spaces/ovzdusie/endpoints/public-air.yaml",
        &ENDPOINT.replace(BOTH, ONE),
    );
    let again = assemble_reusing(
        &org,
        &link(&links, "ovzdusie", &c1),
        &into,
        None,
        &mut renders,
    )
    .expect("assembles");
    assert_eq!(
        representations(&again),
        serde_json::json!(["ngsi-ld", "geojson"])
    );
    assert!(again.entries[0].reused);
    assert!(
        into.join("projects/ovzdusie/spaces/ovzdusie/endpoints/public-air.yaml")
            .is_file(),
        "the reused render is in the new tree"
    );

    let c2 = temp_dir("reuse-c2");
    write(&c2, ".jc/layout", "2\n");
    write(&c2, "project.yaml", PROJECT);
    write(&c2, "spaces/ovzdusie/space.yaml", SPACE);
    std::fs::create_dir_all(c2.join("spaces/ovzdusie/endpoints")).expect("dir");
    std::fs::copy(
        &endpoint,
        c2.join("spaces/ovzdusie/endpoints/public-air.yaml"),
    )
    .expect("copy");
    let moved = assemble_reusing(
        &org,
        &link(&links, "ovzdusie", &c2),
        &into,
        None,
        &mut renders,
    )
    .expect("assembles");
    assert_eq!(representations(&moved), serde_json::json!(["ngsi-ld"]));
    assert!(!moved.entries[0].reused, "the link moved");
}

/// T-4460: a checkout that is not a link can change in place, so it is rendered every time and
/// nothing is kept for it.
#[test]
fn a_checkout_that_is_no_link_renders_every_time() {
    let org = organization("plain", &[("ovzdusie", ENTRY)]);
    let dir = project_repository("plain");
    let into = temp_dir("plain-into").join("r");
    let mut renders = Renders::default();
    assemble_reusing(
        &org,
        &checkouts(&[("ovzdusie", &dir)]),
        &into,
        None,
        &mut renders,
    )
    .expect("assembles");
    write(
        &dir,
        "spaces/ovzdusie/endpoints/public-air.yaml",
        &ENDPOINT.replace(BOTH, ONE),
    );
    let again = assemble_reusing(
        &org,
        &checkouts(&[("ovzdusie", &dir)]),
        &into,
        None,
        &mut renders,
    )
    .expect("assembles");
    assert_eq!(representations(&again), serde_json::json!(["ngsi-ld"]));
    assert!(!again.entries[0].reused);
}

/// T-4460: a refused assembly puts every reused render back where it was and keeps the renders
/// it had, so the last tree still serves a project whose checkout goes missing and the next
/// assembly still reuses.
#[test]
fn a_refused_assembly_puts_the_reused_renders_back() {
    let entry = |slug: &str| ENTRY.replace("ovzdusie", slug);
    let org = organization("putback", &[("ovzdusie", ENTRY)]);
    let tree = project_repository("putback");
    let links = temp_dir("putback-links");
    let into = temp_dir("putback-into").join("r");
    let mut renders = Renders::default();
    let linked = link(&links, "ovzdusie", &tree);
    assemble_reusing(&org, &linked, &into, None, &mut renders).expect("assembles");

    // doprava claims ovzdusie's Endpoint slug: the assembly is refused after the reuse.
    write(&org, "projects/doprava.yaml", &entry("doprava"));
    let clash = temp_dir("putback-doprava");
    write(&clash, ".jc/layout", "2\n");
    write(
        &clash,
        "project.yaml",
        &PROJECT.replace("ovzdusie", "doprava"),
    );
    write(
        &clash,
        "spaces/doprava/space.yaml",
        &SPACE.replace("ovzdusie", "doprava"),
    );
    write(
        &clash,
        "spaces/doprava/endpoints/public-air.yaml",
        &ENDPOINT.replace("ovzdusie", "doprava"),
    );
    let mut both = linked.0.clone();
    both.insert("doprava".to_owned(), clash);
    let refused = assemble_reusing(&org, &Directories(both), &into, None, &mut renders)
        .expect_err("one slug, two projects");
    assert!(
        matches!(refused, AssembleError::Conflict { .. }),
        "{refused}"
    );
    assert!(
        into.join("projects/ovzdusie/spaces/ovzdusie/endpoints/public-air.yaml")
            .is_file(),
        "the reused render is back in the last tree"
    );

    std::fs::remove_file(org.join("projects/doprava.yaml")).expect("unregister");
    let next = assemble_reusing(&org, &linked, &into, None, &mut renders).expect("assembles");
    assert!(next.entries[0].reused, "the renders it had are kept");
    let fallback = assemble(&org, &Directories::default(), &into, None).expect("assembles");
    assert!(
        fallback.entries[0].rendered,
        "the last tree still serves ovzdusie"
    );
}
