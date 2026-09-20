//! Rendering a `DataSource` into the Bento input of one pipeline (T-0291, PL-39, MF-35).
//!
//! The author writes what the data means: the processors that turn a payload into entities and
//! the output that writes them. Where the payload comes from is a `DataSource`, declared once
//! and referenced, and this module is the translation between the two (Architecture/08 §6).
//!
//! Two rules make the result reviewable. A credential is never written, only its environment
//! variable interpolation (PL-16), and nothing below the input is touched: the author's
//! `bento.yaml` comes back with an `input` in front of it and everything else byte for byte,
//! except for the decoder the GTFS-realtime type has to prepend.

use jc_core::kinds::data_source::{check_class, env_var_of};
use jc_core::kinds::{DataSourceSpec, DataSourceType, GtfsFeed, PipelineSpec};
use serde_norway::{Mapping, Value};

/// The GTFS-realtime descriptors the runner image carries, so no pipeline ships a copy.
const GTFS_IMPORT_PATH: &str = "/opt/bento/gtfs-realtime";
/// The protobuf message every GTFS-realtime feed is wrapped in.
const GTFS_MESSAGE: &str = "transit_realtime.FeedMessage";

/// The environment variable a mapping mints the `{orgDomain}` of an id from (PF-44, PF-42).
///
/// The reconciler resolves it from the project's Organization and injects it into the runner,
/// and a mapping reads it with `env("JC_ORG_DOMAIN")`. It is an environment variable rather
/// than a Bloblang helper because a pipeline file has to stay native Bento that runs unmodified
/// under `bento lint` and `bento test` (PL-03), and stock Bloblang has no function registry a
/// reconciler could extend without a plugin and a forked runner binary. The gain is the same:
/// a pipeline never writes a domain of its own, so a project moved to another Organization
/// mints its ids under the new one without an edit (Architecture/03 §3).
pub const ORG_DOMAIN_VAR: &str = "JC_ORG_DOMAIN";

/// The variable a mapping mints the `{space}` segment of an id from (PL-57, PF-84).
///
/// One runner serves every pipeline of a project, so unlike [`ORG_DOMAIN_VAR`] the space
/// cannot be a variable of the runner's process: the renderer writes it into each stream,
/// the rendered segment of that output's space in place of `env("JC_SPACE")`. The first
/// output is `JC_SPACE`, the second `JC_SPACE_2`, and so on ([`space_var`]).
pub const SPACE_VAR: &str = "JC_SPACE";

/// The variable output `index` (0-based) reads its space from (PL-57).
pub fn space_var(index: usize) -> String {
    match index {
        0 => SPACE_VAR.to_owned(),
        n => format!("{SPACE_VAR}_{}", n + 1),
    }
}

/// Writes each output's segment into every string of a stream where a mapping reads it,
/// `env("JC_SPACE")` as a Bloblang string and `${JC_SPACE}` as the bare segment (PL-57).
///
/// `segments[i]` is the segment of output `i`. A variable with no output behind it is left
/// as it is: the runner then reads an unset variable, and the write guard refuses the id.
pub fn inject_space(value: &mut serde_json::Value, segments: &[String]) {
    match value {
        serde_json::Value::String(text) => {
            if !text.contains(SPACE_VAR) {
                return;
            }
            // Longest name first, so `JC_SPACE` never rewrites the head of `JC_SPACE_2`.
            for (index, segment) in segments.iter().enumerate().rev() {
                let var = space_var(index);
                *text = text
                    .replace(&format!("env(\"{var}\")"), &format!("\"{segment}\""))
                    .replace(&format!("${{{var}}}"), segment);
            }
        }
        serde_json::Value::Array(items) => items
            .iter_mut()
            .for_each(|item| inject_space(item, segments)),
        serde_json::Value::Object(map) => map
            .values_mut()
            .for_each(|item| inject_space(item, segments)),
        _ => {}
    }
}

/// Every line of a Bento file that writes one of `names` as a string literal, as
/// `(1-based line, the name)` (CC-83).
///
/// Only a whole quoted literal counts: `"helsinki"` is the space typed in, while a URL or a
/// title that contains the word is not. Comment lines are skipped.
pub fn literal_names(bento: &str, names: &[&str]) -> Vec<(usize, String)> {
    let mut found = Vec::new();
    for (index, line) in bento.lines().enumerate() {
        if line.trim_start().starts_with('#') {
            continue;
        }
        for name in names.iter().filter(|name| !name.is_empty()) {
            if line.contains(&format!("\"{name}\"")) || line.contains(&format!("'{name}'")) {
                found.push((index + 1, (*name).to_owned()));
            }
        }
    }
    found
}

/// What the renderer needs beyond the connection itself.
#[derive(Debug, Clone, Copy, Default)]
pub struct InputContext<'a> {
    /// The `DataSource` name, which seeds the environment variable names.
    pub source: &'a str,
    /// The project the pipeline belongs to, part of the MQTT client id.
    pub project: &'a str,
    /// The pipeline name, part of the MQTT client id so two streams never collide.
    pub pipeline: &'a str,
    /// Optional pipeline specification to validate execution class against source.
    pub pipeline_spec: Option<&'a PipelineSpec>,
}

impl<'a> InputContext<'a> {
    /// Creates a new context with the given source, project, and pipeline names.
    pub const fn new(source: &'a str, project: &'a str, pipeline: &'a str) -> Self {
        Self {
            source,
            project,
            pipeline,
            pipeline_spec: None,
        }
    }

    /// Attaches a pipeline spec for class compatibility validation.
    pub const fn with_pipeline_spec(mut self, spec: &'a PipelineSpec) -> Self {
        self.pipeline_spec = Some(spec);
        self
    }
}

/// Why a pipeline and a connection cannot be rendered together.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RenderError {
    /// The author's `bento.yaml` is not a YAML mapping.
    #[error("bento.yaml is not a mapping, so there is nothing to add an input to")]
    NotAMapping,
    /// The author wrote an input and the reference names another one (PL-39).
    #[error("bento.yaml already declares an input, and spec.source.dataSourceRef names another one; keep one of the two")]
    InputAlreadyDeclared,
    /// The pipeline's operational class cannot execute the source (PL-04, PL-50).
    #[error("pipeline class does not fit data source: {0}")]
    Class(String),
    /// The text does not parse as YAML.
    #[error("bento.yaml does not parse: {0}")]
    Parse(String),
}

/// The `input` block one connection becomes (PL-39).
pub fn input_of(spec: &DataSourceSpec, context: &InputContext) -> Value {
    let mut input = Mapping::new();
    match &spec.source_type {
        DataSourceType::Mqtt => {
            input.insert(key("mqtt"), Value::Mapping(mqtt(spec, context)));
        }
        DataSourceType::Http => {
            input.insert(key("http_client"), Value::Mapping(http(spec, context)));
        }
        DataSourceType::WebSocket => {
            input.insert(key("websocket"), Value::Mapping(websocket(spec)));
        }
        DataSourceType::GtfsRt => {
            input.insert(key("http_client"), Value::Mapping(gtfs(spec)));
        }
        DataSourceType::Runner(name) => {
            let runner_val = spec
                .input
                .as_ref()
                .and_then(|v| serde_norway::to_value(v).ok())
                .unwrap_or_else(|| Value::Mapping(Mapping::new()));
            input.insert(key(name), runner_val);
        }
    }
    Value::Mapping(input)
}

/// The processors the connection contributes before the author's own (PL-39).
///
/// Only GTFS-realtime has any: the feed arrives as protobuf and every pipeline reading it would
/// otherwise decode it again in its own mapping.
pub fn prepended_processors(spec: &DataSourceSpec) -> Vec<Value> {
    if spec.source_type != DataSourceType::GtfsRt {
        return Vec::new();
    }
    let mut protobuf = Mapping::new();
    protobuf.insert(key("operator"), text("to_json"));
    protobuf.insert(key("message"), text(GTFS_MESSAGE));
    protobuf.insert(
        key("import_paths"),
        Value::Sequence(vec![text(GTFS_IMPORT_PATH)]),
    );
    let mut processor = Mapping::new();
    processor.insert(key("protobuf"), Value::Mapping(protobuf));
    vec![Value::Mapping(processor)]
}

/// The author's `bento.yaml` with the connection's input in front of it (PL-03, PL-39).
///
/// Returns the rendered YAML. Everything the author wrote is preserved as parsed: the input is
/// added, the GTFS decoder is prepended to `pipeline.processors`, and no other key is read,
/// reordered or removed.
pub fn render(
    bento_yaml: &str,
    spec: &DataSourceSpec,
    context: &InputContext,
) -> Result<String, RenderError> {
    if let Some(pipeline) = context.pipeline_spec {
        check_class(pipeline, spec).map_err(|e| RenderError::Class(e.to_string()))?;
    }

    let parsed: Value = match bento_yaml.trim().is_empty() {
        true => Value::Mapping(Mapping::new()),
        false => {
            serde_norway::from_str(bento_yaml).map_err(|e| RenderError::Parse(e.to_string()))?
        }
    };
    let Value::Mapping(mut config) = parsed else {
        return Err(RenderError::NotAMapping);
    };
    if config.contains_key(key("input")) {
        return Err(RenderError::InputAlreadyDeclared);
    }

    let decoders = prepended_processors(spec);
    if !decoders.is_empty() {
        let mut pipeline = match config.remove(key("pipeline")) {
            Some(Value::Mapping(existing)) => existing,
            _ => Mapping::new(),
        };
        let mut processors = decoders;
        if let Some(Value::Sequence(authored)) = pipeline.remove(key("processors")) {
            processors.extend(authored);
        }
        pipeline.insert(key("processors"), Value::Sequence(processors));
        config.insert(key("pipeline"), Value::Mapping(pipeline));
    }

    // The input goes first, the way an author would write it, so the generated file reads like
    // a hand-written one: rebuild the mapping rather than appending at the end.
    let mut out = Mapping::new();
    out.insert(key("input"), input_of(spec, context));
    for (k, v) in config {
        out.insert(k, v);
    }
    serde_norway::to_string(&Value::Mapping(out)).map_err(|e| RenderError::Parse(e.to_string()))
}

fn mqtt(spec: &DataSourceSpec, context: &InputContext) -> Mapping {
    let Some(connection) = &spec.mqtt else {
        return Mapping::new();
    };
    let mut out = Mapping::new();
    out.insert(key("urls"), sequence(&connection.urls));
    out.insert(key("topics"), sequence(&connection.topics));
    out.insert(
        key("client_id"),
        text(&format!("jc-{}-{}", context.project, context.pipeline)),
    );
    if let Some(qos) = connection.qos {
        out.insert(key("qos"), Value::Number(qos.into()));
    }
    if let Some(clean) = connection.clean_session {
        out.insert(key("clean_session"), Value::Bool(clean));
    }
    if let Some(user) = &connection.username {
        out.insert(key("user"), text(user));
    }
    if let Some(password) = &connection.password_ref {
        out.insert(
            key("password"),
            text(&interpolation(context.source, password)),
        );
    }
    let encrypted = connection
        .urls
        .iter()
        .any(|url| url.starts_with("tls://") || url.starts_with("wss://"));
    if let Some(tls) = tls_block(spec, context, encrypted) {
        out.insert(key("tls"), Value::Mapping(tls));
    }
    out
}

fn http(spec: &DataSourceSpec, context: &InputContext) -> Mapping {
    let Some(connection) = &spec.http else {
        return Mapping::new();
    };
    let mut out = Mapping::new();
    out.insert(key("url"), text(&connection.url));
    out.insert(
        key("verb"),
        text(connection.verb.as_deref().unwrap_or("GET")),
    );

    let mut headers = Mapping::new();
    for (name, value) in &connection.headers {
        headers.insert(key(name), text(value));
    }
    if let Some(authorization) = &connection.authorization {
        // A vendor key header takes the bare key, so the scheme is a prefix only when there is
        // one to write (MF-35, Architecture/08 §6).
        let value = interpolation(context.source, &authorization.header_ref);
        let value = match authorization.scheme_prefix() {
            "" => value,
            scheme => format!("{scheme} {value}"),
        };
        headers.insert(key(authorization.header_name()), text(&value));
    }
    if !headers.is_empty() {
        out.insert(key("headers"), Value::Mapping(headers));
    }
    if let Some(timeout) = &connection.timeout {
        out.insert(key("timeout"), text(timeout));
    }
    if let Some(tls) = tls_block(spec, context, connection.url.starts_with("https://")) {
        out.insert(key("tls"), Value::Mapping(tls));
    }
    out
}

fn websocket(spec: &DataSourceSpec) -> Mapping {
    let Some(connection) = &spec.web_socket else {
        return Mapping::new();
    };
    let mut out = Mapping::new();
    out.insert(key("url"), text(&connection.url));
    if let Some(message) = &connection.open_message {
        out.insert(key("open_message"), text(message));
        out.insert(key("open_message_type"), text("text"));
    }
    out
}

fn gtfs(spec: &DataSourceSpec) -> Mapping {
    let Some(connection) = &spec.gtfs_rt else {
        return Mapping::new();
    };
    let mut out = Mapping::new();
    out.insert(key("url"), text(&connection.url));
    out.insert(key("verb"), text("GET"));
    // The feed says which messages the decoded envelope carries; it travels as metadata so a
    // mapping can branch on it without parsing the URL.
    let mut metadata = Mapping::new();
    metadata.insert(key("gtfs_feed"), text(feed_name(connection.feed)));
    out.insert(key("metadata"), Value::Mapping(metadata));
    out
}

const fn feed_name(feed: GtfsFeed) -> &'static str {
    feed.as_str()
}

/// Bento's TLS block, rendered only when it says something: an encrypted scheme or a private
/// certificate authority. Verification is never switched off, so no key does that here.
fn tls_block(spec: &DataSourceSpec, context: &InputContext, encrypted: bool) -> Option<Mapping> {
    let authority = spec.tls.as_ref().and_then(|tls| tls.ca_cert_ref.as_ref());
    if !encrypted && authority.is_none() {
        return None;
    }
    let mut out = Mapping::new();
    out.insert(key("enabled"), Value::Bool(true));
    if let Some(reference) = authority {
        out.insert(
            key("root_cas"),
            text(&interpolation(context.source, reference)),
        );
    }
    Some(out)
}

/// `${DS_MQTT_MESTO_PASSWORD}`: the only form a credential takes in a generated config (PL-16).
fn interpolation(source: &str, reference: &jc_core::envelope::SecretRef) -> String {
    format!("${{{}}}", env_var_of(source, reference))
}

fn key(name: &str) -> Value {
    Value::String(name.to_owned())
}

fn text(value: &str) -> Value {
    Value::String(value.to_owned())
}

fn sequence(values: &[String]) -> Value {
    Value::Sequence(values.iter().map(|v| text(v)).collect())
}
