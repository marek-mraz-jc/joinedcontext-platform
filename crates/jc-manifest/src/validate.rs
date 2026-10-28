//! `jcctl validate --repo-dir <path>` (T-0125, CC-12, MF-09, TS-18).
//!
//! Every manifest is parsed into its typed kind and validated by `jc-core`, which is
//! strictly stronger than checking it against the exported JSON Schema: the typed parse
//! refuses unknown fields, so an inline secret beside a `secretRef` is a parse error, and
//! it then runs the cross-field invariants a schema cannot express. Finally each manifest
//! must sit at the path its kind prescribes (MF-06).

use crate::assemble::{assemble, AssembleError, Assembly, Directories, Resolver};
use crate::loader::{LoadError, Repository, ResourceId};
use jc_core::kinds::{
    ContextSpaceSpec, DataModelLifecycle, DataModelSpec, EndpointSpec, ModelProjectionSpec,
};
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
    match Repository::load(repo_dir) {
        Ok(repo) => check(repo_dir, repo, true),
        Err(err) => refused(err.into()),
    }
}

/// The environment [`Repository::load`] renders with (CC-73), so a render checked from memory
/// is the one it would read back.
fn environment() -> Option<String> {
    std::env::var("JC_ENVIRONMENT")
        .ok()
        .filter(|value| !value.trim().is_empty())
}

/// Validates an organization of either layout (CC-86): layout 1 where it is, layout 2 as the
/// render the organization repository and the project checkouts `resolver` names assemble to.
/// A project that cannot be checked out, a literal where a parameter exists and a mapping
/// reading an undeclared one are findings like any other (CC-83, CC-88).
pub fn run_assembled(org_dir: &Path, resolver: &dyn Resolver) -> Report {
    let into = scratch("validate");
    let environment = environment();
    let report = match assemble(
        org_dir,
        resolver,
        &into.join("render"),
        environment.as_deref(),
    ) {
        Ok(assembly) if assembly.layout == 1 => run(org_dir),
        Ok(assembly) => {
            let mut found = assembly_findings(&assembly, |path| path.to_path_buf());
            let mut report = check(&into.join("render"), assembly.repository, true);
            found.append(&mut report.findings);
            report.findings = found;
            report
        }
        Err(err) => refused(err),
    };
    let _ = std::fs::remove_dir_all(&into);
    report
}

/// Validates one project repository alone, without its organization (CC-90): the checks
/// every manifest carries by itself, with the parameters at their defaults over `values`, and
/// none of the checks that read the organization's groups, bindings or domain. `slug` is the
/// project's name in the render, by default its `project.yaml` name. Findings name paths of
/// the project repository.
pub fn run_project(
    project_dir: &Path,
    slug: Option<&str>,
    org_domain: &str,
    values: &BTreeMap<String, serde_json::Value>,
) -> Report {
    let into = scratch("validate-project");
    let report = match stage_project(project_dir, slug, org_domain, values, &into) {
        Ok(slug) => {
            let prefix = PathBuf::from("projects").join(&slug);
            let resolver = Directories(BTreeMap::from([(slug.clone(), project_dir.to_path_buf())]));
            let environment = environment();
            let render = into.join("render");
            match assemble(
                &into.join("org"),
                &resolver,
                &render,
                environment.as_deref(),
            ) {
                Ok(assembly) => {
                    let local =
                        |path: &Path| path.strip_prefix(&prefix).unwrap_or(path).to_path_buf();
                    let found = assembly_findings(&assembly, |path| path.to_path_buf());
                    let mut report = check(&render, assembly.repository, false);
                    for finding in report.findings.iter_mut().chain(report.warnings.iter_mut()) {
                        finding.path = local(&finding.path);
                    }
                    let mut found = found;
                    found.append(&mut report.findings);
                    report.findings = found;
                    report
                }
                Err(err) => refused(err),
            }
        }
        Err(message) => Report {
            checked: 0,
            findings: vec![Finding {
                path: PathBuf::from(jc_core::project::PROJECT_FILE),
                document: 1,
                line: 1,
                message,
            }],
            warnings: Vec::new(),
        },
    };
    let _ = std::fs::remove_dir_all(&into);
    report
}

/// The organization `run_project` checks a project in: the Organization its `project.yaml`
/// names and one registry entry pointing at the project, with `values`. Returns the slug.
fn stage_project(
    project_dir: &Path,
    slug: Option<&str>,
    org_domain: &str,
    values: &BTreeMap<String, serde_json::Value>,
    into: &Path,
) -> Result<String, String> {
    let file = project_dir.join(jc_core::project::PROJECT_FILE);
    let text =
        std::fs::read_to_string(&file).map_err(|err| format!("{}: {err}", file.display()))?;
    let own = jc_core::kinds::Project::from_yaml(&text).map_err(|err| err.to_string())?;
    let slug = match slug {
        Some(slug) => slug.to_owned(),
        None if own.metadata.name != jc_core::project::PROJECT_PLACEHOLDER => {
            own.metadata.name.clone()
        }
        None => {
            return Err(format!(
                "names itself `{}`; give the slug to check it under with --slug",
                jc_core::project::PROJECT_PLACEHOLDER
            ))
        }
    };
    let organization = own.spec.organization_ref.name().to_owned();
    let org = serde_json::json!({
        "apiVersion": jc_core::API_VERSION,
        "kind": "Organization",
        "metadata": { "name": organization, "namespace": "org" },
        "spec": { "domain": org_domain, "locales": ["en"], "defaultLocale": "en" },
    });
    let entry = serde_json::json!({
        "apiVersion": jc_core::API_VERSION,
        "kind": "Project",
        "metadata": { "name": slug, "namespace": "org" },
        "spec": {
            "organizationRef": organization,
            "repository": { "name": slug },
            "ref": "HEAD",
            "parameters": values,
        },
    });
    let write = |rel: &str, body: String| {
        let path = into.join("org").join(rel);
        std::fs::create_dir_all(path.parent().expect("a relative path has a parent"))
            .and_then(|()| std::fs::write(&path, body))
            .map_err(|err| format!("{}: {err}", path.display()))
    };
    let yaml =
        |value: &serde_json::Value| serde_norway::to_string(value).map_err(|e| e.to_string());
    write("org.yaml", yaml(&org)?)?;
    write(jc_core::project::LAYOUT_FILE, "2\n".to_owned())?;
    write(&format!("projects/{slug}.yaml"), yaml(&entry)?)?;
    Ok(slug)
}

fn assembly_findings(assembly: &Assembly, path: impl Fn(&Path) -> PathBuf) -> Vec<Finding> {
    let unfetched = assembly.entries.iter().filter_map(|entry| {
        let error = entry.error.as_ref()?;
        Some(Finding {
            path: PathBuf::from(format!("projects/{}.yaml", entry.slug)),
            document: 1,
            line: 1,
            message: format!(
                "the ref `{}` could not be checked out: {error}",
                entry.git_ref
            ),
        })
    });
    let found = assembly.findings.iter().map(|finding| Finding {
        path: path(&finding.path),
        document: 1,
        line: 1,
        message: format!("project `{}`: {}", finding.slug, finding.message),
    });
    unfetched.chain(found).collect()
}

fn refused(err: AssembleError) -> Report {
    let finding = match err {
        AssembleError::Load(err) => finding_of(err),
        other => Finding {
            path: PathBuf::new(),
            document: 1,
            line: 1,
            message: other.to_string(),
        },
    };
    Report {
        checked: 0,
        findings: vec![finding],
        warnings: Vec::new(),
    }
}

/// A directory of its own for one run, under the system's temporary directory.
fn scratch(what: &str) -> PathBuf {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or_default();
    std::env::temp_dir().join(format!("jcctl-{what}-{}-{now}", std::process::id()))
}

/// `repo` is the repository loaded from `repo_dir`. `organization` is false for a project
/// checked alone: its groups, bindings and domain are the organization's, which the check does
/// not have.
fn check(repo_dir: &Path, repo: Repository, organization: bool) -> Report {
    let mut report = Report {
        checked: 0,
        findings: Vec::new(),
        warnings: Vec::new(),
    };

    for (id, path, text) in repo.literal_domains().iter().filter(|_| organization) {
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

    for (location, message) in replacing_shared_writes(&repo) {
        report.findings.push(Finding {
            path: location.0,
            document: location.1,
            line: location.2,
            message,
        });
    }

    for (location, message) in writes_nobody_can_make(&repo) {
        report.findings.push(Finding {
            path: location.0,
            document: location.1,
            line: location.2,
            message,
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

    for (location, message) in surfaces_without_their_model(repo_dir, &repo) {
        report.findings.push(Finding {
            path: location.0,
            document: location.1,
            line: location.2,
            message,
        });
    }

    for (location, message) in displays_off_the_projection(repo_dir, &repo) {
        report.findings.push(Finding {
            path: location.0,
            document: location.1,
            line: location.2,
            message,
        });
    }

    for (location, message) in assistant_references(&repo) {
        report.findings.push(Finding {
            path: location.0,
            document: location.1,
            line: location.2,
            message,
        });
    }

    let (refused, warned) = one_model_per_space(repo_dir, &repo);
    for (location, message) in refused {
        report.findings.push(Finding {
            path: location.0,
            document: location.1,
            line: location.2,
            message,
        });
    }
    for (location, message) in warned {
        report.warnings.push(Finding {
            path: location.0,
            document: location.1,
            line: location.2,
            message,
        });
    }

    let (mqtt_found, mqtt_warned) = mqtt_brokers(&repo);
    for (location, message) in mqtt_found {
        report.findings.push(Finding {
            path: location.0,
            document: location.1,
            line: location.2,
            message,
        });
    }
    for (location, message) in mqtt_warned {
        report.warnings.push(Finding {
            path: location.0,
            document: location.1,
            line: location.2,
            message,
        });
    }

    for (location, message) in federation_topology(&repo) {
        report.findings.push(Finding {
            path: location.0,
            document: location.1,
            line: location.2,
            message,
        });
    }

    for (location, message) in bindings_to_undeclared_groups(&repo)
        .into_iter()
        .filter(|_| organization)
    {
        report.findings.push(Finding {
            path: location.0,
            document: location.1,
            line: location.2,
            message,
        });
    }

    for (location, message) in roles_that_do_not_resolve(&repo)
        .into_iter()
        .chain(brokers_that_do_not_resolve(&repo))
        .filter(|_| organization)
    {
        report.findings.push(Finding {
            path: location.0,
            document: location.1,
            line: location.2,
            message,
        });
    }

    for (location, message) in app_writes_open_to_everyone(&repo) {
        report.warnings.push(Finding {
            path: location.0,
            document: location.1,
            line: location.2,
            message,
        });
    }

    for (location, message) in app_names_claimed_twice(&repo)
        .into_iter()
        .chain(space_segments_claimed_twice(&repo))
        .chain(public_ids_claimed_twice(&repo))
    {
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

    if organization {
        for (location, message) in organizations_of_the_installation(&repo) {
            report.findings.push(finding_at(location, message));
        }
        for (location, message) in projects_of_unknown_organizations(&repo) {
            report.findings.push(finding_at(location, message));
        }
        for (id, loaded) in repo.iter() {
            if crate::loader::is_legacy_organization(id, loaded) {
                report.warnings.push(Finding {
                    path: loaded.path.clone(),
                    document: loaded.document,
                    line: loaded.line,
                    message: format!(
                        "`{}` is the legacy place of an Organization; move it to \
                         `organizations/{}.yaml` (PF-121, ADR-N-058).",
                        jc_core::kinds::LEGACY_ORGANIZATION_PATH,
                        id.name
                    ),
                });
            }
        }
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

fn finding_at((path, document, line): Location, message: String) -> Finding {
    Finding {
        path,
        document,
        line,
        message,
    }
}

fn location_of(loaded: &crate::loader::LoadedResource) -> Location {
    (loaded.path.clone(), loaded.document, loaded.line)
}

/// With several Organizations exactly one is the installation's, by `spec.installation: true` or
/// the legacy `org.yaml` (PF-121, ADR-N-058 section 2.2).
fn organizations_of_the_installation(repo: &Repository) -> Located {
    let organizations: Vec<_> = repo
        .iter()
        .filter(|(id, _)| id.kind == "Organization")
        .collect();
    if organizations.len() < 2 {
        return Vec::new();
    }
    let claims: Vec<_> = organizations
        .iter()
        .filter(|(id, loaded)| crate::loader::claims_installation(id, loaded))
        .collect();
    match claims[..] {
        [_] => Vec::new(),
        [] => organizations
            .iter()
            .map(|(_, loaded)| {
                (
                    location_of(loaded),
                    "the repository holds several Organizations and none is the installation's: \
                     set `spec.installation: true` on exactly one (PF-121)"
                        .to_owned(),
                )
            })
            .collect(),
        _ => {
            let files = claims
                .iter()
                .map(|(_, loaded)| format!("`{}`", loaded.path.display()))
                .collect::<Vec<_>>()
                .join(", ");
            claims
                .iter()
                .map(|(_, loaded)| {
                    (
                        location_of(loaded),
                        format!(
                            "`spec.installation`: exactly one Organization is the installation's, \
                             and {files} all claim it (PF-121)"
                        ),
                    )
                })
                .collect()
        }
    }
}

/// A Project names an Organization of the repository, and a project of an Organization other
/// than the installation's publishes under that Organization's domain (PF-122, ADR-N-058
/// section 2.3).
fn projects_of_unknown_organizations(repo: &Repository) -> Located {
    let organizations: BTreeMap<
        &str,
        (&crate::loader::ResourceId, jc_core::kinds::OrganizationSpec),
    > = repo
        .iter()
        .filter(|(id, _)| id.kind == "Organization")
        .filter_map(|(id, loaded)| {
            serde_json::from_value(loaded.manifest.spec.clone())
                .ok()
                .map(|spec| (id.name.as_str(), (id, spec)))
        })
        .collect();
    if organizations.is_empty() {
        return Vec::new();
    }
    let installation = repo.installation_organization().map(|(id, _)| id);
    let mut found = Vec::new();
    for (id, loaded) in repo.iter().filter(|(id, _)| id.kind == "Project") {
        let Ok(project) =
            serde_json::from_value::<jc_core::kinds::ProjectSpec>(loaded.manifest.spec.clone())
        else {
            continue;
        };
        let named = project.organization_ref.name();
        let Some((org_id, org)) = organizations.get(named) else {
            let known = organizations.keys().copied().collect::<Vec<_>>().join(", ");
            found.push((
                location_of(loaded),
                format!(
                    "Project `{}`: `spec.organizationRef` names `{named}`, which is no \
                     Organization of this repository (it holds {known}) (PF-122)",
                    id.name
                ),
            ));
            continue;
        };
        if installation == Some(*org_id) {
            continue;
        }
        if let Some(domain) = project.domain.as_deref().filter(|d| !org.owns_domain(d)) {
            found.push((
                location_of(loaded),
                format!(
                    "Project `{}`: `spec.domain` `{domain}` is not the domain of its Organization \
                     `{named}` (`{}`) or a subdomain of it (PF-122)",
                    id.name, org.domain
                ),
            ));
        }
    }
    found
}

/// Messages, each at the place it is about.
type Located = Vec<(Location, String)>;

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

/// Every `subjects[].group` of a `RoleBinding`, and every Group subject of a `DelegationPolicy`,
/// that no `Group` manifest declares (PF-64, AG-150).
///
/// A binding to a group nobody declared matches nobody, silently: the people it was written for
/// read nothing and no error says why. The group's membership is configuration (PF-62), so the
/// manifest is here to be found, unless the binding marks the group `source: provider`: the
/// identity provider owns that one, and a manifest of the same name is the finding instead. A
/// `ServiceAccount` names no group — its `spec.roles[]` carries a
/// role and a scope and nothing else — so there is nothing of its to check here.
fn bindings_to_undeclared_groups(repo: &Repository) -> Vec<(Location, String)> {
    let declared: BTreeSet<&str> = repo
        .iter()
        .filter(|(id, _)| id.kind == "Group")
        .map(|(id, _)| id.name.as_str())
        .collect();

    let mut findings = Vec::new();
    for (id, resource) in repo.iter() {
        // A DelegationPolicy names its Groups as `{kind: Group, name}` (AG-150).
        if id.kind == "DelegationPolicy" {
            let groups = resource
                .manifest
                .spec
                .get("subjects")
                .and_then(|subjects| subjects.as_array())
                .into_iter()
                .flatten()
                .filter(|subject| subject.get("kind").and_then(|k| k.as_str()) == Some("Group"))
                .filter_map(|subject| subject.get("name").and_then(|n| n.as_str()))
                .filter(|group| !declared.contains(group));
            for group in groups {
                findings.push((
                    (resource.path.clone(), resource.document, resource.line),
                    format!(
                        "{id} spec.subjects names group `{group}`, which no Group manifest of \
                         this organization declares: add `users/groups/{group}.yaml` (AG-150, \
                         PF-62)"
                    ),
                ));
            }
            continue;
        }
        // A RoleBinding's subjects, and the members of every role an App declares (AP-91).
        let subjects: Vec<&serde_json::Value> = match id.kind.as_str() {
            "RoleBinding" => resource
                .manifest
                .spec
                .get("subjects")
                .and_then(|subjects| subjects.as_array())
                .map(|subjects| subjects.iter().collect())
                .unwrap_or_default(),
            "App" => app_subjects(&resource.manifest.spec).collect(),
            _ => continue,
        };
        for subject in subjects {
            let Some(group) = subject.get("group").and_then(|g| g.as_str()) else {
                continue;
            };
            // A group the identity provider owns is marked in the binding and has no manifest;
            // one that also has a manifest would have two owners (PF-63, PF-64).
            if subject.get("source").and_then(|s| s.as_str()) == Some("provider") {
                if declared.contains(group) {
                    findings.push((
                        (resource.path.clone(), resource.document, resource.line),
                        format!(
                            "{id} marks group `{group}` `source: provider`, and \
                             `users/groups/{group}.yaml` declares it too: drop the mark to bind \
                             the Group manifest, or the manifest if the identity provider owns \
                             its members (PF-63, PF-64)"
                        ),
                    ));
                }
                continue;
            }
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

/// Refused: brokers without exactly one default, and a space whose `brokerRef` names no
/// `ContextBroker` of the organization (PF-110, PF-111). The brokers are the organization's, so
/// a project repository checked alone is not judged here.
fn brokers_that_do_not_resolve(repo: &Repository) -> Vec<(Location, String)> {
    let brokers: Vec<(&str, jc_core::kinds::ContextBrokerSpec, Location)> = repo
        .iter()
        .filter(|(id, _)| id.kind == "ContextBroker")
        .filter_map(|(id, resource)| {
            let spec = serde_json::from_value(resource.manifest.spec.clone()).ok()?;
            let at = (resource.path.clone(), resource.document, resource.line);
            Some((id.name.as_str(), spec, at))
        })
        .collect();
    let named = || brokers.iter().map(|(name, spec, _)| (*name, spec));
    let mut findings = Vec::new();
    if let (Err(err), Some((_, _, at))) =
        (jc_core::kinds::validate_brokers(named()), brokers.first())
    {
        findings.push((at.clone(), format!("ContextBroker: {err} (PF-110)")));
    }
    for (id, resource) in repo.iter().filter(|(id, _)| id.kind == "ContextSpace") {
        let broker_ref = resource
            .manifest
            .spec
            .get("brokerRef")
            .and_then(|b| b.as_str());
        if broker_ref.is_none() {
            continue;
        }
        if let Err(err) = jc_core::kinds::broker_of(broker_ref, named()) {
            findings.push((
                (resource.path.clone(), resource.document, resource.line),
                format!(
                    "{id}: {err}: add `brokers/{}.yaml`",
                    broker_ref.unwrap_or_default()
                ),
            ));
        }
    }
    findings
}

/// The subjects of every role an App's `spec.access` lists (AP-91).
fn app_subjects(spec: &serde_json::Value) -> impl Iterator<Item = &serde_json::Value> {
    spec.get("access")
        .and_then(|access| access.as_array())
        .into_iter()
        .flatten()
        .filter_map(|entry| entry.get("subjects").and_then(|s| s.as_array()))
        .flatten()
}

/// Every App data need that writes and names no role: everyone who can open the app can make
/// that write, which is allowed and is said (AP-98).
fn app_writes_open_to_everyone(repo: &Repository) -> Vec<(Location, String)> {
    let mut warnings = Vec::new();
    for (id, resource) in repo.iter() {
        if id.kind != "App" {
            continue;
        }
        let Ok(spec) =
            serde_json::from_value::<jc_core::kinds::AppSpec>(resource.manifest.spec.clone())
        else {
            continue;
        };
        for need in spec
            .data_needs
            .iter()
            .filter(|n| n.has_write() && n.roles.is_empty())
        {
            let operations: Vec<&str> = need
                .operations
                .iter()
                .filter(|o| o.is_write())
                .map(|o| o.as_str())
                .collect();
            warnings.push((
                (resource.path.clone(), resource.document, resource.line),
                format!(
                    "everyone who can open {} can {} {}; name the roles that may in \
                     dataNeeds[].roles (AP-98)",
                    id.name,
                    operations.join(", "),
                    need.types.join(", ")
                ),
            ));
        }
    }
    warnings
}

/// Every `App` whose name another project's `App` also declares (AP-14a): the host
/// `{name}.apps.{domain}` and the pod `app-{name}` are one address for the organization, so the
/// edge cannot serve both.
fn app_names_claimed_twice(repo: &Repository) -> Vec<(Location, String)> {
    claimed_twice(
        repo,
        "App",
        |_, name| name.to_owned(),
        |name, _, projects| {
            format!(
                "App {name} is declared by projects {projects}; the host {name}.apps.{{domain}} is \
                 one address for the whole organization, rename all but one (AP-14a)"
            )
        },
    )
}

/// Every `ContextSpace` whose `{space}` id segment another project's space also renders (PF-44,
/// PF-84, AP-115): `{project}-{name}` of `a-b`/`c` and `a`/`b-c` are one segment, and so are two
/// equal `urnSegment` pins, and an id could then land in either space.
fn space_segments_claimed_twice(repo: &Repository) -> Vec<(Location, String)> {
    claimed_twice(
        repo,
        "ContextSpace",
        |project, name| repo.space_segment(project, name),
        |name, segment, projects| {
            format!(
                "Context Space {name} renders the id segment '{segment}', which projects \
                 {projects} all render; a segment is unique in the organization, rename the \
                 space or pin spec.urnSegment (PF-44, PF-84)"
            )
        },
    )
}

/// Every `AssistantDeployment` whose `publicId` another deployment also declares, in this project
/// or another (MF-52, T-3283): the chat route and the widget answer one deployment per id, so a
/// second one would take the first's public address.
fn public_ids_claimed_twice(repo: &Repository) -> Vec<(Location, String)> {
    let deployments: Vec<(String, Location, String)> = repo
        .iter()
        .filter(|(id, _)| id.kind == "AssistantDeployment")
        .filter_map(|(id, resource)| {
            let public_id = resource.manifest.spec.get("publicId")?.as_str()?.to_owned();
            let owner = format!(
                "{}/{}",
                id.namespace.as_deref().unwrap_or_default(),
                id.name
            );
            Some((
                public_id,
                (resource.path.clone(), resource.document, resource.line),
                owner,
            ))
        })
        .collect();
    let mut owners: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for (public_id, _, owner) in &deployments {
        owners.entry(public_id).or_default().push(owner);
    }
    deployments
        .iter()
        .filter_map(|(public_id, location, owner)| {
            let all = owners.get(public_id.as_str())?;
            (all.len() > 1).then(|| {
                (
                    location.clone(),
                    format!(
                        "AssistantDeployment {owner}: spec.publicId `{public_id}` is declared by {}; \
                         a publicId is one public address for the whole organization, give each \
                         deployment its own (MF-52)",
                        all.join(" and ")
                    ),
                )
            })
        })
        .collect()
}

/// Every `kind` manifest whose `key` another project's manifest of that kind also has, located
/// and named with every project holding it: `message(name, key, "a and b")`.
fn claimed_twice(
    repo: &Repository,
    kind: &str,
    key: impl Fn(&str, &str) -> String,
    message: impl Fn(&str, &str, &str) -> String,
) -> Vec<(Location, String)> {
    let of_kind = || {
        repo.iter()
            .filter(move |(id, _)| id.kind == kind)
            .map(|(id, resource)| {
                let project = id.namespace.as_deref().unwrap_or_default();
                (id, resource, project, key(project, &id.name))
            })
    };
    let mut projects: BTreeMap<String, BTreeSet<&str>> = BTreeMap::new();
    for (_, _, project, key) in of_kind() {
        projects.entry(key).or_default().insert(project);
    }
    of_kind()
        .filter_map(|(id, resource, _, key)| {
            let claimants = projects.get(&key)?;
            (claimants.len() > 1).then(|| {
                let names: Vec<&str> = claimants.iter().copied().collect();
                (
                    (resource.path.clone(), resource.document, resource.line),
                    message(&id.name, &key, &names.join(" and ")),
                )
            })
        })
        .collect()
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

/// Every Pipeline output in `replace` into a space and type another Pipeline of its project also
/// writes (PL-79, T-4408), on the replacing Pipeline. The Portal refuses the same proposal through
/// the same jc-core rule. A Pipeline or Endpoint that does not parse is reported on its own.
fn replacing_shared_writes(repo: &Repository) -> Vec<(Location, String)> {
    use jc_core::kinds::{replace_conflicts, EndpointSpec, PipelineSpec};
    let mut spaces: BTreeMap<(String, String), String> = BTreeMap::new();
    let mut pipelines: BTreeMap<String, Vec<(String, PipelineSpec, Location)>> = BTreeMap::new();
    for (id, resource) in repo.iter() {
        let project = id.namespace.clone().unwrap_or_default();
        let spec = resource.manifest.spec.clone();
        match id.kind.as_str() {
            "Endpoint" => {
                if let Ok(spec) = serde_json::from_value::<EndpointSpec>(spec) {
                    spaces.insert(
                        (project, id.name.clone()),
                        spec.context_space_ref.name().to_owned(),
                    );
                }
            }
            "Pipeline" => {
                if let Ok(spec) = serde_json::from_value::<PipelineSpec>(spec) {
                    let location = (resource.path.clone(), resource.document, resource.line);
                    pipelines
                        .entry(project)
                        .or_default()
                        .push((id.name.clone(), spec, location));
                }
            }
            _ => {}
        }
    }
    let mut found = Vec::new();
    for (project, members) in &pipelines {
        let specs: Vec<(&str, &PipelineSpec)> =
            members.iter().map(|(n, s, _)| (n.as_str(), s)).collect();
        let conflicts = replace_conflicts(&specs, |endpoint| {
            spaces.get(&(project.clone(), endpoint.to_owned())).cloned()
        });
        for conflict in conflicts {
            if let Some((_, _, location)) = members.iter().find(|(n, ..)| *n == conflict.pipeline) {
                found.push((location.clone(), conflict.to_string()));
            }
        }
    }
    found
}

/// Every write nobody can make (PL-111, ADR-N-057 decision 5, T-5035): a Policy granting a write that no
/// Endpoint of its project binds, and a Pipeline output whose `targetEndpoint` is not declared in
/// the Pipeline's project or does not bind that output's derived write Policy (PL-20). Binding is
/// the gateway's rule (`context-gateway` store.rs `bound_policies`): the Endpoint serves the
/// Policy's space, names it in `policyRefs` (or `policyRef`) or names none, and an App's generated Endpoint takes
/// its own grants alone (AP-139). The bbsk podujatia pipelines wrote through a read Endpoint's
/// `policyRef` this way and every upsert answered 404 (T-5033).
fn writes_nobody_can_make(repo: &Repository) -> Located {
    unmade_writes(repo)
        .into_iter()
        .filter_map(|unmade| {
            let resource = repo.get(&unmade.id)?;
            Some((
                (resource.path.clone(), resource.document, resource.line),
                unmade.message,
            ))
        })
        .collect()
}

/// A write nobody can make (PL-111): the manifest it is found on, the member to change when one
/// names it (a Pipeline output's `targetEndpoint`), and the sentence `jcctl validate` prints.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnmadeWrite {
    /// The Policy or Pipeline the finding is on.
    pub id: ResourceId,
    /// The member's path in the manifest, `spec.outputs.0.targetEndpoint`; `None` for a Policy.
    pub field: Option<String>,
    /// What is wrong and what to change.
    pub message: String,
}

/// The findings of `jcctl validate`'s PL-111 check by resource: the Portal's change review shows the
/// same sentences on a proposal (ADR-N-057 decision 5, T-5040).
pub fn unmade_writes(repo: &Repository) -> Vec<UnmadeWrite> {
    let bindings = WriteBindings::new(repo);
    let mut found = bindings.unbound_write_policies();
    found.extend(bindings.unreachable_pipeline_targets());
    found
}

/// The path of output `index` of [`jc_core::kinds::PipelineSpec::outputs`] in the manifest.
fn output_field(spec: &jc_core::kinds::PipelineSpec, index: usize) -> String {
    let lane = spec.lane_outputs().len();
    if index < lane {
        return match spec.is_second_shape() {
            true => format!("spec.outputs.{index}.targetEndpoint"),
            false => "spec.targetEndpoint".to_owned(),
        };
    }
    let mut rest = index - lane;
    for (route, outputs) in spec.routes.iter().map(|r| r.outputs.len()).enumerate() {
        if rest < outputs {
            return format!("spec.routes.{route}.outputs.{rest}.targetEndpoint");
        }
        rest -= outputs;
    }
    "spec.outputs".to_owned()
}

/// The Endpoints of a repository keyed by (project, name), with the gateway's binding rule.
struct WriteBindings<'a> {
    repo: &'a Repository,
    org_domain: Option<&'a str>,
    endpoints: BTreeMap<(String, String), jc_core::kinds::EndpointSpec>,
}

impl<'a> WriteBindings<'a> {
    fn new(repo: &'a Repository) -> Self {
        let mut endpoints = BTreeMap::new();
        for (id, resource) in repo.iter().filter(|(id, _)| id.kind == "Endpoint") {
            if let Ok(spec) = serde_json::from_value(resource.manifest.spec.clone()) {
                endpoints.insert(
                    (id.namespace.clone().unwrap_or_default(), id.name.clone()),
                    spec,
                );
            }
        }
        Self {
            repo,
            org_domain: repo.org_domain(),
            endpoints,
        }
    }

    fn is_app(&self, project: &str, endpoint: &str) -> bool {
        // Looked up by id, not found by a scan (F-PLT3-020, as `space_segment`).
        endpoint.strip_prefix("app-").is_some_and(|app| {
            let id = ResourceId::new("joinedcontext.com", "App", Some(project.to_owned()), app);
            self.repo.get(&id).is_some()
        })
    }

    /// Why `endpoint` does not bind `policy`, or `None` when it does.
    fn refusal(
        &self,
        (project, endpoint): &(String, String),
        spec: &jc_core::kinds::EndpointSpec,
        name: &str,
        policy: &jc_core::kinds::PolicySpec,
    ) -> Option<String> {
        let space = spec.context_space_ref.name();
        if space != policy.context_space_ref.name() {
            return Some(format!("serves the space {space}"));
        }
        if let Some(refs) = spec.policy_refs() {
            let segment = self.repo.space_segment(project, space);
            let ours = |reference: &jc_core::Urn| {
                self.org_domain
                    .is_none_or(|domain| reference.org_domain() == Some(domain))
                    && reference.space() == Some(segment.as_str())
            };
            let named: Vec<_> = refs.iter().filter(|r| r.local_id() == name).collect();
            if named.is_empty() {
                let listed: Vec<String> = refs.iter().map(ToString::to_string).collect();
                return Some(format!("binds only its policyRefs {}", listed.join(", ")));
            }
            if !named.iter().any(|reference| ours(reference)) {
                return Some(format!(
                    "names it as {}, which binds nothing: write \
                     urn:ngsi-ld:Policy:{}:{segment}:{name}",
                    named[0],
                    self.org_domain.unwrap_or("{orgDomain}")
                ));
            }
        }
        if self.is_app(project, endpoint) && !policy.is_endpoint_grant(project, endpoint) {
            return Some(format!(
                "is the App {}'s own Endpoint",
                &endpoint["app-".len()..]
            ));
        }
        None
    }

    /// A Policy granting a write that no Endpoint of its project binds.
    fn unbound_write_policies(&self) -> Vec<UnmadeWrite> {
        use jc_core::kinds::PolicySpec;
        let mut found = Vec::new();
        for (id, resource) in self.repo.iter().filter(|(id, _)| id.kind == "Policy") {
            let Ok(spec) = serde_json::from_value::<PolicySpec>(resource.manifest.spec.clone())
            else {
                continue;
            };
            // An invalid shape is the shape check's finding; each rule is its own grant.
            for (rule, policy) in spec.expand(&id.name).unwrap_or_default() {
                if policy.effect.is_prohibition() || !writes_entities(&policy) {
                    continue;
                }
                let project = id.namespace.clone().unwrap_or_default();
                let space = policy.context_space_ref.name();
                let mut reasons = Vec::new();
                let mut bound = false;
                for (key, spec) in self.endpoints.iter().filter(|((p, _), _)| *p == project) {
                    match self.refusal(key, spec, &id.name, &policy) {
                        None => bound = true,
                        Some(why) if spec.context_space_ref.name() == space => {
                            reasons.push(format!("{} {why}", key.1))
                        }
                        Some(_) => {}
                    }
                }
                if bound {
                    continue;
                }
                let why = if reasons.is_empty() {
                    format!("the space {space} has no Endpoint in project {project}")
                } else {
                    reasons.join("; ")
                };
                found.push(UnmadeWrite {
                    id: id.clone(),
                    field: None,
                    message: format!(
                        "Policy {rule} grants a write on the space {space}, but no Endpoint binds \
                         it ({why}): name it in an Endpoint's policyRef or add an Endpoint of the \
                         space without one (EP-14, ADR-N-057)"
                    ),
                });
            }
        }
        found
    }

    /// A Pipeline output whose `targetEndpoint` is not declared in the Pipeline's project or does
    /// not bind that output's derived write Policy (PL-20).
    fn unreachable_pipeline_targets(&self) -> Vec<UnmadeWrite> {
        use jc_core::kinds::pipeline_identity::{account_name, bounded, derive};
        use jc_core::kinds::PipelineSpec;
        let mut found = Vec::new();
        for (id, resource) in self.repo.iter().filter(|(id, _)| id.kind == "Pipeline") {
            let Ok(spec) = serde_json::from_value::<PipelineSpec>(resource.manifest.spec.clone())
            else {
                continue;
            };
            let project = id.namespace.clone().unwrap_or_default();
            let space_of = |endpoint: &str| {
                self.endpoints
                    .get(&(project.clone(), endpoint.to_owned()))
                    .map(|spec| spec.context_space_ref.name().to_owned())
            };
            let derived = derive(
                &project,
                &id.name,
                &spec,
                "",
                self.org_domain.unwrap_or_default(),
                space_of,
            );
            let account = account_name(&id.name);
            for (index, output) in spec.outputs().iter().enumerate() {
                let target = output.target_endpoint.local_id();
                let key = (project.clone(), target.to_owned());
                let why = match self.endpoints.get(&key) {
                    None => Some(format!("project {project} declares no Endpoint {target}")),
                    Some(endpoint) => {
                        let name = bounded(&format!("{account}-w-{}", index + 1));
                        derived
                            .policies
                            .iter()
                            .find(|(policy, _)| *policy == name)
                            .and_then(|(name, policy)| self.refusal(&key, endpoint, name, policy))
                            .map(|why| format!("{target} {why}"))
                    }
                };
                if let Some(why) = why {
                    found.push(UnmadeWrite {
                        id: id.clone(),
                        field: Some(output_field(&spec, index)),
                        message: format!(
                            "Pipeline {} output {} writes as {account}, but its targetEndpoint grants \
                             that account no write ({why}), so every write answers 404: aim it at an \
                             Endpoint of the space without policyRef (PL-20, ADR-N-057)",
                            id.name,
                            index + 1
                        ),
                    });
                }
            }
        }
        found
    }
}

/// Whether `policy` grants a write of context data. A subscription is the caller's own state, so
/// a read Policy that lets its reader subscribe grants no write here.
fn writes_entities(policy: &jc_core::kinds::PolicySpec) -> bool {
    use jc_core::kinds::{Operation, OperationRef};
    policy.operations.iter().any(|op| match op {
        OperationRef::Single(
            Operation::CreateSubscription
            | Operation::UpdateSubscription
            | Operation::DeleteSubscription,
        ) => false,
        op => op.is_write(),
    })
}

/// The knowledge assistant's references, each resolved inside its own project (MF-51, MF-52): a
/// `ckan` source's `CkanInstance`, a deployment's sources and its connectors' Endpoints, and no
/// `internal` source behind a channel that answers people nobody signed in. Each finding names
/// the field to change, so the person who wrote it does not meet the gap at crawl or chat time.
fn assistant_references(repo: &Repository) -> Vec<(Location, String)> {
    use std::collections::BTreeMap;
    let declared = |kind: &str| -> std::collections::BTreeSet<(Option<String>, String)> {
        repo.iter()
            .filter(|(id, _)| id.kind == kind)
            .map(|(id, _)| (id.namespace.clone(), id.name.clone()))
            .collect()
    };
    let instances = declared("CkanInstance");
    let endpoints = declared("Endpoint");
    // A source's visibility, `internal` when the manifest leaves it out (MF-51).
    let sources: BTreeMap<(Option<String>, String), String> = repo
        .iter()
        .filter(|(id, _)| id.kind == "KnowledgeSource")
        .map(|(id, resource)| {
            let visibility = resource
                .manifest
                .spec
                .get("visibility")
                .and_then(|v| v.as_str())
                .unwrap_or("internal")
                .to_owned();
            ((id.namespace.clone(), id.name.clone()), visibility)
        })
        .collect();

    // Every Endpoint as its kind reads it, for the named MCP servers' members (MF-53).
    let endpoint_specs: BTreeMap<(Option<String>, String), jc_core::kinds::EndpointSpec> = repo
        .iter()
        .filter(|(id, _)| id.kind == "Endpoint")
        .filter_map(|(id, resource)| {
            let spec = serde_json::from_value(resource.manifest.spec.clone()).ok()?;
            Some(((id.namespace.clone(), id.name.clone()), spec))
        })
        .collect();

    // Every named MCP server, for the deployments that name one (MF-56).
    let server_specs: BTreeMap<(Option<String>, String), jc_core::kinds::McpServerSpec> = repo
        .iter()
        .filter(|(id, _)| id.kind == "McpServer")
        .filter_map(|(id, resource)| {
            let spec = serde_json::from_value(resource.manifest.spec.clone()).ok()?;
            Some(((id.namespace.clone(), id.name.clone()), spec))
        })
        .collect();
    let servers: BTreeMap<(Option<String>, String), jc_core::kinds::Audience> = server_specs
        .iter()
        .map(|(key, spec)| (key.clone(), spec.audience))
        .collect();
    // What the resolver knows of each Endpoint from the repository: the classes of its space's
    // model as its types, their classification, and the themes set by hand. A projection may
    // serve fewer types; the finding below names the Endpoints so one can be excluded.
    let model_classes = |project: &Option<String>, space: &str| -> Vec<String> {
        // Looked up by id: a scan per Endpoint made a large repository quadratic (F-PLT3-020).
        let space_id = ResourceId::new("joinedcontext.com", "ContextSpace", project.clone(), space);
        let model = repo.get(&space_id).and_then(|space| {
            let reference = &space.manifest.spec["dataModelRef"];
            let name = reference.as_str().or_else(|| reference["name"].as_str())?;
            let namespace = reference["namespace"].as_str().map(str::to_owned);
            Some((namespace.or_else(|| project.clone()), name.to_owned()))
        });
        model
            .and_then(|(namespace, name)| {
                repo.get(&ResourceId::new(
                    "joinedcontext.com",
                    "DataModel",
                    namespace,
                    name,
                ))
            })
            .map(|model| {
                model.manifest.spec["classes"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|class| class.as_str().map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default()
    };
    let facts: Vec<jc_core::reach::EndpointFacts> = endpoint_specs
        .iter()
        .map(|((namespace, name), spec)| {
            jc_core::reach::EndpointFacts::of(
                namespace.as_deref().unwrap_or_default(),
                name,
                spec,
                model_classes(namespace, spec.context_space_ref.name()),
            )
        })
        .collect();
    // The projects and spaces a selector may name (MF-55): a project is any namespace a
    // manifest lives in.
    let projects: std::collections::BTreeSet<String> = repo
        .iter()
        .filter_map(|(id, _)| match id.kind.as_str() {
            "Project" => Some(id.name.clone()),
            _ => id.namespace.clone(),
        })
        .filter(|project| project != jc_core::envelope::ORG_NAMESPACE)
        .collect();
    let spaces = declared("ContextSpace");
    // Every knowledge source and whether it is public, for the servers that attach one (MF-57).
    let knowledge: BTreeMap<(Option<String>, String), bool> = repo
        .iter()
        .filter(|(id, _)| id.kind == "KnowledgeSource")
        .map(|(id, resource)| {
            let public = resource
                .manifest
                .spec
                .get("visibility")
                .and_then(|v| v.as_str())
                == Some("public");
            ((id.namespace.clone(), id.name.clone()), public)
        })
        .collect();

    let mut found = Vec::new();
    for (id, resource) in repo.iter() {
        let at = (resource.path.clone(), resource.document, resource.line);
        let spec = &resource.manifest.spec;
        let here = |name: &str| (id.namespace.clone(), name.to_owned());
        match id.kind.as_str() {
            "McpServer" => {
                // A spec the kind refuses is reported by the schema check; nothing more to say here.
                let Ok(server) =
                    serde_json::from_value::<jc_core::kinds::McpServerSpec>(spec.clone())
                else {
                    continue;
                };
                let namespace = id.namespace.clone().unwrap_or_default();
                if let Some(select) = &server.select {
                    for project in select.projects.iter().flatten() {
                        if !projects.contains(project) {
                            found.push((at.clone(), format!(
                                "{id}: spec.select.projects names project `{project}`, which the repository does not declare (MF-55)"
                            )));
                        }
                    }
                    for space in select.spaces.iter().flatten() {
                        let (project, name) = space.split_once('/').unwrap_or_default();
                        if !spaces.contains(&(Some(project.to_owned()), name.to_owned())) {
                            found.push((at.clone(), format!(
                                "{id}: spec.select.spaces names Context Space `{space}`, which no manifest declares (MF-55)"
                            )));
                        }
                    }
                }
                for (project, name) in server.knowledge_source_ids(&namespace) {
                    match knowledge.get(&(Some(project.to_owned()), name.to_owned())) {
                        None => found.push((at.clone(), format!(
                            "{id}: spec.knowledgeSources names KnowledgeSource `{project}/{name}`, which no manifest declares (MF-57)"
                        ))),
                        Some(false) => found.push((at.clone(), format!(
                            "{id}: spec.knowledgeSources names KnowledgeSource `{project}/{name}`, which is not `visibility: public`; a server searches public sources only (MF-57)"
                        ))),
                        Some(true) => {}
                    }
                }
                for (project, name) in server.member_ids(&namespace) {
                    let Some(member) =
                        endpoint_specs.get(&(Some(project.to_owned()), name.to_owned()))
                    else {
                        found.push((at.clone(), format!(
                            "{id}: spec.members names Endpoint `{project}/{name}`, which no manifest declares (MF-53)"
                        )));
                        continue;
                    };
                    if !member
                        .served_representations()
                        .contains(&jc_core::kinds::Representation::Mcp)
                    {
                        found.push((at.clone(), format!(
                            "{id}: member Endpoint `{project}/{name}` serves no MCP (`mcp: false`); a named server reads its members through their MCP surface (MF-53)"
                        )));
                    }
                    if jc_core::kinds::mcp_server::breadth(member.audience)
                        < jc_core::kinds::mcp_server::breadth(server.audience)
                    {
                        found.push((at.clone(), format!(
                            "{id}: spec.audience is `{}`, wider than member Endpoint `{project}/{name}`, which admits `{}`; a server is no wider than its narrowest member (MF-53)",
                            server.audience, member.audience
                        )));
                    }
                }
            }
            "KnowledgeSource" => {
                if let Some(instance) = spec.get("ckanInstanceRef").and_then(|v| v.as_str()) {
                    if !instances.contains(&here(instance)) {
                        found.push((at.clone(), format!(
                            "{id}: spec.ckanInstanceRef names CkanInstance `{instance}`, which no manifest of this project declares (MF-51)"
                        )));
                    }
                }
            }
            "AssistantDeployment" => {
                let channel = spec
                    .get("channel")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default();
                if let Ok(deployment) =
                    serde_json::from_value::<jc_core::kinds::AssistantDeploymentSpec>(spec.clone())
                {
                    let own = id.namespace.clone().unwrap_or_default();
                    found.extend(
                        deployment_reach(
                            &own,
                            &deployment,
                            &projects,
                            &spaces,
                            &endpoints,
                            &facts,
                            &server_specs,
                        )
                        .into_iter()
                        .map(|finding| (at.clone(), format!("{id}: {finding}"))),
                    );
                }
                if let Some(server) = spec.get("mcpServer").and_then(|v| v.as_str()) {
                    let (project, name) = jc_core::kinds::assistant::connector_target(server);
                    // `org/all` is every organization's, served without a manifest (EP-122).
                    let audience = match servers.get(&(project.map(str::to_owned), name.to_owned()))
                    {
                        Some(audience) => Some(*audience),
                        None if server == "org/all" => Some(jc_core::kinds::Audience::Organization),
                        None => None,
                    };
                    match audience {
                        None => found.push((at.clone(), format!(
                            "{id}: spec.mcpServer names McpServer `{server}`, which no manifest declares (MF-56)"
                        ))),
                        Some(audience) if channel != "internal" && audience != jc_core::kinds::Audience::Public => found.push((at.clone(), format!(
                            "{id}: spec.mcpServer names McpServer `{server}`, which is `{audience}`; a `{channel}` channel answers people nobody signed in, so it reads a public server only. Name a `public` server or use channel `internal` (MF-56)"
                        ))),
                        Some(_) => {}
                    }
                }
                for name in spec
                    .get("sources")
                    .and_then(|v| v.as_array())
                    .into_iter()
                    .flatten()
                    .filter_map(|v| v.as_str())
                {
                    // Another project's source, `{project}/{source}`, is looked up there; the kind
                    // already refused a project its scope does not list (T-4281).
                    let (project, source) = jc_core::kinds::assistant::connector_target(name);
                    let key = match project {
                        Some(project) => (Some(project.to_owned()), source.to_owned()),
                        None => here(source),
                    };
                    match sources.get(&key) {
                        None => found.push((at.clone(), match project {
                            Some(project) => format!(
                                "{id}: spec.sources names KnowledgeSource `{name}`, which no manifest of project `{project}` declares (MF-52)"
                            ),
                            None => format!(
                                "{id}: spec.sources names KnowledgeSource `{name}`, which no manifest of this project declares (MF-52)"
                            ),
                        })),
                        Some(visibility) if visibility != "public" && channel != "internal" => found.push((at.clone(), format!(
                            "{id}: spec.sources names KnowledgeSource `{name}`, which is `{visibility}`; a `{channel}` channel answers people nobody signed in, so it reads public sources only. Make the source `visibility: public` or use channel `internal` (MF-52)"
                        ))),
                        Some(_) => {}
                    }
                }
                for connector in spec
                    .get("connectors")
                    .and_then(|v| v.as_array())
                    .into_iter()
                    .flatten()
                {
                    let Some(endpoint) = connector.get("endpoint").and_then(|v| v.as_str()) else {
                        continue;
                    };
                    match jc_core::kinds::assistant::connector_target(endpoint) {
                        // Another project's Endpoint: declared there and public (T-4177); the kind
                        // already refused a project its scope does not list.
                        (Some(project), name) => {
                            match endpoint_specs.get(&(Some(project.to_owned()), name.to_owned())) {
                                None => found.push((at.clone(), format!(
                                    "{id}: spec.connectors names Endpoint `{endpoint}`, which no manifest of project `{project}` declares (MF-52)"
                                ))),
                                Some(spec) if spec.audience != jc_core::kinds::Audience::Public => found.push((at.clone(), format!(
                                    "{id}: spec.connectors names Endpoint `{endpoint}`, which is `{}`; another project's Endpoint is reached only when it is `audience: public` (MF-52)",
                                    spec.audience
                                ))),
                                Some(_) => {}
                            }
                        }
                        (None, name) => {
                            if !endpoints.contains(&here(name)) {
                                found.push((at.clone(), format!(
                                    "{id}: spec.connectors names Endpoint `{endpoint}`, which no manifest of this project declares (MF-52)"
                                )));
                            }
                        }
                    }
                }
            }
            _ => {}
        }
    }
    found
}

/// What a deployment's selector and exclusions name, checked against the repository (MF-58,
/// MF-59, AG-122): projects and spaces in its reach and declared, excluded Endpoints declared,
/// and no more Endpoints resolved than one deployment serves.
fn deployment_reach(
    own: &str,
    deployment: &jc_core::kinds::AssistantDeploymentSpec,
    projects: &std::collections::BTreeSet<String>,
    spaces: &std::collections::BTreeSet<(Option<String>, String)>,
    endpoints: &std::collections::BTreeSet<(Option<String>, String)>,
    facts: &[jc_core::reach::EndpointFacts],
    servers: &std::collections::BTreeMap<(Option<String>, String), jc_core::kinds::McpServerSpec>,
) -> Vec<String> {
    let mut found = Vec::new();
    let in_reach =
        |project: &str| project == own || deployment.scope_projects().iter().any(|p| p == project);
    if let Some(select) = &deployment.select {
        for project in select.projects.iter().flatten() {
            if !in_reach(project) {
                found.push(format!(
                    "spec.select.projects names project `{project}`, which is neither this project nor in scope.projects; add it to scope.projects (MF-58)"
                ));
            } else if !projects.contains(project) {
                found.push(format!(
                    "spec.select.projects names project `{project}`, which the repository does not declare (MF-58)"
                ));
            }
        }
        for space in select.spaces.iter().flatten() {
            let (project, name) = space.split_once('/').unwrap_or_default();
            if !in_reach(project) {
                found.push(format!(
                    "spec.select.spaces names Context Space `{space}` of project `{project}`, which is neither this project nor in scope.projects; add it to scope.projects (MF-58)"
                ));
            } else if !spaces.contains(&(Some(project.to_owned()), name.to_owned())) {
                found.push(format!(
                    "spec.select.spaces names Context Space `{space}`, which no manifest declares (MF-58)"
                ));
            }
        }
    }
    for excluded in &deployment.exclude {
        let (project, name) = jc_core::kinds::assistant::connector_target(excluded);
        let project = project.unwrap_or(own);
        if !endpoints.contains(&(Some(project.to_owned()), name.to_owned())) {
            found.push(format!(
                "spec.exclude names Endpoint `{excluded}`, which no manifest of project `{project}` declares (MF-59)"
            ));
        }
    }
    // `org/all` has no manifest: every organization Endpoint, served as the hub's (EP-122).
    let org_all: Option<jc_core::kinds::McpServerSpec> =
        serde_json::from_value(serde_json::json!({"audience": "organization", "select": {}})).ok();
    let server = deployment.mcp_server.as_deref().and_then(|named| {
        let (project, name) = jc_core::kinds::assistant::connector_target(named);
        let project = project.unwrap_or(own);
        match servers.get(&(Some(project.to_owned()), name.to_owned())) {
            Some(spec) => Some((project, spec)),
            None if named == "org/all" => org_all
                .as_ref()
                .map(|spec| (jc_core::envelope::ORG_NAMESPACE, spec)),
            None => None,
        }
    });
    let resolution = jc_core::reach::resolve_connectors(own, deployment, facts, server);
    if !resolution.omitted.is_empty() {
        found.push(format!(
            "resolves {} Endpoints, more than the {} one deployment serves; narrow spec.select or add spec.exclude. Left out: {} (AG-122)",
            resolution.endpoints.len() + resolution.omitted.len(),
            jc_core::reach::MAX_RESOLVED,
            resolution.omitted.join(", ")
        ));
    }
    found
}

/// The MQTT edge brokers checked against the repository (MF-54, MF-62, PL-76): every role names
/// ServiceAccounts its project declares, no two brokers hold one dedicated port or one custom
/// hostname and no two projects' allowances one hostname (each finding names the holder), and a
/// DataSource's `brokerRef` names a broker of its own project.
/// Findings and warnings, each at its manifest.
type Found = Vec<(Location, String)>;

fn mqtt_brokers(repo: &Repository) -> (Found, Found) {
    use jc_core::kinds::{MqttBrokerSpec, ServiceAccountSpec};
    use std::collections::{BTreeMap, BTreeSet};

    let accounts: BTreeSet<(Option<String>, String)> = repo
        .iter()
        .filter(|(id, _)| id.kind == "ServiceAccount")
        .map(|(id, _)| (id.namespace.clone(), id.name.clone()))
        .collect();
    // Each broker of each project with the roles it declares, for the bindings that name them.
    let declared: BTreeMap<(Option<String>, String), BTreeSet<String>> = repo
        .iter()
        .filter(|(id, _)| id.kind == "MqttBroker")
        .map(|(id, resource)| {
            let roles = serde_json::from_value::<MqttBrokerSpec>(resource.manifest.spec.clone())
                .map(|spec| spec.roles.into_iter().map(|role| role.name).collect())
                .unwrap_or_default();
            ((id.namespace.clone(), id.name.clone()), roles)
        })
        .collect();
    let brokers: BTreeSet<&(Option<String>, String)> = declared.keys().collect();
    let mut ports: BTreeMap<u16, String> = BTreeMap::new();
    let mut hostnames: BTreeMap<String, String> = BTreeMap::new();
    let mut assigned: BTreeMap<String, String> = BTreeMap::new();
    let mut found = Vec::new();
    let mut warned = Vec::new();
    for (id, resource) in repo.iter() {
        let at = (resource.path.clone(), resource.document, resource.line);
        match id.kind.as_str() {
            "MqttBroker" => {
                // A spec the kind refuses is reported by the schema check.
                let Ok(broker) =
                    serde_json::from_value::<MqttBrokerSpec>(resource.manifest.spec.clone())
                else {
                    continue;
                };
                for overlap in jc_core::mqtt::topics::overlaps(&broker.roles) {
                    warned.push((at.clone(), format!("{id}: {overlap} (MF-54)")));
                }
                for role in &broker.roles {
                    if !role.service_accounts.is_empty() {
                        warned.push((at.clone(), format!(
                            "{id}: role `{}` lists serviceAccounts, which is deprecated: `jcctl migrate mqtt-roles` moves them to `mqtt/{}/{}` bindings on each account (ADR-N-048 §2.13)",
                            role.name, id.name, role.name
                        )));
                    }
                    for account in &role.service_accounts {
                        if !accounts.contains(&(id.namespace.clone(), account.clone())) {
                            found.push((at.clone(), format!(
                                "{id}: role `{}` names ServiceAccount `{account}`, which no manifest of this project declares (MF-54)",
                                role.name
                            )));
                        }
                    }
                }
                if let Some(problem) = broker_hostname(&broker, &id.to_string(), &mut hostnames) {
                    found.push((at.clone(), problem));
                }
                if let Some(port) = broker.port {
                    match ports.get(&port) {
                        Some(holder) => found.push((at.clone(), format!(
                            "{id}: spec.port {port} is already held by {holder}; pick another port from the reserved range (MF-54)"
                        ))),
                        None => {
                            ports.insert(port, id.to_string());
                        }
                    }
                }
            }
            "ServiceAccount" => {
                let Ok(account) =
                    serde_json::from_value::<ServiceAccountSpec>(resource.manifest.spec.clone())
                else {
                    continue;
                };
                for binding in &account.roles {
                    let Some(Ok(grant)) = jc_core::mqtt::Grant::parse(&binding.role) else {
                        continue;
                    };
                    let holds = |(project, broker): &(Option<String>, String),
                                 roles: &BTreeSet<String>| {
                        *project == id.namespace
                            && grant.broker.is_none_or(|name| name == broker)
                            && roles.contains(grant.role)
                    };
                    if !declared.iter().any(|(key, roles)| holds(key, roles)) {
                        found.push((at.clone(), format!(
                            "{id}: spec.roles `{}` names a role no MqttBroker of this project declares (ADR-N-048 §2.13)",
                            binding.role
                        )));
                    }
                }
            }
            "Project" => {
                for problem in
                    project_hostnames(&resource.manifest.spec, &id.to_string(), &mut assigned)
                {
                    found.push((at.clone(), problem));
                }
            }
            "DataSource" => {
                let Some(name) = resource
                    .manifest
                    .spec
                    .pointer("/mqtt/brokerRef/name")
                    .and_then(|v| v.as_str())
                else {
                    continue;
                };
                if !brokers.contains(&(id.namespace.clone(), name.to_owned())) {
                    found.push((at, format!(
                        "{id}: spec.mqtt.brokerRef names MqttBroker `{name}`, which no manifest of this project declares (PL-76)"
                    )));
                }
            }
            _ => {}
        }
    }
    (found, warned)
}

/// A broker served on its own hostname claims it; a second broker naming it is refused (MF-62).
fn broker_hostname(
    broker: &jc_core::kinds::MqttBrokerSpec,
    id: &str,
    hostnames: &mut std::collections::BTreeMap<String, String>,
) -> Option<String> {
    let (jc_core::kinds::MqttAddress::CustomHostname, Some(name)) =
        (broker.effective_address(), &broker.hostname)
    else {
        return None;
    };
    match hostnames.get(name) {
        Some(holder) => Some(format!(
            "{id}: spec.hostname {name} is already served by {holder}; a hostname reaches one broker (MF-62)"
        )),
        None => {
            hostnames.insert(name.clone(), id.to_owned());
            None
        }
    }
}

/// A Project's `quotas.mqtt.customHostnames` claim each name for that project alone
/// (ADR-N-048 §2.11, MF-62).
fn project_hostnames(
    spec: &serde_json::Value,
    id: &str,
    assigned: &mut std::collections::BTreeMap<String, String>,
) -> Vec<String> {
    let names = spec
        .pointer("/quotas/mqtt/customHostnames")
        .and_then(|v| v.as_array())
        .into_iter()
        .flatten()
        .filter_map(|entry| entry.get("name")?.as_str());
    let mut found = Vec::new();
    for name in names {
        match assigned.get(name) {
            Some(holder) => found.push(format!(
                "{id}: spec.quotas.mqtt.customHostnames {name} is already assigned to {holder}; a hostname is assigned to one project at a time (ADR-N-048 §2.11, MF-62)"
            )),
            None => {
                assigned.insert(name.to_owned(), id.to_owned());
            }
        }
    }
    found
}

/// The federation topology the registrations declare, checked as a whole (CC-13). Per project:
/// a registration names a hub space and an Endpoint the project declares (no dangling peer),
/// a hub is served by an Endpoint of its own (what it registers reaches somebody), and no
/// space reaches itself through registrations (a loop the broker would forward round forever).
/// Each selector's own rules (an anchored `idPattern`, R24) are the kind's, checked above.
fn federation_topology(repo: &Repository) -> Vec<(Location, String)> {
    use jc_core::kinds::ContextSourceRegistrationSpec;

    let space_of = |spec: &serde_json::Value| -> Option<String> {
        match spec.get("contextSpaceRef")? {
            serde_json::Value::String(name) => Some(name.clone()),
            serde_json::Value::Object(map) => map.get("name")?.as_str().map(str::to_owned),
            _ => None,
        }
    };
    // Per project: its spaces, each Endpoint's space, and the registrations.
    let mut spaces: BTreeSet<(Option<String>, String)> = BTreeSet::new();
    let mut endpoints: BTreeMap<(Option<String>, String), String> = BTreeMap::new();
    let mut registrations = Vec::new();
    for (id, resource) in repo.iter() {
        let spec = &resource.manifest.spec;
        match id.kind.as_str() {
            "ContextSpace" => {
                spaces.insert((id.namespace.clone(), id.name.clone()));
            }
            "Endpoint" => {
                if let Some(space) = space_of(spec) {
                    endpoints.insert((id.namespace.clone(), id.name.clone()), space);
                }
            }
            "ContextSourceRegistration" => {
                // A registration the typed parse refused is already a finding above.
                if let Ok(csr) =
                    serde_json::from_value::<ContextSourceRegistrationSpec>(spec.clone())
                {
                    let location = (resource.path.clone(), resource.document, resource.line);
                    registrations.push((id.to_string(), id.namespace.clone(), csr, location));
                }
            }
            _ => {}
        }
    }

    let mut found = Vec::new();
    // hub -> member spaces, per project, for the loop check.
    let mut edges: BTreeMap<(Option<String>, String), BTreeSet<String>> = BTreeMap::new();
    let mut first_of_hub: BTreeMap<(Option<String>, String), (String, Location)> = BTreeMap::new();
    for (id, project, csr, location) in &registrations {
        let hub = csr.context_space_ref.name().to_owned();
        if !spaces.contains(&(project.clone(), hub.clone())) {
            found.push((
                location.clone(),
                format!("{id} registers into space `{hub}`, which no ContextSpace of this project declares (CC-13)"),
            ));
        }
        first_of_hub
            .entry((project.clone(), hub.clone()))
            .or_insert_with(|| (id.clone(), location.clone()));
        let Some(member) = &csr.endpoint_ref else {
            continue;
        };
        match endpoints.get(&(project.clone(), member.name().to_owned())) {
            None => found.push((
                location.clone(),
                format!(
                    "{id} registers Endpoint `{}`, which no manifest of this project declares: a dangling peer (CC-13)",
                    member.name()
                ),
            )),
            Some(space) => {
                edges
                    .entry((project.clone(), hub.clone()))
                    .or_default()
                    .insert(space.clone());
            }
        }
    }

    // Reachability: a hub nobody can read makes every registration into it a dead end.
    for ((project, hub), (id, location)) in &first_of_hub {
        let served = endpoints
            .iter()
            .any(|((namespace, _), space)| namespace == project && space == hub);
        if !served {
            found.push((
                location.clone(),
                format!("{id}: space `{hub}` holds registrations and no Endpoint of this project serves it, so what they register reaches nobody (CC-13)"),
            ));
        }
    }

    // Loops: a hub reaching itself through its members, once per hub on the cycle.
    for ((project, hub), (id, location)) in &first_of_hub {
        let mut seen = BTreeSet::new();
        let mut stack: Vec<String> = edges
            .get(&(project.clone(), hub.clone()))
            .map(|m| m.iter().cloned().collect())
            .unwrap_or_default();
        while let Some(space) = stack.pop() {
            if &space == hub {
                found.push((
                    location.clone(),
                    format!("{id}: space `{hub}` reaches itself through its registrations, a federation loop (CC-13)"),
                ));
                break;
            }
            if seen.insert(space.clone()) {
                if let Some(next) = edges.get(&(project.clone(), space)) {
                    stack.extend(next.iter().cloned());
                }
            }
        }
    }
    found
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
            // The spec's own rules keep `linkml` inside the model's folder: an invalid one is
            // never opened (CC-08).
            Some((spec, _)) if spec.validate().is_err() => format!(
                "{id} references DataModel `{}`, whose spec does not validate",
                wanted.name
            ),
            Some((spec, model_path)) => {
                let linkml = repo_dir
                    .join(&model_path)
                    .parent()
                    .map(|dir| dir.join(&spec.linkml))
                    .unwrap_or_else(|| repo_dir.join(&spec.linkml));
                let classes = std::fs::read_to_string(&linkml)
                    .map_err(|err| err.to_string())
                    .and_then(|text| jc_core::kinds::model_projection::linkml_classes(&text));
                match classes {
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

/// An Endpoint that serves a domain-standard representation needs a mapping that fits its
/// space's data model: one finding per gap (EP-125, API/02 §8b).
fn surfaces_without_their_model(repo_dir: &Path, repo: &Repository) -> Vec<(Location, String)> {
    let mut refused = Vec::new();
    for (id, resource) in repo.iter() {
        if id.kind != "Endpoint" {
            continue;
        }
        // An Endpoint the typed parse refused is already a finding above.
        let Ok(spec) = serde_json::from_value::<EndpointSpec>(resource.manifest.spec.clone())
        else {
            continue;
        };
        let wanted: Vec<_> = spec
            .served_representations()
            .into_iter()
            .filter(|r| r.needs_model())
            .collect();
        if wanted.is_empty() {
            continue;
        }
        let space = spec.context_space_ref.name();
        let location = (resource.path.clone(), resource.document, resource.line);
        let endpoint = format!(
            "{}/{}",
            id.namespace.as_deref().unwrap_or_default(),
            id.name
        );
        // A draft can still change under a public feed (DM-26).
        let lifecycle = space_model(repo, id.namespace.as_deref(), space)
            .map(|(model, _)| model.lifecycle)
            .filter(|lifecycle| !lifecycle.backs_surfaces());
        if let Some(lifecycle) = lifecycle {
            for representation in wanted {
                refused.push((
                    location.clone(),
                    format!(
                        "Endpoint {endpoint}: `{representation}` needs a published data model in space {space}; its model is {} (EP-125, DM-26)",
                        lifecycle.as_str()
                    ),
                ));
            }
            continue;
        }
        let linkml = space_linkml(repo_dir, repo, id.namespace.as_deref(), space);
        let gaps = match &linkml {
            None => spec.representation_gaps(None),
            Some(Err(err)) => Err(err.clone()),
            Some(Ok(text)) => spec.representation_gaps(Some(text)),
        };
        let at = format!("Endpoint {endpoint}, space {space}");
        match gaps {
            Ok(gaps) => refused.extend(
                gaps.into_iter()
                    .map(|gap| (location.clone(), format!("{at}: {gap} (EP-125)"))),
            ),
            Err(err) => refused.push((
                location,
                format!(
                    "{at}: the representations cannot be checked against the data model: {err} (EP-125)"
                ),
            )),
        }
    }
    refused
}

/// A space's own data model and its manifest path, a published one first: `None` when the space
/// has none.
fn space_model(
    repo: &Repository,
    namespace: Option<&str>,
    space: &str,
) -> Option<(DataModelSpec, PathBuf)> {
    repo.iter()
        .filter(|(model, _)| model.kind == "DataModel" && model.namespace.as_deref() == namespace)
        .filter_map(|(_, model)| {
            serde_json::from_value::<DataModelSpec>(model.manifest.spec.clone())
                .ok()
                .map(|spec| (spec, model.path.clone()))
        })
        .filter(|(model, _)| {
            model.context_space_ref.as_deref() == Some(space)
                && model.lifecycle != DataModelLifecycle::Mirrored
                && model.validate().is_ok()
        })
        .min_by_key(|(model, _)| !model.lifecycle.backs_surfaces())
}

/// The LinkML source of a space's own data model: `None` when the space has none, `Err` when its
/// file cannot be read.
fn space_linkml(
    repo_dir: &Path,
    repo: &Repository,
    namespace: Option<&str>,
    space: &str,
) -> Option<std::result::Result<String, String>> {
    space_model(repo, namespace, space).map(|(model, path)| {
        let file = repo_dir
            .join(&path)
            .parent()
            .map(|dir| dir.join(&model.linkml))
            .unwrap_or_else(|| repo_dir.join(&model.linkml));
        std::fs::read_to_string(&file).map_err(|err| {
            format!(
                "its LinkML source cannot be read at `{}`: {err}",
                file.display()
            )
        })
    })
}

/// An Endpoint's `spec.display` names only types and slots of its projection: the space's model,
/// narrowed by its `projectionRef` when it has one (EP-135).
fn displays_off_the_projection(repo_dir: &Path, repo: &Repository) -> Vec<(Location, String)> {
    let mut refused = Vec::new();
    for (id, resource) in repo.iter() {
        if id.kind != "Endpoint" {
            continue;
        }
        let Ok(spec) = serde_json::from_value::<EndpointSpec>(resource.manifest.spec.clone())
        else {
            continue;
        };
        if spec.display.is_empty() {
            continue;
        }
        let space = spec.context_space_ref.name();
        let classes = match space_linkml(repo_dir, repo, id.namespace.as_deref(), space) {
            None => Err(format!("space {space} has no data model")),
            Some(linkml) => {
                linkml.and_then(|text| jc_core::kinds::model_projection::linkml_classes(&text))
            }
        };
        let projection = spec.projection_ref.as_ref().and_then(|wanted| {
            repo.iter()
                .find(|(p, _)| {
                    p.kind == "ModelProjection"
                        && p.namespace == id.namespace
                        && p.name == wanted.name
                })
                .and_then(|(_, p)| {
                    serde_json::from_value::<ModelProjectionSpec>(p.manifest.spec.clone()).ok()
                })
        });
        for (class, display) in &spec.display {
            let slots = classes.as_ref().map(|classes| match &projection {
                Some(projection) => projection.attributes_of(class),
                None => classes.get(class).cloned(),
            });
            let problem = match slots {
                Err(err) => format!("{err} (EP-135)"),
                Ok(None) => format!("{class} is no type of its projection (EP-135)"),
                Ok(Some(slots)) => match display.check(class, &slots) {
                    Ok(()) => continue,
                    Err(err) => err,
                },
            };
            refused.push((
                (resource.path.clone(), resource.document, resource.line),
                format!(
                    "Endpoint {}/{}: spec.display: {problem}",
                    id.namespace.as_deref().unwrap_or_default(),
                    id.name
                ),
            ));
        }
    }
    refused
}

/// A Context Space has one data model, and names it (DM-61, DM-62, ADR-N-033).
///
/// Refused: a second model of one space, a `dataModelRef` that names no model of that space,
/// and a LinkML import that reaches out of the model's folder into another space's or project's
/// files, which is neither read-only nor pinned. Warned, while a repository is being migrated: a
/// space that names no model. A mirrored model is a peer's read-only copy (DM-48), not one of
/// the space's own, so it is not counted.
fn one_model_per_space(repo_dir: &Path, repo: &Repository) -> (Located, Located) {
    let (mut refused, mut warned) = (Vec::new(), Vec::new());
    let mut models: BTreeMap<(Option<String>, String), Vec<String>> = BTreeMap::new();
    for (id, resource) in repo.iter() {
        if id.kind != "DataModel" {
            continue;
        }
        // A model the typed parse refused is already a finding above.
        let Ok(spec) = serde_json::from_value::<DataModelSpec>(resource.manifest.spec.clone())
        else {
            continue;
        };
        if spec.lifecycle == DataModelLifecycle::Mirrored {
            continue;
        }
        let location = (resource.path.clone(), resource.document, resource.line);
        // An organization model and a project model no space owns count for no space (DM-75).
        if let Some(space) = &spec.context_space_ref {
            let held = models
                .entry((id.namespace.clone(), space.clone()))
                .or_default();
            if let Some(first) = held.first() {
                refused.push((
                    location.clone(),
                    format!(
                        "DataModel `{}` is a second model of space `{space}` beside `{first}`; a \
                         space has one model (DM-61). Merge them into one: jcctl model merge \
                         --repo-dir <repo> --project {} --space {space} (DM-62)",
                        id.name,
                        id.namespace.as_deref().unwrap_or_default(),
                    ),
                ));
            }
            held.push(id.name.clone());
        }
        if spec.validate().is_ok() {
            for import in imports_out_of_the_folder(repo_dir, &resource.path, &spec.linkml) {
                refused.push((
                    location.clone(),
                    format!(
                        "DataModel `{}` imports `{import}` from outside its folder; import \
                         another space's model by its published schema URL at a pinned version, \
                         read-only (DM-61)",
                        id.name
                    ),
                ));
            }
        }
    }

    for (id, resource) in repo.iter() {
        if id.kind != "ContextSpace" {
            continue;
        }
        let Ok(spec) = serde_json::from_value::<ContextSpaceSpec>(resource.manifest.spec.clone())
        else {
            continue;
        };
        let location = (resource.path.clone(), resource.document, resource.line);
        let held = models
            .get(&(id.namespace.clone(), id.name.clone()))
            .map(Vec::as_slice)
            .unwrap_or_default();
        match (&spec.data_model_ref, held) {
            (Some(named), held) if !held.iter().any(|model| model == named.name()) => {
                refused.push((
                    location,
                    format!(
                        "ContextSpace `{}` names DataModel `{}` in spec.dataModelRef, which is not \
                         a model of this space{} (DM-61)",
                        id.name,
                        named.name(),
                        match held.first() {
                            Some(model) => format!("; its model is `{model}`"),
                            None => String::new(),
                        }
                    ),
                ));
            }
            (Some(_), _) => {}
            (None, [model, ..]) => warned.push((
                location,
                format!(
                    "ContextSpace `{}` names no data model; add spec.dataModelRef: \
                     {{ kind: DataModel, name: {model} }} (DM-61)",
                    id.name
                ),
            )),
            (None, []) => warned.push((
                location,
                format!(
                    "ContextSpace `{}` has no data model; create its model and name it in \
                     spec.dataModelRef, so a write of a type it does not declare is refused \
                     (DM-61, DM-62)",
                    id.name
                ),
            )),
        }
    }
    (refused, warned)
}

/// The `imports` of a model's LinkML source that climb out of the model's folder.
///
/// A relative import names a file of the repository; one that leaves the folder reads another
/// space's source as it is today rather than a published version of it. A source that cannot
/// be read or parsed imports nothing here: `jcctl model validate` is what compiles it.
fn imports_out_of_the_folder(repo_dir: &Path, manifest: &Path, linkml: &str) -> Vec<String> {
    let path = repo_dir
        .join(manifest)
        .parent()
        .map(|dir| dir.join(linkml))
        .unwrap_or_else(|| repo_dir.join(linkml));
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let Ok(source) = serde_norway::from_str::<serde_json::Value>(&text) else {
        return Vec::new();
    };
    source
        .get("imports")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(serde_json::Value::as_str)
        .filter(|import| {
            !import.contains("://")
                && (import.starts_with('/') || import.split('/').any(|segment| segment == ".."))
        })
        .map(str::to_owned)
        .collect()
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
