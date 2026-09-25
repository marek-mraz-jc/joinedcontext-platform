//! Building the endpoint table from the manifest repository (T-0005, EP-01, CC-08).
//!
//! The repository is the configuration, so the gateway's table is a projection of it and
//! nothing else: an `Endpoint` manifest, plus every `Policy` of the same project that
//! names the same context space — or the one Policy its `spec.policyRef` names, when several
//! endpoints publish different slices of one space. A manifest the gateway cannot make sense of is left out
//! rather than half-applied — an endpoint that is not in the table answers 404, which is
//! the same thing an endpoint that does not exist answers (EP-03).

use crate::auth::accounts::{accounts_of, ServiceAccounts};
use crate::auth::dataspace_token::{agreements_of, Agreements};
use crate::federation::{federations_of, Federations};
use crate::relationships::RelationshipRules;
use crate::resolver::{DeclaredTypes, Endpoint, EndpointRoles, Model, Space};
use crate::translators::view_mapping::ViewMapping;
use crate::units::UnitRules;
use jc_core::kinds::{
    Audience, ContextSpaceSpec, DataModelLifecycle, DataModelSpec, EndpointSpec, MappingSpec,
    ModelProjectionSpec, PolicySpec, Representation,
};
use jc_core::Urn;
use jcctl::loader::{RawManifest, Repository};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// The tables the gateway serves, in the order [`load`] returns them.
pub type Tables = (
    Vec<Endpoint>,
    Vec<Space>,
    ServiceAccounts,
    Federations,
    Agreements,
);

/// Where a layout 2 organization's projects are read and assembled (CC-86, ADR-N-029).
///
/// `projects` holds one checkout per registry slug at its pinned ref, which the deployment
/// mounts; the gateway reads them and never the forge, so it holds no forge credential.
/// `assembly` is a scratch directory the pod owns, outside both, where the render is written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Checkouts {
    /// The project checkouts, one directory per slug (`JC_GATEWAY_PROJECTS_DIR`).
    pub projects: PathBuf,
    /// Where the organization is assembled (`JC_GATEWAY_ASSEMBLY_DIR`).
    pub assembly: PathBuf,
}

/// Loads the endpoint table and the identity table from the repository under `dir`
/// (CC-08, PF-46).
pub fn load(dir: &Path) -> Result<Tables, jcctl::LoadError> {
    let repo = Repository::load(dir)?;
    Ok(tables(&repo, dir))
}

/// [`load`] of the organization checked out at `dir`: a layout 1 repository as it is, a
/// layout 2 one assembled with the project checkouts of `checkouts` at their refs (CC-86). A
/// project whose checkout is missing keeps the render the last assembly gave it; a name two
/// projects claim refuses the whole load, so the tables that are serving keep serving.
pub fn load_from(
    dir: &Path,
    checkouts: Option<&Checkouts>,
) -> Result<Tables, jcctl::assemble::AssembleError> {
    use jcctl::assemble::{assemble, layout_at, AssembleError, Directories};
    if layout_at(dir, jc_core::project::RepositoryRole::Organization)? == 1 {
        return Ok(load(dir)?);
    }
    let Some(checkouts) = checkouts else {
        return Err(AssembleError::Refused {
            path: jc_core::project::LAYOUT_FILE.to_owned(),
            message: "the organization is layout 2 and no project checkouts were given \
                      (JC_GATEWAY_PROJECTS_DIR)"
                .to_owned(),
        });
    };
    let listing = std::fs::read_dir(&checkouts.projects).map_err(|source| AssembleError::Io {
        path: checkouts.projects.clone(),
        source,
    })?;
    let directories = listing
        .flatten()
        .filter(|entry| entry.path().is_dir())
        .filter_map(|entry| {
            let slug = entry.file_name().into_string().ok()?;
            (!slug.starts_with('.')).then(|| (slug, entry.path()))
        })
        .collect();
    let environment = std::env::var("JC_ENVIRONMENT")
        .ok()
        .filter(|value| !value.trim().is_empty());
    let assembly = assemble(
        dir,
        &Directories(directories),
        &checkouts.assembly,
        environment.as_deref(),
    )?;
    for entry in assembly
        .entries
        .iter()
        .filter(|entry| entry.error.is_some())
    {
        tracing::warn!(
            project = %entry.slug,
            git_ref = %entry.git_ref,
            error = entry.error.as_deref().unwrap_or_default(),
            kept = entry.rendered,
            "a registered project has no checkout"
        );
    }
    Ok(tables(&assembly.repository, &checkouts.assembly))
}

fn tables(repo: &Repository, root: &Path) -> Tables {
    (
        endpoints_with_models(repo, Some(root)),
        spaces_of(repo, Some(root)),
        accounts_of(repo),
        federations_of(repo),
        agreements_of(repo),
    )
}

/// [`load_from`], plus every preview under `previews` rendered with its prefix (CC-78). A preview
/// that does not render is left out with the reason in the log; a slug or a tenant `main`
/// already serves is never taken over by one, so a preview answers beside `main` and never in
/// its place (PF-83, T-1709).
pub fn load_with_previews(
    dir: &Path,
    checkouts: Option<&Checkouts>,
    previews: Option<&Path>,
) -> Result<Tables, jcctl::assemble::AssembleError> {
    let (mut endpoints, mut spaces, accounts, federations, agreements) = load_from(dir, checkouts)?;
    let environment = std::env::var("JC_ENVIRONMENT")
        .ok()
        .filter(|value| !value.trim().is_empty());
    for (prefix, root) in previews.map(crate::previews::listed).unwrap_or_default() {
        let repo = match Repository::load_preview(&root, environment.as_deref(), &prefix) {
            Ok(repo) => repo,
            Err(error) => {
                tracing::warn!(%prefix, %error, "the preview does not render and is not served");
                continue;
            }
        };
        let slugs: BTreeSet<String> = endpoints.iter().map(|e| e.slug.clone()).collect();
        // Every tenant already answered for, whether a space manifest names it or an endpoint
        // alone reaches it: a preview record on one of them would read and write data of the
        // project the copy came from, which is the one thing a preview may never do (PF-83).
        let segments: BTreeSet<String> = spaces
            .iter()
            .map(|s| s.endpoint.space.clone())
            .chain(endpoints.iter().map(|e| e.space.clone()))
            .collect();
        endpoints.extend(
            endpoints_with_models(&repo, Some(&root))
                .into_iter()
                .filter(|endpoint| {
                    !slugs.contains(&endpoint.slug) && !segments.contains(&endpoint.space)
                }),
        );
        spaces.extend(
            spaces_of(&repo, Some(&root))
                .into_iter()
                .filter(|space| !segments.contains(&space.endpoint.space)),
        );
    }
    Ok((endpoints, spaces, accounts, federations, agreements))
}

/// The endpoint table a loaded repository describes.
///
/// Deterministic: the repository is indexed by resource identity, so two runs on the same
/// commit build the same table in the same order (CC-27).
pub fn endpoints_of(repo: &Repository) -> Vec<Endpoint> {
    endpoints_with_models(repo, None)
}

/// The client `app-{name}` when Endpoint `endpoint` of `project` is the one the Portal generates
/// for the App `{name}` declared in that project (ADR-N-030, AP-97).
fn app_of(repo: &Repository, project: &str, endpoint: &str) -> Option<String> {
    let app = endpoint.strip_prefix("app-")?;
    repo.iter()
        .any(|(id, _)| {
            id.kind == "App" && id.name == app && id.namespace.as_deref() == Some(project)
        })
        .then(|| endpoint.to_owned())
}

/// The endpoint table, with the model artifacts read from `root` when one is given.
///
/// `DataModel.spec.artifacts` names files committed beside the LinkML source, so the
/// schema surface publishes what was reviewed rather than what a compiler produces now
/// (DM-02, EP-46).
pub fn endpoints_with_models(repo: &Repository, root: Option<&Path>) -> Vec<Endpoint> {
    let policies = policies_by_space(repo);
    let models = models_by_space(repo, root);
    let declared = declared_types(repo, &models);
    let views = view_mappings(repo, root);
    let projections = projections(repo);

    let mut endpoints = Vec::new();
    for (id, resource) in repo.iter() {
        if id.kind != "Endpoint" {
            continue;
        }
        let Some(spec) = spec_of::<EndpointSpec>(&resource.manifest) else {
            continue;
        };
        let project = id.namespace.clone().unwrap_or_default();
        let space_name = spec.context_space_ref.name().to_owned();
        // Tenant, entity ids and policy URNs all carry the rendered segment (PF-84); the
        // manifests beside the endpoint name the space locally.
        let space = repo.space_segment(&project, &space_name);
        let key = (project.clone(), space_name.clone());
        let named = policies.get(&key).map(Vec::as_slice).unwrap_or_default();
        let bound = match &spec.policy_ref {
            Some(policy_ref) => bound_policy(&id.name, policy_ref, &space, named),
            None => named.iter().map(|(_, spec)| spec.clone()).collect(),
        };
        let projection = spec.projection_ref.as_ref().and_then(|reference| {
            let named = (
                reference
                    .namespace
                    .clone()
                    .unwrap_or_else(|| project.clone()),
                reference.name.clone(),
            );
            match projections.get(&named) {
                Some(projection) if projection.context_space_ref == space_name => {
                    Some(Arc::clone(projection))
                }
                Some(_) => {
                    // A projection of another space's model would describe entities this
                    // endpoint never serves: the endpoint stays, unprojected, and says so.
                    tracing::warn!(
                        endpoint = %id.name,
                        projection = %reference.name,
                        "the projection belongs to another space and is not applied"
                    );
                    None
                }
                None => {
                    tracing::warn!(
                        endpoint = %id.name,
                        projection = %reference.name,
                        "projectionRef names no ModelProjection; the endpoint is not narrowed by it"
                    );
                    None
                }
            }
        });
        let representations = spec.served_representations();
        endpoints.push(Endpoint {
            roles: match app_of(repo, &project, &id.name) {
                Some(client) => EndpointRoles::of_app(&project, &id.name, &spec, client),
                None => EndpointRoles::of(&project, &id.name, &spec),
            },
            slug: spec.slug.to_string(),
            title: language_map(&resource.manifest.metadata.rest, "title"),
            description: language_map(&resource.manifest.metadata.rest, "description"),
            policies: bound,
            models: models.get(&key).cloned().unwrap_or_default(),
            declared_types: declared.get(&key).cloned(),
            space,
            project,
            audience: spec.audience,
            allowed_projects: spec.allowed_projects,
            representations,
            rate_limit: spec.rate_limits,
            file_limits: spec.file_limits,
            hidden_attributes: spec
                .projection
                .map(|projection| projection.hidden_attributes.into_iter().collect())
                .unwrap_or_default(),
            projection,
            base_path: format!("/api/endpoint/{}", spec.slug),
            catalog: spec.catalog.map(Arc::new),
            view_mapping: spec.view_mapping_ref.and_then(|view| {
                let named = (
                    view.namespace
                        .clone()
                        .unwrap_or_else(|| id.namespace.clone().unwrap_or_default()),
                    view.name.clone(),
                );
                match views.get(&named) {
                    Some(mapping) => Some(Arc::clone(mapping)),
                    None => {
                        // Serving the source model under the target model's name is worse
                        // than not serving a view: the endpoint stays, as the space's own.
                        tracing::warn!(
                            endpoint = %id.name,
                            mapping = %view.name,
                            "the view mapping names no Mapping with a readable gateway IR"
                        );
                        None
                    }
                }
            }),
        });
    }
    endpoints
}

/// Every `ModelProjection` of the repository, by project and name (MP-01).
fn projections(repo: &Repository) -> BTreeMap<(String, String), Arc<ModelProjectionSpec>> {
    let mut projections = BTreeMap::new();
    for (id, resource) in repo.iter() {
        if id.kind != "ModelProjection" {
            continue;
        }
        if let Some(spec) = spec_of::<ModelProjectionSpec>(&resource.manifest) {
            projections.insert(
                (id.namespace.clone().unwrap_or_default(), id.name.clone()),
                Arc::new(spec),
            );
        }
    }
    projections
}

/// The compiled mapping IR of every Mapping that carries one, by project and name (DM-52).
///
/// A Mapping without `spec.artifacts.gatewayIr`, or one whose IR this build cannot
/// interpret, is left out: an endpoint that names it then serves the space's own model and
/// says so in the log, rather than serving source data under target names.
fn view_mappings(
    repo: &Repository,
    root: Option<&Path>,
) -> BTreeMap<(String, String), Arc<ViewMapping>> {
    let mut mappings = BTreeMap::new();
    for (id, resource) in repo.iter() {
        if id.kind != "Mapping" {
            continue;
        }
        let Some(spec) = spec_of::<MappingSpec>(&resource.manifest) else {
            continue;
        };
        let Some(document) = beside(root, &resource.path, &spec.artifacts.gateway_ir, &id.name)
        else {
            continue;
        };
        match ViewMapping::parse(&document) {
            Ok(mapping) => {
                mappings.insert(
                    (id.namespace.clone().unwrap_or_default(), id.name.clone()),
                    Arc::new(mapping),
                );
            }
            Err(error) => {
                tracing::warn!(mapping = %id.name, %error, "the gateway mapping IR is not usable")
            }
        }
    }
    mappings
}

/// The space table a loaded repository describes (SP-01, SP-10).
///
/// A space is served through the same enforcement record an endpoint is, built from the
/// same policy set: `/cs/{space}/ngsi-ld/v1/` and an endpoint on the same space evaluate
/// the same grants because they read the same `Vec<PolicySpec>`. The audience is `public`
/// so that the decision belongs to the policy set alone: a space with no grant for the
/// caller answers 404, which is what a space that does not exist answers (SP-06, SP-11).
pub fn spaces_of(repo: &Repository, root: Option<&Path>) -> Vec<Space> {
    let policies = policies_by_space(repo);
    let models = models_by_space(repo, root);
    let declared = declared_types(repo, &models);

    let mut spaces = Vec::new();
    for (id, resource) in repo.iter() {
        if id.kind != "ContextSpace" {
            continue;
        }
        let Some(spec) = spec_of::<ContextSpaceSpec>(&resource.manifest) else {
            continue;
        };
        let project = id.namespace.clone().unwrap_or_default();
        let key = (project.clone(), id.name.clone());
        let segment = repo.space_segment(&project, &id.name);
        spaces.push(Space {
            endpoint: Arc::new(Endpoint {
                roles: Default::default(),
                slug: segment.clone(),
                // The canonical surface is the space itself, so its record reads the
                // space's own title and description below.
                title: BTreeMap::new(),
                description: BTreeMap::new(),
                space: segment.clone(),
                project,
                audience: Audience::Public,
                allowed_projects: Vec::new(),
                // The dump is a space's own child (SP-04, SP-13): the file bundle over what the
                // caller's grants read, generated per request (T-2391).
                representations: vec![
                    Representation::NgsiLd,
                    Representation::Mcp,
                    Representation::Zip,
                ],
                // No limit of its own: the manifest has no field for one, and the limiter
                // counts the canonical surface in its own default bucket (T-0813).
                rate_limit: None,
                file_limits: None,
                // A space is the whole space: narrowing is a decision of a published
                // endpoint, and the canonical surface publishes nothing of its own.
                hidden_attributes: BTreeSet::new(),
                projection: None,
                policies: policies
                    .get(&key)
                    .map(|named| named.iter().map(|(_, spec)| spec.clone()).collect())
                    .unwrap_or_default(),
                models: models.get(&key).cloned().unwrap_or_default(),
                declared_types: declared.get(&key).cloned(),
                // A space's canonical surface serves the space's own model; a view is a
                // decision of a published endpoint (SP-01, EP-54).
                view_mapping: None,
                catalog: None,
                base_path: format!("/cs/{segment}"),
            }),
            title: language_map(&resource.manifest.metadata.rest, "title"),
            description: language_map(&resource.manifest.metadata.rest, "description"),
            is_sandbox: spec.is_sandbox,
            default_locale: spec.default_locale,
        });
    }
    spaces
}

/// One `metadata` text as a language map, or an empty one when the manifest carries none
/// (PF-24, UI-50).
///
/// The loader keeps metadata beyond name and namespace as raw JSON, so this reads the
/// shape rather than a type. A plain string, the form every new manifest writes, belongs
/// to no locale and is kept under the empty key; the legacy `{locale: text}` map is kept
/// as it is. Anything else is a manifest the Portal would have rejected, and here it
/// simply describes nothing.
fn language_map(
    metadata: &serde_json::Map<String, serde_json::Value>,
    key: &str,
) -> BTreeMap<String, String> {
    match metadata.get(key) {
        Some(serde_json::Value::String(text)) if !text.is_empty() => {
            BTreeMap::from([(String::new(), text.clone())])
        }
        Some(serde_json::Value::Object(map)) => map
            .iter()
            .filter_map(|(locale, text)| Some((locale.clone(), text.as_str()?.to_owned())))
            .collect(),
        _ => BTreeMap::new(),
    }
}

/// The policies of every space, keyed by the project and space they name (GW8).
/// Every Policy of every space, by project and space, with the manifest name it is bound
/// to by `spec.policyRef` (`urn:ngsi-ld:Policy:{org}:{space}:{name}`).
#[allow(clippy::type_complexity)]
fn policies_by_space(repo: &Repository) -> BTreeMap<(String, String), Vec<(String, PolicySpec)>> {
    let mut policies: BTreeMap<(String, String), Vec<(String, PolicySpec)>> = BTreeMap::new();
    for (id, resource) in repo.iter() {
        if id.kind != "Policy" {
            continue;
        }
        if let Some(spec) = spec_of::<PolicySpec>(&resource.manifest) {
            let project = id.namespace.clone().unwrap_or_default();
            let space = spec.context_space_ref.name().to_owned();
            policies
                .entry((project, space))
                .or_default()
                .push((id.name.clone(), spec));
        }
    }
    policies
}

/// The one Policy an endpoint with `spec.policyRef` evaluates (EP-14, GW8).
///
/// Without a reference an endpoint evaluates every Policy of its space; with one it evaluates
/// that Policy alone, which is how several endpoints over one space each publish a different
/// slice of it. A reference to a Policy of another space, or to none the repository has, binds
/// nothing and says so: an endpoint that widened to the whole space instead would publish
/// exactly what its author narrowed away.
fn bound_policy(
    endpoint: &str,
    policy_ref: &Urn,
    space: &str,
    named: &[(String, PolicySpec)],
) -> Vec<PolicySpec> {
    let bound: Vec<PolicySpec> = named
        .iter()
        .filter(|(name, _)| policy_ref.space() == space && name == policy_ref.local_id())
        .map(|(_, spec)| spec.clone())
        .collect();
    if bound.is_empty() {
        tracing::warn!(
            endpoint,
            policy = %policy_ref,
            "policyRef names no Policy of the endpoint's space; the endpoint grants nothing"
        );
    }
    bound
}

/// The data models of every space, with the artifacts the checkout carries (DM-02).
///
/// A mirrored foreign model is left out: it is a read-only copy of what a peer publishes, and
/// an endpoint that served it under its own schema surface would be claiming another
/// organisation's model as its own, which is exactly what DM-49 forbids. The reconciler
/// commits such a model beside the reference that fetched it, and the Mappings that read it
/// are how it reaches local data (DM-48, DM-49).
fn models_by_space(
    repo: &Repository,
    root: Option<&Path>,
) -> BTreeMap<(String, String), Vec<Model>> {
    let mut models: BTreeMap<(String, String), Vec<Model>> = BTreeMap::new();
    for (id, resource) in repo.iter() {
        if id.kind != "DataModel" {
            continue;
        }
        if let Some(spec) = spec_of::<DataModelSpec>(&resource.manifest) {
            if spec.lifecycle == DataModelLifecycle::Mirrored {
                continue;
            }
            // A model no space owns is served by no space; a space that imports it carries its
            // classes in its own compiled artifacts (DM-75, DM-76).
            let Some(space) = spec.context_space_ref.clone() else {
                continue;
            };
            let project = id.namespace.clone().unwrap_or_default();
            models.entry((project, space)).or_default().push(model_of(
                &id.name,
                &spec,
                root,
                &resource.path,
            ));
        }
    }
    models
}

/// The classes of every space's one model, for the spaces that name it (DM-61).
///
/// A space that names a model its repository does not hold declares nothing here and says so:
/// `jcctl validate` refuses that before a commit, and a gateway that refused every write of the
/// space over it would take a working surface down for a reference.
fn declared_types(
    repo: &Repository,
    models: &BTreeMap<(String, String), Vec<Model>>,
) -> BTreeMap<(String, String), DeclaredTypes> {
    let mut declared = BTreeMap::new();
    for (id, resource) in repo.iter() {
        if id.kind != "ContextSpace" {
            continue;
        }
        let Some(space) = spec_of::<ContextSpaceSpec>(&resource.manifest) else {
            continue;
        };
        let Some(named) = space.data_model_ref else {
            continue;
        };
        let key = (id.namespace.clone().unwrap_or_default(), id.name.clone());
        let model = models
            .get(&key)
            .and_then(|held| held.iter().find(|model| model.name == named.name()));
        match model {
            Some(model) => {
                declared.insert(
                    key,
                    DeclaredTypes {
                        model: model.name.clone(),
                        classes: model.classes.iter().cloned().collect(),
                        units: model
                            .json_schema
                            .as_ref()
                            .map(|schema| UnitRules::from_schema(schema, space.missing_unit_code))
                            .unwrap_or_default(),
                        relationships: model
                            .json_schema
                            .as_ref()
                            .map(RelationshipRules::from_schema)
                            .unwrap_or_default(),
                    },
                );
            }
            None => tracing::warn!(
                space = %id.name,
                model = %named.name(),
                "spec.dataModelRef names no model of the space; its writes are not narrowed to one"
            ),
        }
    }
    declared
}

/// Deserializes one manifest's `spec`, discarding a manifest the gateway cannot read.
///
/// The Portal and `jcctl validate` reject a malformed manifest before it is ever
/// committed; a gateway that refused to start over one would take the whole surface down
/// for a resource it does not serve.
fn spec_of<T: serde::de::DeserializeOwned>(manifest: &RawManifest) -> Option<T> {
    match serde_json::from_value(manifest.spec.clone()) {
        Ok(spec) => Some(spec),
        Err(error) => {
            tracing::warn!(
                kind = %manifest.kind,
                name = %manifest.metadata.name,
                %error,
                "manifest left out of the endpoint table"
            );
            None
        }
    }
}

/// One data model, with whatever artifacts the checkout carries beside its manifest.
///
/// An artifact that is missing or unreadable is simply absent: the schema surface falls
/// back to deriving the document from the endpoint's grants, which is a narrower answer
/// but never a wrong one.
/// One JSON artifact committed beside its manifest (DM-02, DM-52).
///
/// The artifacts live beside their manifest; a path that climbs out of the repository is a
/// manifest bug and reads nothing.
fn beside(
    root: Option<&Path>,
    manifest_path: &Path,
    relative: &Option<String>,
    owner: &str,
) -> Option<serde_json::Value> {
    let (root, relative) = (root?, relative.as_deref()?);
    let directory = manifest_path.parent().unwrap_or(Path::new(""));
    let path = normalize(&root.join(directory).join(relative))?;
    if !path.starts_with(root) {
        tracing::warn!(owner, "artifact path leaves the repository");
        return None;
    }
    match std::fs::read_to_string(&path) {
        Ok(text) => serde_json::from_str(&text).ok(),
        Err(error) => {
            tracing::warn!(owner, path = %path.display(), %error, "artifact not read");
            None
        }
    }
}

fn model_of(name: &str, spec: &DataModelSpec, root: Option<&Path>, manifest_path: &Path) -> Model {
    let beside = |relative: &Option<String>| beside(root, manifest_path, relative, name);

    Model {
        name: name.to_owned(),
        version: spec.version.to_string(),
        major: spec.version.major(),
        classes: spec.classes.clone(),
        json_schema: beside(&spec.artifacts.json_schema),
        context: beside(&spec.artifacts.context),
    }
}

/// Resolves `.` and `..` without touching the filesystem, so a path that escapes the
/// repository is visible before anything is opened.
fn normalize(path: &Path) -> Option<std::path::PathBuf> {
    let mut out = std::path::PathBuf::new();
    for part in path.components() {
        match part {
            std::path::Component::ParentDir => {
                if !out.pop() {
                    return None;
                }
            }
            std::path::Component::CurDir => {}
            other => out.push(other),
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::{language_map, normalize};
    use serde_json::json;
    use std::collections::BTreeMap;
    use std::path::{Path, PathBuf};

    /// A plain title reads as text of no locale, the legacy map as its locales, and a
    /// shape no manifest may carry as nothing (UI-50).
    #[test]
    fn a_plain_title_and_the_legacy_map_both_read() {
        let metadata = |title: serde_json::Value| {
            json!({ "title": title })
                .as_object()
                .cloned()
                .expect("object")
        };
        assert_eq!(
            language_map(&metadata(json!("Air quality")), "title"),
            BTreeMap::from([(String::new(), "Air quality".to_owned())])
        );
        assert_eq!(
            language_map(
                &metadata(json!({ "fi": "Ilmanlaatu", "en": "Air" })),
                "title"
            ),
            BTreeMap::from([
                ("en".to_owned(), "Air".to_owned()),
                ("fi".to_owned(), "Ilmanlaatu".to_owned())
            ])
        );
        assert!(language_map(&metadata(json!("")), "title").is_empty());
        assert!(language_map(&metadata(json!(7)), "title").is_empty());
        assert!(language_map(&metadata(json!({})), "description").is_empty());
    }

    /// An artifact path is checked before anything is opened, so a manifest that names
    /// `../../.secrets/x` is visible as leaving the repository rather than being read.
    #[test]
    fn normalize_resolves_traversal_without_touching_the_filesystem() {
        let root = Path::new("/repo");
        assert_eq!(
            normalize(&root.join("projects/a/./model.schema.json")),
            Some(PathBuf::from("/repo/projects/a/model.schema.json"))
        );
        assert_eq!(
            normalize(&root.join("projects/a/../b/model.schema.json")),
            Some(PathBuf::from("/repo/projects/b/model.schema.json"))
        );

        // Two ways out of the repository, and neither is readable: one resolves to a
        // path outside it, the other climbs past the filesystem root and resolves to
        // nothing at all.
        let escaped = normalize(&root.join("projects/../../etc/passwd")).expect("resolves");
        assert_eq!(escaped, PathBuf::from("/etc/passwd"));
        assert!(!escaped.starts_with(root), "{}", escaped.display());
        assert_eq!(normalize(&root.join("../../etc/passwd")), None);
    }
}
