//! A workspace preview is the repository rendered with `ws-{name}-` in front of every
//! organization-unique identity, and a render that leaks one is refused (CC-78, PF-83; T-1230).

use jcctl::loader::{LoadError, Repository, ResourceId};
use std::path::{Path, PathBuf};

fn write(dir: &Path, rel: &str, body: &str) {
    let path = dir.join(rel);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, body).unwrap();
}

/// Project `helsinki`: a pinned space `hub`, an unpinned space `bikes`, an Endpoint, a Policy
/// whose ids name the pinned segment, a Pipeline writing through the Endpoint, and a
/// RoleBinding scoped to the project.
fn repo(test: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("jcctl-preview-{test}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    write(&dir, "org.yaml", "apiVersion: joinedcontext.com/v1alpha1\nkind: Organization\nmetadata:\n  name: hel\n  namespace: org\nspec:\n  domain: hel.fi\n  locales: [\"en\"]\n  defaultLocale: en\n");
    write(&dir, "projects/helsinki/project.yaml", "apiVersion: joinedcontext.com/v1alpha1\nkind: Project\nmetadata:\n  name: helsinki\n  namespace: org\nspec:\n  organizationRef: hel\n");
    write(&dir, "projects/helsinki/spaces/hub/space.yaml", "apiVersion: joinedcontext.com/v1alpha1\nkind: ContextSpace\nmetadata:\n  name: hub\n  namespace: helsinki\nspec:\n  urnSegment: helsinki-hub\n");
    write(&dir, "projects/helsinki/spaces/bikes/space.yaml", "apiVersion: joinedcontext.com/v1alpha1\nkind: ContextSpace\nmetadata:\n  name: bikes\n  namespace: helsinki\nspec:\n  isSandbox: false\n");
    write(&dir, "projects/helsinki/spaces/hub/endpoints/all.yaml", "apiVersion: joinedcontext.com/v1alpha1\nkind: Endpoint\nmetadata:\n  name: all\n  namespace: helsinki\nspec:\n  slug: scsd2eehkx42n53z2zyd6vshfh7s7irf\n  contextSpaceRef: hub\n  audience: organization\n  policyRef: urn:ngsi-ld:Policy:{orgDomain}:helsinki-hub:read\n  enabledRepresentations: [ngsi-ld]\n");
    write(&dir, "projects/helsinki/spaces/hub/policies/read.yaml", "apiVersion: joinedcontext.com/v1alpha1\nkind: Policy\nmetadata:\n  name: read\n  namespace: helsinki\nspec:\n  contextSpaceRef: hub\n  assigner: did:web:{orgDomain}\n  assignee: { kind: role, id: viewer }\n  operations: [retrieveOps]\n  information:\n    - entities: [{ idPattern: \"^urn:ngsi-ld:Vehicle:hel\\\\.fi:helsinki-hub:.*$\" }]\n");
    write(&dir, "projects/helsinki/pipelines/bikes/pipeline.yaml", "apiVersion: joinedcontext.com/v1alpha1\nkind: Pipeline\nmetadata:\n  name: bikes\n  namespace: helsinki\nspec:\n  class: auto\n  targetEndpoint: urn:ngsi-ld:Endpoint:{orgDomain}:helsinki-hub:all\n  source:\n    endpointRef: { kind: Endpoint, name: all, namespace: helsinki }\n");
    dir
}

fn id(kind: &str, namespace: &str, name: &str) -> ResourceId {
    ResourceId {
        group: "joinedcontext.com".to_owned(),
        kind: kind.to_owned(),
        namespace: Some(namespace.to_owned()),
        name: name.to_owned(),
    }
}

#[test]
fn preview_render_prefix_applied() {
    let dir = repo("applied");
    let preview = Repository::load_preview(&dir, None, "ws-demo-").expect("the preview renders");

    // The namespace, and the Project that names it.
    assert!(preview
        .get(&id("ContextSpace", "ws-demo-helsinki", "bikes"))
        .is_some());
    assert!(preview
        .get(&id("Project", "org", "ws-demo-helsinki"))
        .is_some());
    // An unpinned space renders from the prefixed namespace; a pinned one keeps its pin, prefixed.
    assert_eq!(
        preview.space_segment("ws-demo-helsinki", "bikes"),
        "ws-demo-helsinki-bikes"
    );
    assert_eq!(
        preview.space_segment("ws-demo-helsinki", "hub"),
        "ws-demo-helsinki-hub"
    );
    // Ids of this organization, plain and as a pattern; a typed reference's namespace.
    let pipeline = &preview
        .get(&id("Pipeline", "ws-demo-helsinki", "bikes"))
        .unwrap()
        .manifest
        .spec;
    assert_eq!(
        pipeline["targetEndpoint"],
        "urn:ngsi-ld:Endpoint:hel.fi:ws-demo-helsinki-hub:all"
    );
    assert_eq!(
        pipeline["source"]["endpointRef"]["namespace"],
        "ws-demo-helsinki"
    );
    let policy = &preview
        .get(&id("Policy", "ws-demo-helsinki", "read"))
        .unwrap()
        .manifest
        .spec;
    assert_eq!(
        policy["information"][0]["entities"][0]["idPattern"],
        "^urn:ngsi-ld:Vehicle:hel\\.fi:ws-demo-helsinki-hub:.*$"
    );
    // Nothing that is not organization-unique moves: the assigner, the file's path.
    assert_eq!(policy["assigner"], "did:web:hel.fi");
    assert_eq!(
        preview
            .get(&id("Pipeline", "ws-demo-helsinki", "bikes"))
            .unwrap()
            .path,
        PathBuf::from("projects/helsinki/pipelines/bikes/pipeline.yaml"),
        "nothing is renamed at rest (CC-77)"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn an_empty_prefix_is_the_ordinary_load() {
    let dir = repo("empty");
    let preview = Repository::load_preview(&dir, None, "").expect("loads");
    let plain = Repository::load_for(&dir, None).expect("loads");
    assert_eq!(preview, plain);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn preview_render_refuses_unprefixed_name() {
    let dir = repo("refused");
    // An id another organization's instance would own is left alone; an id of this one that
    // the renderer cannot place (a space typed into a registration's free text) is the leak.
    write(&dir, "projects/helsinki/spaces/hub/registrations/r.yaml", "apiVersion: joinedcontext.com/v1alpha1\nkind: ContextSourceRegistration\nmetadata:\n  name: r\n  namespace: helsinki\nspec:\n  contextSpaceRef: hub\n  endpoint: https://other.example/ngsi-ld/v1\n  information:\n    - entities: [{ id: \"urn:ngsi-ld:Vehicle:tampere.fi:tampere:1\" }]\n  federation: { identity: anonymous }\n");
    Repository::load_preview(&dir, None, "ws-demo-")
        .expect("another organization's id is not ours to prefix");

    // An id of this organization typed into a mapping is what no renderer can move.
    write(&dir, "projects/helsinki/pipelines/bikes/pipeline.yaml", "apiVersion: joinedcontext.com/v1alpha1\nkind: Pipeline\nmetadata:\n  name: bikes\n  namespace: helsinki\nspec:\n  class: auto\n  targetEndpoint: urn:ngsi-ld:Endpoint:{orgDomain}:helsinki-hub:all\n  source:\n    endpointRef: { kind: Endpoint, name: all }\n  compute:\n    kind: bloblang\n    bloblang: 'root.id = \"urn:ngsi-ld:Vehicle:hel.fi:helsinki-hub:\" + this.id'\n");
    let err = Repository::load_preview(&dir, None, "ws-demo-").expect_err("the mapping leaks");
    assert!(matches!(err, LoadError::UnprefixedName { .. }), "{err}");
    assert!(
        err.to_string().contains("Pipeline"),
        "names the resource: {err}"
    );
    assert!(err.to_string().contains("helsinki-hub"), "{err}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_preview_endpoint_answers_on_a_minted_slug_of_its_own() {
    let dir = repo("slug");
    let origin = "scsd2eehkx42n53z2zyd6vshfh7s7irf";
    let preview = Repository::load_preview(&dir, None, "ws-demo-").expect("the preview renders");
    let slug = preview
        .get(&id("Endpoint", "ws-demo-helsinki", "all"))
        .unwrap()
        .manifest
        .spec["slug"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_ne!(slug, origin);
    assert_eq!(
        slug,
        jcctl::loader::preview_slug("ws-demo-", origin),
        "the same on every render"
    );
    assert_ne!(
        slug,
        jcctl::loader::preview_slug("ws-other-", origin),
        "one per workspace"
    );
    jc_core::kinds::EndpointSlug::new(&slug).expect("opaque base32 like every slug (EP-02)");
    // The ordinary load keeps the origin's slug.
    let main = Repository::load_preview(&dir, None, "").unwrap();
    assert_eq!(
        main.get(&id("Endpoint", "helsinki", "all"))
            .unwrap()
            .manifest
            .spec["slug"],
        origin
    );
}
