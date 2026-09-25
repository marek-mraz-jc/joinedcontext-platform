//! `Organization.spec.policies` and `spec.limits`: every organization-wide choice and number,
//! with its default, its built-in bound and the operator's bound (ADR-N-035, PF-96, PF-97).
//!
//! [`CATALOG`] is the one list of numeric entries. Validation, the operator's bounds file and the
//! Portal's settings page all read it, so an entry's default and bound are written once.

use std::collections::BTreeMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::kinds::OrganizationSpec;

fn is_default<T: Default + PartialEq>(value: &T) -> bool {
    *value == T::default()
}

/// `spec.policies`: the organization's choices (ADR-N-035 §3).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct OrganizationPolicies {
    /// What the organization's Apps may be.
    #[serde(default, skip_serializing_if = "is_default")]
    pub apps: AppsPolicy,
    /// How people sign in.
    #[serde(default, skip_serializing_if = "is_default")]
    pub sign_in: SignInPolicy,
    /// What the assistant and agent runs may use.
    #[serde(default, skip_serializing_if = "is_default")]
    pub agents: AgentsPolicy,
}

/// `spec.policies.apps` (PF-103).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct AppsPolicy {
    /// Whether an App may be `public`; `refused` makes every door refuse a Change that sets an
    /// App public, and leaves the Apps already public as they are (PF-100, PF-103).
    #[serde(default, skip_serializing_if = "is_default")]
    pub public: PublicApps,
}

/// Whether the organization's Apps may be public (PF-103).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum PublicApps {
    /// An App may be public.
    #[default]
    Allowed,
    /// No Change may set an App public.
    Refused,
}

/// `spec.policies.signIn`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct SignInPolicy {
    /// The realm's password policy.
    #[serde(default, skip_serializing_if = "is_default")]
    pub password: PasswordPolicy,
}

/// `spec.policies.signIn.password`: written to the realm's password policy.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct PasswordPolicy {
    /// The shortest password the realm accepts; at least 12.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_length: Option<u32>,
    /// How many earlier passwords a new one may not repeat; 0 to 24.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub history: Option<u32>,
}

/// `spec.policies.agents`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct AgentsPolicy {
    /// The models the agent proxy lets a run call. Absent is every model the installation's
    /// agent profiles name; a model outside those is never reachable whatever this lists.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub models: Option<Vec<String>>,
}

impl OrganizationPolicies {
    /// Whether the manifest sets no policy at all.
    pub fn is_unset(&self) -> bool {
        is_default(self)
    }
}

impl OrganizationLimits {
    /// Whether the manifest sets no limit at all.
    pub fn is_unset(&self) -> bool {
        is_default(self)
    }
}

/// `spec.limits`: the organization's numbers (ADR-N-035 §3). Every field is optional; an
/// absent one is its [`CATALOG`] default.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct OrganizationLimits {
    /// What the edge admits.
    #[serde(default, skip_serializing_if = "is_default")]
    pub edge: EdgeLimits,
    /// What the context gateway admits.
    #[serde(default, skip_serializing_if = "is_default")]
    pub gateway: GatewayLimits,
    /// How long a sign-in lasts.
    #[serde(default, skip_serializing_if = "is_default")]
    pub sign_in: SignInLimits,
    /// Invitations.
    #[serde(default, skip_serializing_if = "is_default")]
    pub people: PeopleLimits,
    /// Model spend of the assistant and agent runs.
    #[serde(default, skip_serializing_if = "is_default")]
    pub agents: AgentLimits,
    /// Pipeline outcomes and tests.
    #[serde(default, skip_serializing_if = "is_default")]
    pub pipelines: PipelineLimits,
    /// Uploads and imports.
    #[serde(default, skip_serializing_if = "is_default")]
    pub data: DataLimits,
}

/// `spec.limits.edge`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct EdgeLimits {
    /// Requests a minute per route class.
    #[serde(default, skip_serializing_if = "is_default")]
    pub requests_per_minute: EdgeRates,
    /// The largest request body the edge accepts, in MiB.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_request_body_megabytes: Option<u32>,
}

/// `spec.limits.edge.requestsPerMinute`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct EdgeRates {
    /// The Portal's pages, per address.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub web: Option<u32>,
    /// The Portal API, per token.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api: Option<u32>,
    /// Context-space reads (`GET`, `HEAD`), per token.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data_read: Option<u32>,
    /// Context-space writes (every other method), per token.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data_write: Option<u32>,
    /// A public endpoint, per address.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub public_endpoint: Option<u32>,
}

/// `spec.limits.gateway`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct GatewayLimits {
    /// The largest request body the gateway accepts, in MiB; at most the edge's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_request_body_megabytes: Option<u32>,
}

/// `spec.limits.signIn`: written to the edge session and the realm alike (AP-29).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct SignInLimits {
    /// Minutes without a request before a session ends.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_idle_minutes: Option<u32>,
    /// Hours after sign-in when a session ends whatever happens.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_max_hours: Option<u32>,
}

/// `spec.limits.people`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct PeopleLimits {
    /// Hours an invitation link stays usable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub invitation_hours: Option<u32>,
}

/// `spec.limits.agents`: model spend in whole units of the currency the agent proxy's provider
/// bills in, the day and the month in UTC.
// ponytail: whole currency units; a cap below one unit needs a minor-unit field.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct AgentLimits {
    /// Spend a day; absent is no limit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spend_per_day: Option<u32>,
    /// Spend a month; absent is no limit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spend_per_month: Option<u32>,
}

/// `spec.limits.pipelines`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct PipelineLimits {
    /// Rejected records a pipeline keeps, newest first.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rejected_kept: Option<u32>,
    /// The largest sample a pipeline test takes, in MiB; at most the edge's body.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sample_megabytes: Option<u32>,
}

/// `spec.limits.data`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct DataLimits {
    /// The largest upload or import, in MiB; at most the edge's body.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upload_megabytes: Option<u32>,
}

/// The settings page's sections, in the catalog's order (PF-102).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum Section {
    /// Projects.
    Projects,
    /// Applications.
    Applications,
    /// Edge.
    Edge,
    /// Sign-in.
    SignIn,
    /// People.
    People,
    /// The assistant and agents.
    Agents,
    /// Pipelines and data.
    PipelinesAndData,
}

/// Which way an entry's bound points.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BoundKind {
    /// A number whose bound is a ceiling the operator may set higher or lower.
    Number,
    /// A security setting: the operator may only tighten the built-in range (ADR-N-035 §3.2).
    Security,
}

/// One numeric entry of ADR-N-035's catalog.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Entry {
    /// The manifest path, e.g. `spec.limits.edge.requestsPerMinute.web`.
    pub path: &'static str,
    /// The settings section it is shown in.
    pub section: Section,
    /// The value an absent field has; `None` is no limit.
    pub default: Option<u32>,
    /// The smallest value the catalog allows.
    pub min: u32,
    /// The largest value the catalog allows; `None` leaves the ceiling to the operator.
    pub max: Option<u32>,
    /// Whether the operator may widen the range.
    pub kind: BoundKind,
}

const fn entry(
    path: &'static str,
    section: Section,
    default: Option<u32>,
    min: u32,
    max: Option<u32>,
    kind: BoundKind,
) -> Entry {
    Entry {
        path,
        section,
        default,
        min,
        max,
        kind,
    }
}

/// The edge body entry, which three others may not exceed.
pub const EDGE_BODY: &str = "spec.limits.edge.maxRequestBodyMegabytes";

/// Every numeric entry of ADR-N-035 §3.7 with its default and built-in bound, in the catalog's
/// order. The quota dimensions of `spec.projects.quota` have no default and no built-in ceiling;
/// the operator's bounds file sets one per dimension (PF-73, PF-97).
pub const CATALOG: &[Entry] = &[
    entry(
        "spec.projects.quota.contextSpaces",
        Section::Projects,
        None,
        0,
        None,
        BoundKind::Number,
    ),
    entry(
        "spec.projects.quota.residentPipelines",
        Section::Projects,
        None,
        0,
        None,
        BoundKind::Number,
    ),
    entry(
        "spec.projects.quota.publicEndpoints",
        Section::Projects,
        None,
        0,
        None,
        BoundKind::Number,
    ),
    entry(
        "spec.projects.quota.ingestEventsPerSecond",
        Section::Projects,
        None,
        0,
        None,
        BoundKind::Number,
    ),
    entry(
        "spec.projects.quota.apps",
        Section::Projects,
        None,
        0,
        None,
        BoundKind::Number,
    ),
    entry(
        "spec.projects.quota.agentRunsPerDay",
        Section::Projects,
        None,
        0,
        None,
        BoundKind::Number,
    ),
    entry(
        "spec.projects.quota.entitiesPerSpace",
        Section::Projects,
        None,
        0,
        None,
        BoundKind::Number,
    ),
    entry(
        "spec.projects.quota.requestsPerMinute",
        Section::Projects,
        None,
        0,
        None,
        BoundKind::Number,
    ),
    entry(
        "spec.projects.nameCooldownDays",
        Section::Projects,
        Some(30),
        0,
        Some(365),
        BoundKind::Number,
    ),
    entry(
        "spec.limits.edge.requestsPerMinute.web",
        Section::Edge,
        Some(300),
        1,
        Some(3000),
        BoundKind::Number,
    ),
    entry(
        "spec.limits.edge.requestsPerMinute.api",
        Section::Edge,
        Some(1200),
        1,
        Some(12000),
        BoundKind::Number,
    ),
    entry(
        "spec.limits.edge.requestsPerMinute.dataRead",
        Section::Edge,
        Some(1200),
        1,
        Some(12000),
        BoundKind::Number,
    ),
    entry(
        "spec.limits.edge.requestsPerMinute.dataWrite",
        Section::Edge,
        Some(1200),
        1,
        Some(12000),
        BoundKind::Number,
    ),
    entry(
        "spec.limits.edge.requestsPerMinute.publicEndpoint",
        Section::Edge,
        Some(5000),
        1,
        Some(50000),
        BoundKind::Number,
    ),
    entry(
        EDGE_BODY,
        Section::Edge,
        Some(16),
        1,
        Some(64),
        BoundKind::Number,
    ),
    entry(
        "spec.limits.gateway.maxRequestBodyMegabytes",
        Section::Edge,
        Some(8),
        1,
        Some(64),
        BoundKind::Number,
    ),
    entry(
        "spec.limits.signIn.sessionIdleMinutes",
        Section::SignIn,
        Some(60),
        5,
        Some(480),
        BoundKind::Security,
    ),
    entry(
        "spec.limits.signIn.sessionMaxHours",
        Section::SignIn,
        Some(10),
        1,
        Some(24),
        BoundKind::Security,
    ),
    entry(
        "spec.policies.signIn.password.minLength",
        Section::SignIn,
        Some(12),
        12,
        None,
        BoundKind::Security,
    ),
    entry(
        "spec.policies.signIn.password.history",
        Section::SignIn,
        Some(0),
        0,
        Some(24),
        BoundKind::Number,
    ),
    entry(
        "spec.limits.people.invitationHours",
        Section::People,
        Some(12),
        1,
        Some(168),
        BoundKind::Security,
    ),
    entry(
        "spec.limits.agents.spendPerDay",
        Section::Agents,
        None,
        0,
        None,
        BoundKind::Number,
    ),
    entry(
        "spec.limits.agents.spendPerMonth",
        Section::Agents,
        None,
        0,
        None,
        BoundKind::Number,
    ),
    entry(
        "spec.limits.pipelines.rejectedKept",
        Section::PipelinesAndData,
        Some(1000),
        0,
        Some(10000),
        BoundKind::Number,
    ),
    entry(
        "spec.limits.pipelines.sampleMegabytes",
        Section::PipelinesAndData,
        Some(5),
        1,
        Some(50),
        BoundKind::Number,
    ),
    entry(
        "spec.limits.data.uploadMegabytes",
        Section::PipelinesAndData,
        Some(16),
        1,
        None,
        BoundKind::Number,
    ),
];

/// The entries that may not exceed the edge's body, since the edge refuses anything larger first.
const UNDER_EDGE_BODY: [&str; 3] = [
    "spec.limits.gateway.maxRequestBodyMegabytes",
    "spec.limits.pipelines.sampleMegabytes",
    "spec.limits.data.uploadMegabytes",
];

/// The catalog entry at `path`.
pub fn entry_at(path: &str) -> Option<&'static Entry> {
    CATALOG.iter().find(|entry| entry.path == path)
}

/// One bound of the operator's bounds file (PF-97).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Bound {
    /// The smallest value an organization may set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min: Option<u32>,
    /// The largest value an organization may set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max: Option<u32>,
}

/// The operator's bounds, `portal.organizationBounds` of the deployment, keyed by catalog path;
/// an entry left out keeps its built-in bound (PF-97).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(transparent)]
pub struct OrganizationBounds(pub BTreeMap<String, Bound>);

/// Where an effective value comes from (PF-101).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum Origin {
    /// The organization's manifest sets it.
    Organization,
    /// Nothing sets it: the catalog's default.
    Default,
}

impl OrganizationBounds {
    /// Refuses a path the catalog does not have, a range upside down, and a security bound that
    /// widens the built-in one: the operator may tighten a security setting, never loosen it.
    pub fn validate(&self) -> Result<()> {
        for (path, bound) in &self.0 {
            let Some(entry) = entry_at(path) else {
                return Err(Error::Name {
                    field: "organizationBounds",
                    value: path.clone(),
                    reason: "no entry of the ADR-N-035 catalog has this path (PF-97)",
                });
            };
            if let (Some(min), Some(max)) = (bound.min, bound.max) {
                if min > max {
                    return Err(Error::Name {
                        field: "organizationBounds",
                        value: path.clone(),
                        reason: "min is larger than max",
                    });
                }
            }
            if entry.kind == BoundKind::Security {
                let looser_min = bound.min.is_some_and(|min| min < entry.min);
                let looser_max =
                    matches!((bound.max, entry.max), (Some(max), Some(ceiling)) if max > ceiling);
                if looser_min || looser_max {
                    return Err(Error::Name {
                        field: "organizationBounds",
                        value: path.clone(),
                        reason: "a security setting's bound may only tighten the built-in range \
                                 (ADR-N-035 §3.2)",
                    });
                }
            }
        }
        Ok(())
    }

    /// The bounds a manifest is held to wherever no operator file is at hand (`jcctl validate`,
    /// parsing): the security ranges, which no operator may widen, and no ceiling on a number,
    /// which an operator may raise. The Portal checks the operator's file on every door.
    pub fn safety() -> Self {
        Self(
            CATALOG
                .iter()
                .filter(|entry| entry.kind == BoundKind::Number)
                .map(|entry| {
                    let bound = Bound {
                        min: Some(entry.min),
                        max: Some(u32::MAX),
                    };
                    (entry.path.to_owned(), bound)
                })
                .collect(),
        )
    }

    /// The range an organization may set `entry` in: the operator's bound where it sets one, else
    /// the built-in one.
    pub fn range(&self, entry: &Entry) -> (u32, Option<u32>) {
        let bound = self.0.get(entry.path).copied().unwrap_or_default();
        (bound.min.unwrap_or(entry.min), bound.max.or(entry.max))
    }
}

impl OrganizationSpec {
    /// The value the manifest sets at a catalog path, `None` when it sets none.
    pub fn setting(&self, path: &str) -> Option<u32> {
        let quota = self.projects.quota.as_ref();
        let limits = &self.limits;
        let rates = &limits.edge.requests_per_minute;
        let password = &self.policies.sign_in.password;
        match path {
            "spec.projects.quota.contextSpaces" => quota.and_then(|q| q.context_spaces),
            "spec.projects.quota.residentPipelines" => quota.and_then(|q| q.resident_pipelines),
            "spec.projects.quota.publicEndpoints" => quota.and_then(|q| q.public_endpoints),
            "spec.projects.quota.ingestEventsPerSecond" => {
                quota.and_then(|q| q.ingest_events_per_second)
            }
            "spec.projects.quota.apps" => quota.and_then(|q| q.apps),
            "spec.projects.quota.agentRunsPerDay" => quota.and_then(|q| q.agent_runs_per_day),
            "spec.projects.quota.entitiesPerSpace" => quota.and_then(|q| q.entities_per_space),
            "spec.projects.quota.requestsPerMinute" => quota.and_then(|q| q.requests_per_minute),
            "spec.projects.nameCooldownDays" => self.projects.name_cooldown_days,
            "spec.limits.edge.requestsPerMinute.web" => rates.web,
            "spec.limits.edge.requestsPerMinute.api" => rates.api,
            "spec.limits.edge.requestsPerMinute.dataRead" => rates.data_read,
            "spec.limits.edge.requestsPerMinute.dataWrite" => rates.data_write,
            "spec.limits.edge.requestsPerMinute.publicEndpoint" => rates.public_endpoint,
            EDGE_BODY => limits.edge.max_request_body_megabytes,
            "spec.limits.gateway.maxRequestBodyMegabytes" => {
                limits.gateway.max_request_body_megabytes
            }
            "spec.limits.signIn.sessionIdleMinutes" => limits.sign_in.session_idle_minutes,
            "spec.limits.signIn.sessionMaxHours" => limits.sign_in.session_max_hours,
            "spec.policies.signIn.password.minLength" => password.min_length,
            "spec.policies.signIn.password.history" => password.history,
            "spec.limits.people.invitationHours" => limits.people.invitation_hours,
            "spec.limits.agents.spendPerDay" => limits.agents.spend_per_day,
            "spec.limits.agents.spendPerMonth" => limits.agents.spend_per_month,
            "spec.limits.pipelines.rejectedKept" => limits.pipelines.rejected_kept,
            "spec.limits.pipelines.sampleMegabytes" => limits.pipelines.sample_megabytes,
            "spec.limits.data.uploadMegabytes" => limits.data.upload_megabytes,
            _ => None,
        }
    }

    /// The value in force at a catalog path and where it comes from; `None` is no limit (PF-99).
    pub fn effective(&self, entry: &Entry) -> (Option<u32>, Origin) {
        match self.setting(entry.path) {
            Some(value) => (Some(value), Origin::Organization),
            None => (entry.default, Origin::Default),
        }
    }

    /// Refuses a value outside its range: the operator's bound where the file sets one, else the
    /// built-in one, and the body sizes that exceed the edge's and a session that idles longer
    /// than it lives (PF-96, PF-97). The refusal names the entry, the value and the bound.
    pub fn check_limits(&self, bounds: &OrganizationBounds) -> Result<()> {
        let invalid = |field: &str, reason: String| Error::Invalid {
            field: field.to_owned(),
            reason,
        };
        for entry in CATALOG {
            let Some(value) = self.setting(entry.path) else {
                continue;
            };
            let (min, max) = bounds.range(entry);
            if value < min || max.is_some_and(|max| value > max) {
                let bound = match max {
                    Some(max) => format!("{min} … {max}"),
                    None => format!("at least {min}"),
                };
                return Err(invalid(
                    entry.path,
                    format!("{value} is outside the bound, {bound} (PF-97)"),
                ));
            }
        }
        let value_at = |path: &str| entry_at(path).and_then(|entry| self.effective(entry).0);
        if let Some(edge_body) = value_at(EDGE_BODY) {
            for path in UNDER_EDGE_BODY {
                if let Some(value) = value_at(path).filter(|value| *value > edge_body) {
                    return Err(invalid(
                        path,
                        format!(
                            "{value} MiB is more than the edge's {edge_body} MiB ({EDGE_BODY}), \
                             which refuses a larger body first (ADR-N-035)"
                        ),
                    ));
                }
            }
        }
        if let (Some(idle), Some(hours)) = (
            value_at("spec.limits.signIn.sessionIdleMinutes"),
            value_at("spec.limits.signIn.sessionMaxHours"),
        ) {
            if u64::from(idle) > u64::from(hours) * 60 {
                return Err(invalid(
                    "spec.limits.signIn.sessionIdleMinutes",
                    format!(
                        "{idle} minutes is longer than the session lives, {hours} h \
                         (spec.limits.signIn.sessionMaxHours)"
                    ),
                ));
            }
        }
        if let Some(models) = &self.policies.agents.models {
            if models.is_empty() || models.iter().any(|model| model.trim().is_empty()) {
                return Err(invalid(
                    "spec.policies.agents.models",
                    "name at least one model and no empty one, or leave it out to allow every \
                     model of the installation"
                        .to_owned(),
                ));
            }
        }
        Ok(())
    }
}
