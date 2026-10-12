//! Rendering a `DataSource` into the Bento input of one pipeline (T-0291, PL-39, MF-35).
//!
//! The author writes what the data means: the processors that turn a payload into entities and
//! the output that writes them. Where the payload comes from is a `DataSource`, declared once
//! and referenced, and this module is the translation between the two (Architecture/08 §6).
//!
//! Two rules make the result reviewable. A credential is never written, only its environment
//! variable interpolation (PL-16), and nothing below the input is touched: the author's
//! `bento.yaml` comes back with an `input` in front of it and everything else byte for byte,
//! except for the decoder the GTFS-realtime type has to prepend and the ranged read of an
//! archive's named members (PL-72).

use jc_core::kinds::data_source::{check_class, env_var_of};
use jc_core::kinds::mqtt_broker;
use jc_core::kinds::{
    DataSourceSpec, DataSourceType, GtfsFeed, HttpArchive, IdMint, Mint, PipelineSpec, TemplatePart,
};
use serde_norway::{Mapping, Value};

/// The GTFS-realtime descriptors the runner image carries, so no pipeline ships a copy.
const GTFS_IMPORT_PATH: &str = "/opt/bento/gtfs-realtime";
/// The protobuf message every GTFS-realtime feed is wrapped in.
const GTFS_MESSAGE: &str = "transit_realtime.FeedMessage";

/// The archive's tail the directory is read from: the 22-byte end of central directory record
/// and the longest comment it may carry (PL-72).
const ZIP_TAIL: &str = "bytes=-65557";

/// A little-endian hex field to an integer. Stock Bloblang has neither a byte reader nor a
/// hex parser, so the bytes are reversed in pairs and folded digit by digit.
const ZIP_INT: &str = r#"map int {
  root = range(0, this.length(), 2).fold("", acc -> this.slice(acc.value, acc.value + 2) + acc.tally).split("").fold(0, acc -> acc.tally * 16 + "0123456789abcdef".index_of(acc.value))
}
"#;

/// The directory of the archive's tail to the named entries, one JSON array (PL-72): every
/// count, size, method and offset is checked before any member is fetched. `MEMBERS` and
/// `MAX_BYTES` are replaced by values `HttpArchive::validate` admitted.
///
/// The directory's sizes may lie, and Bloblang's `decompress` has no output limit, so a deflate
/// member is read only when its packed size times 1032, the most deflate expands, is within
/// `maxBytes`: no member inflates past the bound, whatever its directory says.
const ZIP_DIRECTORY: &str = r#"let wanted = MEMBERS
let max_bytes = MAX_BYTES
let tail = if content().length() > 65557 { throw("the archive server ignored Range and sent %v bytes".format(content().length())) } else { content() }
let h = $tail.encode("hex")
let eocd = $h.length() - $h.split("504b0506").index(-1).length() - 8
let eocd = if $eocd < 0 || $eocd % 2 != 0 { throw("no end of central directory in the archive's last 64 KiB") } else { $eocd }
let entries = $h.slice($eocd + 20, $eocd + 24).apply("int")
let cd_size = $h.slice($eocd + 24, $eocd + 32).apply("int")
let cd_at = $eocd - $cd_size * 2
let cd = if $cd_at < 0 { throw("the archive's central directory is larger than the 64 KiB read") } else { $h.slice($cd_at, $eocd) }
let all = $cd.split("504b0102").slice(1).map_each(e -> {
  "method": e.slice(12, 16).apply("int"),
  "csize": e.slice(32, 40).apply("int"),
  "size": e.slice(40, 48).apply("int"),
  "nlen": e.slice(48, 52).apply("int"),
  "elen": e.slice(52, 56).apply("int"),
  "offset": e.slice(76, 84).apply("int"),
  "name": e.slice(84, 84 + e.slice(48, 52).apply("int") * 2).decode("hex").string()
})
let found = if $all.length() != $entries { throw("the archive's directory lists %v entries, its end record %v".format($all.length(), $entries)) } else { $all.filter(e -> $wanted.contains(e.name)) }
let missing = $wanted.filter(w -> !$found.map_each(e -> e.name).contains(w))
root = if $missing.length() > 0 { throw("the archive has no member %v".format($missing)) } else { $found }
root = root.map_each(e -> if e.size > $max_bytes || e.csize > $max_bytes { throw("%v: %v bytes, %v packed, over maxBytes %v".format(e.name, e.size, e.csize, $max_bytes)) } else if ![0, 8].contains(e.method) { throw("%v: compression method %v, only stored and deflate are read".format(e.name, e.method)) } else if e.method == 8 && e.csize * 1032 > $max_bytes { throw("%v: %v packed may inflate to %v bytes, over maxBytes %v".format(e.name, e.csize, e.csize * 1032, $max_bytes)) } else if e.offset == 4294967295 { throw("%v: a Zip64 archive is not read".format(e.name)) } else { e })
"#;

/// One directory entry to the metadata the member's ranged read needs (PL-72). The range
/// covers the 30-byte local header, its name and extra field as the directory gives them, the
/// packed bytes, and 1 KiB in case the local extra field is longer than the directory's.
const ZIP_MEMBER: &str = r#"meta archive_filename = this.name
meta zip = this.format_json(no_indent: true)
meta range = "bytes=%v-%v".format(this.offset, this.offset + 30 + this.nlen + this.elen + this.csize + 1024)
"#;

/// The member's local header and packed bytes to its content, checked against the directory.
const ZIP_INFLATE: &str = r#"let z = @zip.parse_json()
let read = if content().length() > 30 + $z.nlen + $z.elen + $z.csize + 1025 { throw("%v: the archive server ignored Range".format($z.name)) } else { content() }
let head = $read.slice(0, 30).encode("hex")
let nlen = $head.slice(52, 56).apply("int")
let start = 30 + $nlen + $head.slice(56, 60).apply("int")
let data = $read.slice($start, $start + $z.csize)
let body = if $head.slice(0, 8) != "504b0304" || $read.slice(30, 30 + $nlen).string() != $z.name || $data.length() != $z.csize { throw("%v: no local header of it at offset %v".format($z.name, $z.offset)) } else if $z.method == 8 { $data.decompress("flate") } else { $data }
root = if $body.length() != $z.size { throw("%v: inflated to %v bytes, the directory says %v".format($z.name, $body.length(), $z.size)) } else { $body }
meta zip = deleted()
meta range = deleted()
"#;

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
    /// The output's mint option cannot be rendered (ADR-N-041 §3.4).
    #[error("spec.output.id: {0}")]
    Mint(String),
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
/// GTFS-realtime prepends its protobuf decoder: the feed arrives as protobuf and every pipeline
/// reading it would otherwise decode it again in its own mapping. An `http` connection with an
/// `archive` prepends the ranged read of its named members (`archive_processors`).
pub fn prepended_processors(spec: &DataSourceSpec, context: &InputContext) -> Vec<Value> {
    if let Some(archive) = spec.http.as_ref().and_then(|http| http.archive.as_ref()) {
        if spec.source_type == DataSourceType::Http {
            return archive_processors(spec, archive, context);
        }
    }
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

/// The ranged read of an archive's named members (PL-72, Architecture/08 §6).
///
/// The input fetched the archive's last 64 KiB; the directory in it becomes one message per
/// named member, each member is fetched by its own `Range` with the connection's headers,
/// authorization and TLS, and inflated. A refused read is logged and dropped, so the author's
/// steps never see archive bytes. Stock Bento only: `unarchive: zip` would inflate every member.
fn archive_processors(
    spec: &DataSourceSpec,
    archive: &HttpArchive,
    context: &InputContext,
) -> Vec<Value> {
    let members = serde_json::to_string(&archive.members).expect("strings serialize");
    let directory = ZIP_DIRECTORY.replacen("MEMBERS", &members, 1).replacen(
        "MAX_BYTES",
        &archive.max_bytes().to_string(),
        1,
    );
    let mut member = http(spec, context);
    let mut headers = match member.remove(key("headers")) {
        Some(Value::Mapping(headers)) => headers,
        _ => Mapping::new(),
    };
    headers.insert(key("Range"), text("${! @range }"));
    member.insert(key("headers"), Value::Mapping(headers));
    let one = |name: &str, body: Value| {
        let mut processor = Mapping::new();
        processor.insert(key(name), body);
        Value::Mapping(processor)
    };
    let mut unarchive = Mapping::new();
    unarchive.insert(key("format"), text("json_array"));
    let mut log = Mapping::new();
    log.insert(key("level"), text("ERROR"));
    log.insert(key("message"), text("archive read refused: ${! error() }"));
    // `try` skips the steps after the first refusal: an unchecked directory must never reach
    // the member request, whose empty range would fetch the whole archive.
    let read = vec![
        one("mapping", text(&format!("{ZIP_INT}{directory}"))),
        one("unarchive", Value::Mapping(unarchive)),
        one("mapping", text(ZIP_MEMBER)),
        one("http", Value::Mapping(member)),
        one("mapping", text(&format!("{ZIP_INT}{ZIP_INFLATE}"))),
    ];
    vec![
        one("try", Value::Sequence(read)),
        one(
            "catch",
            Value::Sequence(vec![
                one("log", Value::Mapping(log)),
                one("mapping", text("root = deleted()")),
            ]),
        ),
    ]
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

    let decoders = prepended_processors(spec, context);
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

    // An output that declares how its ids are made gets one last processor that makes them
    // (ADR-N-041 §3.4); without the declaration nothing is appended and the file is the author's.
    if let Some(minting) = context
        .pipeline_spec
        .and_then(|pipeline| pipeline.output.as_ref())
        .and_then(|output| {
            output
                .id
                .as_ref()
                .map(|mint| id_processor(mint, &output.entity_type))
        })
    {
        let minting = minting?;
        let mut pipeline = match config.remove(key("pipeline")) {
            Some(Value::Mapping(existing)) => existing,
            _ => Mapping::new(),
        };
        let mut processors = match pipeline.remove(key("processors")) {
            Some(Value::Sequence(authored)) => authored,
            _ => Vec::new(),
        };
        processors.push(minting);
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

/// The `mapping` processor that sets `root.id` by the output's mint option (ADR-N-041 §3.4).
///
/// Every value that reaches the Bloblang was checked by [`IdMint::validate`]: the type is a
/// PascalCase name, `from` and each `{field}` a dotted path of identifiers, a template's text RFC
/// 8141 characters without `$` or a quote, so nothing written into a manifest becomes code.
pub fn id_processor(mint: &IdMint, entity_type: &str) -> Result<Value, RenderError> {
    mint.validate(entity_type)
        .map_err(|e| RenderError::Mint(e.to_string()))?;
    let field = |path: &str| format!("this.{path}.string()");
    let id = match mint.mint {
        Mint::Prefixed => format!(
            "\"urn:ngsi-ld:{entity_type}:%s:%s:%s\".format(env(\"{ORG_DOMAIN_VAR}\"), env(\"{SPACE_VAR}\"), {}.re_replace_all(\"[^A-Za-z0-9._~-]\", \"-\"))",
            field(mint.from.as_deref().unwrap_or_default())
        ),
        Mint::Keep => {
            let from = field(mint.from.as_deref().unwrap_or_default());
            format!(
                "if {from}.has_prefix(\"urn:ngsi-ld:\") {{ {from} }} else {{ \"urn:ngsi-ld:{entity_type}:\" + {from} }}"
            )
        }
        Mint::Template => mint
            .template_parts()
            .map_err(|e| RenderError::Mint(e.to_string()))?
            .iter()
            .map(|part| match part {
                TemplatePart::Text(text) => format!("\"{text}\""),
                TemplatePart::Field(path) => field(path),
            })
            .collect::<Vec<_>>()
            .join(" + "),
    };
    let mut processor = Mapping::new();
    processor.insert(
        key("mapping"),
        text(&format!("root = this\nroot.id = {id}\n")),
    );
    Ok(Value::Mapping(processor))
}

fn mqtt(spec: &DataSourceSpec, context: &InputContext) -> Mapping {
    let Some(connection) = &spec.mqtt else {
        return Mapping::new();
    };
    let mut out = Mapping::new();
    // A platform broker is read on its internal listener as the runner, with no credential
    // (PL-76); the gateway grants the runner the topics of the DataSources that name the broker.
    let urls = match &connection.broker_ref {
        Some(broker) => vec![mqtt_broker::internal_url(context.project, &broker.name)],
        None => connection.urls.clone(),
    };
    out.insert(key("urls"), sequence(&urls));
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
    if connection.broker_ref.is_some() {
        out.insert(key("user"), text(mqtt_broker::RUNNER_USERNAME));
    } else if let Some(user) = &connection.username {
        out.insert(key("user"), text(user));
    }
    if let Some(password) = &connection.password_ref {
        out.insert(
            key("password"),
            text(&interpolation(context.source, password)),
        );
    }
    let encrypted = urls
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
    if connection.archive.is_some() {
        headers.insert(key("Range"), text(ZIP_TAIL));
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
