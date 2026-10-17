//! A generated organization of layout 2 for the scale bench (T-4443, CC-54): `endpoints`
//! public Endpoints over `endpoints / 100` projects (the `publicEndpoints` quota is 100 a
//! project), each Endpoint in its own ContextSpace with its own read Policy, one DataModel a
//! project and one Pipeline per ten Endpoints. The same `endpoints` gives the same tree, byte for
//! byte: names count, slugs come from a fixed-seed generator. Synthetic data only, no secret.

use std::io;
use std::path::{Path, PathBuf};

/// Endpoints a project holds at most, the default `publicEndpoints` quota.
pub const PER_PROJECT: usize = 100;

/// The organization the tree registers its projects in.
pub const ORGANIZATION: &str = "bench";

/// Where [`write()`] put the tree: the organization checkout and the project checkouts beside it.
#[derive(Debug, Clone)]
pub struct Tree {
    /// The organization checkout: `org.yaml` and the project registry.
    pub org: PathBuf,
    /// The project checkouts, one directory per project.
    pub projects: PathBuf,
    /// Every Endpoint's slug, in the order written.
    pub slugs: Vec<String>,
}

const HEAD: &str = "apiVersion: joinedcontext.com/v1alpha1\n";

const LINKML: &str = "id: https://bench.example/models/reading\nname: reading\n\
prefixes:\n  linkml: https://w3id.org/linkml/\n  bench: https://bench.example/models/\n\
default_prefix: bench\ndefault_range: string\nimports: [linkml:types]\n\
classes:\n  Reading:\n    class_uri: bench:Reading\n    attributes:\n      value:\n        range: float\n";

/// Writes the tree under `root`, which must not hold one yet.
pub fn write(root: &Path, endpoints: usize) -> io::Result<Tree> {
    let org = root.join("org");
    let projects = root.join("checkouts");
    put(&org, ".jc/layout", "2\n")?;
    put(
        &org,
        "org.yaml",
        &format!(
            "{HEAD}kind: Organization\nmetadata:\n  name: {ORGANIZATION}\n  namespace: org\n\
             spec:\n  domain: bench.example\n  locales: [\"en\"]\n  defaultLocale: en\n"
        ),
    )?;
    let mut random = 0x7443_u64;
    let mut slugs = Vec::with_capacity(endpoints);
    for (p, first) in (0..endpoints).step_by(PER_PROJECT).enumerate() {
        let project = format!("p{p:04}");
        put(
            &org,
            &format!("projects/{project}.yaml"),
            &format!(
                "{HEAD}kind: Project\nmetadata:\n  name: {project}\n  namespace: org\n\
                 spec:\n  organizationRef: {ORGANIZATION}\n  repository: {{ name: {project} }}\n  ref: main\n"
            ),
        )?;
        let dir = projects.join(&project);
        put(&dir, ".jc/layout", "2\n")?;
        put(
            &dir,
            "project.yaml",
            &format!(
                "{HEAD}kind: Project\nmetadata:\n  name: {project}\n  namespace: org\n\
                 spec:\n  organizationRef: {ORGANIZATION}\n"
            ),
        )?;
        put(&dir, "datamodels/reading/reading.linkml.yaml", LINKML)?;
        put(
            &dir,
            "datamodels/reading/reading.yaml",
            &format!(
                "{HEAD}kind: DataModel\nmetadata:\n  name: reading\n  namespace: {project}\n\
                 spec:\n  linkml: reading.linkml.yaml\n  version: 1.0.0\n  lifecycle: draft\n  classes: [Reading]\n"
            ),
        )?;
        for i in first..endpoints.min(first + PER_PROJECT) {
            let space = format!("s{i:05}");
            let slug = opaque(&mut random, &[&project, &space]);
            let at = format!("spaces/{space}");
            put(
                &dir,
                &format!("{at}/space.yaml"),
                &format!(
                    "{HEAD}kind: ContextSpace\nmetadata:\n  name: {space}\n  namespace: {project}\n\
                     spec:\n  isSandbox: false\n"
                ),
            )?;
            put(
                &dir,
                &format!("{at}/endpoints/e{i:05}.yaml"),
                &format!(
                    "{HEAD}kind: Endpoint\nmetadata:\n  name: e{i:05}\n  namespace: {project}\n\
                     spec:\n  contextSpaceRef: {space}\n  slug: {slug}\n  audience: public\n  \
                     enabledRepresentations: [\"ngsi-ld\"]\n"
                ),
            )?;
            put(
                &dir,
                &format!("{at}/policies/r{i:05}.yaml"),
                &format!(
                    "{HEAD}kind: Policy\nmetadata:\n  name: r{i:05}\n  namespace: {project}\n\
                     spec:\n  contextSpaceRef: {{ kind: ContextSpace, name: {space} }}\n  \
                     assigner: did:web:bench.example\n  assignee: {{ kind: role, id: public }}\n  \
                     operations: [retrieveOps]\n"
                ),
            )?;
            if i % 10 == 0 {
                put(
                    &dir,
                    &format!("pipelines/f{i:05}/pipeline.yaml"),
                    &format!(
                        "{HEAD}kind: Pipeline\nmetadata:\n  name: f{i:05}\n  namespace: {project}\n\
                         spec:\n  class: auto\n  \
                         targetEndpoint: urn:ngsi-ld:Endpoint:bench.example:{space}:e{i:05}\n  \
                         source:\n    endpointRef: {{ kind: Endpoint, name: e{i:05}, namespace: {project} }}\n"
                    ),
                )?;
            }
            slugs.push(slug);
        }
    }
    Ok(Tree {
        org,
        projects,
        slugs,
    })
}

fn put(dir: &Path, rel: &str, body: &str) -> io::Result<()> {
    let path = dir.join(rel);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, body)
}

/// A 26-letter base32 slug that contains no three letters of any of `names` (EP-02, EP-03).
fn opaque(state: &mut u64, names: &[&str]) -> String {
    const ALPHABET: &[u8; 32] = b"abcdefghijklmnopqrstuvwxyz234567";
    loop {
        let slug: String = (0..26)
            .map(|_| {
                // splitmix64: a fixed seed gives the same slugs every run.
                *state = state.wrapping_add(0x9e37_79b9_7f4a_7c15);
                let mut z = *state;
                z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
                z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
                char::from(ALPHABET[((z ^ (z >> 31)) % 32) as usize])
            })
            .collect();
        let leaks = names.iter().any(|name| {
            name.as_bytes()
                .windows(3)
                .any(|w| std::str::from_utf8(w).is_ok_and(|w| slug.contains(w)))
        });
        if !leaks {
            return slug;
        }
    }
}
