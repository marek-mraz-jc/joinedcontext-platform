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
}

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
    ServiceAccounts { by_client_id }
}
