//! Manifest kinds for Bento data pipelines and CronJob schedules (T-0116, PL-01..PL-28).

use crate::envelope::{Kind, ObjectMeta, Ref, Scope, SecretRef};
use crate::error::{Error, Result};
use crate::names;
use crate::urn::Urn;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::fmt;
use std::sync::LazyLock;

/// A Bento duration: a positive count and one of the units Bento accepts (PL-26, PL-27).
static PERIOD_RE: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"^[1-9][0-9]*(ms|s|m|h)$").expect("valid regex"));

/// The cadence at which a scheduled run stops being worth a pod start (PL-26).
pub const RESIDENT_BELOW_SECONDS: u64 = 30;

/// Returns whether a pipeline executes as a scheduled CronJob (PL-04, PL-26).
///
/// An explicit class wins; `auto` runs scheduled when `period >= 30s`.
fn is_scheduled(spec: &PipelineSpec) -> bool {
    match spec.class {
        PipelineClass::Scheduled => true,
        PipelineClass::Resident => false,
        PipelineClass::Auto => match spec.period_seconds() {
            Some(seconds) => seconds >= RESIDENT_BELOW_SECONDS,
            None => false,
        },
    }
}

/// Desired specification of a [`Pipeline`][crate::kinds::Pipeline] resource (PL-01..PL-28).
///
/// Two shapes (PL-54, ADR-N-023): `v1alpha1` holds one `source`, one `compute`, one `output`
/// and `targetEndpoint`; `v1alpha2` holds `sources`, `steps` and `outputs`. The accessors
/// [`PipelineSpec::sources`], [`PipelineSpec::steps`] and [`PipelineSpec::outputs`] read
/// either as the second, so nothing downstream has to know which one a file was written in.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct PipelineSpec {
    /// Operational execution class (PL-04, PL-05).
    pub class: PipelineClass,
    /// Whether the pipeline runs (PL-40). `false` pauses it: the reconciler leaves a resident
    /// stream out of the runner and suspends a scheduled CronJob, the manifest stays, and a
    /// Resume is the flag flipped back. Absent means `true`, and `true` is not written back.
    #[serde(default = "default_true", skip_serializing_if = "is_true")]
    pub enabled: bool,
    /// Standard cron schedule expression for scheduled runs (PL-04, PL-26..PL-28).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schedule: Option<String>,
    /// How often the pipeline polls its source, as a Bento duration (`15s`, `5m`).
    ///
    /// The reconciler reads it to pick the class (PL-26) and copies it into the runner's
    /// `input.generate.interval` (PL-27). Absent means push-based: an MQTT or
    /// subscription input driven by its source rather than by a clock.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub period: Option<String>,
    /// Optional context source query or subscription trigger for derived pipelines (PL-31).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<PipelineSource>,
    /// Optional compute execution specification (PL-33).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compute: Option<Compute>,
    /// Target endpoint URN through which writes occur (PF-39, PL-18); `v1alpha1` only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_endpoint: Option<Urn>,
    /// Optional output entity type and write mode.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<Output>,
    /// The author knows this resident pipeline triggers itself and accepts it (PL-37).
    ///
    /// A derived pipeline whose output type is the type its own subscription watches feeds its
    /// own trigger, and the reconciler refuses it. Setting this says the loop is deliberate,
    /// which is why PL-37 puts the change in the yellow lane: a person reviews it.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub allow_feedback: bool,
    /// The inputs, merged by a Bento `broker` when there are several (PL-52, PL-53); `v1alpha2`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sources: Vec<PipelineSource>,
    /// The processors, in the order they run (PL-52); `v1alpha2`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub steps: Vec<Step>,
    /// Where the result is written, each through its own Endpoint (PL-52, PL-55); `v1alpha2`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub outputs: Vec<PipelineOutput>,
    /// Secret references injected into runner environments (PL-14..PL-16).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub secret_refs: Vec<SecretRef>,
    /// Resource quotas allocated to this pipeline runner (PL-11).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quotas: Option<PipelineQuotas>,
}

fn default_true() -> bool {
    true
}

fn is_true(value: &bool) -> bool {
    *value
}

/// Operational execution class of a data pipeline (PL-04, PL-05).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum PipelineClass {
    /// Reconciler automatically determines class from cadence or input mode (PL-05, PL-26).
    Auto,
    /// Continuous process hosted inside project pipeline runner deployment.
    Resident,
    /// Ephemeral Kubernetes CronJob executed periodically.
    Scheduled,
}

impl PipelineClass {
    /// Returns the kebab-case wire name for this pipeline class.
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Resident => "resident",
            Self::Scheduled => "scheduled",
        }
    }
}

impl fmt::Display for PipelineClass {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

/// Source specification for derived pipelines (PL-31).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct PipelineSource {
    /// Reference to source Endpoint for reading context entities.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint_ref: Option<Ref>,
    /// Query parameters for pulling entities.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub query: Option<SourceQuery>,
    /// Event subscription trigger for resident processing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trigger: Option<Trigger>,
    /// Reference to the `DataSource` whose connection becomes this pipeline's Bento input
    /// (PL-39, MF-35). Excludes `endpointRef`: a pipeline reads the outside world or the
    /// platform's own spaces, not both in one input.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data_source_ref: Option<Ref>,
}

impl PipelineSource {
    /// Validates the input side of a pipeline (PL-31, PL-39).
    ///
    /// One input: either the platform's own spaces through an Endpoint, or the outside world
    /// through a `DataSource`. A manifest naming both describes two inputs and the reconciler
    /// would have to choose one, so it is refused here instead.
    pub fn validate(&self) -> Result<()> {
        let Some(reference) = &self.data_source_ref else {
            return Ok(());
        };
        if self.endpoint_ref.is_some() {
            return Err(Error::Name {
                field: "spec.source.dataSourceRef",
                value: reference.name().to_owned(),
                reason: "a pipeline reads a DataSource or an Endpoint, not both (PL-39)",
            });
        }
        if let Some(kind) = reference.kind() {
            if kind != "DataSource" {
                return Err(Error::Kind {
                    expected: "DataSource",
                    got: kind.to_string(),
                });
            }
        }
        names::validate_dns1123_label(reference.name())
    }
}

/// NGSI-LD query parameters for pipeline source entity retrieval.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct SourceQuery {
    /// Target entity type short name.
    #[serde(rename = "type", default, skip_serializing_if = "Option::is_none")]
    pub entity_type: Option<String>,
    /// Attributes projection list.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attrs: Vec<String>,
    /// The entities to read instead of the whole type (PL-42): PF-42 URNs whose `{Type}` is
    /// `type` when both are set. Rendered as the `id=` parameter of the fetch.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ids: Vec<Urn>,
    /// Entity query filter string.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub q: Option<String>,
    /// Scope query filter string.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope_q: Option<String>,
    /// Geographic query filter string.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub geo_q: Option<String>,
    /// Temporal window constraint.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temporal_q: Option<TemporalWindow>,
}

/// ISO 8601 temporal window for source queries.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct TemporalWindow {
    /// ISO 8601 duration string (e.g. `P1D`).
    pub window: String,
}

/// Subscription trigger for resident pipelines (PL-31).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Trigger {
    /// Underlying context subscription details.
    pub subscription: SubscriptionTrigger,
}

/// Subscription configuration triggering pipeline execution.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct SubscriptionTrigger {
    /// Target entity type name.
    #[serde(rename = "type")]
    pub entity_type: String,
    /// Whitelist of watched attribute names.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub watched_attributes: Vec<String>,
}

/// Compute execution specification (PL-33).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Compute {
    /// Engine kind executing compute logic.
    pub kind: ComputeKind,
    /// Path or package of the compute module (for wasm).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub module: Option<String>,
    /// Entry point function name (for wasm).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub function: Option<String>,
    /// Reference to a [`Mapping`][crate::kinds::Mapping] resource.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mapping_ref: Option<Ref>,
    /// The inline mapping of a `bloblang` step (PL-41): the reconciler renders it as the last
    /// `mapping` processor of the generated `bento.yaml`. Absent means the author keeps the
    /// mapping in `bento.yaml`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bloblang: Option<String>,
}

/// Execution technology category for pipeline compute steps (PL-33).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum ComputeKind {
    /// Inline Bento Bloblang processor.
    Bloblang,
    /// Compiled LinkML-Map mapping processor (PL-29).
    Mapping,
    /// Sandboxed WebAssembly WASI module (PL-34).
    Wasm,
    /// Ephemeral containerized job (PL-35).
    Container,
}

impl ComputeKind {
    /// Returns the kebab-case wire name for this compute kind.
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Bloblang => "bloblang",
            Self::Mapping => "mapping",
            Self::Wasm => "wasm",
            Self::Container => "container",
        }
    }
}

impl fmt::Display for ComputeKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

/// Output specification for derived pipeline computations (PL-32).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Output {
    /// Target entity type name produced.
    #[serde(rename = "type")]
    pub entity_type: String,
    /// Write mode applied to target context space.
    pub mode: OutputMode,
}

/// Mutation mode for pipeline output writers (PL-32).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum OutputMode {
    /// Create or replace complete entity.
    Upsert,
    /// Update or append attributes onto existing entity.
    UpdateAttrs,
}

impl OutputMode {
    /// Returns the kebab-case wire name for this output mode.
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Upsert => "upsert",
            Self::UpdateAttrs => "update-attrs",
        }
    }
}

impl fmt::Display for OutputMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

/// One step of a `v1alpha2` pipeline (PL-52): a compute kind of PL-33, or one processor the
/// pinned runner ships, its configuration verbatim in the processor's own field names.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum Step {
    /// `processor: { <name>: <config> }`.
    Processor(ProcessorStep),
    /// `kind: bloblang | mapping | wasm | container` with that kind's fields.
    Compute(Compute),
}

/// A step that names a runner processor (PL-52).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProcessorStep {
    /// Exactly one entry: the processor's name and its configuration.
    pub processor: std::collections::BTreeMap<String, serde_json::Value>,
}

/// One output of a `v1alpha2` pipeline (PL-52): the Endpoint written through and how.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct PipelineOutput {
    /// The Endpoint the writes go through (PF-39, PL-18).
    pub target_endpoint: Urn,
    /// The entity type written, when the pipeline names one.
    #[serde(rename = "type", default, skip_serializing_if = "Option::is_none")]
    pub entity_type: Option<String>,
    /// Write mode; absent is `upsert`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<OutputMode>,
}

/// Resource quotas for a pipeline runner (PL-11).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct PipelineQuotas {
    /// Maximum allowed memory allocation in megabytes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_memory_mb: Option<u32>,
    /// CPU allocation in millicores.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu_millicores: Option<u32>,
}

impl Kind for PipelineSpec {
    const KIND: &'static str = "Pipeline";
    const PLURAL: &'static str = "pipelines";
    const SCOPE: Scope = Scope::Project;
    const PATH_TEMPLATE: &'static str = "projects/{project}/pipelines/{name}/pipeline.yaml";

    fn validate_spec(&self, meta: &ObjectMeta) -> Result<()> {
        names::validate_dns1123_label(&meta.name)?;
        self.validate()
    }

    fn validate_api_version(&self, api_version: &str) -> Result<()> {
        let second = api_version == crate::envelope::API_VERSION_V1ALPHA2;
        match (second, self.is_second_shape(), self.has_first_shape()) {
            (true, _, true) => Err(Error::Invalid {
                field: "spec".to_owned(),
                reason: "a `joinedcontext.com/v1alpha2` Pipeline carries `sources`, `steps` and \
                         `outputs`, never `source`, `compute`, `output` or `targetEndpoint` (PL-52)"
                    .to_owned(),
            }),
            (false, true, _) => Err(Error::Invalid {
                field: "spec".to_owned(),
                reason: "`sources`, `steps` and `outputs` need `apiVersion: joinedcontext.com/v1alpha2` (PL-54)"
                    .to_owned(),
            }),
            _ => Ok(()),
        }
    }
}

impl PipelineSpec {
    /// The polling period in whole seconds, `None` when the pipeline is push-based or the
    /// period does not parse (PL-26).
    ///
    /// A sub-second period floors to zero, which is still shorter than thirty seconds and
    /// therefore still resident.
    pub fn period_seconds(&self) -> Option<u64> {
        let period = self.period.as_deref()?;
        let split = period.find(|c: char| !c.is_ascii_digit())?;
        let (count, unit) = period.split_at(split);
        let count: u64 = count.parse().ok()?;
        match unit {
            "ms" => Some(count / 1000),
            "s" => Some(count),
            "m" => Some(count * 60),
            "h" => Some(count * 3600),
            _ => None,
        }
    }

    /// Returns whether this pipeline executes as a scheduled CronJob (PL-04, PL-26).
    pub fn is_scheduled(&self) -> bool {
        is_scheduled(self)
    }

    /// Whether the spec is written in the `v1alpha2` shape: any of `sources`, `steps`, `outputs`.
    pub fn is_second_shape(&self) -> bool {
        !self.sources.is_empty() || !self.steps.is_empty() || !self.outputs.is_empty()
    }

    fn has_first_shape(&self) -> bool {
        self.source.is_some()
            || self.compute.is_some()
            || self.output.is_some()
            || self.target_endpoint.is_some()
    }

    /// The inputs, in either shape (PL-54): `v1alpha1`'s one `source`, or none when its input
    /// lives in `bento.yaml`.
    pub fn sources(&self) -> Vec<PipelineSource> {
        match self.is_second_shape() {
            true => self.sources.clone(),
            false => self.source.clone().into_iter().collect(),
        }
    }

    /// The steps, in either shape (PL-54): `v1alpha1`'s `compute` as the one step.
    pub fn steps(&self) -> Vec<Step> {
        match self.is_second_shape() {
            true => self.steps.clone(),
            false => self
                .compute
                .clone()
                .map(Step::Compute)
                .into_iter()
                .collect(),
        }
    }

    /// The outputs, in either shape (PL-54): `v1alpha1`'s `targetEndpoint` with its `output`.
    pub fn outputs(&self) -> Vec<PipelineOutput> {
        match self.is_second_shape() {
            true => self.outputs.clone(),
            false => self
                .target_endpoint
                .clone()
                .map(|target_endpoint| PipelineOutput {
                    target_endpoint,
                    entity_type: self.output.as_ref().map(|out| out.entity_type.clone()),
                    mode: self.output.as_ref().map(|out| out.mode),
                })
                .into_iter()
                .collect(),
        }
    }

    /// The first output's Endpoint: where a one-output pipeline writes.
    pub fn target(&self) -> Option<Urn> {
        self.outputs()
            .into_iter()
            .next()
            .map(|out| out.target_endpoint)
    }

    /// Validates class scheduling, target endpoint, compute engine, and resource bounds.
    pub fn validate(&self) -> Result<()> {
        if self.is_second_shape() && self.has_first_shape() {
            return Err(Error::Invalid {
                field: "spec".to_owned(),
                reason: "a pipeline is written with `sources`, `steps` and `outputs` or with \
                         `source`, `compute`, `output` and `targetEndpoint`, not both (PL-52)"
                    .to_owned(),
            });
        }
        if self.is_second_shape() {
            self.validate_second_shape()?;
        } else {
            let Some(target) = &self.target_endpoint else {
                return Err(Error::Name {
                    field: "spec.targetEndpoint",
                    value: String::new(),
                    reason: "targetEndpoint is required",
                });
            };
            validate_target(target)?;
            if let Some(source) = &self.source {
                validate_source(source)?;
            }
            if let Some(compute) = &self.compute {
                validate_compute(compute)?;
            }
            if let Some(ref out) = self.output {
                names::validate_entity_type(&out.entity_type)?;
            }
        }

        match self.class {
            PipelineClass::Scheduled => {
                if self.schedule.is_none() {
                    return Err(Error::Name {
                        field: "spec.schedule",
                        value: String::new(),
                        reason: "schedule is required when class is `scheduled`",
                    });
                }
            }
            PipelineClass::Resident => {
                if let Some(ref s) = self.schedule {
                    return Err(Error::Name {
                        field: "spec.schedule",
                        value: s.clone(),
                        reason: "schedule must not be present when class is `resident`",
                    });
                }
            }
            PipelineClass::Auto => {}
        }

        if let Some(ref period) = self.period {
            if !PERIOD_RE.is_match(period) {
                return Err(Error::Name {
                    field: "spec.period",
                    value: period.clone(),
                    reason: "period must be a Bento duration such as `250ms`, `15s`, `5m` or `1h`",
                });
            }
        }

        if let Some(ref sched) = self.schedule {
            // ponytail: we validate the cron expression by checking exactly five whitespace-separated
            // fields. Full cron syntax and range parsing is performed downstream by the Kubernetes
            // CronJob controller or Bento runner.
            let fields: Vec<&str> = sched.split_whitespace().collect();
            if fields.len() != 5 {
                return Err(Error::Name {
                    field: "spec.schedule",
                    value: sched.clone(),
                    reason: "schedule must contain exactly 5 whitespace-separated fields (standard cron expression)",
                });
            }
        }

        if let Some(ref q) = self.quotas {
            if let Some(mem) = q.max_memory_mb {
                if mem == 0 {
                    return Err(Error::Name {
                        field: "spec.quotas.maxMemoryMb",
                        value: "0".to_string(),
                        reason: "maxMemoryMb must be >= 1",
                    });
                }
            }
            if let Some(cpu) = q.cpu_millicores {
                if cpu == 0 {
                    return Err(Error::Name {
                        field: "spec.quotas.cpuMillicores",
                        value: "0".to_string(),
                        reason: "cpuMillicores must be >= 1",
                    });
                }
            }
        }

        for sref in &self.secret_refs {
            names::validate_dns1123_label(&sref.name)?;
        }

        Ok(())
    }
}

/// A write target is an Endpoint (PF-39, PL-18).
fn validate_target(target: &Urn) -> Result<()> {
    if target.entity_type() != "Endpoint" {
        return Err(Error::Kind {
            expected: "Endpoint",
            got: target.entity_type().to_string(),
        });
    }
    Ok(())
}

/// One input: its own rule (PL-39) and its query, trigger and reference (PL-31, PL-42).
fn validate_source(src: &PipelineSource) -> Result<()> {
    src.validate()?;

    if let Some(ref q) = src.query {
        if let Some(ref et) = q.entity_type {
            names::validate_entity_type(et)?;
        }
        if let Some(ref et) = q.entity_type {
            if let Some(other) = q.ids.iter().find(|id| id.entity_type() != et) {
                return Err(Error::Kind {
                    expected: "query.type",
                    got: format!("{} in query.ids ({other})", other.entity_type()),
                });
            }
        }
    }
    if let Some(ref tr) = src.trigger {
        names::validate_entity_type(&tr.subscription.entity_type)?;
    }
    if let Some(ref er) = src.endpoint_ref {
        names::validate_dns1123_label(er.name())?;
        if let Some(kind) = er.kind() {
            if kind != "Endpoint" {
                return Err(Error::Kind {
                    expected: "Endpoint",
                    got: kind.to_string(),
                });
            }
        }
    }
    Ok(())
}

/// One compute step (PL-33, PL-34, PL-41).
/// Every string a processor step's configuration holds, however deeply nested.
///
/// A processor carries Bloblang in two places: a `mapping` or `mutation` body, and the
/// `${! … }` interpolation any field may hold. Both are strings, so both are scanned with the
/// same eye as an inline mapping.
fn config_strings(value: &serde_json::Value, found: &mut Vec<String>) {
    match value {
        serde_json::Value::String(text) => found.push(text.clone()),
        serde_json::Value::Array(items) => {
            for item in items {
                config_strings(item, found);
            }
        }
        serde_json::Value::Object(fields) => {
            for field in fields.values() {
                config_strings(field, found);
            }
        }
        _ => {}
    }
}

/// The names an author's mapping may read out of the runner (PL-57, PF-84).
///
/// - `JC_ORG_DOMAIN` — the organisation's domain, a variable of the runner's process.
/// - `JC_SPACE`, `JC_SPACE_2`, … — the space segment of each output, which the renderer writes
///   into the stream before it reaches the runner.
/// - `JC_SOURCE_SPACE` — the space an indicator pipeline records its provenance from.
///
/// None of the four names a credential, and everything else in that environment does.
fn is_injected_env(name: &str) -> bool {
    name == "JC_ORG_DOMAIN"
        || name == "JC_SOURCE_SPACE"
        || name == "JC_SPACE"
        || name
            .strip_prefix("JC_SPACE_")
            .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
}

/// The Bloblang functions that read the runner's host rather than the message (PL-16, PL-50).
const HOST_FUNCTIONS: &[&str] = &["file", "file_json", "file_rel", "hostname", "env"];

/// Refuses a mapping that reads the runner's environment or its filesystem (PL-16, PL-50, PL-18).
///
/// A project's runner holds every pipeline of that project in one process: the `pipeline-secrets`
/// environment with each source's credential under its own `envVar`, the runner's own
/// `JC_CLIENT_SECRET` for the gateway, and the `/data` volume every file source reads. A
/// `DataSource` cannot name any of that — `${VAR}` has to name an entry of its own
/// `spec.secrets` — and a mapping must not be the second door into it: `env("…")` reads only
/// what the reconciler injects for it, and `file(…)` reads nothing at all, because whatever a
/// mapping reads it writes to the pipeline's own target.
fn validate_author_bloblang(field: &'static str, mapping: &str) -> Result<()> {
    for (function, argument) in host_calls(mapping) {
        if function == "env" {
            match argument.as_deref() {
                Some(name) if is_injected_env(name) => continue,
                Some(name) => {
                    return Err(Error::Name {
                        field,
                        value: format!("env(\"{name}\")"),
                        reason: "a mapping reads only the variables the platform injects for \
                                 it: JC_ORG_DOMAIN, JC_SOURCE_SPACE and the JC_SPACE of each \
                                 output. The rest of the runner's environment is the project's \
                                 credentials (PL-16, PL-50)",
                    });
                }
                None => {
                    return Err(Error::Name {
                        field,
                        value: "env(…)".to_owned(),
                        reason: "the name a mapping reads with env() is a literal string, so \
                                 what it reads can be checked before it runs (PL-16, PL-50)",
                    });
                }
            }
        }
        return Err(Error::Name {
            field,
            value: format!("{function}(…)"),
            reason: "a mapping reads the message, never the runner it runs on: the runner's \
                     files volume and its host carry the whole project's data and credentials \
                     (PL-16, PL-18, PL-50)",
        });
    }
    Ok(())
}

/// Every call an author's mapping makes to one of [`HOST_FUNCTIONS`], with its first argument
/// when that argument is a plain string literal.
///
/// The scan walks the text rather than matching it, so a `#` comment and the inside of a string
/// are not mistaken for code — a mapping that *mentions* `file(` in a comment is not a mapping
/// that calls it — and a name is only a call when a `(` follows it and no `.` precedes it, which
/// leaves Bloblang's methods (`this.foo.format(…)`) alone.
fn host_calls(mapping: &str) -> Vec<(String, Option<String>)> {
    let bytes = mapping.as_bytes();
    let mut calls = Vec::new();
    let mut i = 0;
    let mut previous_was_dot = false;
    while i < bytes.len() {
        let byte = bytes[i];
        match byte {
            b'#' => {
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
                previous_was_dot = false;
            }
            b'"' | b'\'' => {
                i = skip_string(bytes, i).unwrap_or(bytes.len());
                previous_was_dot = false;
            }
            b'.' => {
                previous_was_dot = true;
                i += 1;
            }
            b if b.is_ascii_alphabetic() || b == b'_' => {
                let start = i;
                while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
                    i += 1;
                }
                let name = &mapping[start..i];
                let mut after = i;
                while after < bytes.len() && (bytes[after] == b' ' || bytes[after] == b'\t') {
                    after += 1;
                }
                let is_call = bytes.get(after) == Some(&b'(');
                if is_call && !previous_was_dot && HOST_FUNCTIONS.contains(&name) {
                    calls.push((name.to_owned(), literal_argument(mapping, after + 1)));
                }
                previous_was_dot = false;
            }
            b' ' | b'\t' | b'\r' | b'\n' => i += 1,
            _ => {
                previous_was_dot = false;
                i += 1;
            }
        }
    }
    calls
}

/// The index just past the string literal starting at `open`, or `None` when it is never closed.
fn skip_string(bytes: &[u8], open: usize) -> Option<usize> {
    let quote = bytes[open];
    let mut i = open + 1;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' if quote == b'"' => i += 2,
            b if b == quote => return Some(i + 1),
            _ => i += 1,
        }
    }
    None
}

/// The first argument of a call opening at `from`, when it is one plain string literal and
/// nothing else: `env("JC_SPACE")` yields the name, `env(this.pick)` and `env("A" + "B")`
/// yield `None`, so a name that cannot be read here is refused rather than allowed.
fn literal_argument(mapping: &str, from: usize) -> Option<String> {
    let bytes = mapping.as_bytes();
    let mut i = from;
    while i < bytes.len() && (bytes[i] == b' ' || bytes[i] == b'\t') {
        i += 1;
    }
    if bytes.get(i) != Some(&b'"') && bytes.get(i) != Some(&b'\'') {
        return None;
    }
    let end = skip_string(bytes, i)?;
    let mut after = end;
    while after < bytes.len() && (bytes[after] == b' ' || bytes[after] == b'\t') {
        after += 1;
    }
    if bytes.get(after) != Some(&b')') {
        return None;
    }
    let raw = &mapping[i + 1..end - 1];
    if raw.contains('\\') {
        return None;
    }
    Some(raw.to_owned())
}

fn validate_compute(c: &Compute) -> Result<()> {
    if let Some(ref mapping) = c.bloblang {
        if c.kind != ComputeKind::Bloblang {
            return Err(Error::Name {
                field: "spec.compute.bloblang",
                value: c.kind.to_string(),
                reason: "bloblang is only valid when compute.kind is `bloblang`",
            });
        }
        if mapping.trim().is_empty() {
            return Err(Error::Name {
                field: "spec.compute.bloblang",
                value: String::new(),
                reason: "bloblang must not be empty; leave the field out to keep the mapping in bento.yaml",
            });
        }
        validate_author_bloblang("spec.compute.bloblang", mapping)?;
    }
    match c.kind {
        ComputeKind::Wasm => {
            if c.module.is_none() {
                return Err(Error::Name {
                    field: "spec.compute.module",
                    value: String::new(),
                    reason: "module is required when compute.kind is `wasm`",
                });
            }
            if c.function.is_none() {
                return Err(Error::Name {
                    field: "spec.compute.function",
                    value: String::new(),
                    reason: "function is required when compute.kind is `wasm`",
                });
            }
        }
        ComputeKind::Mapping => {
            if c.mapping_ref.is_none() {
                return Err(Error::Name {
                    field: "spec.compute.mappingRef",
                    value: String::new(),
                    reason: "mappingRef is required when compute.kind is `mapping`",
                });
            }
            if let Some(ref m) = c.module {
                return Err(Error::Name {
                    field: "spec.compute.module",
                    value: m.clone(),
                    reason: "module is forbidden when compute.kind is `mapping`",
                });
            }
            if let Some(ref f) = c.function {
                return Err(Error::Name {
                    field: "spec.compute.function",
                    value: f.clone(),
                    reason: "function is forbidden when compute.kind is `mapping`",
                });
            }
        }
        _ => {}
    }

    if let Some(ref mr) = c.mapping_ref {
        names::validate_dns1123_label(mr.name())?;
        if let Some(kind) = mr.kind() {
            if kind != "Mapping" {
                return Err(Error::Kind {
                    expected: "Mapping",
                    got: kind.to_string(),
                });
            }
        }
    }
    Ok(())
}

impl PipelineSpec {
    /// The `v1alpha2` rules (PL-52, PL-55): at least one source and one output, each checked
    /// as its `v1alpha1` counterpart is; a processor the runner ships; a `container` step alone
    /// in a `scheduled` pipeline.
    fn validate_second_shape(&self) -> Result<()> {
        if self.sources.is_empty() {
            return Err(Error::Name {
                field: "spec.sources",
                value: String::new(),
                reason: "a pipeline reads at least one source (PL-52)",
            });
        }
        if self.outputs.is_empty() {
            return Err(Error::Name {
                field: "spec.outputs",
                value: String::new(),
                reason: "a pipeline writes through at least one output (PL-52)",
            });
        }
        for source in &self.sources {
            validate_source(source)?;
        }
        for output in &self.outputs {
            validate_target(&output.target_endpoint)?;
            if let Some(entity_type) = &output.entity_type {
                names::validate_entity_type(entity_type)?;
            }
        }
        for step in &self.steps {
            match step {
                Step::Compute(compute) => {
                    validate_compute(compute)?;
                    if compute.kind == ComputeKind::Container
                        && (self.steps.len() > 1 || !self.is_scheduled())
                    {
                        return Err(Error::Name {
                            field: "spec.steps",
                            value: "container".to_owned(),
                            reason: "a container step is the only step of a scheduled pipeline: it runs as a Job, not inside a Bento stream (PL-35, PL-55)",
                        });
                    }
                }
                Step::Processor(step) => {
                    let mut names = step.processor.keys();
                    let (Some(name), None) = (names.next(), names.next()) else {
                        return Err(Error::Name {
                            field: "spec.steps.processor",
                            value: step
                                .processor
                                .keys()
                                .cloned()
                                .collect::<Vec<_>>()
                                .join(", "),
                            reason: "a processor step names exactly one processor (PL-52)",
                        });
                    };
                    if !super::bento_processors::PROCESSORS.contains(&name.as_str()) {
                        return Err(Error::Invalid {
                            field: "spec.steps.processor".to_owned(),
                            reason: format!(
                                "unknown processor `{name}`; accepted processors are: {}",
                                super::bento_processors::PROCESSORS.join(", ")
                            ),
                        });
                    }
                    // A `mapping` body and a `${! … }` interpolation are Bloblang too, and a
                    // step that reads the runner is the same escape wherever it is written.
                    let mut texts = Vec::new();
                    for config in step.processor.values() {
                        config_strings(config, &mut texts);
                    }
                    for text in &texts {
                        validate_author_bloblang("spec.steps.processor", text)?;
                    }
                }
            }
        }
        Ok(())
    }
}
