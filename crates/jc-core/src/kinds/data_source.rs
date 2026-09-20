//! Manifest kind for the connection of one external feed (T-0290, MF-35, PL-39).
//!
//! A `DataSource` says where a feed is and how to authenticate to it, and stops there: no
//! mapping, no schedule, no target. It moves nothing on its own; a [`PipelineSpec`] references
//! one and the reconciler renders it into that pipeline's Bento input (Architecture/08 §6).
//!
//! Every credential is a [`SecretRef`]. That is not a convention here, it is the shape of the
//! type: there is no `password`, `token` or `apiKey` member anywhere below, so a plaintext
//! credential is a parse error rather than a review finding (CC-06, PL-14).
//!
//! [`PipelineSpec`]: crate::kinds::PipelineSpec

use crate::envelope::{Kind, ObjectMeta, Scope, SecretRef};
use crate::error::{Error, Result};
use crate::kinds::PipelineSpec;
use crate::names;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fmt;
use std::sync::LazyLock;

use super::bento_inputs;

static ENV_VAR_RE: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"^[A-Z][A-Z0-9_]*$").expect("valid regex"));

/// The password of a `user:password@` part, or of a `password=` parameter, in a connection string.
static EMBEDDED_PASSWORD_RE: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(r"(?i)(?:[^\s/@:?&=]+:([^\s/@]+)@|(?:^|[?&;\s])password=([^&;\s]+))")
        .expect("valid regex")
});

/// Which kind of feed a [`DataSourceSpec`] connects to: one of the four typed feeds or any
/// input the runner ships (MF-35, PL-50).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum DataSourceType {
    /// An MQTT broker the runner subscribes to.
    Mqtt,
    /// An HTTP resource the runner polls.
    Http,
    /// A WebSocket the runner keeps open.
    WebSocket,
    /// A GTFS-realtime protobuf feed, polled over HTTP and decoded by the runner.
    GtfsRt,
    /// Any input the pinned Bento runner ships (PL-50).
    Runner(String),
}

impl DataSourceType {
    /// The wire name, as written in `spec.type`.
    pub fn as_str(&self) -> &str {
        match self {
            Self::Mqtt => "mqtt",
            Self::Http => "http",
            Self::WebSocket => "websocket",
            Self::GtfsRt => "gtfs-rt",
            Self::Runner(name) => name.as_str(),
        }
    }

    /// The runner input name if this is a runner-provided input.
    pub fn runner_name(&self) -> Option<&str> {
        match self {
            Self::Runner(name) => Some(name.as_str()),
            _ => None,
        }
    }
}

impl fmt::Display for DataSourceType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

impl Serialize for DataSourceType {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for DataSourceType {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        Ok(match s.as_str() {
            "mqtt" => Self::Mqtt,
            "http" => Self::Http,
            "websocket" => Self::WebSocket,
            "gtfs-rt" => Self::GtfsRt,
            _ => Self::Runner(s),
        })
    }
}

impl JsonSchema for DataSourceType {
    fn schema_name() -> String {
        "DataSourceType".to_string()
    }

    fn json_schema(_gen: &mut schemars::gen::SchemaGenerator) -> schemars::schema::Schema {
        schemars::schema::SchemaObject {
            instance_type: Some(schemars::schema::InstanceType::String.into()),
            metadata: Some(Box::new(schemars::schema::Metadata {
                description: Some(
                    "Which kind of feed a DataSource connects to: a typed connection or any runner input (MF-35, PL-50)."
                        .to_string(),
                ),
                examples: vec![
                    serde_json::json!("mqtt"),
                    serde_json::json!("http"),
                    serde_json::json!("websocket"),
                    serde_json::json!("gtfs-rt"),
                    serde_json::json!("kafka"),
                    serde_json::json!("csv"),
                    serde_json::json!("sql_select"),
                ],
                ..Default::default()
            })),
            ..Default::default()
        }
        .into()
    }
}

/// An MQTT connection (Architecture/08 §6).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct MqttConnection {
    /// Broker URLs, `tcp://host:1883` or `tls://host:8883`; at least one.
    pub urls: Vec<String>,
    /// Topic filters to subscribe to; at least one.
    pub topics: Vec<String>,
    /// Quality of service, 0, 1 or 2. Absent leaves Bento's default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub qos: Option<u8>,
    /// Whether the broker forgets the subscription between connections.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub clean_session: Option<bool>,
    /// The user the runner authenticates as; the password is a reference.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
    /// Where the password comes from (CC-06).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub password_ref: Option<SecretRef>,
}

/// An HTTP resource the runner polls (Architecture/08 §6).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct HttpConnection {
    /// The URL to fetch, `http://` or `https://`.
    pub url: String,
    /// HTTP method, `GET` when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verb: Option<String>,
    /// Static request headers. A header carrying a credential belongs in `authorization`.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub headers: BTreeMap<String, String>,
    /// The `Authorization` header, built from a secret (CC-06).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub authorization: Option<Authorization>,
    /// Request timeout as a Bento duration, `10s`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout: Option<String>,
}

/// A WebSocket the runner keeps open (Architecture/08 §6).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct WebSocketConnection {
    /// The socket URL, `ws://` or `wss://`.
    pub url: String,
    /// A message sent once the socket opens, usually a subscription request.
    ///
    /// A socket that needs a credential authenticates inside this message: Bento's websocket
    /// input sends no `Authorization` header, and a field the renderer cannot honour would be
    /// a promise the runner breaks.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub open_message: Option<String>,
}

/// A GTFS-realtime feed (Architecture/08 §6).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct GtfsRtConnection {
    /// The protobuf feed URL, `http://` or `https://`.
    pub url: String,
    /// Which of the three GTFS-realtime feeds this URL serves.
    pub feed: GtfsFeed,
}

/// The three feeds GTFS-realtime defines.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum GtfsFeed {
    /// Where the vehicles are.
    VehiclePositions,
    /// How the trips are running against the timetable.
    TripUpdates,
    /// Service alerts.
    Alerts,
}

impl GtfsFeed {
    /// The camelCase wire name, as written in `spec.gtfsRt.feed`.
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::VehiclePositions => "vehiclePositions",
            Self::TripUpdates => "tripUpdates",
            Self::Alerts => "alerts",
        }
    }
}

/// An `Authorization` header whose value is assembled from a secret (CC-06).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Authorization {
    /// The header the credential is written into, `Authorization` when absent.
    ///
    /// An open-data feed that authenticates with an API key usually wants it in a header of
    /// its own; without this the only way to reach such a feed would be to write the key into
    /// `headers` as a value, which is what MF-35 exists to prevent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub header: Option<String>,
    /// The scheme written before the secret.
    ///
    /// `Bearer` when absent on `Authorization`, and nothing at all on any other header: a
    /// vendor key header takes the bare key and refuses a scheme in front of it. An explicit
    /// scheme always wins.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scheme: Option<String>,
    /// Where the credential comes from.
    pub header_ref: SecretRef,
}

impl Authorization {
    /// The header this credential is written into.
    pub fn header_name(&self) -> &str {
        self.header.as_deref().unwrap_or("Authorization")
    }

    /// The scheme written in front of the credential, empty when the header takes a bare value.
    pub fn scheme_prefix(&self) -> &str {
        match &self.scheme {
            Some(scheme) => scheme,
            None if self.header_name().eq_ignore_ascii_case("Authorization") => "Bearer",
            None => "",
        }
    }
}

/// How the runner trusts the feed's certificate.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct TlsSettings {
    /// Kept so a manifest can state the choice; only `false` is accepted (see [`DataSourceSpec::validate`]).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub insecure_skip_verify: bool,
    /// A private certificate authority the feed's certificate chains to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ca_cert_ref: Option<SecretRef>,
}

/// Desired state of a `DataSource`: one connection and its credentials (MF-35).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct DataSourceSpec {
    /// Which connection block below is the live one.
    #[serde(rename = "type")]
    pub source_type: DataSourceType,
    /// The MQTT connection, present when `type` is `mqtt`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mqtt: Option<MqttConnection>,
    /// The HTTP connection, present when `type` is `http`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub http: Option<HttpConnection>,
    /// The WebSocket connection, present when `type` is `websocket`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub web_socket: Option<WebSocketConnection>,
    /// The GTFS-realtime connection, present when `type` is `gtfs-rt`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gtfs_rt: Option<GtfsRtConnection>,
    /// Transport security for the connection above.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tls: Option<TlsSettings>,
    /// Verbatim input configuration document for runner-provided inputs (PL-50).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input: Option<serde_json::Value>,
    /// Secret references for runner-provided inputs (PL-50, MF-35).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub secrets: Vec<SecretRef>,
}

impl Kind for DataSourceSpec {
    const KIND: &'static str = "DataSource";
    const PLURAL: &'static str = "datasources";
    const SCOPE: Scope = Scope::Project;
    const PATH_TEMPLATE: &'static str = "projects/{project}/datasources/{name}.yaml";

    fn validate_spec(&self, meta: &ObjectMeta) -> Result<()> {
        names::validate_dns1123_label(&meta.name)?;
        self.validate()
    }
}

/// The URL schemes each connection type may reach, so a manifest cannot point an MQTT
/// subscription at a file or an HTTP poll at a broker.
const MQTT_SCHEMES: &[&str] = &["tcp://", "tls://", "ws://", "wss://"];
const HTTP_SCHEMES: &[&str] = &["http://", "https://"];
const WS_SCHEMES: &[&str] = &["ws://", "wss://"];

impl DataSourceSpec {
    /// Whether this input ends on its own once it has read what there is (PL-04, PL-50).
    ///
    /// Scheduled pipelines may only run terminating inputs; socket and broker inputs never
    /// terminate and must run resident.
    pub fn terminates(&self) -> bool {
        match &self.source_type {
            DataSourceType::Http | DataSourceType::GtfsRt => true,
            DataSourceType::Mqtt | DataSourceType::WebSocket => false,
            DataSourceType::Runner(name) => bento_inputs::TERMINATING.contains(&name.as_str()),
        }
    }

    /// Validates that exactly the declared connection is present and reachable as written.
    pub fn validate(&self) -> Result<()> {
        let present: Vec<&'static str> = [
            self.mqtt.is_some().then_some("mqtt"),
            self.http.is_some().then_some("http"),
            self.web_socket.is_some().then_some("webSocket"),
            self.gtfs_rt.is_some().then_some("gtfsRt"),
        ]
        .into_iter()
        .flatten()
        .collect();

        match &self.source_type {
            DataSourceType::Mqtt
            | DataSourceType::Http
            | DataSourceType::WebSocket
            | DataSourceType::GtfsRt => {
                let expected = match self.source_type {
                    DataSourceType::Mqtt => "mqtt",
                    DataSourceType::Http => "http",
                    DataSourceType::WebSocket => "webSocket",
                    DataSourceType::GtfsRt => "gtfsRt",
                    DataSourceType::Runner(_) => unreachable!(),
                };
                if present != [expected] {
                    return Err(Error::Name {
                        field: "spec",
                        value: present.join(", "),
                        reason: "exactly the connection block named by `type` is present (MF-35)",
                    });
                }
            }
            DataSourceType::Runner(name) => {
                if !bento_inputs::INPUTS.contains(&name.as_str()) {
                    let mut accepted = Vec::with_capacity(4 + bento_inputs::INPUTS.len());
                    accepted.extend_from_slice(&["mqtt", "http", "websocket", "gtfs-rt"]);
                    accepted.extend_from_slice(bento_inputs::INPUTS);
                    return Err(Error::Invalid {
                        field: "spec.type".to_string(),
                        reason: format!(
                            "unknown type `{name}`; accepted types are: {}",
                            accepted.join(", ")
                        ),
                    });
                }
                if !present.is_empty() {
                    return Err(Error::Name {
                        field: "spec",
                        value: present.join(", "),
                        reason: "runner data sources do not use typed connection blocks; configure `spec.input` instead",
                    });
                }
            }
        }

        if let Some(tls) = &self.tls {
            if self.source_type.runner_name().is_some() {
                return Err(Error::Name {
                    field: "spec.tls",
                    value: String::new(),
                    reason: "runner data sources carry their own tls configuration inside `spec.input`, not `spec.tls`",
                });
            }
            if tls.insecure_skip_verify {
                return Err(Error::Name {
                    field: "spec.tls.insecureSkipVerify",
                    value: "true".to_owned(),
                    reason: "a feed's certificate is always verified; use `caCertRef` for a private authority",
                });
            }
        }

        self.validate_secret_references()?;

        match &self.source_type {
            DataSourceType::Mqtt => {
                self.validate_typed_common()?;
                self.validate_mqtt()
            }
            DataSourceType::Http => {
                self.validate_typed_common()?;
                let http = self.http.as_ref().expect("checked above");
                url(&http.url, HTTP_SCHEMES, "spec.http.url")?;
                if let Some(authorization) = &http.authorization {
                    header_name(authorization.header_name())?;
                }
                verb(http.verb.as_deref())
            }
            DataSourceType::WebSocket => {
                self.validate_typed_common()?;
                url(
                    &self.web_socket.as_ref().expect("checked above").url,
                    WS_SCHEMES,
                    "spec.webSocket.url",
                )
            }
            DataSourceType::GtfsRt => {
                self.validate_typed_common()?;
                url(
                    &self.gtfs_rt.as_ref().expect("checked above").url,
                    HTTP_SCHEMES,
                    "spec.gtfsRt.url",
                )
            }
            DataSourceType::Runner(name) => self.validate_runner(name),
        }
    }

    /// Every credential reference this source carries, by the field a person typed it into.
    ///
    /// Until T-2238 only the runner path checked anything, and it checked the name alone: a typed
    /// `passwordRef` naming a GitHub token was checked green and would have been committed to the
    /// configuration repository in the clear. Each block is walked whatever `type` says, because a
    /// block that does not belong to the type is refused elsewhere and a reference in it is still a
    /// reference a person wrote.
    fn validate_secret_references(&self) -> Result<()> {
        if let Some(mqtt) = &self.mqtt {
            if let Some(password) = &mqtt.password_ref {
                password.validate("spec.mqtt.passwordRef")?;
            }
        }
        if let Some(http) = &self.http {
            if let Some(authorization) = &http.authorization {
                authorization
                    .header_ref
                    .validate("spec.http.authorization.headerRef")?;
            }
        }
        if let Some(tls) = &self.tls {
            if let Some(ca) = &tls.ca_cert_ref {
                ca.validate("spec.tls.caCertRef")?;
            }
        }
        for (index, secret) in self.secrets.iter().enumerate() {
            secret.validate(&format!("spec.secrets[{index}]"))?;
        }
        Ok(())
    }

    fn validate_typed_common(&self) -> Result<()> {
        if self.input.is_some() {
            return Err(Error::Name {
                field: "spec.input",
                value: String::new(),
                reason: "typed data sources do not declare `input`; use the connection block named by `type`",
            });
        }
        if !self.secrets.is_empty() {
            return Err(Error::Name {
                field: "spec.secrets",
                value: String::new(),
                reason: "typed data sources do not declare `secrets`; credentials belong in the connection block",
            });
        }
        Ok(())
    }

    fn validate_runner(&self, runner_name: &str) -> Result<()> {
        let input_obj = match &self.input {
            Some(serde_json::Value::Object(map)) if !map.is_empty() => map,
            _ => {
                return Err(Error::Name {
                    field: "spec.input",
                    value: String::new(),
                    reason: "`spec.input` must be present and a non-empty mapping for runner data sources",
                });
            }
        };

        for sref in &self.secrets {
            // The name and the key are checked for every type by `validate_secret_references`,
            // which names the field and never repeats what it refused (T-2238).
            match &sref.env_var {
                Some(ev) if ENV_VAR_RE.is_match(ev) => {}
                Some(ev) => {
                    return Err(Error::Name {
                        field: "spec.secrets.envVar",
                        value: ev.clone(),
                        reason: "envVar must match [A-Z][A-Z0-9_]*",
                    });
                }
                None => {
                    return Err(Error::Name {
                        field: "spec.secrets.envVar",
                        value: String::new(),
                        reason: "envVar is required for runner secrets (PL-50)",
                    });
                }
            }
        }

        let known_vars: std::collections::HashSet<&str> = self
            .secrets
            .iter()
            .filter_map(|s| s.env_var.as_deref())
            .collect();

        let secret_paths = bento_inputs::SECRET_FIELDS
            .iter()
            .find(|(n, _)| *n == runner_name)
            .map(|(_, paths)| *paths)
            .unwrap_or(&[]);

        let input_val = self.input.as_ref().expect("checked above");
        for path in secret_paths {
            let segments: Vec<&str> = path.split('.').collect();
            let mut targets = Vec::new();
            collect_secret_targets(input_val, &segments, "", &mut targets);
            for (concrete_path, target_val) in targets {
                let valid = if let serde_json::Value::String(s) = target_val {
                    if let Some(var) = is_exact_var_interpolation(s) {
                        known_vars.contains(var)
                    } else {
                        false
                    }
                } else {
                    false
                };

                if !valid {
                    return Err(Error::Invalid {
                        field: format!("spec.input.{concrete_path}"),
                        reason: "a runner-documented secret field holds a `${VAR}` interpolation naming an entry of spec.secrets, never a value (MF-35, PL-16)".to_string(),
                    });
                }
            }
        }

        let mut named = Vec::new();
        interpolated_names(input_val, &mut named);
        if let Some(name) = named.into_iter().find(|n| !known_vars.contains(n)) {
            return Err(Error::Invalid {
                field: "spec.input".to_string(),
                reason: format!(
                    "`${{{name}}}` names no envVar of spec.secrets; the runner's own environment is not reachable from a manifest (PL-50, PL-16)"
                ),
            });
        }

        let mut strings = Vec::new();
        strings_of(input_val, "spec.input".to_string(), &mut strings);
        for (path, text) in strings {
            for caps in EMBEDDED_PASSWORD_RE.captures_iter(text) {
                let password = caps
                    .get(1)
                    .or_else(|| caps.get(2))
                    .map_or("", |m| m.as_str());
                if is_exact_var_interpolation(password).is_none() {
                    return Err(Error::Invalid {
                        field: path,
                        reason: "a password inside a connection string is a `${VAR}` naming an envVar of spec.secrets, never a literal (PL-50, MF-35)".to_string(),
                    });
                }
            }
        }

        if bento_inputs::FILE_READERS.contains(&runner_name) {
            let check_file_path = |field: &'static str, val: &str| -> Result<()> {
                if !val.starts_with("/data/") || val.split('/').any(|seg| seg == "..") {
                    return Err(Error::Name {
                        field,
                        value: val.to_string(),
                        reason: "a file-reading input reads the runner's files volume under /data/",
                    });
                }
                Ok(())
            };

            if let Some(paths) = input_obj.get("paths") {
                if let Some(arr) = paths.as_array() {
                    for item in arr {
                        if let Some(s) = item.as_str() {
                            check_file_path("spec.input.paths", s)?;
                        }
                    }
                } else if let Some(s) = paths.as_str() {
                    check_file_path("spec.input.paths", s)?;
                }
            }

            if let Some(path) = input_obj.get("path") {
                if let Some(s) = path.as_str() {
                    check_file_path("spec.input.path", s)?;
                }
            }
        }

        Ok(())
    }

    fn validate_mqtt(&self) -> Result<()> {
        let mqtt = self.mqtt.as_ref().expect("checked by the caller");
        if mqtt.urls.is_empty() {
            return Err(Error::Name {
                field: "spec.mqtt.urls",
                value: String::new(),
                reason: "an MQTT connection needs at least one broker URL",
            });
        }
        if mqtt.topics.is_empty() {
            return Err(Error::Name {
                field: "spec.mqtt.topics",
                value: String::new(),
                reason: "an MQTT connection needs at least one topic filter",
            });
        }
        for broker in &mqtt.urls {
            url(broker, MQTT_SCHEMES, "spec.mqtt.urls")?;
        }
        if let Some(qos) = mqtt.qos {
            if qos > 2 {
                return Err(Error::Name {
                    field: "spec.mqtt.qos",
                    value: qos.to_string(),
                    reason: "MQTT quality of service is 0, 1 or 2",
                });
            }
        }
        Ok(())
    }

    /// The connection's secret references, in the order the runner needs them injected.
    ///
    /// The reconciler resolves these and nothing else: a secret nobody references is never
    /// read, and a credential is never carried in the manifest itself (PL-15).
    pub fn secret_refs(&self) -> Vec<&SecretRef> {
        match &self.source_type {
            DataSourceType::Mqtt => {
                let mut refs = Vec::new();
                if let Some(m) = &self.mqtt {
                    if let Some(p) = &m.password_ref {
                        refs.push(p);
                    }
                }
                if let Some(tls) = &self.tls {
                    if let Some(ca) = &tls.ca_cert_ref {
                        refs.push(ca);
                    }
                }
                refs
            }
            DataSourceType::Http => {
                let mut refs = Vec::new();
                if let Some(h) = &self.http {
                    if let Some(a) = &h.authorization {
                        refs.push(&a.header_ref);
                    }
                }
                if let Some(tls) = &self.tls {
                    if let Some(ca) = &tls.ca_cert_ref {
                        refs.push(ca);
                    }
                }
                refs
            }
            DataSourceType::WebSocket | DataSourceType::GtfsRt => {
                let mut refs = Vec::new();
                if let Some(tls) = &self.tls {
                    if let Some(ca) = &tls.ca_cert_ref {
                        refs.push(ca);
                    }
                }
                refs
            }
            DataSourceType::Runner(_) => self.secrets.iter().collect(),
        }
    }
}

/// Validates that the pipeline's execution class is compatible with the data source (PL-04, PL-50).
///
/// A scheduled pipeline (explicit `class: scheduled` or `auto` with period >= 30s) must only
/// read terminating data sources (e.g. files, queries, batch fetches). Sockets, brokers, and
/// streams never terminate on their own and must execute as resident pipelines.
pub fn check_class(pipeline: &PipelineSpec, source: &DataSourceSpec) -> Result<()> {
    if pipeline.is_scheduled() && !source.terminates() {
        return Err(Error::Invalid {
            field: "spec.class".to_string(),
            reason: format!(
                "{} never ends on its own; a broker or socket input is resident (PL-04, PL-50)",
                source.source_type
            ),
        });
    }
    Ok(())
}

fn collect_secret_targets<'a>(
    val: &'a serde_json::Value,
    segments: &[&str],
    current: &str,
    out: &mut Vec<(String, &'a serde_json::Value)>,
) {
    if segments.is_empty() {
        out.push((current.to_string(), val));
        return;
    }
    let seg = segments[0];
    let rest = &segments[1..];
    if let Some(array_field) = seg.strip_suffix("[]") {
        if let Some(serde_json::Value::Array(items)) = val.get(array_field) {
            for (idx, item) in items.iter().enumerate() {
                let next_prefix = if current.is_empty() {
                    format!("{array_field}[{idx}]")
                } else {
                    format!("{current}.{array_field}[{idx}]")
                };
                collect_secret_targets(item, rest, &next_prefix, out);
            }
        }
    } else if let Some(child) = val.get(seg) {
        let next_prefix = if current.is_empty() {
            seg.to_string()
        } else {
            format!("{current}.{seg}")
        };
        collect_secret_targets(child, rest, &next_prefix, out);
    }
}

/// Every string value in the document with its dotted path, for checks that read text.
fn strings_of<'a>(val: &'a serde_json::Value, path: String, out: &mut Vec<(String, &'a str)>) {
    match val {
        serde_json::Value::String(s) => out.push((path, s)),
        serde_json::Value::Array(items) => {
            for (i, v) in items.iter().enumerate() {
                strings_of(v, format!("{path}[{i}]"), out);
            }
        }
        serde_json::Value::Object(map) => {
            for (k, v) in map {
                strings_of(v, format!("{path}.{k}"), out);
            }
        }
        _ => {}
    }
}

/// Every name a `${NAME}` or `${NAME:default}` in the document interpolates, keys included:
/// the runner replaces them in the raw configuration before it parses it.
fn interpolated_names<'a>(val: &'a serde_json::Value, out: &mut Vec<&'a str>) {
    fn scan<'a>(mut rest: &'a str, out: &mut Vec<&'a str>) {
        while let Some(start) = rest.find("${") {
            let tail = &rest[start + 2..];
            let end = tail.find(['}', ':']).unwrap_or(tail.len());
            out.push(&tail[..end]);
            rest = &tail[end..];
        }
    }
    match val {
        serde_json::Value::String(s) => scan(s, out),
        serde_json::Value::Array(items) => items.iter().for_each(|v| interpolated_names(v, out)),
        serde_json::Value::Object(map) => map.iter().for_each(|(k, v)| {
            scan(k, out);
            interpolated_names(v, out);
        }),
        _ => {}
    }
}

fn is_exact_var_interpolation(s: &str) -> Option<&str> {
    if s.starts_with("${") && s.ends_with('}') && s.len() > 3 {
        let var = &s[2..s.len() - 1];
        if !var.is_empty()
            && !var.contains('$')
            && !var.contains('{')
            && !var.contains('}')
            && !var.contains(' ')
        {
            return Some(var);
        }
    }
    None
}

/// The environment variable a secret reference is injected as (PL-15, PL-16).
///
/// `envVar` wins where the manifest states one, so a `bento.yaml` that already expects a name
/// keeps working; otherwise the name is derived from the source and the key and cannot collide
/// with another source's in the same runner.
pub fn env_var_of(source_name: &str, reference: &SecretRef) -> String {
    if let Some(explicit) = &reference.env_var {
        return explicit.clone();
    }
    let part = |s: &str| {
        s.chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() {
                    c.to_ascii_uppercase()
                } else {
                    '_'
                }
            })
            .collect::<String>()
    };
    match &reference.key {
        Some(key) => format!("DS_{}_{}", part(source_name), part(key)),
        None => format!("DS_{}", part(source_name)),
    }
}

fn url(value: &str, schemes: &'static [&'static str], field: &'static str) -> Result<()> {
    if schemes.iter().any(|s| value.starts_with(s)) && value.len() > 8 {
        return Ok(());
    }
    Err(Error::Name {
        field,
        value: value.to_owned(),
        reason: "the URL scheme is not one this connection type speaks",
    })
}

/// An RFC 9110 field name: a non-empty token, so a manifest cannot smuggle a second header or
/// a value past the colon into the name.
fn header_name(value: &str) -> Result<()> {
    const TOKEN_PUNCTUATION: &str = "!#$%&'*+-.^_`|~";
    let is_token = !value.is_empty()
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || TOKEN_PUNCTUATION.contains(c));
    match is_token {
        true => Ok(()),
        false => Err(Error::Name {
            field: "spec.http.authorization.header",
            value: value.to_owned(),
            reason: "a header name is a token: letters, digits and !#$%&'*+-.^_`|~",
        }),
    }
}

fn verb(value: Option<&str>) -> Result<()> {
    match value {
        None | Some("GET") | Some("POST") => Ok(()),
        Some(other) => Err(Error::Name {
            field: "spec.http.verb",
            value: other.to_owned(),
            reason: "a polled feed is read with GET or POST",
        }),
    }
}
