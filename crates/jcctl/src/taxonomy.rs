//! The role taxonomy every organization starts from (PF-56, PF-71, T-0873).
//!
//! Nine roles, written as `Role` manifests into `users/roles/`: what a person may read, what
//! each kind of editor may propose, who approves, and who may let data out to the public. They
//! are a seed a person extends by proposing further roles, never a list the platform enforces:
//! `jcctl roles seed` writes a file only where there is none, so an edited role stays edited.
//!
//! "Every project kind" is read from the catalogue rather than typed out, so a kind added to
//! `jc-core` tomorrow is covered by the seed the day it lands.

use jc_core::envelope::Scope;
use jc_core::kinds::{Constraint, Rule, Verb};
use jc_core::registry;
use serde::Serialize;
use std::path::{Path, PathBuf};

/// The audience an Endpoint carries when its data is open to anyone (EP-14).
const PUBLIC: &str = "public";

/// One seeded role, as the file it is written to.
pub struct Seeded {
    /// `metadata.name`, which is also the file name.
    pub name: &'static str,
    /// What the role is for, one sentence, written into the manifest as a comment.
    pub purpose: &'static str,
    /// Its rules.
    pub rules: Vec<Rule>,
}

impl Seeded {
    /// Where the role is written: the organization's own roles (PF-49).
    pub fn path(&self) -> String {
        format!("users/roles/{}.yaml", self.name)
    }
}

#[derive(Serialize)]
struct Manifest<'a> {
    #[serde(rename = "apiVersion")]
    api_version: &'a str,
    kind: &'a str,
    metadata: Meta<'a>,
    spec: Spec,
}

#[derive(Serialize)]
struct Meta<'a> {
    name: &'a str,
    namespace: &'a str,
}

#[derive(Serialize)]
struct Spec {
    rules: Vec<Rule>,
}

fn rule(kinds: Vec<String>, verbs: Vec<Verb>) -> Rule {
    Rule {
        kinds,
        verbs,
        constraints: Vec::new(),
    }
}

fn constrained(kinds: Vec<String>, verbs: Vec<Verb>, constraint: Constraint) -> Rule {
    Rule {
        kinds,
        verbs,
        constraints: vec![constraint],
    }
}

fn audience_is_public() -> Constraint {
    Constraint {
        field: "spec.audience".into(),
        one_of: vec![PUBLIC.into()],
        not_in: Vec::new(),
        equals: None,
        pattern: None,
    }
}

fn audience_is_not_public() -> Constraint {
    Constraint {
        field: "spec.audience".into(),
        one_of: Vec::new(),
        not_in: vec![PUBLIC.into()],
        equals: None,
        pattern: None,
    }
}

fn visibility_is_public() -> Constraint {
    Constraint {
        field: "spec.visibility".into(),
        one_of: vec![PUBLIC.into()],
        not_in: Vec::new(),
        equals: None,
        pattern: None,
    }
}

/// An App without `spec.visibility` is `project` (AP-120), which `not_in` admits.
fn visibility_is_not_public() -> Constraint {
    Constraint {
        field: "spec.visibility".into(),
        one_of: Vec::new(),
        not_in: vec![PUBLIC.into()],
        equals: None,
        pattern: None,
    }
}

/// Every verb on the people of the realm (PF-91, ADR-N-031).
fn people() -> Rule {
    rule(
        vec![jc_core::kinds::PERSON.into()],
        vec![
            Verb::Read,
            Verb::Create,
            Verb::Update,
            Verb::Disable,
            Verb::Delete,
        ],
    )
}

/// Every kind that lives inside a project, in catalogue order.
pub fn project_kinds() -> Vec<String> {
    registry::KINDS
        .iter()
        .filter(|info| info.scope == Scope::Project)
        .map(|info| info.kind.to_owned())
        .collect()
}

/// Every kind there is, the organization's included.
fn all_kinds() -> Vec<String> {
    registry::KINDS
        .iter()
        .map(|info| info.kind.to_owned())
        .collect()
}

fn without(kinds: Vec<String>, dropped: &str) -> Vec<String> {
    kinds.into_iter().filter(|kind| kind != dropped).collect()
}

/// The nine roles of the taxonomy (PF-56, PF-71, PF-91).
pub fn taxonomy() -> Vec<Seeded> {
    let project = project_kinds;
    vec![
        Seeded {
            name: "viewer",
            purpose: "Reads everything in a project and changes nothing.",
            rules: vec![rule(project(), vec![Verb::Read])],
        },
        Seeded {
            name: "model-editor",
            purpose: "Writes the data models and their mappings.",
            rules: vec![rule(
                vec!["DataModel".into(), "Mapping".into()],
                vec![Verb::Propose],
            )],
        },
        Seeded {
            name: "pipeline-editor",
            purpose: "Writes the pipelines and the sources they read.",
            rules: vec![rule(
                vec!["Pipeline".into(), "DataSource".into()],
                vec![Verb::Propose],
            )],
        },
        Seeded {
            name: "endpoint-editor",
            purpose: "Writes the endpoints; letting one out to the public is the publisher's.",
            rules: vec![rule(vec!["Endpoint".into()], vec![Verb::Propose])],
        },
        Seeded {
            name: "app-editor",
            purpose: "Writes the applications.",
            rules: vec![rule(vec!["App".into()], vec![Verb::Propose])],
        },
        Seeded {
            name: "steward",
            purpose: "The domain lead: proposes and approves everything in the project, except \
                      letting an endpoint or an app out to the public (PF-71).",
            rules: vec![
                rule(
                    without(without(project(), "Endpoint"), "App"),
                    vec![Verb::Propose, Verb::Approve],
                ),
                // Proposing a public App is the author's; approving it is the publisher's (AP-120).
                rule(vec!["App".into()], vec![Verb::Propose]),
                constrained(
                    vec!["App".into()],
                    vec![Verb::Approve],
                    visibility_is_not_public(),
                ),
                constrained(
                    vec!["Endpoint".into()],
                    vec![Verb::Propose, Verb::Approve],
                    audience_is_not_public(),
                ),
            ],
        },
        Seeded {
            name: "publisher",
            purpose: "Decides what the city publishes to the public, and nothing else (PF-71).",
            rules: vec![
                rule(project(), vec![Verb::Read]),
                constrained(
                    vec!["Endpoint".into()],
                    vec![Verb::Approve],
                    audience_is_public(),
                ),
                constrained(
                    vec!["App".into()],
                    vec![Verb::Approve],
                    visibility_is_public(),
                ),
            ],
        },
        Seeded {
            name: "people-admin",
            purpose: "Creates, edits, disables and deletes the people of the organization's \
                      realm; what they may do stays with the bindings (PF-91).",
            rules: vec![people()],
        },
        Seeded {
            name: "org-admin",
            purpose: "The one role that approves the red lane: every verb on every kind, roles \
                      and bindings included (CC-70), and the people of the realm (PF-91).",
            rules: vec![
                rule(
                    all_kinds(),
                    vec![Verb::Propose, Verb::Approve, Verb::Delete],
                ),
                people(),
            ],
        },
    ]
}

/// The taxonomy as `(path, file content)`, ready to be written.
pub fn files() -> Vec<(String, String)> {
    taxonomy()
        .into_iter()
        .map(|seeded| {
            let manifest = Manifest {
                api_version: jc_core::API_VERSION,
                kind: "Role",
                metadata: Meta {
                    name: seeded.name,
                    namespace: jc_core::envelope::ORG_NAMESPACE,
                },
                spec: Spec {
                    rules: seeded.rules.clone(),
                },
            };
            let body = serde_norway::to_string(&manifest).expect("a seeded role serialises");
            let header = format!(
                "# {}\n# Seeded by `jcctl roles seed` (PF-56). Edit it: the seed never \
                 overwrites a file that exists.\n",
                seeded.purpose
            );
            (seeded.path(), format!("{header}{body}"))
        })
        .collect()
}

/// Writes the roles the repository does not have yet, and returns what it wrote.
///
/// A role somebody edited, or removed on purpose, is not written back: only a missing file is
/// filled in, so seeding an organization twice changes nothing the second time.
pub fn seed(repo_dir: &Path) -> std::io::Result<Vec<PathBuf>> {
    let mut written = Vec::new();
    for (rel, content) in files() {
        let path = repo_dir.join(&rel);
        if path.exists() {
            continue;
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&path, content)?;
        written.push(PathBuf::from(rel));
    }
    Ok(written)
}
