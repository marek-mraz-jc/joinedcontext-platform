//! What the gateway knows about a space's registrations (T-0345, EP-70, EP-71, PF-48).
//!
//! A hub endpoint has no code path of its own. The gateway pins the tenant of the space and
//! forwards, and the broker does the distributed operation CIM 009 clause 4.3.6 defines: it
//! matches the query against its registrations, forwards, merges and protects itself against a
//! loop. That is the whole read path, and this module adds nothing to it.
//!
//! What it does hold is the one thing the gateway has to decide before forwarding: whose
//! identity a forward carries. `serviceAccount` forwards as the hub's account; `caller` forwards
//! a token of the caller the gateway exchanged for the member brokers' audiences ([`exchange`],
//! T-3999, PF-113), which only a member on this platform receives (PF-48).
//!
//! It also holds the member names, which the MCP surface lists so a tool description can say
//! what an endpoint federates. Names only: an address never leaves this platform (EP-71).

/// The caller's token, exchanged for the brokers a federated read reaches (T-3999).
pub mod exchange;

use jc_core::kinds::{ContextSourceRegistrationSpec, FederationIdentity};
use jc_manifest::loader::Repository;
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
    /// The slug of the member's Endpoint when it is on this platform, which names the broker
    /// its space lives on (T-3999); `None` for a source elsewhere or an Endpoint not found.
    pub view: Option<String>,
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

    /// The registrations of one space that ask for the caller's own identity (PF-48): a read over
    /// this space forwards a token of the caller exchanged for these members' brokers (T-3999).
    pub fn caller_members<'a>(
        &'a self,
        project: &str,
        space: &str,
    ) -> impl Iterator<Item = &'a Member> {
        self.members(project, space)
            .iter()
            .filter(|member| member.identity == FederationIdentity::Caller)
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
        let view = spec.endpoint_ref.as_ref().and_then(|reference| {
            repo.iter()
                .find(|(other, _)| {
                    other.kind == "Endpoint"
                        && other.name == reference.name()
                        && other.namespace.as_deref() == Some(project.as_str())
                })
                .and_then(|(_, endpoint)| endpoint.manifest.spec["slug"].as_str())
                .map(str::to_owned)
        });
        by_space.entry((project, space)).or_default().push(Member {
            name: id.name.clone(),
            identity: spec.federation.identity,
            external: spec.endpoint_ref.is_none(),
            view,
        });
    }
    Federations { by_space }
}
