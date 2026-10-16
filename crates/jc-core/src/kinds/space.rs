//! Manifest kinds for projects and context spaces (T-0112, PF-05, PF-06, PF-07, PF-09, PF-17, PF-19).

use crate::envelope::{Kind, ObjectMeta, Ref, Scope};
use crate::error::{Error, Result};
use crate::kinds::SemVer;
use crate::names;
use crate::project::{Parameter, ProjectRepository};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Desired specification of a [`Project`][crate::kinds::Project] resource (PF-05, PF-17).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct ProjectSpec {
    /// Reference to the parent Organization.
    pub organization_ref: Ref,
    /// The internet domain of the body that publishes the project's data, the `did:web` assigner
    /// of the Policies compiled for its Apps; absent, the Organization's (PF-108). It describes
    /// who publishes and grants, verifies and routes nothing; pipelines keep minting over
    /// `JC_ORG_DOMAIN` (PF-44).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub domain: Option<String>,
    /// Optional resource quotas for this Project (PF-17); the project file's, never a registry
    /// entry's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quotas: Option<Quotas>,
    /// Where the project's configuration lives. Its presence makes this manifest a registry
    /// entry, `projects/{slug}.yaml` of the organization repository (PF-86).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repository: Option<ProjectRepository>,
    /// The tag, branch or commit of `repository` this deployment runs; a registry entry only.
    #[serde(default, rename = "ref", skip_serializing_if = "Option::is_none")]
    pub git_ref: Option<String>,
    /// The project's version, released as the tag `v{version}`; the project file only (CC-88).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<SemVer>,
    /// Declarations in the project file, this deployment's values in a registry entry (CC-88).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub parameters: BTreeMap<String, Parameter>,
    /// What the project's Apps may use: a narrower service list and lower quotas than the
    /// organization's (AP-163, AP-165).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub apps: Option<ProjectApps>,
}

/// `spec.apps` of a Project (AP-163, AP-165, ADR-N-045).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct ProjectApps {
    /// The platform services the project's Apps may use; absent is the organization's list.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub services: Option<Vec<crate::kinds::AppService>>,
    /// The per-App quotas; an absent one is the organization's.
    #[serde(
        default,
        skip_serializing_if = "crate::kinds::AppServiceLimits::is_unset"
    )]
    pub limits: crate::kinds::AppServiceLimits,
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
        if let Some(domain) = &self.domain {
            names::validate_org_domain(domain).map_err(|e| Error::Invalid {
                field: "spec.domain".to_owned(),
                reason: e.to_string(),
            })?;
        }
        let entry = self.is_registry_entry();
        let belongs = |field: &str, place: &str| Error::Invalid {
            field: field.to_owned(),
            reason: format!("belongs to {place}"),
        };
        match (&self.repository, &self.git_ref) {
            (Some(repository), Some(git_ref)) => {
                repository.validate()?;
                validate_git_ref(git_ref)?;
            }
            (Some(_), None) => {
                return Err(Error::Invalid {
                    field: "spec.ref".to_owned(),
                    reason: "a registry entry pins the tag, branch or commit it runs".to_owned(),
                })
            }
            (None, Some(_)) => {
                return Err(belongs(
                    "spec.ref",
                    "a registry entry, beside spec.repository",
                ))
            }
            (None, None) => {}
        }
        if entry && self.version.is_some() {
            return Err(belongs("spec.version", "the project's own project.yaml"));
        }
        if entry && self.quotas.is_some() {
            return Err(belongs("spec.quotas", "the project's own project.yaml"));
        }
        crate::project::validate_parameters(&self.parameters, entry)
    }

    /// Whether this is a registry entry of the organization repository rather than the project
    /// file of a project repository (PF-86).
    pub fn is_registry_entry(&self) -> bool {
        self.repository.is_some()
    }
}

/// A ref is handed to git: no option, no range, no whitespace (PF-86).
fn validate_git_ref(git_ref: &str) -> Result<()> {
    let refused = git_ref.is_empty()
        || git_ref.len() > 255
        || git_ref.starts_with('-')
        || git_ref.starts_with('/')
        || git_ref.contains("..")
        || git_ref
            .chars()
            .any(|c| c.is_whitespace() || c.is_control() || "~^:?*[\\".contains(c));
    if refused {
        return Err(Error::Invalid {
            field: "spec.ref".to_owned(),
            reason: "must be a tag, a branch or a commit as git names them".to_owned(),
        });
    }
    Ok(())
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
    /// The one LinkML Data Model of this space, whose classes are its types (DM-61, ADR-N-033).
    ///
    /// Absent on a space created before DM-61, which `jcctl validate` warns about. It names a
    /// model of the space's own project: another project's model is reused through the LinkML
    /// `imports` of this one, read-only and at a pinned version.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data_model_ref: Option<Ref>,
    /// Enforced time-to-live in days for ephemeral sandboxes (1..=14, PF-19).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ttl_days: Option<u32>,
    /// The `{space}` segment of this space's entity ids, when it is not the rendered
    /// `{project}-{name}`; pins the segment of a space that predates PF-84 (PF-84).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub urn_segment: Option<String>,
    /// What a write through an Endpoint does with a quantity Property of the model that carries
    /// no `unitCode`: `fill` writes the model's code and says so, `refuse` answers `400` (DM-06).
    /// A `unitCode` other than the model's is refused either way.
    #[serde(default, skip_serializing_if = "MissingUnitCode::is_fill")]
    pub missing_unit_code: MissingUnitCode,
    /// How long the broker keeps each type's data; a type without a rule is kept forever
    /// (DM-82, ADR-N-046).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub retention: Vec<RetentionRule>,
    /// The space's own quality rules beside its model's constraints, at most 64 (DM-87,
    /// ADR-N-046).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub quality: Vec<QualityRule>,
    /// The `ContextBroker` this space lives on; the organization's default when absent
    /// (PF-111, ADR-N-047). Changing it on a space that holds entities is a move job (PF-115).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub broker_ref: Option<String>,
}

/// The most quality rules a space declares (DM-92).
pub const MAX_QUALITY_RULES: usize = 64;

/// What a quality rule checks (DM-87).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum QualityKind {
    /// A number Property's value against `min` and/or `max`.
    Range,
    /// Every point of a GeoProperty inside `bbox`.
    Area,
    /// The attribute's `observedAt` against `maxAhead` (on write) and `maxAge` (by the sweep).
    Freshness,
}

/// What a broken quality rule does (DM-88, DM-89, DM-90).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum QualityOutcome {
    /// The write is refused.
    Reject,
    /// The entity is stored and the attribute carries `qualityFlags`.
    Flag,
    /// The entity is stored with `quarantinedAt` and hidden from all but the owner and steward.
    Quarantine,
}

/// One named quality rule of a space (DM-87, ADR-N-046 2.4).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct QualityRule {
    /// A DNS-1123 label, unique in the space: it names the rule in a refusal and the metric.
    pub name: String,
    /// A class of the space's Data Model.
    pub entity_type: String,
    /// What the rule checks.
    pub kind: QualityKind,
    /// The slot of `entity_type` the rule reads.
    pub attribute: String,
    /// `range`: the smallest value allowed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min: Option<serde_json::Number>,
    /// `range`: the largest value allowed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max: Option<serde_json::Number>,
    /// `area`: `[minLon, minLat, maxLon, maxLat]` in WGS 84.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bbox: Option<[serde_json::Number; 4]>,
    /// `freshness`: how far `observedAt` may lie in the future, checked on write.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_ahead: Option<String>,
    /// `freshness`: how old `observedAt` may be, checked by the sweep.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_age: Option<String>,
    /// Overrides the kind's default outcome.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome: Option<QualityOutcome>,
}

impl QualityRule {
    /// The outcome of a check on write: `range` and `area` flag, `maxAhead` rejects (DM-87).
    pub fn write_outcome(&self) -> QualityOutcome {
        self.outcome.unwrap_or(match self.kind {
            QualityKind::Freshness => QualityOutcome::Reject,
            QualityKind::Range | QualityKind::Area => QualityOutcome::Flag,
        })
    }

    /// The outcome of the sweep's `maxAge` check: quarantine, the only one validation admits.
    pub fn sweep_outcome(&self) -> QualityOutcome {
        self.outcome.unwrap_or(QualityOutcome::Quarantine)
    }

    fn validate(&self) -> Result<()> {
        let refuse = |field: &'static str, value: String, reason: &'static str| {
            Err(Error::Name {
                field,
                value,
                reason,
            })
        };
        if names::validate_dns1123_label(&self.name).is_err() {
            return refuse(
                "spec.quality.name",
                self.name.clone(),
                "a quality rule's name is a DNS-1123 label (DM-87)",
            );
        }
        names::validate_entity_type(&self.entity_type)?;
        if self.attribute.is_empty()
            || self.attribute.len() > 256
            || self
                .attribute
                .chars()
                .any(|c| c.is_control() || c.is_whitespace())
        {
            return refuse(
                "spec.quality.attribute",
                self.attribute.clone(),
                "an attribute is a slot name of the type (DM-87)",
            );
        }
        let number = |n: &serde_json::Number| n.as_f64().filter(|v| v.is_finite());
        let (range, area, fresh) = (
            self.min.is_some() || self.max.is_some(),
            self.bbox.is_some(),
            self.max_ahead.is_some() || self.max_age.is_some(),
        );
        let fits = match self.kind {
            QualityKind::Range => range && !area && !fresh,
            QualityKind::Area => area && !range && !fresh,
            QualityKind::Freshness => fresh && !range && !area,
        };
        if !fits {
            return refuse(
                "spec.quality",
                self.name.clone(),
                "a range rule sets min or max, an area rule bbox, a freshness rule maxAhead \
                 or maxAge, and nothing else (DM-87)",
            );
        }
        // The sweep finds a stale entity after its write: nothing is left to refuse or flag.
        if self.max_age.is_some()
            && self
                .outcome
                .is_some_and(|outcome| outcome != QualityOutcome::Quarantine)
        {
            return refuse(
                "spec.quality.outcome",
                self.name.clone(),
                "a rule with a maxAge quarantines; its outcome is quarantine or none (DM-87)",
            );
        }
        if let (Some(min), Some(max)) = (self.min.as_ref(), self.max.as_ref()) {
            if number(min).zip(number(max)).is_none_or(|(a, b)| a > b) {
                return refuse(
                    "spec.quality.min",
                    self.name.clone(),
                    "min is at most max (DM-87)",
                );
            }
        }
        if let Some(bbox) = &self.bbox {
            let v: Vec<f64> = bbox.iter().filter_map(number).collect();
            let ok = v.len() == 4
                && v[0] <= v[2]
                && v[1] <= v[3]
                && [v[0], v[2]].iter().all(|x| (-180.0..=180.0).contains(x))
                && [v[1], v[3]].iter().all(|y| (-90.0..=90.0).contains(y));
            if !ok {
                return refuse(
                    "spec.quality.bbox",
                    self.name.clone(),
                    "bbox is [minLon, minLat, maxLon, maxLat] in WGS 84, min <= max (DM-87)",
                );
            }
        }
        for (field, value) in [
            ("spec.quality.maxAhead", &self.max_ahead),
            ("spec.quality.maxAge", &self.max_age),
        ] {
            if let Some(v) = value.as_deref().filter(|v| duration_seconds(v).is_none()) {
                return refuse(
                    field,
                    v.to_owned(),
                    "a freshness duration is PT1S to P3650D, as PT30S, PT5M or P1D (DM-87)",
                );
            }
        }
        Ok(())
    }
}

/// One type's retention in a space (DM-82, ADR-N-046): both durations absent keeps it forever.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct RetentionRule {
    /// A class of the space's Data Model.
    pub entity_type: String,
    /// How long an entity not modified stays, as `PT5M`, `PT8H`, `P30D` or `P1DT12H` (DM-83).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current: Option<String>,
    /// How long a temporal instance is kept, in the same form (DM-84); `off` keeps the current
    /// state only and records no history, `forever` keeps it all and needs a `reason` (T-4275).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub history: Option<String>,
    /// Why the type keeps its whole history: required with `history: forever` (T-4275).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// The `history` of a rule that records nothing for its type (T-4275).
pub const HISTORY_OFF: &str = "off";
/// The `history` of a rule that keeps everything, with a `reason` (T-4275).
pub const HISTORY_FOREVER: &str = "forever";

/// Seconds of a retention duration `P[nD][T[nH][nM][nS]]` from one minute to 3650 days, the
/// form the broker's `/q/retention` reads; `None` for anything else (DM-82).
pub fn retention_seconds(raw: &str) -> Option<u64> {
    duration_seconds(raw).filter(|total| *total >= 60)
}

/// Seconds of a duration `P[nD][T[nH][nM][nS]]` from one second to 3650 days; `None` for
/// anything else (DM-82, DM-87).
pub fn duration_seconds(raw: &str) -> Option<u64> {
    let rest = raw.strip_prefix('P')?;
    let (days, time) = match rest.split_once('T') {
        Some((_, "")) => return None,
        Some((d, t)) => (d, t),
        None => (rest, ""),
    };
    if days.is_empty() && time.is_empty() {
        return None;
    }
    let mut total: u64 = match days {
        "" => 0,
        d => d
            .strip_suffix('D')?
            .parse::<u64>()
            .ok()?
            .checked_mul(86_400)?,
    };
    let mut time = time;
    for (unit, scale) in [('H', 3600), ('M', 60), ('S', 1)] {
        if let Some((n, tail)) = time.split_once(unit) {
            total = total.checked_add(n.parse::<u64>().ok()?.checked_mul(scale)?)?;
            time = tail;
        }
    }
    (time.is_empty() && (1..=3650 * 86_400).contains(&total)).then_some(total)
}

/// What a space does with a quantity written without its `unitCode` (DM-06).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum MissingUnitCode {
    /// The gateway writes the model's code and names it in the answer.
    #[default]
    Fill,
    /// The gateway refuses the write, naming the attribute and the code it needs.
    Refuse,
}

impl MissingUnitCode {
    /// Whether this is the default, which a manifest leaves unwritten.
    pub fn is_fill(&self) -> bool {
        *self == Self::Fill
    }
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

        let mut ruled = std::collections::BTreeSet::new();
        for rule in &self.retention {
            names::validate_entity_type(&rule.entity_type)?;
            if !ruled.insert(rule.entity_type.as_str()) {
                return Err(Error::Name {
                    field: "spec.retention",
                    value: rule.entity_type.clone(),
                    reason: "a type has at most one retention rule (DM-82)",
                });
            }
            let word = rule
                .history
                .as_deref()
                .filter(|h| [HISTORY_OFF, HISTORY_FOREVER].contains(h));
            let reasoned = rule.reason.as_deref().is_some_and(|r| !r.trim().is_empty());
            if (word == Some(HISTORY_FOREVER)) != reasoned {
                return Err(Error::Name {
                    field: "spec.retention.reason",
                    value: rule.entity_type.clone(),
                    reason: "history: forever names why the type keeps all of it, and only \
                             it carries a reason (T-4275)",
                });
            }
            for (field, value) in [
                ("spec.retention.current", &rule.current),
                (
                    "spec.retention.history",
                    &rule.history.clone().filter(|_| word.is_none()),
                ),
            ] {
                if let Some(v) = value.as_deref().filter(|v| retention_seconds(v).is_none()) {
                    return Err(Error::Name {
                        field,
                        value: v.to_owned(),
                        reason: "a retention duration is PT1M to P3650D, as PT5M, PT8H, \
                                 P30D or P1DT12H; history may also be off, or forever with a \
                                 reason (DM-82)",
                    });
                }
            }
        }

        if self.quality.len() > MAX_QUALITY_RULES {
            return Err(Error::Name {
                field: "spec.quality",
                value: self.quality.len().to_string(),
                reason: "a space declares at most 64 quality rules (DM-92)",
            });
        }
        let mut named = std::collections::BTreeSet::new();
        for rule in &self.quality {
            rule.validate()?;
            if !named.insert(rule.name.as_str()) {
                return Err(Error::Name {
                    field: "spec.quality.name",
                    value: rule.name.clone(),
                    reason: "a quality rule's name is unique in the space (DM-87)",
                });
            }
        }

        if let Some(broker) = &self.broker_ref {
            names::validate_dns1123_label(broker)?;
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
        if let (Some(owner), Some(project)) = (
            self.data_model_ref.as_ref().and_then(Ref::namespace),
            meta.namespace.as_deref(),
        ) {
            if owner != project {
                return Err(Error::Name {
                    field: "spec.dataModelRef.namespace",
                    value: owner.to_string(),
                    reason: "a space's data model belongs to its own project; reuse another \
                             project's model through an `imports` entry of this one, at a \
                             pinned version (DM-61)",
                });
            }
        }
        match (&self.urn_segment, &meta.namespace) {
            (Some(pin), _) => names::validate_space_name(pin),
            (None, Some(project)) => {
                names::validate_space_name(&urn_segment(project, &meta.name, None))
            }
            (None, None) => Ok(()),
        }
    }
}
