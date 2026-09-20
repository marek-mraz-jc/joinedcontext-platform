//! The in-memory endpoint table (T-0144, EP-17, EP-18, EP-19, EP-03).
//!
//! Every request on the endpoint surface starts by turning an opaque slug into the space
//! behind it, so this lookup is on the hot path of the whole gateway. The table is read
//! far more often than it is written: the reconciler replaces it when an `Endpoint`
//! changes, readers never block, and a reader that is mid-request keeps the snapshot it
//! started with (EP-18, EP-19).
//!
//! The table answers with the space name only after the slug matched. An unknown slug
//! yields nothing at all, so the caller cannot learn whether a space exists behind a slug
//! it guessed (EP-03, EP-23).

use arc_swap::ArcSwap;
use jc_core::kinds::{Audience, FileLimits, PolicySpec, RateLimits, Representation};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Arc;

/// Everything the gateway needs about one endpoint, resolved in a single lookup.
#[derive(Debug, Clone, PartialEq)]
pub struct Endpoint {
    /// The opaque slug the client uses; the key of the table.
    pub slug: String,
    /// The context space behind it, which becomes the pinned `NGSILD-Tenant` (EP-22).
    pub space: String,
    /// The project the space belongs to, for audience checks (EP-14, EP-15).
    pub project: String,
    /// The endpoint's own `metadata.title` per locale; the DCAT record prefers it to the
    /// space's, so two endpoints over one space are two datasets with two names (EP-27).
    pub title: BTreeMap<String, String>,
    /// The endpoint's own `metadata.description` per locale (EP-27).
    pub description: BTreeMap<String, String>,
    /// Who may use the endpoint at all (EP-14).
    pub audience: Audience,
    /// The projects allowed when the audience is `project-list` (EP-15).
    pub allowed_projects: Vec<String>,
    /// The representations this endpoint serves; anything else is 404 (EP-05).
    pub representations: Vec<Representation>,
    /// The token-bucket configuration, absent when the endpoint sets no limit (EP-20).
    pub rate_limit: Option<RateLimits>,
    /// The ceiling on one `file.*` download, absent when the endpoint sets none (EP-44).
    pub file_limits: Option<FileLimits>,
    /// Attributes this endpoint never serves, whatever the policies grant (EP-61).
    pub hidden_attributes: BTreeSet<String>,
    /// The named subset of the space's model this endpoint reads (MP-02): its classes, their
    /// slots and its residual filter, intersected with every grant before any representation
    /// is encoded, so it narrows and never widens.
    pub projection: Option<std::sync::Arc<jc_core::kinds::ModelProjectionSpec>>,
    /// The path this record answers under, which is also its RFC 8707 resource when the
    /// deployment names a public URL: `/api/endpoint/{slug}` or `/cs/{space}` (SP-01).
    pub base_path: String,
    /// The policies the PDP evaluates for callers of this endpoint (GW8).
    pub policies: Vec<PolicySpec>,
    /// The data models of the space, with whatever artifacts the repository carries
    /// beside them (EP-46, DM-02).
    pub models: Vec<Model>,
    /// The Mapping this endpoint serves its space through, when it serves a view of another
    /// model rather than the space's own (EP-54, DM-51).
    ///
    /// Present only when the manifest names one *and* the compiled IR beside it could be
    /// read: an endpoint that is meant to be a view and has no IR would otherwise serve the
    /// source model under the target model's name, which is worse than not serving at all.
    pub view_mapping: Option<std::sync::Arc<crate::translators::view_mapping::ViewMapping>>,
}

/// One version of one data model, as the schema surface publishes it (EP-46, DM-22).
#[derive(Debug, Clone, PartialEq)]
pub struct Model {
    /// The manifest name.
    pub name: String,
    /// The full semantic version, as written.
    pub version: String,
    /// The major, which is the `schema/v{major}` the endpoint serves (DM-22).
    pub major: u32,
    /// The NGSI-LD entity types the model defines.
    pub classes: Vec<String>,
    /// The generated JSON Schema, when the repository carries it (DM-02).
    pub json_schema: Option<serde_json::Value>,
    /// The generated JSON-LD `@context`, when the repository carries it (DM-02).
    pub context: Option<serde_json::Value>,
}

impl Endpoint {
    /// Whether this endpoint serves the representation, so an unlisted one can be refused
    /// before any work is done (EP-05).
    pub fn serves(&self, representation: Representation) -> bool {
        self.representations.contains(&representation)
    }

    /// Whether a caller from `project` may use this endpoint at all (EP-14, EP-15).
    ///
    /// `None` is an anonymous caller: only a public endpoint serves one.
    pub fn admits(&self, project: Option<&str>) -> bool {
        match self.audience {
            Audience::Public => true,
            Audience::Organization => project.is_some(),
            Audience::ProjectList => project.is_some_and(|caller| {
                caller == self.project || self.allowed_projects.iter().any(|p| p == caller)
            }),
        }
    }
}

/// One context space as its own surface (SP-01, SP-10).
///
/// The enforcement record is an [`Endpoint`] like any other, so a request to
/// `/cs/{space}/ngsi-ld/v1/` passes the same six steps through the same code as a request
/// to an endpoint slug: the space surface cannot drift from the endpoint surface because
/// there is only one of them. What a space carries beyond it is the description a DCAT-AP
/// record needs.
#[derive(Debug, Clone, PartialEq)]
pub struct Space {
    /// The record the PDP decides on and the enforcement point pins the tenant from.
    pub endpoint: Arc<Endpoint>,
    /// The manifest's title per locale (PF-24).
    pub title: BTreeMap<String, String>,
    /// The manifest's description per locale (PF-24).
    pub description: BTreeMap<String, String>,
    /// Whether this space is an ephemeral sandbox (PF-19).
    pub is_sandbox: bool,
    /// The locale the space declares for its entities, when it declares one (PF-25).
    pub default_locale: Option<String>,
}

impl Space {
    /// The space's name, which is the `{space}` segment of every URL and every URN.
    pub fn name(&self) -> &str {
        &self.endpoint.space
    }
}

/// The tables a request is resolved against, swapped whole when the reconciler changes a
/// manifest (EP-17, EP-19).
///
/// Endpoints and spaces are two tables rather than one, because an opaque slug and a space
/// name are two namespaces: a slug that happened to spell a space name must not resolve to
/// it, and a space name must never be reachable by guessing a slug (EP-03).
#[derive(Debug, Default)]
pub struct SlugResolver {
    table: ArcSwap<HashMap<String, Arc<Endpoint>>>,
    spaces: ArcSwap<HashMap<String, Arc<Space>>>,
}

impl SlugResolver {
    /// An empty table: every slug resolves to nothing until the reconciler fills it.
    pub fn new() -> Self {
        Self::default()
    }

    /// A table holding the given endpoints.
    pub fn with(endpoints: impl IntoIterator<Item = Endpoint>) -> Self {
        let resolver = Self::new();
        resolver.replace(endpoints);
        resolver
    }

    /// The endpoint behind a slug, or nothing (EP-18).
    ///
    /// Wait-free: the reader takes the current snapshot and never blocks a writer or
    /// another reader.
    pub fn resolve(&self, slug: &str) -> Option<Arc<Endpoint>> {
        self.table.load().get(slug).map(Arc::clone)
    }

    /// Replaces the whole table in one atomic step (EP-19).
    ///
    /// A reader either sees the entire old table or the entire new one, never a mixture,
    /// so an endpoint is never briefly missing while its space is being updated.
    pub fn replace(&self, endpoints: impl IntoIterator<Item = Endpoint>) {
        let table: HashMap<String, Arc<Endpoint>> = endpoints
            .into_iter()
            .map(|endpoint| (endpoint.slug.clone(), Arc::new(endpoint)))
            .collect();
        self.table.store(Arc::new(table));
    }

    /// The space behind a name, or nothing (SP-06).
    pub fn resolve_space(&self, space: &str) -> Option<Arc<Space>> {
        self.spaces.load().get(space).map(Arc::clone)
    }

    /// Every space the gateway serves, by name, for the catalog to narrow (SP-11).
    ///
    /// Sorted, so the catalog a caller reads twice reads the same way twice.
    pub fn spaces(&self) -> Vec<Arc<Space>> {
        let mut spaces: Vec<Arc<Space>> = self.spaces.load().values().map(Arc::clone).collect();
        spaces.sort_by(|left, right| left.name().cmp(right.name()));
        spaces
    }

    /// Replaces the whole space table in one atomic step (EP-19).
    pub fn replace_spaces(&self, spaces: impl IntoIterator<Item = Space>) {
        let table: HashMap<String, Arc<Space>> = spaces
            .into_iter()
            .map(|space| (space.endpoint.space.clone(), Arc::new(space)))
            .collect();
        self.spaces.store(Arc::new(table));
    }

    /// How many endpoints the table currently holds.
    pub fn len(&self) -> usize {
        self.table.load().len()
    }

    /// Whether the table is empty, which is how the gateway starts.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}
