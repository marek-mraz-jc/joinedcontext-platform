//! The gateway's store over layout 2: the organization checkout and every registered project's
//! checkout, assembled into the tables it serves (T-2641, CC-86, CC-89, ADR-N-029).

use context_gateway::app::Gateway;
use context_gateway::pdp::reaper::Reaper;
use context_gateway::pdp::PolicyPdp;
use context_gateway::proxy::Broker;
use context_gateway::store::{self, Checkouts};
use std::path::{Path, PathBuf};
use std::sync::Arc;

const ORG: &str = "apiVersion: joinedcontext.com/v1alpha1\nkind: Organization\nmetadata:\n  \
                   name: banskabystrica\n  namespace: org\nspec:\n  domain: banskabystrica.sk\n  \
                   locales: [\"sk\"]\n  defaultLocale: sk\n";

const AIR: &str = "zt4qm7ge2xdv6ksb3ncf5arw2y";
const TRAFFIC: &str = "b3ncf5arw2yzt4qm7ge2xdv6ks";
const PARKING: &str = "ge2xdv6ksb3ncf5arw2yzt4qm7";

fn temp_dir(test: &str) -> PathBuf {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock after the epoch")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("gw-store-layout-{test}-{now}"));
    std::fs::create_dir_all(&dir).expect("create the directory");
    dir
}

fn write(dir: &Path, rel: &str, body: &str) {
    let path = dir.join(rel);
    std::fs::create_dir_all(path.parent().expect("a parent")).expect("create the parent");
    std::fs::write(path, body).expect("write the file");
}

fn entry(slug: &str) -> String {
    format!(
        "apiVersion: joinedcontext.com/v1alpha1\nkind: Project\nmetadata:\n  name: {slug}\n  \
         namespace: org\nspec:\n  organizationRef: banskabystrica\n  repository: {{ name: {slug} }}\n  \
         ref: main\n"
    )
}

/// A project repository of layout 2 with one space and one public endpoint at `endpoint_slug`.
fn project(checkouts: &Path, slug: &str, endpoint_slug: &str) {
    let dir = checkouts.join(slug);
    write(&dir, ".jc/layout", "2\n");
    write(
        &dir,
        "project.yaml",
        &format!(
            "apiVersion: joinedcontext.com/v1alpha1\nkind: Project\nmetadata:\n  name: {slug}\n  \
             namespace: org\nspec:\n  organizationRef: banskabystrica\n"
        ),
    );
    write(
        &dir,
        &format!("spaces/{slug}/space.yaml"),
        &format!(
            "apiVersion: joinedcontext.com/v1alpha1\nkind: ContextSpace\nmetadata:\n  name: {slug}\n  \
             namespace: {slug}\nspec:\n  isSandbox: false\n"
        ),
    );
    write(
        &dir,
        &format!("spaces/{slug}/endpoints/public.yaml"),
        &format!(
            "apiVersion: joinedcontext.com/v1alpha1\nkind: Endpoint\nmetadata:\n  name: public\n  \
             namespace: {slug}\nspec:\n  contextSpaceRef: {slug}\n  slug: {endpoint_slug}\n  \
             audience: public\n  enabledRepresentations: [\"ngsi-ld\"]\n"
        ),
    );
}

/// An organization of layout 2 registering `ovzdusie` and `doprava`, and their checkouts.
fn organization(test: &str) -> (PathBuf, Checkouts) {
    let root = temp_dir(test);
    let org = root.join("org");
    write(&org, "org.yaml", ORG);
    write(&org, ".jc/layout", "2\n");
    write(&org, "projects/ovzdusie.yaml", &entry("ovzdusie"));
    write(&org, "projects/doprava.yaml", &entry("doprava"));
    let checkouts = Checkouts {
        projects: root.join("checkouts"),
        assembly: root.join("assembly"),
    };
    project(&checkouts.projects, "ovzdusie", AIR);
    project(&checkouts.projects, "doprava", TRAFFIC);
    (org, checkouts)
}

fn slugs(endpoints: &[context_gateway::resolver::Endpoint]) -> Vec<&str> {
    let mut slugs: Vec<&str> = endpoints.iter().map(|e| e.slug.as_str()).collect();
    slugs.sort_unstable();
    slugs
}

/// CC-86: both projects' endpoints are in the table, each in its own project and space.
#[test]
fn an_organization_and_two_project_checkouts_load_into_one_table() {
    let (org, checkouts) = organization("two");
    let (endpoints, spaces, ..) = store::load_from(&org, Some(&checkouts)).expect("assembles");
    assert_eq!(slugs(&endpoints), [TRAFFIC, AIR]);
    assert_eq!(spaces.len(), 2);
    let air = endpoints
        .iter()
        .find(|e| e.slug == AIR)
        .expect("the air endpoint");
    assert_eq!(air.project, "ovzdusie");

    // Without its checkouts a layout 2 organization is refused rather than served empty.
    let refused = store::load_from(&org, None).expect_err("no checkouts");
    assert!(
        refused.to_string().contains("JC_GATEWAY_PROJECTS_DIR"),
        "{refused}"
    );
}

/// CC-86: a project added to the registry is served on the next tick, and one taken out of it
/// stops being served, without a restart; its checkout left on disk serves nothing.
#[test]
fn the_reaper_follows_the_registry_and_the_checkouts() {
    let (org, checkouts) = organization("reaper");
    let (endpoints, spaces, ..) = store::load_from(&org, Some(&checkouts)).expect("assembles");
    let gateway = Arc::new(
        Gateway::new(
            Broker::new("http://127.0.0.1:1".to_owned()),
            Box::new(PolicyPdp),
            "banskabystrica.sk",
        )
        .serve(endpoints)
        .serve_spaces(spaces),
    );
    let mut reaper = Reaper::new(Arc::clone(&gateway), &org).with_checkouts(checkouts.clone());
    assert!(!reaper.tick(), "nothing changed yet");

    project(&checkouts.projects, "parkovanie", PARKING);
    reaper.tick();
    assert!(
        gateway.resolver.resolve(PARKING).is_none(),
        "a checkout nobody registered serves nothing"
    );

    write(&org, "projects/parkovanie.yaml", &entry("parkovanie"));
    assert!(reaper.tick(), "the registry changed");
    assert!(
        gateway.resolver.resolve(PARKING).is_some(),
        "the new project serves"
    );

    std::fs::remove_file(org.join("projects/doprava.yaml")).expect("unregister");
    assert!(reaper.tick(), "the registry changed again");
    assert!(
        gateway.resolver.resolve(TRAFFIC).is_none(),
        "the removed project stops"
    );
    assert!(gateway.resolver.resolve(AIR).is_some());
}

/// CC-86, PF-44: an endpoint slug two projects claim refuses the load, naming the slug and both
/// projects, and a reaper keeps serving the table it had.
#[test]
fn an_endpoint_slug_claimed_by_two_projects_is_refused() {
    let (org, checkouts) = organization("clash");
    let (endpoints, spaces, ..) = store::load_from(&org, Some(&checkouts)).expect("assembles");
    let gateway = Arc::new(
        Gateway::new(
            Broker::new("http://127.0.0.1:1".to_owned()),
            Box::new(PolicyPdp),
            "banskabystrica.sk",
        )
        .serve(endpoints)
        .serve_spaces(spaces),
    );
    let mut reaper = Reaper::new(Arc::clone(&gateway), &org).with_checkouts(checkouts.clone());

    project(&checkouts.projects, "doprava", AIR);
    let clash = store::load_from(&org, Some(&checkouts)).expect_err("one slug, two projects");
    let message = clash.to_string();
    for name in [AIR, "ovzdusie", "doprava"] {
        assert!(message.contains(name), "{name} is not in: {message}");
    }
    assert!(!reaper.tick(), "a refused load swaps nothing");
    assert!(
        gateway.resolver.resolve(TRAFFIC).is_some(),
        "the last table keeps serving"
    );
}

/// ADR-N-030, AP-97: the Endpoint `app-{name}` of a project that declares the App `{name}` is
/// that App's, reached by its client `app-{name}`; one named like an App nobody declares there,
/// and every other endpoint, gives its roles to the subjects its manifest names.
#[test]
fn an_apps_own_endpoint_is_known_by_the_app_declared_beside_it() {
    let (org, checkouts) = organization("app-client");
    let dir = checkouts.projects.join("ovzdusie");
    write(
        &dir,
        "apps/radar/app.yaml",
        "apiVersion: joinedcontext.com/v1alpha1\nkind: App\nmetadata:\n  name: radar\n  \
         namespace: ovzdusie\nspec:\n  kind: static\n  source:\n    path: ./src\n  build:\n    \
         node: \"22\"\n  visibility: organization\n  lifecycle: published\n  dataNeeds: []\n",
    );
    for (name, slug) in [
        ("app-radar", "r4d4rr4d4rr4d4rr4d4rr4d4rr"),
        ("app-ghost", "ghqstghqstghqstghqstghqstg"),
    ] {
        write(
            &dir,
            &format!("spaces/ovzdusie/endpoints/{name}.yaml"),
            &format!(
                "apiVersion: joinedcontext.com/v1alpha1\nkind: Endpoint\nmetadata:\n  name: {name}\n  \
                 namespace: ovzdusie\nspec:\n  contextSpaceRef: ovzdusie\n  slug: {slug}\n  \
                 audience: organization\n  enabledRepresentations: [\"ngsi-ld\"]\n  callerRole: true\n"
            ),
        );
    }
    let (endpoints, ..) = store::load_from(&org, Some(&checkouts)).expect("assembles");
    let client = |slug: &str| {
        endpoints
            .iter()
            .find(|endpoint| endpoint.slug == slug)
            .unwrap_or_else(|| panic!("{slug} is served"))
            .roles
            .app_client
            .clone()
    };
    assert_eq!(
        client("r4d4rr4d4rr4d4rr4d4rr4d4rr").as_deref(),
        Some("app-radar")
    );
    assert_eq!(client("ghqstghqstghqstghqstghqstg"), None);
    assert_eq!(client(AIR), None);
}
