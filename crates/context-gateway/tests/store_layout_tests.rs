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
        renders: Default::default(),
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

/// AP-113 (T-2860): an App's client is admitted on the Endpoints its App reads, by the rule the
/// Portal gives the client's audiences with: its own generated Endpoint for its space (not the
/// other Endpoint of that space), the target of the project's SharedSpaceReference, and nothing
/// else. An `app-*` client no App names reaches nothing; any other client is not narrowed.
#[test]
fn an_apps_client_is_admitted_on_the_endpoints_its_app_reads_and_no_other() {
    let (org, checkouts) = organization("app-audiences");
    let dir = checkouts.projects.join("ovzdusie");
    const OWN: &str = "r4d4rr4d4rr4d4rr4d4rr4d4rr";
    write(
        &dir,
        "apps/radar/app.yaml",
        "apiVersion: joinedcontext.com/v1alpha1\nkind: App\nmetadata:\n  name: radar\n  \
         namespace: ovzdusie\nspec:\n  kind: static\n  source:\n    path: ./src\n  build:\n    \
         node: \"22\"\n  visibility: organization\n  lifecycle: published\n  dataNeeds:\n  \
         - contextSpaceRef: { kind: ContextSpace, name: ovzdusie }\n    types: [AirQualityObserved]\n    \
         operations: [queryEntity]\n",
    );
    write(
        &dir,
        "spaces/ovzdusie/endpoints/app-radar.yaml",
        &format!(
            "apiVersion: joinedcontext.com/v1alpha1\nkind: Endpoint\nmetadata:\n  name: app-radar\n  \
             namespace: ovzdusie\n  annotations:\n    joinedcontext.com/generated-by: {}\nspec:\n  \
             contextSpaceRef: ovzdusie\n  slug: {OWN}\n  audience: organization\n  \
             enabledRepresentations: [\"ngsi-ld\"]\n  callerRole: true\n",
            jc_core::kinds::app::APP_ENDPOINT_GENERATOR
        ),
    );
    write(
        &dir,
        "shared/traffic.yaml",
        "apiVersion: joinedcontext.com/v1alpha1\nkind: SharedSpaceReference\nmetadata:\n  \
         name: traffic\n  namespace: ovzdusie\nspec:\n  endpointRef: { project: doprava, name: public }\n  \
         alias: traffic\n",
    );
    let (endpoints, _, accounts, ..) = store::load_from(&org, Some(&checkouts)).expect("assembles");
    assert!(
        slugs(&endpoints).contains(&TRAFFIC) && slugs(&endpoints).contains(&OWN),
        "{:?}",
        slugs(&endpoints)
    );

    assert!(accounts.admits_on("app-radar", OWN), "its own Endpoint");
    assert!(accounts.admits_on("app-radar", TRAFFIC), "the shared one");
    assert!(
        !accounts.admits_on("app-radar", AIR),
        "another Endpoint of its space is not one the App reads"
    );
    assert!(!accounts.admits_on("app-ghost", OWN), "no App named ghost");
    assert!(accounts.admits_on("edge", AIR), "a client that is no App's");
}

/// AP-04 (T-2933): an App may read one further space of its project, through that space's public
/// Endpoints alone; its client is admitted on the public one and not on another Endpoint of the
/// same space.
#[test]
fn an_apps_client_reaches_a_further_space_through_its_public_endpoint_alone() {
    let (org, checkouts) = organization("app-further-space");
    let dir = checkouts.projects.join("ovzdusie");
    const PUBLIC: &str = "pvbhcpvbhcpvbhcpvbhcpvbhcp";
    const INTERNAL: &str = "ntrnantrnantrnantrnantrnan";
    write(
        &dir,
        "apps/radar/app.yaml",
        "apiVersion: joinedcontext.com/v1alpha1\nkind: App\nmetadata:\n  name: radar\n  \
         namespace: ovzdusie\nspec:\n  kind: static\n  source:\n    path: ./src\n  build:\n    \
         node: \"22\"\n  visibility: organization\n  lifecycle: published\n  dataNeeds:\n  \
         - contextSpaceRef: { kind: ContextSpace, name: ovzdusie }\n    types: [AirQualityObserved]\n    \
         operations: [queryEntity]\n  \
         - contextSpaceRef: { kind: ContextSpace, name: registre }\n    types: [AdministrativeArea]\n    \
         operations: [queryEntity]\n",
    );
    write(
        &dir,
        "spaces/registre/space.yaml",
        "apiVersion: joinedcontext.com/v1alpha1\nkind: ContextSpace\nmetadata:\n  name: registre\n  \
         namespace: ovzdusie\nspec:\n  isSandbox: false\n",
    );
    for (name, slug, audience) in [
        ("registre-public", PUBLIC, "public"),
        ("registre-internal", INTERNAL, "organization"),
    ] {
        write(
            &dir,
            &format!("spaces/registre/endpoints/{name}.yaml"),
            &format!(
                "apiVersion: joinedcontext.com/v1alpha1\nkind: Endpoint\nmetadata:\n  name: {name}\n  \
                 namespace: ovzdusie\nspec:\n  contextSpaceRef: registre\n  slug: {slug}\n  \
                 audience: {audience}\n  enabledRepresentations: [\"ngsi-ld\"]\n"
            ),
        );
    }
    let (endpoints, _, accounts, ..) = store::load_from(&org, Some(&checkouts)).expect("assembles");
    assert!(
        slugs(&endpoints).contains(&PUBLIC) && slugs(&endpoints).contains(&INTERNAL),
        "{:?}",
        slugs(&endpoints)
    );

    assert!(
        accounts.admits_on("app-radar", PUBLIC),
        "the further space's public Endpoint"
    );
    assert!(
        !accounts.admits_on("app-radar", INTERNAL),
        "a further space is never read through an Endpoint that is not public"
    );
    assert!(
        accounts.admits_on("app-radar", AIR),
        "its own space's endpoint, none generated yet"
    );
}

/// T-4444: checkouts linked in by commit, as `jcctl checkouts` writes them, are fingerprinted by
/// their links: an idle tick stats no file of any project, and a project relinked to a commit
/// that changes a Policy is served on the next tick.
#[cfg(unix)]
#[test]
fn an_idle_tick_reads_the_checkout_links_and_walks_no_project_file() {
    let root = temp_dir("linked");
    let tree = jcctl::bench_org::write(&root.join("bench"), 200).expect("the tree");
    let projects = root.join("checkouts");
    let cache = projects.join(".cache");
    std::fs::create_dir_all(&cache).expect("the cache");
    for slug in ["p0000", "p0001"] {
        let commit = cache.join(format!("{slug}.tree-c1"));
        std::fs::rename(tree.projects.join(slug), &commit).expect("the tree moves");
        std::os::unix::fs::symlink(format!(".cache/{slug}.tree-c1"), projects.join(slug))
            .expect("the link");
    }
    let checkouts = Checkouts {
        projects: projects.clone(),
        assembly: root.join("assembly"),
        renders: Default::default(),
    };
    let (endpoints, spaces, ..) = store::load_from(&tree.org, Some(&checkouts)).expect("loads");
    assert_eq!(endpoints.len(), 200);
    let gateway = Arc::new(
        Gateway::new(
            Broker::new("http://127.0.0.1:1".to_owned()),
            Box::new(PolicyPdp),
            "bench.example",
        )
        .serve(endpoints)
        .serve_spaces(spaces),
    );
    let mut reaper = Reaper::new(Arc::clone(&gateway), &tree.org).with_checkouts(checkouts);
    assert!(!reaper.tick(), "nothing changed");
    // org.yaml, the registry directory and its two entries; none of the 600-odd project files.
    assert_eq!(reaper.files_walked(), 4);

    // The next commit of p0001: its first Endpoint withdrawn, swapped in by one rename.
    let next = cache.join("p0001.tree-c2");
    copy(&cache.join("p0001.tree-c1"), &next);
    std::fs::remove_file(next.join("spaces/s00100/endpoints/e00100.yaml")).expect("withdrawn");
    let link = projects.join(".p0001.link");
    std::os::unix::fs::symlink(".cache/p0001.tree-c2", &link).expect("the next link");
    std::fs::rename(&link, projects.join("p0001")).expect("relinked");
    assert!(reaper.tick(), "the relinked project reloads");
    assert!(gateway.resolver.resolve(&tree.slugs[100]).is_none());
    assert!(gateway.resolver.resolve(&tree.slugs[101]).is_some());
    let _ = std::fs::remove_dir_all(&root);
}

/// T-4460, R48: a relinked project whose one Policy changed is the only one rendered again. Its
/// Endpoint carries the new Policy at once, and the other project's Endpoint is the same `Arc`
/// it was.
#[test]
fn a_policy_change_in_one_project_leaves_the_other_projects_endpoints_as_they_were() {
    let root = temp_dir("one-project");
    let tree = jcctl::bench_org::write(&root.join("bench"), 200).expect("the tree");
    let projects = root.join("checkouts");
    let cache = projects.join(".cache");
    std::fs::create_dir_all(&cache).expect("the cache");
    for slug in ["p0000", "p0001"] {
        let commit = cache.join(format!("{slug}.tree-c1"));
        std::fs::rename(tree.projects.join(slug), &commit).expect("the tree moves");
        std::os::unix::fs::symlink(format!(".cache/{slug}.tree-c1"), projects.join(slug))
            .expect("the link");
    }
    let checkouts = Checkouts {
        projects: projects.clone(),
        assembly: root.join("assembly"),
        renders: Default::default(),
    };
    let (endpoints, spaces, ..) = store::load_from(&tree.org, Some(&checkouts)).expect("loads");
    let gateway = Arc::new(
        Gateway::new(
            Broker::new("http://127.0.0.1:1".to_owned()),
            Box::new(PolicyPdp),
            "bench.example",
        )
        .serve(endpoints)
        .serve_spaces(spaces),
    );
    let mut reaper = Reaper::new(Arc::clone(&gateway), &tree.org).with_checkouts(checkouts);
    let operations = |slug: &str| {
        let endpoint = gateway.resolver.resolve(slug).expect("served");
        serde_json::to_value(&endpoint.policies[0]).expect("a policy")["operations"].clone()
    };
    let a = gateway.resolver.resolve(&tree.slugs[0]).expect("served");
    let b = gateway.resolver.resolve(&tree.slugs[100]).expect("served");
    assert_eq!(
        operations(&tree.slugs[0]),
        serde_json::json!(["retrieveOps"])
    );

    // The next commit of p0000 grants one more operation, swapped in by one rename.
    let next = cache.join("p0000.tree-c2");
    copy(&cache.join("p0000.tree-c1"), &next);
    let policy = next.join("spaces/s00000/policies/r00000.yaml");
    let text = std::fs::read_to_string(&policy).expect("the policy");
    std::fs::write(
        &policy,
        text.replace("[retrieveOps]", "[retrieveOps, queryEntity]"),
    )
    .expect("the policy changes");
    let link = projects.join(".p0000.link");
    std::os::unix::fs::symlink(".cache/p0000.tree-c2", &link).expect("the next link");
    std::fs::rename(&link, projects.join("p0000")).expect("relinked");
    assert!(reaper.tick(), "the policy changed");

    assert_eq!(
        operations(&tree.slugs[0]),
        serde_json::json!(["retrieveOps", "queryEntity"])
    );
    assert!(!Arc::ptr_eq(
        &a,
        &gateway.resolver.resolve(&tree.slugs[0]).expect("served")
    ));
    assert!(
        Arc::ptr_eq(
            &b,
            &gateway.resolver.resolve(&tree.slugs[100]).expect("served")
        ),
        "p0001 did not change, and its Endpoint is the one it was"
    );

    // Withdrawn again: the grant goes in the same swap (R48).
    let back = projects.join(".p0000.back");
    std::os::unix::fs::symlink(".cache/p0000.tree-c1", &back).expect("the old link");
    std::fs::rename(&back, projects.join("p0000")).expect("relinked back");
    assert!(reaper.tick(), "the policy changed back");
    assert_eq!(
        operations(&tree.slugs[0]),
        serde_json::json!(["retrieveOps"])
    );
    let _ = std::fs::remove_dir_all(&root);
}

fn copy(from: &Path, to: &Path) {
    for entry in std::fs::read_dir(from).expect("a directory").flatten() {
        let target = to.join(entry.file_name());
        if entry.path().is_dir() {
            std::fs::create_dir_all(&target).expect("a directory");
            copy(&entry.path(), &target);
        } else {
            std::fs::create_dir_all(to).expect("a directory");
            std::fs::copy(entry.path(), &target).expect("a copy");
        }
    }
}

/// The Organization's `spec.dcatProfiles` is what the record routes offer (T-4499): a listed
/// profile is served, an unlisted one is not, and a list that does not parse serves DCAT-AP
/// alone instead of failing the load.
#[test]
fn the_organizations_dcat_profiles_are_read_and_a_broken_list_serves_the_core_alone() {
    for (listed, sk, cz) in [
        ("  dcatProfiles: [dcat-ap-sk]\n", true, false),
        ("  dcatProfiles: [dcat-ap-cz, dcat-ap-sk]\n", true, true),
        ("", false, false),
        ("  dcatProfiles: [dcat-ap-xx]\n", false, false),
        ("  dcatProfiles: dcat-ap-sk\n", false, false),
    ] {
        let org = temp_dir("dcat-profiles");
        write(&org, "org.yaml", &format!("{ORG}{listed}"));
        let repo = jcctl::loader::Repository::load(&org).expect("the repository loads");
        let offered = store::limits_of(&repo).dcat_profiles;
        assert_eq!(offered.pick("dcat-ap-sk").is_some(), sk, "{listed:?}");
        assert_eq!(offered.pick("dcat-ap-cz").is_some(), cz, "{listed:?}");
        assert!(offered.pick("dcat-ap").is_some(), "{listed:?}");
    }
}
