//! Manifest kinds for projects and context spaces (T-0112, PF-05, PF-06, PF-07, PF-09, PF-17, PF-19).

use crate::envelope::{Kind, ObjectMeta, Ref, Scope};
use crate::error::{Error, Result};
use crate::names;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Desired specification of a [`Project`][crate::kinds::Project] resource (PF-05, PF-17).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct ProjectSpec {
    /// Reference to the parent Organization.
    pub organization_ref: Ref,
    /// Optional resource quotas for this Project (PF-17).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quotas: Option<Quotas>,
}

impl Kind for ProjectSpec {
    const KIND: &'static str = "Project";
    const PLURAL: &'static str = "projects";
    const SCOPE: Scope = Scope::Organization;
    const PATH_TEMPLATE: &'static str = "projects/{name}/project.yaml";

    fn validate_spec(&self, _meta: &ObjectMeta) -> Result<()> {
        self.validate()
    }
}

impl ProjectSpec {
    /// Validates the organization reference and optional quotas.
    pub fn validate(&self) -> Result<()> {
        let org_name = self.organization_ref.name();
        names::validate_dns1123_label(org_name)?;
        if let Some(kind) = self.organization_ref.kind() {
            if kind != "Organization" {
                return Err(Error::Kind {
                    expected: "Organization",
                    got: kind.to_string(),
                });
            }
        }
        if let Some(ref q) = self.quotas {
            q.validate()?;
        }
        Ok(())
    }
}

/// Structural resource quotas per project (PF-17).
///
/// An absent field indicates unlimited quota for that resource.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Quotas {
    /// Maximum number of active Context Spaces.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_spaces: Option<u32>,
    /// Maximum number of resident streaming pipelines.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resident_pipelines: Option<u32>,
    /// Maximum number of public endpoints.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub public_endpoints: Option<u32>,
    /// Maximum ingestion rate in events per second.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ingest_events_per_second: Option<u32>,
    /// Maximum number of Apps the project deploys (PF-73).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub apps: Option<u32>,
    /// Maximum number of agent runs the project starts in one day (PF-73, PF-74).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_runs_per_day: Option<u32>,
    /// Maximum number of entities one Context Space of the project holds (PF-73).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entities_per_space: Option<u32>,
    /// Maximum requests per minute one Endpoint of the project serves (PF-73, EP-17).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requests_per_minute: Option<u32>,
}

impl Quotas {
    /// Every dimension by its manifest field name, in the order PF-73 lists them.
    pub fn dimensions(&self) -> [(&'static str, Option<u32>); 8] {
        [
            ("contextSpaces", self.context_spaces),
            ("residentPipelines", self.resident_pipelines),
            ("publicEndpoints", self.public_endpoints),
            ("ingestEventsPerSecond", self.ingest_events_per_second),
            ("apps", self.apps),
            ("agentRunsPerDay", self.agent_runs_per_day),
            ("entitiesPerSpace", self.entities_per_space),
            ("requestsPerMinute", self.requests_per_minute),
        ]
    }

    /// Validates that all defined quotas are positive integers (>= 1).
    pub fn validate(&self) -> Result<()> {
        for (field, value) in self.dimensions() {
            if value == Some(0) {
                return Err(Error::Name {
                    field: "quotas",
                    value: format!("{field}: 0"),
                    reason: "quota must be >= 1",
                });
            }
        }
        Ok(())
    }

    /// Which of this quota's values stand above `default`, as `(field, value, default)` (PF-73).
    ///
    /// An override below the default is the project's own business, in the yellow lane; one above
    /// it is the organization's, which is what makes it red. A dimension the default leaves open
    /// is not exceeded by any value.
    pub fn above(&self, default: &Quotas) -> Vec<(&'static str, u32, u32)> {
        self.dimensions()
            .into_iter()
            .zip(default.dimensions())
            .filter_map(|((field, mine), (_, theirs))| match (mine, theirs) {
                (Some(mine), Some(theirs)) if mine > theirs => Some((field, mine, theirs)),
                _ => None,
            })
            .collect()
    }

    /// Whether every value of this quota is within `default` (PF-73).
    pub fn within(&self, default: &Quotas) -> bool {
        self.above(default).is_empty()
    }
}

/// Desired specification of a [`ContextSpace`][crate::kinds::ContextSpace] resource (PF-06, PF-09, PF-19).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct ContextSpaceSpec {
    /// Whether this context space is an ephemeral sandbox (CC-67, PF-19).
    #[serde(default)]
    pub is_sandbox: bool,
    /// Default locale for entities and metadata within this space (PF-25).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_locale: Option<String>,
    /// Reference to the primary LinkML Data Model for this space (Architecture/03).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data_model_ref: Option<Ref>,
    /// Enforced time-to-live in days for ephemeral sandboxes (1..=14, PF-19).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ttl_days: Option<u32>,
    /// The `{space}` segment of this space's entity ids, when it is not the rendered
    /// `{project}-{name}`; pins the segment of a space that predates PF-84 (PF-84).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub urn_segment: Option<String>,
}

/// The `{space}` segment of every entity id of a space: its pin, else `{project}-{name}` (PF-84).
///
/// The one function the gateway, the reconciler and the Portal call, so no caller-supplied
/// value ever decides which space an id belongs to.
pub fn urn_segment(project: &str, name: &str, pin: Option<&str>) -> String {
    match pin {
        Some(pin) => pin.to_owned(),
        None => format!("{project}-{name}"),
    }
}

impl Kind for ContextSpaceSpec {
    const KIND: &'static str = "ContextSpace";
    const PLURAL: &'static str = "spaces";
    const SCOPE: Scope = Scope::Project;
    const PATH_TEMPLATE: &'static str = "projects/{project}/spaces/{name}/space.yaml";

    fn validate_spec(&self, meta: &ObjectMeta) -> Result<()> {
        self.validate_with_meta(meta)
    }
}

impl ContextSpaceSpec {
    /// Validates locale and sandbox TTL rules without metadata.
    pub fn validate(&self) -> Result<()> {
        if let Some(ref loc) = self.default_locale {
            names::validate_locale(loc)?;
        }

        if let Some(ref dm) = self.data_model_ref {
            names::validate_dns1123_label(dm.name())?;
            if let Some(kind) = dm.kind() {
                if kind != "DataModel" {
                    return Err(Error::Kind {
                        expected: "DataModel",
                        got: kind.to_string(),
                    });
                }
            }
        }

        if let Some(ttl) = self.ttl_days {
            if !self.is_sandbox {
                return Err(Error::Name {
                    field: "spec.ttlDays",
                    value: ttl.to_string(),
                    reason: "ttlDays is only allowed on sandbox context spaces (PF-19)",
                });
            }
            if !(1..=14).contains(&ttl) {
                return Err(Error::Name {
                    field: "spec.ttlDays",
                    value: ttl.to_string(),
                    reason: "sandbox ttlDays must be between 1 and 14 calendar days (PF-19)",
                });
            }
        }

        Ok(())
    }

    /// Validates the context space against PF-09, PF-19 and locale rules.
    pub fn validate_with_meta(&self, meta: &ObjectMeta) -> Result<()> {
        names::validate_space_name(&meta.name)?;
        self.validate()?;
        match (&self.urn_segment, &meta.namespace) {
            (Some(pin), _) => names::validate_space_name(pin),
            (None, Some(project)) => {
                names::validate_space_name(&urn_segment(project, &meta.name, None))
            }
            (None, None) => Ok(()),
        }
    }
}
