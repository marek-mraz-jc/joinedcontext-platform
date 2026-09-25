//! `jcctl validate --repo-dir <path>` (T-0125, CC-12, MF-09, TS-18).
//!
//! Every manifest is parsed into its typed kind and validated by `jc-core`, which is
//! strictly stronger than checking it against the exported JSON Schema: the typed parse
//! refuses unknown fields, so an inline secret beside a `secretRef` is a parse error, and
//! it then runs the cross-field invariants a schema cannot express. Finally each manifest
//! must sit at the path its kind prescribes (MF-06).

use crate::assemble::{assemble, AssembleError, Assembly, Directories, Resolver};
use crate::loader::{LoadError, Repository};
use jc_core::kinds::{ContextSpaceSpec, DataModelLifecycle, DataModelSpec, ModelProjectionSpec};
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
    check(repo_dir, true)
}

/// Validates an organization of either layout (CC-86): layout 1 where it is, layout 2 as the
/// render the organization repository and the project checkouts `resolver` names assemble to.
/// A project that cannot be checked out, a literal where a parameter exists and a mapping
/// reading an undeclared one are findings like any other (CC-83, CC-88).
pub fn run_assembled(org_dir: &Path, resolver: &dyn Resolver) -> Report {
    let into = scratch("validate");
    let report = match assemble(org_dir, resolver, &into.join("render"), None) {
        Ok(assembly) if assembly.layout == 1 => run(org_dir),
        Ok(assembly) => {
            let mut report = check(&into.join("render"), true);
            let mut found = assembly_findings(&assembly, |path| path.to_path_buf());
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
            match assemble(&into.join("org"), &resolver, &into.join("render"), None) {
                Ok(assembly) => {
                    let local =
                        |path: &Path| path.strip_prefix(&prefix).unwrap_or(path).to_path_buf();
                    let mut report = check(&into.join("render"), false);
                    for finding in report.findings.iter_mut().chain(report.warnings.iter_mut()) {
                        finding.path = local(&finding.path);
                    }
                    let mut found = assembly_findings(&assembly, |path| path.to_path_buf());
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

/// `organization` is false for a project checked alone: its groups, bindings and domain are
/// the organization's, which the check does not have.
fn check(repo_dir: &Path, organization: bool) -> Report {
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

    for (location, message) in stale_projections(repo_dir, &repo) {
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
        .filter(|_| organization)
    {
        report.findings.push(Finding {
            path: location.0,
            document: location.1,
            line: location.2,
            message,
        });
    }

    for (location, message) in app_members_outside_the_domain(&repo)
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

/// Every `subjects[].group` of a `RoleBinding` that no `Group` manifest declares (PF-64).
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

/// The subjects of every role an App's `spec.access` lists (AP-91).
fn app_subjects(spec: &serde_json::Value) -> impl Iterator<Item = &serde_json::Value> {
    spec.get("access")
        .and_then(|access| access.as_array())
        .into_iter()
        .flatten()
        .filter_map(|entry| entry.get("subjects").and_then(|s| s.as_array()))
        .flatten()
}

/// Every App role member whose e-mail is outside the organization's domain (AP-91, PF-41): a
/// role is a grant inside the organization, and an address of another domain is a person
/// nobody here verified.
fn app_members_outside_the_domain(repo: &Repository) -> Vec<(Location, String)> {
    let Some(domain) = repo.org_domain().map(str::to_ascii_lowercase) else {
        return Vec::new();
    };
    let mut findings = Vec::new();
    for (id, resource) in repo.iter() {
        if id.kind != "App" {
            continue;
        }
        for user in app_subjects(&resource.manifest.spec)
            .filter_map(|subject| subject.get("user").and_then(|u| u.as_str()))
        {
            let host = user
                .rsplit_once('@')
                .map(|(_, host)| host.to_ascii_lowercase());
            let inside =
                host.is_some_and(|host| host == domain || host.ends_with(&format!(".{domain}")));
            if !inside {
                findings.push((
                    (resource.path.clone(), resource.document, resource.line),
                    format!(
                        "{id} gives a role to `{user}`, outside the organization's domain \
                         `{domain}`: a role member is a person of this organization (AP-91, PF-41)"
                    ),
                ));
            }
        }
    }
    findings
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
