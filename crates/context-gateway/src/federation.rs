//! What the gateway knows about a space's registrations (T-0345, EP-70, EP-71, PF-48).
//!
//! A hub endpoint has no code path of its own. The gateway pins the tenant of the space and
//! forwards, and the broker does the distributed operation CIM 009 clause 4.3.6 defines: it
//! matches the query against its registrations, forwards, merges and protects itself against a
//! loop. That is the whole read path, and this module adds nothing to it.
//!
//! What it does hold is the one thing the gateway has to decide before forwarding: whose
//! identity a forward carries. `serviceAccount` is the mode the platform implements; `caller`
//! needs an RFC 8693 token exchange that does not exist yet, and a hub that silently forwarded
//! its own account instead would be a widening nobody wrote down (PF-48).
//!
//! It also holds the member names, which the MCP surface lists so a tool description can say
//! what an endpoint federates. Names only: an address never leaves this platform (EP-71).

/// Turning an accepted ODRL agreement into the policies that enforce it (DS-10).
pub mod odrl_compiler;

use jc_core::kinds::{ContextSourceRegistrationSpec, FederationIdentity};
use jcctl::loader::Repository;
use std::collections::BTreeMap;

/// One registered source, as the gateway needs it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Member {
    /// The registration's `metadata.name`, which is also how a result names its source and how
    /// the broker identifies a part that failed. Never an address (EP-71).
    pub name: String,
    /// Whose identity a forward to this source carries (PF-48).
    pub identity: FederationIdentity,
    /// Whether the source is outside this platform.
    pub external: bool,
}

/// The registrations of every space, keyed by project and space.
#[derive(Debug, Clone, Default)]
pub struct Federations {
    by_space: BTreeMap<(String, String), Vec<Member>>,
}

impl Federations {
    /// A gateway that knows of no registration at all, which is what a repository without one
    /// describes and what every endpoint had before federation existed.
    pub fn new() -> Self {
        Self::default()
    }

    /// The registrations of one space, in manifest order.
    pub fn members(&self, project: &str, space: &str) -> &[Member] {
        self.by_space
            .get(&(project.to_owned(), space.to_owned()))
            .map(Vec::as_slice)
            .unwrap_or_default()
    }

    /// The registrations of one space that ask for the caller's own identity (PF-48).
    ///
    /// Non-empty means the gateway cannot serve a read over this space yet: forwarding as its
    /// own account instead would answer with data the caller was never granted.
    pub fn needing_caller_identity(&self, project: &str, space: &str) -> Vec<&str> {
        self.members(project, space)
            .iter()
            .filter(|member| member.identity == FederationIdentity::Caller)
            .map(|member| member.name.as_str())
            .collect()
    }

    /// Whether this space federates at all.
    pub fn is_federated(&self, project: &str, space: &str) -> bool {
        !self.members(project, space).is_empty()
    }
}

/// The federation table a loaded repository describes (CC-08).
///
/// Deterministic, like every other table the gateway builds from the repository: two runs on
/// one commit produce the same members in the same order (CC-27). A registration the gateway
/// cannot parse is left out rather than half-applied, which is what every other kind does here.
pub fn federations_of(repo: &Repository) -> Federations {
    let mut by_space: BTreeMap<(String, String), Vec<Member>> = BTreeMap::new();
    for (id, resource) in repo.iter() {
        if id.kind != "ContextSourceRegistration" {
            continue;
        }
        let Ok(spec) =
            serde_json::from_value::<ContextSourceRegistrationSpec>(resource.manifest.spec.clone())
        else {
            continue;
        };
        if spec.validate().is_err() {
            continue;
        }
        let project = id.namespace.clone().unwrap_or_default();
        let space = repo.space_segment(&project, spec.context_space_ref.name());
        by_space.entry((project, space)).or_default().push(Member {
            name: id.name.clone(),
            identity: spec.federation.identity,
            external: spec.endpoint_ref.is_none(),
        });
    }
    Federations { by_space }
}
