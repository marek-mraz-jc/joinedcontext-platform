//! `jcctl validate --repo-dir <path>` (T-0125, CC-12, MF-09, TS-18).
//!
//! Every manifest is parsed into its typed kind and validated by `jc-core`, which is
//! strictly stronger than checking it against the exported JSON Schema: the typed parse
//! refuses unknown fields, so an inline secret beside a `secretRef` is a parse error, and
//! it then runs the cross-field invariants a schema cannot express. Finally each manifest
//! must sit at the path its kind prescribes (MF-06).

use crate::loader::{LoadError, Repository};
use jc_core::kinds::{DataModelSpec, ModelProjectionSpec};
use jc_core::registry;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::{Path, PathBuf};

/// One rejected manifest, located well enough to fix it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    /// Repository-relative file the manifest came from.
    pub path: PathBuf,
    /// 1-based YAML document inside that file.
    pub document: usize,
    /// 1-based line the document starts at.
    pub line: usize,
    /// What is wrong, naming the failing field where the parser knows it.
    pub message: String,
}

impl fmt::Display for Finding {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}:{} (document {}): {}",
            self.path.display(),
            self.line,
            self.document,
            self.message
        )
    }
}

/// What `validate` found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    /// Manifests that parsed and validated.
    pub checked: usize,
    /// Manifests that did not, in repository order.
    pub findings: Vec<Finding>,
    /// What is not wrong yet: a manifest that wrote the organization's domain out where
    /// `{orgDomain}` belongs, so the same file cannot render two environments (CC-74). A
    /// warning while a repository is being migrated, an error once it is.
    pub warnings: Vec<Finding>,
}

impl Report {
    /// Whether every manifest in the repository is valid.
    pub fn is_valid(&self) -> bool {
        self.findings.is_empty()
    }
}

/// Validates every manifest under `repo_dir` (CC-12, MF-09).
///
/// A repository that cannot be walked at all (a malformed document, an unknown kind, a
/// symlink out of the tree) yields the one finding that stopped the walk: the loader
/// refuses to build a half-repository it would then validate against itself.
pub fn run(repo_dir: &Path) -> Report {
    let repo = match Repository::load(repo_dir) {
        Ok(repo) => repo,
        Err(err) => {
            return Report {
                checked: 0,
                findings: vec![finding_of(err)],
                warnings: Vec::new(),
            }
        }
    };

    let mut report = Report {
        checked: 0,
        findings: Vec::new(),
        warnings: Vec::new(),
    };

    for (id, path, text) in repo.literal_domains() {
        let where_from = repo.get(id);
        report.warnings.push(Finding {
            path: path.clone(),
            document: where_from.map(|r| r.document).unwrap_or(1),
            line: where_from.map(|r| r.line).unwrap_or(1),
            message: format!(
                "{} writes the organization's domain out: `{text}`. Write {} instead, so the \
                 same manifest renders every environment (CC-74).",
                id.kind,
                crate::loader::ORG_DOMAIN_PLACEHOLDER
            ),
        });
    }

    for (id, path, why) in repo.unresolved_references() {
        let where_from = repo.get(id);
        report.findings.push(Finding {
            path: path.clone(),
            document: where_from.map(|r| r.document).unwrap_or(1),
            line: where_from.map(|r| r.line).unwrap_or(1),
            message: format!("SharedSpaceReference {}: {why}", id.name),
        });
    }

    for (id, path, why) in repo.contested_agreement_ids() {
        let where_from = repo.get(id);
        report.findings.push(Finding {
            path: path.clone(),
            document: where_from.map(|r| r.document).unwrap_or(1),
            line: where_from.map(|r| r.line).unwrap_or(1),
            message: format!("DataAgreement {}: {why}", id.name),
        });
    }

    for (id, path, line, literal) in repo.literal_spaces() {
        report.warnings.push(Finding {
            path,
            document: 1,
            line,
            message: format!(
                "Pipeline {} types the space as \"{literal}\". Build the id from env(\"{}\") \
                 instead, so the pipeline mints ids of the space it runs in (CC-83, PL-57).",
                id.name,
                crate::bento::SPACE_VAR
            ),
        });
    }

    for (id, resource) in repo.iter() {
        let yaml = match serde_norway::to_string(&resource.manifest) {
            Ok(yaml) => yaml,
            Err(err) => {
                report.findings.push(Finding {
                    path: resource.path.clone(),
                    document: resource.document,
                    line: resource.line,
                    message: err.to_string(),
                });
                continue;
            }
        };

        match registry::validate_yaml(&id.kind, &yaml) {
            Some(Ok(())) => report.checked += 1,
            Some(Err(err)) => report.findings.push(Finding {
                path: resource.path.clone(),
                document: resource.document,
                line: resource.line,
                message: err.to_string(),
            }),
            None => report.findings.push(Finding {
                path: resource.path.clone(),
                document: resource.document,
                line: resource.line,
                message: format!("kind `{}` is not in the kind registry", id.kind),
            }),
        }
    }

    for (id, resource, reference) in dangling_data_sources(&repo) {
        report.findings.push(Finding {
            path: resource.0,
            document: resource.1,
            line: resource.2,
            message: format!(
                "{id} references DataSource `{reference}`, which no manifest of this project declares (PL-39)"
            ),
        });
    }

    for (location, message) in stale_projections(repo_dir, &repo) {
        report.findings.push(Finding {
            path: location.0,
            document: location.1,
            line: location.2,
            message,
        });
    }

    for (location, message) in bindings_to_undeclared_groups(&repo) {
        report.findings.push(Finding {
            path: location.0,
            document: location.1,
            line: location.2,
            message,
        });
    }

    for (location, message) in roles_that_do_not_resolve(&repo) {
        report.findings.push(Finding {
            path: location.0,
            document: location.1,
            line: location.2,
            message,
        });
    }

    for (directory, message) in projects_without_a_manifest(repo_dir, &repo) {
        report.findings.push(Finding {
            path: directory,
            document: 1,
            line: 1,
            message,
        });
    }

    for (id, actual, expected) in repo.misplaced() {
        let resource = repo.get(&id).expect("misplaced reports loaded resources");
        report.findings.push(Finding {
            path: actual,
            document: resource.document,
            line: resource.line,
            message: format!("{id} belongs at `{expected}` (MF-06)"),
        });
    }

    report
}

/// Where a finding sits: the file, the document inside it and its first line.
type Location = (PathBuf, usize, usize);

/// What the roles compiler refuses: a role name in two places, a binding that names a role it
/// cannot reach, or one that names no role at all (PF-68, PF-69, PF-49).
///
/// The compiler already knows these rules, because it writes `CODEOWNERS` and
/// `policies/roles.json` from them. Running it here is what turns "the render fails" into a
/// finding `jcctl validate` reports with the file and line, before anyone pushes.
fn roles_that_do_not_resolve(repo: &Repository) -> Vec<(Location, String)> {
    let touches_roles = repo
        .iter()
        .any(|(id, _)| id.kind == "Role" || id.kind == "RoleBinding");
    if !touches_roles {
        return Vec::new();
    }
    let Err(err) = crate::roles::compile(repo) else {
        return Vec::new();
    };
    let named = match &err {
        crate::roles::RolesError::RoleNameClash { name, project } => {
            Some(("Role", name.clone(), Some(project.clone())))
        }
        crate::roles::RolesError::RoleOutOfReach { binding, .. }
        | crate::roles::RolesError::MissingRole { binding, .. } => {
            Some(("RoleBinding", binding.clone(), None))
        }
        _ => None,
    };
    let at = named.and_then(|(kind, name, namespace)| {
        repo.iter().find_map(|(id, loaded)| {
            let matches = id.kind == kind
                && id.name == name
                && namespace
                    .as_deref()
                    .is_none_or(|ns| id.namespace.as_deref() == Some(ns));
            matches.then(|| (loaded.path.clone(), loaded.document, loaded.line))
        })
    });
    let location = at.unwrap_or_else(|| (PathBuf::from("users"), 1, 1));
    vec![(location, err.to_string())]
}

/// Every `subjects[].group` of a `RoleBinding` that no `Group` manifest declares (PF-64).
///
/// A binding to a group nobody declared matches nobody, silently: the people it was written for
/// read nothing and no error says why. The group's membership is configuration (PF-62), so the
/// manifest is here to be found. A `ServiceAccount` names no group — its `spec.roles[]` carries a
/// role and a scope and nothing else — so there is nothing of its to check here.
fn bindings_to_undeclared_groups(repo: &Repository) -> Vec<(Location, String)> {
    let declared: BTreeSet<&str> = repo
        .iter()
        .filter(|(id, _)| id.kind == "Group")
        .map(|(id, _)| id.name.as_str())
        .collect();

    let mut findings = Vec::new();
    for (id, resource) in repo.iter() {
        if id.kind != "RoleBinding" {
            continue;
        }
        let named = resource
            .manifest
            .spec
            .get("subjects")
            .and_then(|subjects| subjects.as_array())
            .map(|subjects| {
                subjects
                    .iter()
                    .filter_map(|subject| subject.get("group").and_then(|g| g.as_str()))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        for group in named {
            if declared.contains(group) {
                continue;
            }
            findings.push((
                (resource.path.clone(), resource.document, resource.line),
                format!(
                    "{id} names group `{group}`, which no Group manifest of this organization \
                     declares: add `users/groups/{group}.yaml` (PF-62, PF-64)"
                ),
            ));
        }
    }
    findings
}

/// Every directory under `projects/` that declares no `Project` (MF-01, PF-05).
///
/// A project directory without its manifest still serves spaces and endpoints, so nothing shows
/// it is missing until a quota, an owner or a project role has nowhere to hang. Two of the three
/// projects of the demo repository were in that state (T-0902).
fn projects_without_a_manifest(repo_dir: &Path, repo: &Repository) -> Vec<(PathBuf, String)> {
    let declared: BTreeSet<String> = repo
        .iter()
        .filter(|(id, _)| id.kind == "Project")
        .map(|(id, _)| id.name.clone())
        .collect();

    let Ok(entries) = std::fs::read_dir(repo_dir.join("projects")) else {
        return Vec::new();
    };
    let mut missing: Vec<(PathBuf, String)> = entries
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
        .filter_map(|entry| entry.file_name().to_str().map(str::to_owned))
        .filter(|name| !declared.contains(name))
        .map(|name| {
            let path = PathBuf::from("projects").join(&name);
            (
                path.clone(),
                format!(
                    "the project directory `{name}` declares no Project: add `{}` (MF-01, PF-05)",
                    path.join("project.yaml").display()
                ),
            )
        })
        .collect();
    missing.sort();
    missing
}

/// Every `spec.source.dataSourceRef` that names no `DataSource` of the same project (PL-39).
///
/// The reference is resolved here, at plan time, and not by the runner: a pipeline whose
/// connection is missing would otherwise start, fail to build an input and restart forever,
/// with the reason three layers away from the person who wrote the reference.
fn dangling_data_sources(repo: &Repository) -> Vec<(String, Location, String)> {
    let declared: std::collections::BTreeSet<(Option<String>, String)> = repo
        .iter()
        .filter(|(id, _)| id.kind == "DataSource")
        .map(|(id, _)| (id.namespace.clone(), id.name.clone()))
        .collect();

    let mut dangling = Vec::new();
    for (id, resource) in repo.iter() {
        if id.kind != "Pipeline" {
            continue;
        }
        let Some(reference) = resource
            .manifest
            .spec
            .get("source")
            .and_then(|source| source.get("dataSourceRef"))
        else {
            continue;
        };
        // A reference is a bare name or a typed `{kind, name, namespace?}`; the namespace of a
        // typed one is the pipeline's own, because a connection is owned by the team that owns
        // its credentials.
        let name = match reference {
            serde_json::Value::String(name) => Some(name.clone()),
            serde_json::Value::Object(map) => {
                map.get("name").and_then(|n| n.as_str()).map(str::to_owned)
            }
            _ => None,
        };
        let Some(name) = name else { continue };
        if !declared.contains(&(id.namespace.clone(), name.clone())) {
            dangling.push((
                id.to_string(),
                (resource.path.clone(), resource.document, resource.line),
                name,
            ));
        }
    }
    dangling
}

/// Every `ModelProjection` that names a class or a slot the referenced DataModel version does
/// not have, every offending name at once (MP-01). The names come from the model's LinkML
/// source beside its manifest, so a typo fails here and not as an endpoint that serves nothing.
fn stale_projections(repo_dir: &Path, repo: &Repository) -> Vec<(Location, String)> {
    let mut stale = Vec::new();
    for (id, resource) in repo.iter() {
        if id.kind != "ModelProjection" {
            continue;
        }
        // A projection the typed parse refused is already a finding above.
        let Ok(projection) =
            serde_json::from_value::<ModelProjectionSpec>(resource.manifest.spec.clone())
        else {
            continue;
        };
        let wanted = &projection.data_model_ref;
        let model = repo
            .iter()
            .find(|(model, _)| {
                model.kind == "DataModel"
                    && model.namespace == id.namespace
                    && model.name == wanted.name
            })
            .and_then(|(_, model)| {
                serde_json::from_value::<DataModelSpec>(model.manifest.spec.clone())
                    .ok()
                    .map(|spec| (spec, model.path.clone()))
            });
        let message = match model {
            None => format!(
                "{id} references DataModel `{}`, which no manifest of this project declares (MP-01)",
                wanted.name
            ),
            Some((spec, _)) if spec.version.major().to_string() != wanted.version => format!(
                "{id} references version {} of DataModel `{}`, which is at {} (MP-01)",
                wanted.version, wanted.name, spec.version
            ),
            Some((spec, model_path)) => {
                let linkml = repo_dir
                    .join(&model_path)
                    .parent()
                    .map(|dir| dir.join(&spec.linkml))
                    .unwrap_or_else(|| repo_dir.join(&spec.linkml));
                match linkml_classes(&linkml) {
                    Err(err) => format!(
                        "{id}: the LinkML source of DataModel `{}` cannot be read at `{}`: {err}",
                        wanted.name,
                        linkml.display()
                    ),
                    Ok(classes) => match projection.check_against(&classes) {
                        Ok(()) => continue,
                        Err(err) => format!("{id}: {err}"),
                    },
                }
            }
        };
        stale.push((
            (resource.path.clone(), resource.document, resource.line),
            message,
        ));
    }
    stale
}

/// The classes of a LinkML schema with the slots each one carries: its `slots` list and its
/// `attributes` keys, plus the `id` and `type` every entity has.
// ponytail: no `is_a` inheritance; slots inherited from a parent class need listing again on
// the child until a projection of an inherited slot is wanted.
fn linkml_classes(path: &Path) -> Result<BTreeMap<String, BTreeSet<String>>, String> {
    let text = std::fs::read_to_string(path).map_err(|err| err.to_string())?;
    let schema: serde_norway::Value =
        serde_norway::from_str(&text).map_err(|err| err.to_string())?;
    let mut classes = BTreeMap::new();
    let Some(declared) = schema.get("classes").and_then(|c| c.as_mapping()) else {
        return Ok(classes);
    };
    for (name, class) in declared {
        let Some(name) = name.as_str() else { continue };
        let mut slots: BTreeSet<String> = ["id", "type"].map(str::to_owned).into();
        if let Some(listed) = class.get("slots").and_then(|s| s.as_sequence()) {
            slots.extend(listed.iter().filter_map(|s| s.as_str()).map(str::to_owned));
        }
        if let Some(attributes) = class.get("attributes").and_then(|a| a.as_mapping()) {
            slots.extend(
                attributes
                    .keys()
                    .filter_map(|k| k.as_str())
                    .map(str::to_owned),
            );
        }
        classes.insert(name.to_owned(), slots);
    }
    Ok(classes)
}

/// Turns the error that stopped the walk into a finding, keeping whatever location it
/// carries.
fn finding_of(err: LoadError) -> Finding {
    let (path, document, line) = match &err {
        LoadError::Parse {
            path,
            document,
            line,
            ..
        } => (path.clone(), *document, *line),
        LoadError::ApiVersion { path, document, .. }
        | LoadError::UnknownKind { path, document, .. } => (path.clone(), *document, 1),
        LoadError::DuplicateIdentity { second, .. } => (second.clone(), 1, 1),
        LoadError::PathEscapesRepository { path }
        | LoadError::Io { path, .. }
        | LoadError::Overlay { path, .. } => (path.clone(), 1, 1),
        // The overlay is missing, so there is no file to point at.
        LoadError::NoSuchEnvironment { .. } => (PathBuf::from("environments"), 1, 1),
        // A preview render is refused as a whole; the message names the resource.
        LoadError::UnprefixedName { .. } => (PathBuf::new(), 1, 1),
    };
    Finding {
        path,
        document,
        line,
        message: err.to_string(),
    }
}
