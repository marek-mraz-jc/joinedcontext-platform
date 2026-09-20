//! What the reconciler can do to the live platform (T-0128, CC-04, CC-16).
//!
//! Every kind reconciles into a different component (Architecture/06 section 2: broker
//! tenants, gateway routes, APISIX config, the Portal mirror), so the reconciler talks to
//! one port and the component behind it is somebody else's problem. Writes always go
//! through the Context Gateway with a technical ServiceAccount, never straight to a
//! broker (CC-04).

use crate::loader::{RawManifest, ResourceId};
use std::collections::BTreeMap;

/// A live platform the reconciler can read and converge.
pub trait Platform {
    /// Every live resource of one kind in one project, shaped as a manifest.
    ///
    /// `project` is the namespace: a project slug, or `org` for organization-level kinds.
    /// An unknown project or kind is an empty list, not an error.
    fn list(&self, project: &str, plural: &str) -> Result<Vec<RawManifest>, PlatformError>;

    /// Creates the resource, or replaces it when it already exists.
    fn put(&mut self, manifest: &RawManifest) -> Result<(), PlatformError>;

    /// Removes the resource. Removing what is not there succeeds: `apply` has to be
    /// re-runnable after a partial failure (CC-18).
    fn delete(&mut self, id: &ResourceId) -> Result<(), PlatformError>;
}

/// Why the platform could not answer.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PlatformError {
    /// The platform could not be reached or refused the caller (API/03 exit code 1).
    #[error("platform unavailable: {0}")]
    Unavailable(String),
    /// The platform reached the resource and rejected it.
    #[error("{id} rejected: {message}")]
    Rejected {
        /// The resource the platform refused.
        id: Box<ResourceId>,
        /// What the platform said.
        message: String,
    },
}

/// A platform held in memory: the test double, and the record of what `apply` did.
#[derive(Debug, Default, Clone)]
pub struct InMemory {
    resources: BTreeMap<ResourceId, RawManifest>,
}

impl InMemory {
    /// An empty platform, as a fresh installation looks.
    pub fn new() -> Self {
        Self::default()
    }

    /// Seeds the platform with a resource that already exists.
    pub fn with(mut self, manifest: RawManifest) -> Self {
        self.resources
            .insert(ResourceId::from_manifest(&manifest), manifest);
        self
    }

    /// The resources the platform currently holds, in identity order.
    pub fn resources(&self) -> impl Iterator<Item = (&ResourceId, &RawManifest)> {
        self.resources.iter()
    }

    /// How many resources the platform holds.
    pub fn len(&self) -> usize {
        self.resources.len()
    }

    /// Whether the platform holds nothing.
    pub fn is_empty(&self) -> bool {
        self.resources.is_empty()
    }
}

impl Platform for InMemory {
    fn list(&self, project: &str, plural: &str) -> Result<Vec<RawManifest>, PlatformError> {
        Ok(self
            .resources
            .iter()
            .filter(|(id, _)| {
                id.namespace.as_deref().unwrap_or("org") == project
                    && jc_core::registry::by_kind(&id.kind).is_some_and(|k| k.plural == plural)
            })
            .map(|(_, manifest)| manifest.clone())
            .collect())
    }

    fn put(&mut self, manifest: &RawManifest) -> Result<(), PlatformError> {
        self.resources
            .insert(ResourceId::from_manifest(manifest), manifest.clone());
        Ok(())
    }

    fn delete(&mut self, id: &ResourceId) -> Result<(), PlatformError> {
        self.resources.remove(id);
        Ok(())
    }
}
