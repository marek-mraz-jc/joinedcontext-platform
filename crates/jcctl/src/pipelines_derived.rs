//! What a derived pipeline compiles into (T-0140, PL-31, PL-33, PL-34, PL-34a, PL-37).
//!
//! [`super::pipelines::runtime_of`] decides *where* a pipeline runs; this decides what the
//! generated `bento.yaml` reads, what it computes with, and what the platform has to hold for it
//! while it runs. A derived pipeline is the one shape whose input is the platform itself: it
//! reads entities out of a Context Space through an Endpoint, computes, and writes entities back
//! through another (Architecture/08, "Derived pipelines").
//!
//! Three rules are enforced here rather than left to the runtime.
//!
//! A `wasm` compute renders against the digest the build lane published and against nothing else
//! (PL-34a): no digest, no processor, no runtime. A pipeline whose module has not been built yet
//! deploys nothing rather than re-running the module from last week, which is the same rule an
//! App follows for its image.
//!
//! A resident pipeline that writes the type its own subscription watches feeds its own trigger,
//! and is refused unless the manifest says the loop is deliberate (PL-37). The decision is made
//! on the entity type, because the type is the only thing both sides declare: `output` carries no
//! attribute list, so nothing here can claim the compute misses the watched attributes.
//!
//! And a source declares one input, never two. `PipelineSource::validate` already refuses an
//! Endpoint and a DataSource together; this refuses a query and a subscription trigger together,
//! for the same reason: two inputs is a merge nobody can review.

use crate::pipelines::{runtime_of, Runtime};
use jc_core::kinds::{ComputeKind, PipelineSource, PipelineSpec, SourceQuery, SubscriptionTrigger};
use jc_core::urn::Urn;
use serde_json::{json, Map, Value};

/// The port every Bento runner serves on, stream API and stream endpoints alike.
const RUNNER_PORT: u16 = 4195;

/// One page of a source query. Bento's `http_client` has no pagination, so a query whose result
/// outgrows this needs a narrower `q` or a shorter window rather than a silent truncation.
const PAGE_LIMIT: u32 = 1000;

/// What the reconciler knows that the manifest does not.
#[derive(Debug, Clone, Copy)]
pub struct DerivedContext<'a> {
    /// Project slug: the namespace the runner lives in and the manifest's own namespace.
    pub project: &'a str,
    /// The pipeline's name, which is also its Bento stream id.
    pub pipeline: &'a str,
    /// The Context Space the source Endpoint reads, the third segment of the subscription's URN.
    pub space: &'a str,
    /// The organization's domain, the second segment of every URN it mints (PF-42).
    pub org_domain: &'a str,
    /// The source Endpoint's NGSI-LD tree without a trailing slash, e.g.
    /// `https://city.example.sk/api/endpoint/ep-air-quality/ngsi-ld/v1`.
    pub source_url: &'a str,
    /// The `joinedcontext.com/module` annotation, when the build lane has published a module
    /// for this pipeline (PL-34a).
    pub module_digest: Option<&'a str>,
}

/// The derived half of a generated `bento.yaml`, plus what the platform must hold for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Derived {
    /// `input:` of the generated config.
    pub input: Value,
    /// The processors the reconciler contributes, in order: the source fetch for a query, then
    /// the compute step for the kinds that are one. `bloblang` compute contributes its inline
    /// `spec.compute.bloblang` as the last `mapping` processor (PL-41), or nothing when the
    /// author keeps the mapping in the pipeline's own `bento.yaml`.
    pub processors: Vec<Value>,
    /// The CIM 009 subscription the reconciler creates through the source Endpoint, for a
    /// resident trigger. A scheduled query needs none.
    pub subscription: Option<Value>,
}

/// Why a derived pipeline does not render.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DerivedError {
    /// The manifest declares no `spec.source`, so it is not a derived pipeline at all.
    #[error("spec.source is absent, so this pipeline reads no context space (PL-31)")]
    NotDerived,
    /// The source names neither a query nor a trigger.
    #[error("spec.source declares neither query nor trigger.subscription, so there is no input to render (PL-31)")]
    NoInput,
    /// The source names both, which is two inputs.
    #[error("spec.source declares both query and trigger.subscription; a pipeline polls or is notified, not both (PL-31)")]
    TwoInputs,
    /// The source has no Endpoint to read through.
    #[error(
        "spec.source.endpointRef is absent, so the pipeline has no read grant on any space (PL-31)"
    )]
    NoEndpoint,
    /// A `wasm` compute whose module the build lane has not published yet.
    #[error("no joinedcontext.com/module annotation yet, so the build has not published a module (PL-34a)")]
    NoModuleDigest,
    /// A `wasm` compute whose entry point the manifest does not name.
    #[error("spec.compute.function is absent, so the module has no entry point (PL-34)")]
    NoFunction,
    /// The digest is not the `sha256:{64 hex}` the artifact store publishes.
    #[error("{0} is not a sha256 digest, so nothing pinned by it can be found (PL-34)")]
    BadDigest(String),
    /// The output feeds the trigger and the manifest does not say that is deliberate.
    #[error("this pipeline writes {entity_type}, which its own subscription watches; set spec.allowFeedback: true if the loop is deliberate (PL-37)")]
    Feedback {
        /// The type on both sides.
        entity_type: String,
    },
    /// A compute kind another renderer owns.
    #[error("compute.kind {kind} is not rendered here: {reason}")]
    Elsewhere {
        /// The kind that was asked for.
        kind: ComputeKind,
        /// Which renderer owns it.
        reason: &'static str,
    },
    /// A temporal window this renderer cannot turn into seconds.
    #[error("{0} is not a duration of days, hours, minutes or seconds (PL-31)")]
    BadWindow(String),
    /// An attribute name of the query holds the list separator, so the gateway would read it as
    /// two names.
    #[error("spec.source.query.attrs names {0:?}, and a comma separates attribute names, so it would be read as two")]
    BadAttribute(String),
}

/// Renders the derived half of one pipeline.
pub fn render(spec: &PipelineSpec, context: &DerivedContext) -> Result<Derived, DerivedError> {
    // Either shape (PL-54); the derived half renders one source, the first.
    let source = spec
        .sources()
        .into_iter()
        .next()
        .ok_or(DerivedError::NotDerived)?;
    let source = &source;
    if source.endpoint_ref.is_none() {
        return Err(DerivedError::NoEndpoint);
    }
    guard_feedback(spec, source)?;

    let (input, mut processors, subscription) = match (&source.trigger, &source.query) {
        (Some(_), Some(_)) => return Err(DerivedError::TwoInputs),
        (Some(trigger), None) => (
            notification_input(),
            Vec::new(),
            Some(subscription(&trigger.subscription, context)?),
        ),
        (None, Some(query)) => {
            let (input, fetch) = query_source(query, spec, context)?;
            (input, vec![fetch], None)
        }
        (None, None) => return Err(DerivedError::NoInput),
    };
    processors.extend(compute_processor(spec, context)?);

    Ok(Derived {
        input,
        processors,
        subscription,
    })
}

/// The address the gateway delivers this pipeline's notifications to.
///
/// Bento streams mode prefixes a stream's HTTP endpoints with the stream id, which is the
/// pipeline's name, so the input's own `path` is the tail of it.
pub fn notification_url(context: &DerivedContext) -> String {
    format!(
        "http://pipeline-runner.{}.svc.cluster.local:{RUNNER_PORT}/{}/notify",
        context.project, context.pipeline
    )
}

/// `input:` for a resident trigger: the runner listens, the gateway delivers.
fn notification_input() -> Value {
    json!({
        "http_server": {
            "path": "/notify",
            "allowed_verbs": ["POST"],
        }
    })
}

/// The clock and the fetch for a query source: one page of the source endpoint per run.
///
/// The input is `generate`, not `http_client`, because a scheduled pipeline is a CronJob pod that
/// has to exit: `generate` stops after `count` messages and takes the pod with it, where an
/// `http_client` input polls until something kills it. The parameters come from the same
/// [`runtime_of`] decision that placed the pipeline, so the cadence in the pod and the cadence in
/// the CronJob cannot disagree (PL-26, PL-27).
fn query_source(
    query: &SourceQuery,
    spec: &PipelineSpec,
    context: &DerivedContext,
) -> Result<(Value, Value), DerivedError> {
    let (interval, count) = match runtime_of(spec) {
        // A run that fetches once needs no interval: `generate` emits at once and stops.
        Runtime::Scheduled(cron) => (cron.interval.unwrap_or_default(), cron.count),
        // Resident: the period is the cadence and the stream never ends.
        Runtime::Resident => (spec.period.clone().unwrap_or_default(), 0),
    };

    // Every value below comes from a manifest a project member proposes, and the URL is a Bento
    // config string: each value goes in as one encoded component, so it can neither end its
    // parameter (`&`) or the query (`#`), nor hand the runner a `${VAR}` it substitutes at load
    // or a `${! … }` it interpolates per message (PL-14, T-2542).
    let mut params: Vec<String> = Vec::new();
    if let Some(entity_type) = &query.entity_type {
        params.push(format!("type={}", component(entity_type)));
    }
    if !query.attrs.is_empty() {
        // The list separator is the comma, so a name holding one would read as two.
        if let Some(name) = query.attrs.iter().find(|name| name.contains(',')) {
            return Err(DerivedError::BadAttribute(name.clone()));
        }
        let attrs: Vec<String> = query.attrs.iter().map(|name| component(name)).collect();
        params.push(format!("attrs={}", attrs.join(",")));
    }
    if !query.ids.is_empty() {
        // PL-42: the ticked entities, and nothing else the type holds.
        let ids: Vec<String> = query
            .ids
            .iter()
            .map(|id| component(&id.to_string()))
            .collect();
        params.push(format!("id={}", ids.join(",")));
    }
    for (name, value) in [
        ("q", &query.q),
        ("scopeQ", &query.scope_q),
        ("geoQ", &query.geo_q),
    ] {
        if let Some(value) = value {
            params.push(format!("{name}={}", component(value)));
        }
    }

    let path = match &query.temporal_q {
        None => "entities",
        Some(window) => {
            let seconds = window_seconds(&window.window)?;
            params.push("timerel=after".to_owned());
            // The instant is computed per run by the runner, not baked in at render time: a
            // config rendered on Monday would otherwise still be asking for Monday in June.
            params.push(format!(
                "timeAt=${{! (now().ts_unix() - {seconds}).ts_format(\"2006-01-02T15:04:05Z\") }}"
            ));
            "temporal/entities"
        }
    };
    params.push(format!("limit={PAGE_LIMIT}"));

    let input = json!({
        "generate": { "count": count, "interval": interval, "mapping": "root = \"\"" }
    });
    let fetch = json!({
        "http": {
            "url": format!("{}/{path}?{}", context.source_url, params.join("&")),
            "verb": "GET",
            "headers": {
                "Accept": "application/ld+json",
                // PL-14: the manifest carries the reference, the runner's environment the value.
                "Authorization": "Bearer ${SERVICE_ACCOUNT_TOKEN}",
            },
        }
    });
    Ok((input, fetch))
}

/// One query-string component: RFC 3986's unreserved characters and `:` (which a query may carry
/// as it is, and every URN is made of) left alone, every other byte percent-encoded, so the
/// gateway decodes exactly the value the manifest wrote.
fn component(value: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~' | b':') {
            encoded.push(char::from(byte));
        } else {
            encoded.push('%');
            encoded.push(char::from(HEX[usize::from(byte >> 4)]));
            encoded.push(char::from(HEX[usize::from(byte & 0x0F)]));
        }
    }
    encoded
}

/// The CIM 009 subscription that makes the gateway deliver to this runner.
fn subscription(
    trigger: &SubscriptionTrigger,
    context: &DerivedContext,
) -> Result<Value, DerivedError> {
    let id = Urn::new(
        "Subscription",
        context.org_domain,
        context.space,
        context.pipeline,
    )
    .map_err(|_| DerivedError::BadDigest(context.pipeline.to_owned()))?;

    let mut body = Map::new();
    body.insert("id".to_owned(), json!(id.to_string()));
    body.insert("type".to_owned(), json!("Subscription"));
    body.insert(
        "entities".to_owned(),
        json!([{ "type": trigger.entity_type }]),
    );
    if !trigger.watched_attributes.is_empty() {
        // An absent list is the widest one: the subscription watches every attribute of the type.
        body.insert(
            "watchedAttributes".to_owned(),
            json!(trigger.watched_attributes),
        );
    }
    body.insert(
        "notification".to_owned(),
        json!({
            "format": "normalized",
            "endpoint": { "uri": notification_url(context), "accept": "application/json" },
        }),
    );
    Ok(Value::Object(body))
}

/// The compute processor, for the kinds that are one.
fn compute_processor(
    spec: &PipelineSpec,
    context: &DerivedContext,
) -> Result<Option<Value>, DerivedError> {
    let Some(compute) = spec.steps().into_iter().find_map(|step| match step {
        jc_core::kinds::Step::Compute(compute) => Some(compute),
        jc_core::kinds::Step::Processor(_) => None,
    }) else {
        return Ok(None);
    };
    let compute = &compute;
    match compute.kind {
        // Inline in the manifest it is the last processor (PL-41); otherwise the author writes
        // the mapping in the pipeline's own bento.yaml and there is nothing to add.
        ComputeKind::Bloblang => Ok(compute
            .bloblang
            .as_ref()
            .map(|mapping| json!({ "mapping": mapping }))),
        ComputeKind::Mapping => Err(DerivedError::Elsewhere {
            kind: compute.kind,
            reason: "a compiled LinkML-Map is rendered by the mapping compiler (PL-29)",
        }),
        ComputeKind::Container => Err(DerivedError::Elsewhere {
            kind: compute.kind,
            reason: "container compute is a Kubernetes Job, not a Bento processor (PL-35)",
        }),
        ComputeKind::Wasm => {
            let digest = context.module_digest.ok_or(DerivedError::NoModuleDigest)?;
            let function = compute
                .function
                .as_deref()
                .ok_or(DerivedError::NoFunction)?;
            Ok(Some(json!({
                "wasm": {
                    "module_path": module_path(digest)?,
                    "function": function,
                }
            })))
        }
    }
}

/// Where the runner finds the module the digest names.
///
/// The file is named after the digest and nothing else, so two pipelines that compiled to the
/// same bytes mount one file and a rendered config names exactly what CI signed (PL-34a).
fn module_path(digest: &str) -> Result<String, DerivedError> {
    let hex = digest
        .strip_prefix("sha256:")
        .filter(|hex| hex.len() == 64 && hex.bytes().all(|b| b.is_ascii_hexdigit()))
        .ok_or_else(|| DerivedError::BadDigest(digest.to_owned()))?;
    Ok(format!("/modules/{hex}.wasm"))
}

/// PL-37: a pipeline that writes what it watches feeds itself.
fn guard_feedback(spec: &PipelineSpec, source: &PipelineSource) -> Result<(), DerivedError> {
    let Some(trigger) = &source.trigger else {
        return Ok(());
    };
    if spec.allow_feedback {
        return Ok(());
    }
    // Any output that writes the watched type feeds the trigger, in either shape (PL-54).
    match spec
        .outputs()
        .into_iter()
        .filter_map(|output| output.entity_type)
        .find(|written| *written == trigger.subscription.entity_type)
    {
        Some(entity_type) => Err(DerivedError::Feedback { entity_type }),
        None => Ok(()),
    }
}

/// Seconds in an ISO 8601 duration of fixed-length parts.
///
/// Years and months are refused rather than approximated: they are 28 to 31 and 365 to 366 days
/// long, and a temporal window that silently changes length between runs is worse than one the
/// author has to write in days.
fn window_seconds(window: &str) -> Result<u64, DerivedError> {
    let bad = || DerivedError::BadWindow(window.to_owned());
    let rest = window.strip_prefix('P').ok_or_else(bad)?;
    let (date, time) = match rest.split_once('T') {
        Some((date, time)) => (date, time),
        None => (rest, ""),
    };

    let mut total: u64 = 0;
    let mut digits = String::new();
    let mut seen = false;
    for (part, units) in [
        (date, [('D', 86_400u64)].as_slice()),
        (time, &[('H', 3_600), ('M', 60), ('S', 1)]),
    ] {
        for character in part.chars() {
            if character.is_ascii_digit() {
                digits.push(character);
                continue;
            }
            let unit = units
                .iter()
                .find(|(letter, _)| *letter == character)
                .ok_or_else(bad)?;
            let count: u64 = digits.parse().map_err(|_| bad())?;
            digits.clear();
            total = total.checked_add(count * unit.1).ok_or_else(bad)?;
            seen = true;
        }
    }
    match seen && digits.is_empty() && total > 0 {
        true => Ok(total),
        false => Err(bad()),
    }
}
