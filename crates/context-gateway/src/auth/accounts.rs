//! Mapping a token's `azp` to the `ServiceAccount` that owns it (T-0228, PF-46).
//!
//! The Keycloak client id is derived from the manifest, never chosen: `{project}-{name}`,
//! both DNS-1123 labels, so it is the same string in Keycloak, in the repository and in
//! the audit log, and it survives credential rotation (Architecture/12 section 3).
//!
//! An `azp` the repository does not name resolves to nothing, and a caller that resolves
//! to nothing has no grants: a token can be perfectly valid and still belong to an account
//! this platform has never heard of.

use jc_core::kinds::ServiceAccountSpec;
use jcctl::loader::Repository;
use std::collections::{BTreeSet, HashMap};

/// The Keycloak client id of a service account (Architecture/12 section 3).
pub use jc_core::kinds::service_account::keycloak_client_id as client_id;

/// What the gateway needs to know about one service account.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Account {
    /// The manifest name, which is what a `Policy` names as its assignee.
    pub name: String,
    /// The project the account belongs to.
    pub project: String,
    /// The account's role bindings, kept with their scope so only the ones that reach the
    /// endpoint being called are handed to the PDP.
    pub roles: Vec<ScopedRole>,
    /// Whether a person's token its client exchanged is decided as that person (ADR-N-038,
    /// AG-95).
    pub delegates: bool,
}

/// One role and where it applies.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScopedRole {
    /// The role name.
    pub role: String,
    /// The context space it is scoped to, if it is scoped to one.
    pub context_space: Option<String>,
    /// The project it is scoped to, if it is scoped to one.
    pub project: Option<String>,
    /// Whether it is scoped to the whole organization.
    pub organization: bool,
}

impl Account {
    /// The roles that reach one space of one project.
    ///
    /// A role scoped to another space is not a role here: that is the whole point of
    /// scoping it (PF-35).
    pub fn roles_in(&self, project: &str, space: &str) -> BTreeSet<String> {
        self.roles
            .iter()
            .filter(|scoped| {
                scoped.organization
                    || scoped.project.as_deref() == Some(project)
                    || scoped.context_space.as_deref() == Some(space)
            })
            .map(|scoped| scoped.role.clone())
            .collect()
    }
}

/// The service accounts of a repository, indexed by Keycloak client id.
#[derive(Debug, Clone, Default)]
pub struct ServiceAccounts {
    by_client_id: HashMap<String, Account>,
    /// The slugs each App's client `app-{name}` is admitted on (AP-113).
    apps: HashMap<String, BTreeSet<String>>,
}

/// Every App's Keycloak client is `app-{name}` (ADR-N-030, AP-14a).
pub const APP_CLIENT_PREFIX: &str = "app-";

impl ServiceAccounts {
    /// An empty table: every `azp` resolves to nothing.
    pub fn new() -> Self {
        Self::default()
    }

    /// The account a token's `azp` names, if the repository names it (PF-46).
    pub fn resolve(&self, azp: &str) -> Option<&Account> {
        self.by_client_id.get(azp)
    }

    /// How many accounts the table holds.
    pub fn len(&self) -> usize {
        self.by_client_id.len()
    }

    /// Whether the table is empty.
    pub fn is_empty(&self) -> bool {
        self.by_client_id.is_empty()
    }

    /// The table with the App client `client` admitted on the Endpoints `slugs` (AP-113).
    pub fn with_app(
        mut self,
        client: impl Into<String>,
        slugs: impl IntoIterator<Item = impl Into<String>>,
    ) -> Self {
        self.apps
            .insert(client.into(), slugs.into_iter().map(Into::into).collect());
        self
    }

    /// Whether a token whose `azp` is `azp` may be admitted on the Endpoint `slug` (AP-113): an
    /// App's client only on the Endpoints that App reads, and an `app-*` client that neither an
    /// App nor a ServiceAccount of the repository names on none. Every other client is no App's
    /// and is not narrowed here.
    pub fn admits_on(&self, azp: &str, slug: &str) -> bool {
        match self.apps.get(azp) {
            Some(slugs) => slugs.contains(slug),
            None => !azp.starts_with(APP_CLIENT_PREFIX) || self.by_client_id.contains_key(azp),
        }
    }
}

/// Builds the table from a loaded repository.
///
/// The hyphen that joins project and name is also a character of both, so two accounts can derive
/// one client id (`helsinki` + `kpi-writer`, `helsinki-kpi` + `writer`). Such an id resolves to
/// nobody rather than to whichever account was read last: a token is never handed another
/// project's roles (T-1454).
pub fn accounts_of(repo: &Repository) -> ServiceAccounts {
    let mut by_client_id = HashMap::new();
    let mut ambiguous = std::collections::BTreeSet::new();
    for (id, resource) in repo.iter() {
        if id.kind != "ServiceAccount" {
            continue;
        }
        let Ok(spec) = serde_json::from_value::<ServiceAccountSpec>(resource.manifest.spec.clone())
        else {
            tracing::warn!(name = %id.name, "service account left out of the identity table");
            continue;
        };
        let project = id.namespace.clone().unwrap_or_default();
        let client = client_id(&project, &id.name);
        if ambiguous.contains(&client) {
            continue;
        }
        if by_client_id.contains_key(&client) {
            tracing::warn!(client = %client, "two service accounts derive one client id; it resolves to nobody");
            by_client_id.remove(&client);
            ambiguous.insert(client);
            continue;
        }
        by_client_id.insert(
            client,
            Account {
                name: id.name.clone(),
                project: project.clone(),
                roles: spec
                    .roles
                    .iter()
                    .map(|binding| ScopedRole {
                        role: binding.role.clone(),
                        // The gateway knows a space by its segment (PF-84).
                        context_space: binding
                            .scope
                            .context_space
                            .as_deref()
                            .map(|name| repo.space_segment(&project, name)),
                        project: binding.scope.project.clone(),
                        organization: binding.scope.organization.is_some(),
                    })
                    .collect(),
                delegates: spec.delegation
                    == Some(jc_core::kinds::service_account::Delegation::TokenExchange),
            },
        );
    }
    ServiceAccounts {
        by_client_id,
        apps: apps_of(repo),
    }
}

/// The slugs each App's client is admitted on, by the one rule the Portal gives its client's
/// audiences with (jc-core `served_endpoints`, AP-113). Slugs are the Endpoints' own, not a
/// preview's, so a token of an App reaches the Endpoints of `main` alone.
fn apps_of(repo: &Repository) -> HashMap<String, BTreeSet<String>> {
    enum Target {
        Named(String, String),
        Slug(String),
    }

    use jc_core::kinds::app::{served_endpoints, EndpointFact, ReferenceFact};
    use jc_core::kinds::{AppSpec, EndpointSpec, SharedSpaceReferenceSpec};

    let mut endpoints = Vec::new();
    let mut references = Vec::new();
    let mut apps = Vec::new();
    for (id, resource) in repo.iter() {
        let project = id.namespace.clone().unwrap_or_default();
        let spec = resource.manifest.spec.clone();
        match id.kind.as_str() {
            "Endpoint" => {
                let Ok(spec) = serde_json::from_value::<EndpointSpec>(spec) else {
                    continue;
                };
                endpoints.push(EndpointFact {
                    project,
                    name: id.name.clone(),
                    slug: spec.slug.as_str().to_owned(),
                    space: spec.context_space_ref.name().to_owned(),
                    generated_by: resource
                        .manifest
                        .metadata
                        .rest
                        .get("annotations")
                        .and_then(|annotations| annotations.get(jc_core::annotations::GENERATED_BY))
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_owned),
                    public: spec.audience == jc_core::kinds::endpoint::Audience::Public,
                });
            }
            "SharedSpaceReference" => {
                let Ok(spec) = serde_json::from_value::<SharedSpaceReferenceSpec>(spec) else {
                    continue;
                };
                // The loader renders an `endpointRef` it resolved as that Endpoint's slug (EP-77),
                // so the reference names its target either way.
                let target = match (spec.endpoint_ref, spec.endpoint_slug) {
                    (Some(target), _) => Target::Named(target.project, target.name),
                    (None, Some(slug)) => Target::Slug(slug.as_str().to_owned()),
                    (None, None) => continue,
                };
                references.push((project, id.name.clone(), target));
            }
            "App" => match serde_json::from_value::<AppSpec>(spec) {
                Ok(spec) => apps.push((project, id.name.clone(), spec)),
                // An App that does not parse has no client the reconciler keeps, so its client
                // resolves to nothing and is admitted nowhere.
                Err(error) => {
                    tracing::warn!(app = %id.name, %error, "App left out of the audience table")
                }
            },
            _ => {}
        }
    }
    let references: Vec<ReferenceFact> = references
        .into_iter()
        .filter_map(|(project, name, target)| {
            let (source_project, endpoint) = match target {
                Target::Named(project, name) => (project, name),
                Target::Slug(slug) => endpoints
                    .iter()
                    .find(|endpoint| endpoint.slug == slug)
                    .map(|endpoint| (endpoint.project.clone(), endpoint.name.clone()))?,
            };
            Some(ReferenceFact {
                project,
                name,
                source_project,
                endpoint,
            })
        })
        .collect();
    apps.into_iter()
        .map(|(project, name, spec)| {
            let slugs = served_endpoints(&project, &name, &spec, &endpoints, &references)
                .into_iter()
                .map(|endpoint| endpoint.slug.clone())
                .collect();
            (format!("{APP_CLIENT_PREFIX}{name}"), slugs)
        })
        .collect()
}
