//! Roles as code compiled for the forge and the repository's own CI (T-0527, PF-51, PF-52).
//!
//! The `Role` and `RoleBinding` manifests under `users/` are the source. From them
//! `render` writes what a merge request outside the Portal is bound by: `CODEOWNERS`
//! (who reviews which paths, CC-41), `policies/roles.json` (the same bindings as data for
//! the Rego gate), the gate itself, its executable cases (PF-52) and the workflow that
//! runs them. `input` turns a diff into the document the gate evaluates.

use crate::loader::{RawManifest, Repository};
use jc_core::envelope::ORG_NAMESPACE;
use jc_core::kinds::{RoleBindingSpec, RoleSpec, Subject, Verb};
use serde::Serialize;
use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};

/// Who reviews which paths, as Gitea reads it (CC-41).
pub const CODEOWNERS: &str = "CODEOWNERS";
/// The bindings as data for the Rego gate.
pub const ROLES_JSON: &str = "policies/roles.json";
/// The gate itself.
pub const ROLES_REGO: &str = "policies/roles.rego";
/// The executable cases of PF-52.
pub const ROLES_TEST_REGO: &str = "policies/tests/roles_test.rego";
/// The workflow that runs validation and the gate on every merge request.
pub const WORKFLOW: &str = ".gitea/workflows/ci.yaml";

const REGO: &str = include_str!("../templates/org/policies/roles.rego");
const REGO_TESTS: &str = include_str!("../templates/org/policies/tests/roles_test.rego");
const WORKFLOW_YAML: &str = include_str!("../templates/org/.gitea/workflows/ci.yaml");

/// Paths only an organization-scoped approver may own: whoever could edit them could widen
/// their own rights below the red lane (CC-42, CC-70).
const ORGANIZATION_ONLY: &[&str] = &[
    "/CODEOWNERS",
    "/users/",
    "/platform/",
    "/policies/",
    "/.gitea/",
];

#[derive(Debug)]
/// Why `users/` could not be compiled.
pub enum RolesError {
    /// A Role or RoleBinding whose spec does not parse into its kind.
    Manifest {
        /// `Role` or `RoleBinding`.
        kind: String,
        /// Its `metadata.name`.
        name: String,
        /// What serde refused.
        source: serde_json::Error,
    },
    /// A binding names a role the repository has no manifest for.
    MissingRole {
        /// The binding's name.
        binding: String,
        /// The role it names.
        role: String,
    },
    /// The same role name in `users/roles/` and inside a project (PF-68).
    RoleNameClash {
        /// The name both carry.
        name: String,
        /// The project whose role clashes with the organization's.
        project: String,
    },
    /// A project's own role bound where it does not reach (PF-69).
    RoleOutOfReach {
        /// The binding's name.
        binding: String,
        /// The role it names.
        role: String,
        /// The project the role belongs to.
        role_project: String,
        /// Where the binding applies, in words.
        scope: String,
    },
    /// A line of the change list names no path inside the checkout: absolute, climbing out
    /// with `..`, or quoted the way git quotes and broken (PF-52).
    ChangePath {
        /// The path column as the list gave it.
        path: String,
    },
    /// A file could not be read or written.
    Io(std::io::Error),
}

impl fmt::Display for RolesError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Manifest { kind, name, source } => {
                write!(f, "{kind} `{name}` does not parse: {source}")
            }
            Self::MissingRole { binding, role } => {
                write!(
                    f,
                    "RoleBinding `{binding}` names Role `{role}`, which neither users/roles/ \
                     nor its own project holds"
                )
            }
            Self::RoleNameClash { name, project } => {
                write!(
                    f,
                    "Role `{name}` is in users/roles/ and in project `{project}`; a project \
                     role takes a name of its own (PF-68)"
                )
            }
            Self::RoleOutOfReach {
                binding,
                role,
                role_project,
                scope,
            } => {
                write!(
                    f,
                    "RoleBinding `{binding}` names Role `{role}` of project `{role_project}` \
                     at {scope}; a project role is bound inside its own project alone (PF-69)"
                )
            }
            Self::ChangePath { path } => write!(
                f,
                "the change list names {path:?}, which is not a path inside the repository; \
                 the gate reads only what `git diff --name-status` lists"
            ),
            Self::Io(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for RolesError {}

impl From<std::io::Error> for RolesError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

/// `policies/roles.json`: the bindings as the Rego gate reads them.
#[derive(Debug, Serialize)]
pub struct RolesData {
    /// The organization's roles, by name.
    pub roles: BTreeMap<String, RoleSpec>,
    /// Each project's own roles, by project and then by name (PF-68).
    #[serde(rename = "projectRoles")]
    pub project_roles: BTreeMap<String, BTreeMap<String, RoleSpec>>,
    /// Every `RoleBinding`, sorted by name.
    pub bindings: Vec<NamedBinding>,
}

#[derive(Debug, Serialize)]
/// A `RoleBinding` with its name beside its spec.
pub struct NamedBinding {
    /// `metadata.name`.
    pub name: String,
    /// The binding as written.
    #[serde(flatten)]
    pub spec: RoleBindingSpec,
}

/// What `render` derives from `users/`.
#[derive(Debug)]
pub struct Compiled {
    /// The text of `CODEOWNERS`.
    pub codeowners: String,
    /// The text of `policies/roles.json`.
    pub roles_json: String,
}

fn parse<T: serde::de::DeserializeOwned>(manifest: &RawManifest) -> Result<T, RolesError> {
    serde_json::from_value(manifest.spec.clone()).map_err(|source| RolesError::Manifest {
        kind: manifest.kind.clone(),
        name: manifest.metadata.name.clone(),
        source,
    })
}

/// The Gitea handle of a subject: `@login` for a user (the local part of an e-mail), and
/// `@{org}/{group}` for a group, which is how a team is written in a CODEOWNERS line.
fn handle(subject: &Subject, org: Option<&str>) -> Option<String> {
    if let Some(user) = &subject.user {
        return Some(format!("@{}", user.split('@').next().unwrap_or(user)));
    }
    let group = subject.group.as_deref()?;
    Some(match org {
        Some(org) => format!("@{org}/{group}"),
        None => format!("@{group}"),
    })
}

/// The role a binding names, looked up where the binding may reach it (PF-69).
///
/// The organization's roles first, then the roles of the project the binding's scope names —
/// its own project, or the project the bound context space lives in. A name cannot be in both,
/// because [`compile`] refuses that before it gets here (PF-68).
fn resolve<'a>(
    binding: &NamedBinding,
    roles: &'a BTreeMap<String, RoleSpec>,
    project_roles: &'a BTreeMap<String, BTreeMap<String, RoleSpec>>,
    project_of_space: &BTreeMap<String, String>,
) -> Result<&'a RoleSpec, RolesError> {
    if let Some(spec) = roles.get(&binding.spec.role) {
        return Ok(spec);
    }
    let scope = &binding.spec.scope;
    let reachable = scope.project.clone().or_else(|| {
        scope
            .context_space
            .as_ref()
            .and_then(|space| project_of_space.get(space).cloned())
    });
    if let Some(project) = &reachable {
        if let Some(spec) = project_roles
            .get(project)
            .and_then(|named| named.get(&binding.spec.role))
        {
            return Ok(spec);
        }
    }
    // The role exists, but not where this binding reaches: say where it is and where the
    // binding is, rather than "missing".
    if let Some((role_project, _)) = project_roles
        .iter()
        .find(|(_, named)| named.contains_key(&binding.spec.role))
    {
        return Err(RolesError::RoleOutOfReach {
            binding: binding.name.clone(),
            role: binding.spec.role.clone(),
            role_project: role_project.clone(),
            scope: match (&scope.organization, &scope.project, &scope.context_space) {
                (Some(org), _, _) => format!("organization scope `{org}`"),
                (_, Some(project), _) => format!("project scope `{project}`"),
                (_, _, Some(space)) => format!("context space scope `{space}`"),
                _ => "an unnamed scope".to_owned(),
            },
        });
    }
    Err(RolesError::MissingRole {
        binding: binding.name.clone(),
        role: binding.spec.role.clone(),
    })
}

/// Compiles the repository's `users/` into `CODEOWNERS` and `policies/roles.json`.
pub fn compile(repo: &Repository) -> Result<Compiled, RolesError> {
    let mut roles = BTreeMap::new();
    let mut project_roles: BTreeMap<String, BTreeMap<String, RoleSpec>> = BTreeMap::new();
    let mut bindings = Vec::new();
    let mut org: Option<String> = None;
    // A context-space binding names no project, so the space's own namespace says which
    // project's roles it may reach (PF-69).
    let mut project_of_space: BTreeMap<String, String> = BTreeMap::new();
    for (id, loaded) in repo.iter() {
        match id.kind.as_str() {
            "Role" => {
                let spec = parse::<RoleSpec>(&loaded.manifest)?;
                match id.namespace.as_deref() {
                    Some(project) if project != ORG_NAMESPACE => {
                        project_roles
                            .entry(project.to_owned())
                            .or_default()
                            .insert(id.name.clone(), spec);
                    }
                    _ => {
                        roles.insert(id.name.clone(), spec);
                    }
                }
            }
            "RoleBinding" => bindings.push(NamedBinding {
                name: id.name.clone(),
                spec: parse::<RoleBindingSpec>(&loaded.manifest)?,
            }),
            "Organization" => org = Some(id.name.clone()),
            "ContextSpace" => {
                if let Some(project) = id.namespace.clone() {
                    project_of_space.insert(id.name.clone(), project);
                }
            }
            _ => {}
        }
    }
    for (project, named) in &project_roles {
        for name in named.keys() {
            if roles.contains_key(name) {
                return Err(RolesError::RoleNameClash {
                    name: name.clone(),
                    project: project.clone(),
                });
            }
        }
    }
    bindings.sort_by(|a, b| a.name.cmp(&b.name));
    for binding in &bindings {
        resolve(binding, &roles, &project_roles, &project_of_space)?;
    }

    // CODEOWNERS: a binding owns the paths its scope covers when its role may approve
    // anything there; what it may approve exactly is the Rego gate's business.
    let approves = |binding: &NamedBinding| {
        resolve(binding, &roles, &project_roles, &project_of_space)
            .map(|spec| {
                spec.rules
                    .iter()
                    .any(|rule| rule.verbs.contains(&Verb::Approve))
            })
            .unwrap_or(false)
    };
    let owners = |binding: &NamedBinding| -> Vec<String> {
        binding
            .spec
            .subjects
            .iter()
            .filter_map(|s| handle(s, org.as_deref()))
            .collect()
    };
    let mut out = String::from(
        "# Written by `jcctl roles render` from users/ (PF-51, CC-41). Edit the Role and\n\
         # RoleBinding manifests, never this file: the next render overwrites it.\n",
    );
    let org_owners: Vec<String> = bindings
        .iter()
        .filter(|b| b.spec.scope.organization.is_some() && approves(b))
        .flat_map(owners)
        .collect();
    out.push_str("\n# Organization scope: reviews everything, and alone owns what sets the rules (CC-42, CC-70).\n");
    if org_owners.is_empty() {
        out.push_str("# (no organization-scoped binding may approve: these paths have no owner until one does)\n");
    }
    for path in ORGANIZATION_ONLY.iter().chain(std::iter::once(&"/")) {
        if !org_owners.is_empty() {
            out.push_str(&format!("{path} {}\n", org_owners.join(" ")));
        }
    }
    for binding in bindings.iter().filter(|b| approves(b)) {
        let names = owners(binding);
        if names.is_empty() {
            continue;
        }
        if let Some(project) = &binding.spec.scope.project {
            out.push_str(&format!(
                "\n# RoleBinding {} (project {project})\n/projects/{project}/ {}\n",
                binding.name,
                names.join(" ")
            ));
        } else if let Some(space) = &binding.spec.scope.context_space {
            out.push_str(&format!(
                "\n# RoleBinding {} (context space {space})\n/projects/*/spaces/{space}/ {}\n",
                binding.name,
                names.join(" ")
            ));
        }
    }

    let mut roles_json = serde_json::to_string_pretty(&RolesData {
        roles,
        project_roles,
        bindings,
    })
    .expect("role data serializes");
    roles_json.push('\n');
    Ok(Compiled {
        codeowners: out,
        roles_json,
    })
}

/// Every file the repository should hold for its roles: the two compiled ones and the three
/// the gate consists of, as `(path, content)`. `None` when the repository has no `users/`,
/// so a repository that has not adopted roles as code is left alone.
pub fn files(repo: &Repository) -> Result<Option<Vec<(&'static str, String)>>, RolesError> {
    if repo
        .iter()
        .all(|(_, r)| r.manifest.kind != "Role" && r.manifest.kind != "RoleBinding")
    {
        return Ok(None);
    }
    let compiled = compile(repo)?;
    Ok(Some(vec![
        (CODEOWNERS, compiled.codeowners),
        (ROLES_JSON, compiled.roles_json),
        (ROLES_REGO, REGO.to_string()),
        (ROLES_TEST_REGO, REGO_TESTS.to_string()),
        (WORKFLOW, WORKFLOW_YAML.to_string()),
    ]))
}

/// Writes the compiled files and the gate into the repository; returns what it wrote.
pub fn render(repo_dir: &Path) -> Result<Vec<PathBuf>, Box<dyn std::error::Error>> {
    let repo = Repository::load(repo_dir)?;
    let mut written = Vec::new();
    for (rel, content) in files(&repo)?.unwrap_or_default() {
        let path = repo_dir.join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&path, content)?;
        written.push(PathBuf::from(rel));
    }
    Ok(written)
}

/// One changed manifest as the gate sees it.
#[derive(Debug, Serialize, PartialEq)]
pub struct Change {
    /// Repository-relative path of the file.
    pub path: String,
    /// `propose` for an added or modified manifest, `delete` for a removed one.
    pub action: &'static str,
    /// `kind` of the manifest.
    pub kind: String,
    /// `metadata.name` of the manifest.
    pub name: String,
    /// The project it belongs to; `None` at organization level.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    /// The whole manifest, for the constraints.
    pub manifest: serde_json::Value,
}

/// The document `policies/roles.rego` evaluates.
#[derive(Debug, Serialize)]
pub struct GateInput {
    /// The forge login of the merge request's author.
    pub author: String,
    /// Their e-mail, when the forge knows it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub author_email: Option<String>,
    /// The groups they belong to; empty when the forge cannot tell.
    pub groups: Vec<String>,
    /// One entry per changed manifest.
    pub changes: Vec<Change>,
}

/// The project a repository path belongs to (`projects/{p}/...`).
fn project_of(path: &str) -> Option<String> {
    let mut parts = path.trim_start_matches('/').split('/');
    match (parts.next(), parts.next()) {
        (Some("projects"), Some(project)) if !project.is_empty() => Some(project.to_owned()),
        _ => None,
    }
}

/// A path column of `git diff --name-status`, unquoted the way git quotes a name with a byte
/// outside printable ASCII (`core.quotePath`: `"…"` with C escapes and three-digit octal
/// bytes), and held inside the checkout: empty, absolute, `.` and `..` are refused (PF-52).
fn change_path(column: &str) -> Result<String, RolesError> {
    let refused = || RolesError::ChangePath {
        path: column.to_owned(),
    };
    let path = match column.strip_prefix('"') {
        None => column.to_owned(),
        Some(quoted) => {
            let raw = quoted.strip_suffix('"').ok_or_else(refused)?.as_bytes();
            let mut bytes = Vec::with_capacity(raw.len());
            let mut at = 0;
            while let Some(&byte) = raw.get(at) {
                if byte == b'"' {
                    return Err(refused());
                }
                if byte != b'\\' {
                    bytes.push(byte);
                    at += 1;
                    continue;
                }
                let (unescaped, width) = match raw.get(at + 1).copied().ok_or_else(refused)? {
                    b'a' => (0x07, 2),
                    b'b' => (0x08, 2),
                    b't' => (b'\t', 2),
                    b'n' => (b'\n', 2),
                    b'v' => (0x0B, 2),
                    b'f' => (0x0C, 2),
                    b'r' => (b'\r', 2),
                    b'"' => (b'"', 2),
                    b'\\' => (b'\\', 2),
                    b'0'..=b'3' => {
                        let digits = raw.get(at + 1..at + 4).ok_or_else(refused)?;
                        if !digits.iter().all(|d| (b'0'..=b'7').contains(d)) {
                            return Err(refused());
                        }
                        // The first digit is at most 3, so the byte cannot overflow.
                        (digits.iter().fold(0_u8, |n, d| n * 8 + (d - b'0')), 4)
                    }
                    _ => return Err(refused()),
                };
                bytes.push(unescaped);
                at += width;
            }
            String::from_utf8(bytes).map_err(|_| refused())?
        }
    };
    let plain = Path::new(&path)
        .components()
        .all(|part| matches!(part, std::path::Component::Normal(_)));
    if path.is_empty() || !plain {
        return Err(refused());
    }
    Ok(path)
}

/// Every manifest document of one changed file, read from `dir`; a file that is not YAML, is
/// a LinkML source, is not on that side, or does not parse as a manifest adds nothing (the
/// loader refuses the last in `jcctl validate`, which runs before the gate).
fn changes_of(
    dir: &Path,
    path: &str,
    action: &'static str,
    changes: &mut Vec<Change>,
) -> Result<(), RolesError> {
    if !(path.ends_with(".yaml") || path.ends_with(".yml")) || path.ends_with(".linkml.yaml") {
        return Ok(());
    }
    let text = match std::fs::read_to_string(dir.join(path)) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e.into()),
    };
    for document in crate::loader::parse_yaml_documents(&text) {
        let Ok(manifest) = serde_norway::from_str::<RawManifest>(&document.content) else {
            continue;
        };
        changes.push(Change {
            path: path.to_owned(),
            action,
            kind: manifest.kind.clone(),
            name: manifest.metadata.name.clone(),
            project: manifest
                .metadata
                .namespace
                .clone()
                .filter(|ns| ns != "org")
                .or_else(|| project_of(path)),
            manifest: serde_json::to_value(&manifest).expect("manifest serializes"),
        });
    }
    Ok(())
}

/// Builds the gate input from a `git diff --name-status` listing: added and modified
/// manifests are read from `repo_dir`, deleted ones from `base_dir` (the base revision,
/// e.g. `git archive origin/main | tar -x -C base`). A rename is both: the new manifest is
/// proposed and the old one deleted, so moving a resource needs the right to remove it where
/// it was. Files that are not manifests are skipped; a path that leaves the checkout is an
/// error (PF-52).
pub fn input(
    repo_dir: &Path,
    base_dir: &Path,
    name_status: &str,
    author: &str,
    author_email: Option<&str>,
    groups: &[String],
) -> Result<GateInput, RolesError> {
    let mut changes = Vec::new();
    for line in name_status.lines() {
        let mut cols = line.split('\t');
        let (Some(status), Some(first)) = (cols.next(), cols.next()) else {
            continue;
        };
        let first = change_path(first)?;
        match cols.next() {
            // A rename or a copy lists the old path, then the new one.
            Some(second) => {
                changes_of(repo_dir, &change_path(second)?, "propose", &mut changes)?;
                if status.starts_with('R') {
                    changes_of(base_dir, &first, "delete", &mut changes)?;
                }
            }
            None if status.starts_with('D') => {
                changes_of(base_dir, &first, "delete", &mut changes)?;
            }
            None => changes_of(repo_dir, &first, "propose", &mut changes)?,
        }
    }
    Ok(GateInput {
        author: author.to_owned(),
        author_email: author_email.map(str::to_owned),
        groups: groups.to_vec(),
        changes,
    })
}
