//! `kind: AgentProfile`, defining autonomous agent runtime constraints, model, limits, tools,
//! and network egress permissions (AG-26, AG-47..AG-50).
//!
//! **Security note**: `AgentProfileSpec` and all nested structures carry `deny_unknown_fields`,
//! ensuring no inline secrets, keys, or credentials can be defined within Git manifests.

use crate::envelope::{Kind, ObjectMeta, Scope};
use crate::error::{Error, Result};
use crate::names;
use regex::Regex;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::sync::LazyLock;

static DIGEST_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^sha256:[0-9a-f]{64}$").expect("valid regex"));
static DURATION_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^PT(?:(\d+)H)?(?:(\d+)M)?(?:(\d+)S)?$").expect("valid regex"));
static DNS_HOST_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^[a-zA-Z0-9]([a-zA-Z0-9-]{0,61}[a-zA-Z0-9])?(\.[a-zA-Z0-9]([a-zA-Z0-9-]{0,61}[a-zA-Z0-9])?)*$")
        .expect("valid regex")
});

/// Profile execution role (AG-26, AG-47).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum AgentProfileRole {
    /// Builder profile: coding tools in ephemeral workspace, egress allowlist.
    Builder,
    /// Steward profile: MCP only, no internet egress.
    Steward,
}

/// Container runtime image specification (AG-49).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct AgentRuntime {
    /// Container image repository and tag.
    pub image: String,
    /// Cryptographic container digest (`sha256:...`). Required for admission (AG-49).
    pub digest: String,
}

/// Model provider choice.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum ModelProvider {
    /// Anthropic's Messages API.
    Anthropic,
    /// Any provider speaking the OpenAI chat-completions protocol.
    #[serde(rename = "openai-compatible")]
    OpenaiCompatible,
}

/// Model selection and token budgeting.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct AgentModel {
    /// Which protocol the proxy speaks to the model provider.
    pub provider: ModelProvider,
    /// Model identifier the provider knows.
    pub name: String,
    /// Tokens one run may consume before the proxy ends it (AG-41).
    pub max_tokens_per_run: u64,
    /// How hard the model thinks on every call of a run; absent, no reasoning setting is sent
    /// (AG-72).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<ReasoningEffort>,
}

/// The provider's reasoning setting a run's model calls carry (AG-72).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum ReasoningEffort {
    /// A short think before each answer.
    Low,
    /// The setting the reference `app-builder` profile uses (SDK-26).
    Medium,
    /// The longest think the provider offers for the setting.
    High,
}

/// Computational, execution, and rate bounds per run (AG-41).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct AgentLimits {
    /// Agent steps one run may take before it stops and hands over the last log (AG-51).
    pub steps_per_run: u32,
    /// ISO 8601 duration (e.g. `PT20M`), maximum 1 hour (`PT1H`).
    pub wall_clock: String,
    /// Runs one organization may have in flight at once.
    pub concurrent_runs_per_organization: u32,
    /// Proxied requests per minute per run.
    pub requests_per_minute: u32,
    /// Bytes one proxied response may carry.
    pub max_response_bytes: u64,
}

/// What one run may read from the internet, and how much of it (AG-50, AG-65).
///
/// The default is nothing: a profile that names no host reaches no host, and the workspace has
/// no route of its own to fall back on (AG-34).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct AgentEgress {
    /// Bare hostnames the package and fetch routes may reach; everything else is refused (AG-50).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allowed_hosts: Vec<String>,
    /// Bytes one run may read from those hosts in total, across every fetch (AG-65).
    ///
    /// Absent means [`DEFAULT_EGRESS_BYTES_PER_RUN`]: reading documentation is what the
    /// allow-list is for, and a profile that had to name a number to get it would have every
    /// author guessing one. The cap is what stops a run from mirroring a site, not what stops
    /// it from reading a page.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_bytes_per_run: Option<u64>,
}

/// What a profile that names hosts but no budget gets: enough for the documentation of a
/// handful of libraries, far too little to mirror anything.
pub const DEFAULT_EGRESS_BYTES_PER_RUN: u64 = 16 * 1024 * 1024;

impl AgentEgress {
    /// The run's total byte budget. Zero when the profile names no host, because a budget
    /// without a host is a number nothing can spend.
    pub fn max_bytes_per_run(&self) -> u64 {
        if self.allowed_hosts.is_empty() {
            return 0;
        }
        self.max_bytes_per_run
            .unwrap_or(DEFAULT_EGRESS_BYTES_PER_RUN)
    }
}

/// Available coding and debugging tools in the builder workspace.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum AgentTool {
    /// A shell in the ephemeral workspace.
    Shell,
    /// The Rust toolchain.
    Cargo,
    /// The Node package manager.
    Pnpm,
    /// Git, reaching the forge through the proxy alone.
    Git,
    /// A headless browser for smoke tests.
    Playwright,
}

static OPERATION_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^jc_[a-z0-9_]+$").expect("valid regex"));

/// What an agent may do to a kind of manifest (AG-70).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum KindVerb {
    /// Read manifests of the kind.
    Read,
    /// Propose a change to a manifest of the kind; a person still approves it.
    Propose,
}

/// What an agent may do through an endpoint (AG-70).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum EndpointVerb {
    /// Read entities through the endpoint.
    Read,
    /// Write entities through the endpoint, under its policy.
    Write,
}

/// One kind an agent may reach, and how.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct KindGrant {
    /// A manifest kind of the catalogue, e.g. `Endpoint`.
    pub kind: String,
    /// At least one verb.
    pub verbs: Vec<KindVerb>,
}

/// One endpoint an agent may reach, and how.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct EndpointGrant {
    /// The endpoint's manifest name, a DNS-1123 label.
    pub name: String,
    /// At least one verb.
    pub verbs: Vec<EndpointVerb>,
}

/// The most an agent of this profile may reach; the starting person's own permissions narrow
/// it further at every call, and nothing here widens them (AG-70, MF-40).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct AgentAccess {
    /// Registry operation names (`jc_…`) the agent may call.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub operations: Vec<String>,
    /// Kinds the agent may read or propose changes to.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub kinds: Vec<KindGrant>,
    /// Endpoints the agent may read or write through.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub endpoints: Vec<EndpointGrant>,
}

impl AgentAccess {
    /// The shape MF-40 asks of the block. Whether an operation is registered is the Portal's
    /// check at admission: the registry lives there.
    pub fn validate(&self) -> Result<()> {
        let mut seen = std::collections::BTreeSet::new();
        for operation in &self.operations {
            if !OPERATION_RE.is_match(operation) {
                return Err(Error::Name {
                    field: "access.operations",
                    value: operation.clone(),
                    reason: "an operation name must match ^jc_[a-z0-9_]+$ (MF-40)",
                });
            }
            if !seen.insert(operation.as_str()) {
                return Err(Error::Name {
                    field: "access.operations",
                    value: operation.clone(),
                    reason: "an operation is listed once",
                });
            }
        }
        let mut seen = std::collections::BTreeSet::new();
        for grant in &self.kinds {
            if !crate::registry::KINDS
                .iter()
                .any(|info| info.kind == grant.kind)
            {
                return Err(Error::Name {
                    field: "access.kinds",
                    value: grant.kind.clone(),
                    reason: "not a manifest kind of the catalogue (MF-40)",
                });
            }
            if !seen.insert(grant.kind.as_str()) {
                return Err(Error::Name {
                    field: "access.kinds",
                    value: grant.kind.clone(),
                    reason: "a kind is listed once",
                });
            }
            if grant.verbs.is_empty() {
                return Err(Error::Name {
                    field: "access.kinds.verbs",
                    value: grant.kind.clone(),
                    reason: "a kind grant names at least one verb",
                });
            }
        }
        let mut seen = std::collections::BTreeSet::new();
        for grant in &self.endpoints {
            names::validate_dns1123_label(&grant.name)?;
            if !seen.insert(grant.name.as_str()) {
                return Err(Error::Name {
                    field: "access.endpoints",
                    value: grant.name.clone(),
                    reason: "an endpoint is listed once",
                });
            }
            if grant.verbs.is_empty() {
                return Err(Error::Name {
                    field: "access.endpoints.verbs",
                    value: grant.name.clone(),
                    reason: "an endpoint grant names at least one verb",
                });
            }
        }
        Ok(())
    }
}

/// Workspace hardware resource allocations.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct AgentWorkspace {
    /// CPU request and limit of the workspace container.
    pub cpu: String,
    /// Memory request and limit of the workspace container.
    pub memory: String,
    /// Size of the `emptyDir` the workspace works in.
    pub ephemeral_storage: String,
}

/// Specification for a `kind: AgentProfile` resource (AG-47…AG-50).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct AgentProfileSpec {
    /// What the profile is allowed to be used for (AG-26).
    pub role: AgentProfileRole,
    /// The runtime image a run of this profile executes.
    pub runtime: AgentRuntime,
    /// The model the proxy calls on the run's behalf.
    pub model: AgentModel,
    /// The bounds every run of this profile is held to.
    pub limits: AgentLimits,
    /// Where the package route may go.
    pub egress: AgentEgress,
    /// Tools the workspace carries; a steward profile carries none.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<AgentTool>,
    /// Resources the workspace container gets.
    pub workspace: AgentWorkspace,
    /// What an agent of this profile may reach. Absent: read-only operations and nothing else
    /// (AG-70).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub access: Option<AgentAccess>,
}

impl Kind for AgentProfileSpec {
    const KIND: &'static str = "AgentProfile";
    const PLURAL: &'static str = "agentprofiles";
    const SCOPE: Scope = Scope::Organization;
    const PATH_TEMPLATE: &'static str = "agentprofiles/{name}.yaml";

    fn validate_spec(&self, meta: &ObjectMeta) -> Result<()> {
        names::validate_dns1123_label(&meta.name)?;
        self.validate()
    }
}

impl AgentProfileSpec {
    /// Validates runtime digest, limits, duration, tools, and host allowlists.
    pub fn validate(&self) -> Result<()> {
        if self.runtime.image.trim().is_empty() {
            return Err(Error::Name {
                field: "runtime.image",
                value: self.runtime.image.clone(),
                reason: "image must not be empty",
            });
        }
        if !DIGEST_RE.is_match(&self.runtime.digest) {
            return Err(Error::Name {
                field: "runtime.digest",
                value: self.runtime.digest.clone(),
                reason: "digest must match ^sha256:[0-9a-f]{64}$ (AG-49)",
            });
        }

        if self.model.name.trim().is_empty() {
            return Err(Error::Name {
                field: "model.name",
                value: self.model.name.clone(),
                reason: "model name must not be empty",
            });
        }
        if self.model.max_tokens_per_run == 0 {
            return Err(Error::Name {
                field: "model.maxTokensPerRun",
                value: "0".to_string(),
                reason: "maxTokensPerRun must be greater than 0",
            });
        }

        if self.limits.steps_per_run == 0 {
            return Err(Error::Name {
                field: "limits.stepsPerRun",
                value: "0".to_string(),
                reason: "stepsPerRun must be greater than 0",
            });
        }
        if self.limits.concurrent_runs_per_organization == 0 {
            return Err(Error::Name {
                field: "limits.concurrentRunsPerOrganization",
                value: "0".to_string(),
                reason: "concurrentRunsPerOrganization must be greater than 0",
            });
        }
        if self.limits.requests_per_minute == 0 {
            return Err(Error::Name {
                field: "limits.requestsPerMinute",
                value: "0".to_string(),
                reason: "requestsPerMinute must be greater than 0",
            });
        }
        if self.limits.max_response_bytes == 0 {
            return Err(Error::Name {
                field: "limits.maxResponseBytes",
                value: "0".to_string(),
                reason: "maxResponseBytes must be greater than 0",
            });
        }

        let wall_clock_secs = parse_iso_duration(&self.limits.wall_clock).ok_or(Error::Name {
            field: "limits.wallClock",
            value: self.limits.wall_clock.clone(),
            reason: "wallClock must be a valid ISO 8601 duration matching ^PT(?:(\\d+)H)?(?:(\\d+)M)?(?:(\\d+)S)?$",
        })?;

        if wall_clock_secs == 0 || wall_clock_secs > 3600 {
            return Err(Error::Name {
                field: "limits.wallClock",
                value: self.limits.wall_clock.clone(),
                reason:
                    "wallClock duration must be greater than 0 and at most 1 hour (PT1H, AG-41)",
            });
        }

        for host in &self.egress.allowed_hosts {
            if host.contains('/')
                || host.contains(':')
                || host.contains('*')
                || !DNS_HOST_RE.is_match(host)
            {
                return Err(Error::Name {
                    field: "egress.allowedHosts",
                    value: host.clone(),
                    reason: "allowed host must be a bare hostname without scheme, port, path, or wildcard (AG-50)",
                });
            }
        }

        if self.egress.max_bytes_per_run == Some(0) {
            return Err(Error::Name {
                field: "egress.maxBytesPerRun",
                value: "0".to_string(),
                reason: "a budget of zero is an allow-list nothing can use; leave the field out \
                         to take the default, or drop allowedHosts to have no egress (AG-65)",
            });
        }

        match self.role {
            AgentProfileRole::Steward => {
                if self.egress.max_bytes_per_run.is_some() {
                    return Err(Error::Name {
                        field: "egress.maxBytesPerRun",
                        value: "declared".to_string(),
                        reason: "steward profile has no internet access; a byte budget would \
                                 suggest it has (AG-26)",
                    });
                }
                if !self.tools.is_empty() {
                    return Err(Error::Name {
                        field: "tools",
                        value: format!("{} tools declared", self.tools.len()),
                        reason: "steward profile cannot carry coding tools (AG-26)",
                    });
                }
                if !self.egress.allowed_hosts.is_empty() {
                    return Err(Error::Name {
                        field: "egress.allowedHosts",
                        value: format!("{} hosts declared", self.egress.allowed_hosts.len()),
                        reason: "steward profile has no internet access; allowedHosts must be empty (AG-26)",
                    });
                }
            }
            AgentProfileRole::Builder => {
                if self.egress.allowed_hosts.is_empty() {
                    return Err(Error::Name {
                        field: "egress.allowedHosts",
                        value: "empty".to_string(),
                        reason: "builder profile requires an egress allow-list; allowedHosts must not be empty (AG-26, AG-50)",
                    });
                }
            }
        }

        if let Some(access) = &self.access {
            access.validate()?;
        }

        if self.workspace.cpu.trim().is_empty() || self.workspace.memory.trim().is_empty() {
            return Err(Error::Name {
                field: "workspace",
                value: "".to_string(),
                reason: "workspace cpu and memory limits must be specified",
            });
        }

        Ok(())
    }
}

/// Parses an ISO 8601 duration in format `PT[#H][#M][#S]` into total seconds.
pub fn parse_iso_duration(raw: &str) -> Option<u64> {
    let caps = DURATION_RE.captures(raw)?;
    let h: u64 = caps.get(1).map_or(0, |m| m.as_str().parse().unwrap_or(0));
    let m: u64 = caps.get(2).map_or(0, |m| m.as_str().parse().unwrap_or(0));
    let s: u64 = caps.get(3).map_or(0, |m| m.as_str().parse().unwrap_or(0));
    Some(h * 3600 + m * 60 + s)
}
