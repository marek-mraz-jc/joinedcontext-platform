//! `kind: MqttBroker`, one MQTT edge broker of a project (MF-54, ADR-N-048).
//!
//! The broker is a release of the unmodified RMQTT image; the gateway answers its CONNECT checks
//! with the ACL of the role the connecting ServiceAccount holds (PL-75). Every rule one manifest
//! can be judged by is judged here; whether each named ServiceAccount exists and whether a
//! dedicated port is free across the repository is `jcctl validate`'s.

use crate::envelope::{Kind, ObjectMeta, Scope};
use crate::error::{Error, Result};
use crate::names;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::ops::RangeInclusive;

/// The placeholder RMQTT replaces with the verified client id (the token's `azp`).
pub const CLIENT_ID: &str = "${clientid}";
/// The ports a broker may hold for itself (ADR-N-048 §2.5).
pub const DEDICATED_PORTS: RangeInclusive<u16> = 18830..=18849;
/// The largest `limits.maxMessageBytes` a manifest may ask for (MF-54).
pub const MAX_MESSAGE_BYTES: u32 = 1024 * 1024;
/// The largest `limits.messagesPerSecond` (MF-54).
pub const MAX_MESSAGES_PER_SECOND: u32 = 1000;
/// The largest `limits.maxConnections` (MF-54).
pub const MAX_CONNECTIONS: u32 = 10_000;
/// The username a pipeline runner connects with on a broker's internal listener (PL-76).
pub const RUNNER_USERNAME: &str = "pipeline-runner";
/// The runner variable naming the namespace the brokers run in, filled by the runner.
pub const NAMESPACE_VARIABLE: &str = "JC_MQTT_NAMESPACE";

/// The broker's release, Service, Kubernetes ServiceAccount and Keycloak client name,
/// `mqtt-{project}-{name}`, bounded to a DNS label (ADR-N-048 §2.6).
pub fn release_name(project: &str, name: &str) -> String {
    crate::kinds::pipeline_identity::bounded(&format!("mqtt-{project}-{name}"))
}

/// The URL a pipeline reads a broker of its project at: the internal listener, plain MQTT on
/// 1883 inside the mesh (PL-76).
pub fn internal_url(project: &str, name: &str) -> String {
    format!(
        "tcp://{}.${{{NAMESPACE_VARIABLE}}}.svc.cluster.local:1883",
        release_name(project, name)
    )
}

/// The longest topic filter, in bytes; a filter is a pattern, not a payload.
const MAX_FILTER_BYTES: usize = 256;

/// One MQTT edge broker (MF-54).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct MqttBrokerSpec {
    /// Who may connect and to which topics; at least one role.
    pub roles: Vec<MqttRole>,
    /// Per-client and per-broker limits, each within the platform ceiling.
    #[serde(default)]
    pub limits: MqttLimits,
    /// A dedicated port from 18830–18849 instead of TLS by SNI on 8883.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    /// What the broker does with a message; absent is `ingest` (ADR-N-048 §2.10).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<MqttMode>,
    /// How much a `capture` broker keeps; read only in that mode.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capture: Option<MqttCapture>,
}

/// What a broker does with a message (ADR-N-048 §2.10).
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "camelCase")]
pub enum MqttMode {
    /// Devices publish and nothing else; the project's pipelines are the only subscribers.
    Ingest,
    /// `ingest`, and the Portal keeps the newest messages for the project's stewards.
    Capture,
    /// Devices may also subscribe, each under its own `${clientid}`.
    Relay,
}

impl MqttMode {
    /// The word a manifest writes.
    pub fn word(self) -> &'static str {
        match self {
            Self::Ingest => "ingest",
            Self::Capture => "capture",
            Self::Relay => "relay",
        }
    }
}

/// The most messages per topic a capture keeps (ADR-N-048 §2.10).
pub const MAX_CAPTURE_MESSAGES: u32 = 1000;
/// The longest a captured message is kept, in seconds.
pub const MAX_CAPTURE_SECONDS: u64 = 24 * 3600;

/// `spec.capture` of a `capture` broker.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct MqttCapture {
    /// Messages kept per topic, newest first.
    #[serde(default = "default_capture_messages")]
    pub messages_per_topic: u32,
    /// How long a message is kept from its arrival, an ISO 8601 duration `PT…`.
    #[serde(default = "default_capture_window")]
    pub window: String,
}

fn default_capture_messages() -> u32 {
    20
}

fn default_capture_window() -> String {
    "PT1H".into()
}

impl Default for MqttCapture {
    fn default() -> Self {
        Self {
            messages_per_topic: default_capture_messages(),
            window: default_capture_window(),
        }
    }
}

impl MqttCapture {
    /// The window in seconds, `None` when it does not parse.
    pub fn window_seconds(&self) -> Option<u64> {
        crate::kinds::agent_profile::parse_iso_duration(&self.window)
    }
}

/// A role: the ServiceAccounts that hold it and the topic filters it grants.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct MqttRole {
    /// A DNS label, unique among the broker's roles.
    pub name: String,
    /// ServiceAccounts of the broker's own project, by name.
    pub service_accounts: Vec<String>,
    /// Topic filters the role may publish to; `${clientid}` is the connecting client.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub publish: Vec<String>,
    /// Topic filters the role may subscribe to, each under the client's own `${clientid}`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub subscribe: Vec<String>,
}

/// The broker's limits (MF-54).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct MqttLimits {
    /// The largest message, in bytes.
    #[serde(default = "default_max_message_bytes")]
    pub max_message_bytes: u32,
    /// Messages one client may publish per second.
    #[serde(default = "default_messages_per_second")]
    pub messages_per_second: u32,
    /// Connections the broker holds at once.
    #[serde(default = "default_max_connections")]
    pub max_connections: u32,
}

fn default_max_message_bytes() -> u32 {
    64 * 1024
}

fn default_messages_per_second() -> u32 {
    10
}

fn default_max_connections() -> u32 {
    1000
}

impl Default for MqttLimits {
    fn default() -> Self {
        Self {
            max_message_bytes: default_max_message_bytes(),
            messages_per_second: default_messages_per_second(),
            max_connections: default_max_connections(),
        }
    }
}

impl Kind for MqttBrokerSpec {
    const KIND: &'static str = "MqttBroker";
    const PLURAL: &'static str = "mqttbrokers";
    const SCOPE: Scope = Scope::Project;
    const PATH_TEMPLATE: &'static str = "projects/{project}/mqttbrokers/{name}.yaml";

    fn validate_spec(&self, meta: &ObjectMeta) -> Result<()> {
        names::validate_dns1123_label(&meta.name)?;
        self.validate()
    }
}

impl MqttBrokerSpec {
    /// Every rule of MF-54 that one manifest can be judged by.
    pub fn validate(&self) -> Result<()> {
        if self.roles.is_empty() {
            return Err(invalid(
                "roles",
                "a broker declares at least one role".into(),
            ));
        }
        let mut names_seen = BTreeSet::new();
        for role in &self.roles {
            names::validate_dns1123_label(&role.name)?;
            if !names_seen.insert(role.name.as_str()) {
                return Err(invalid(
                    "roles[].name",
                    format!("role `{}` is declared twice", role.name),
                ));
            }
            if role.service_accounts.is_empty() {
                return Err(invalid(
                    "roles[].serviceAccounts",
                    format!("role `{}` names no ServiceAccount", role.name),
                ));
            }
            for account in &role.service_accounts {
                names::validate_dns1123_label(account)?;
            }
            if role.publish.is_empty() && role.subscribe.is_empty() {
                return Err(invalid(
                    "roles[]",
                    format!("role `{}` grants no topic", role.name),
                ));
            }
            for filter in &role.publish {
                check_filter("roles[].publish", filter)?;
            }
            if !role.subscribe.is_empty() && self.mode != Some(MqttMode::Relay) {
                let reason = match self.mode {
                    None => format!(
                        "role `{}` lets devices subscribe, which only a relay broker allows: set \
                         spec.mode: relay (ADR-N-048 §2.10)",
                        role.name
                    ),
                    Some(mode) => format!(
                        "role `{}` lets devices subscribe, and in {} mode devices get no \
                         subscribe right; use spec.mode: relay",
                        role.name,
                        mode.word()
                    ),
                };
                return Err(invalid("roles[].subscribe", reason));
            }
            for filter in &role.subscribe {
                check_filter("roles[].subscribe", filter)?;
                if !under_client_id(filter) {
                    return Err(invalid(
                        "roles[].subscribe",
                        format!(
                            "`{filter}` reaches beyond the client's own topics: a device subscribes only under {CLIENT_ID}, with no wildcard before it"
                        ),
                    ));
                }
            }
        }
        let limits = [
            (
                "maxMessageBytes",
                self.limits.max_message_bytes,
                MAX_MESSAGE_BYTES,
            ),
            (
                "messagesPerSecond",
                self.limits.messages_per_second,
                MAX_MESSAGES_PER_SECOND,
            ),
            (
                "maxConnections",
                self.limits.max_connections,
                MAX_CONNECTIONS,
            ),
        ];
        for (field, value, ceiling) in limits {
            if value == 0 || value > ceiling {
                return Err(invalid(
                    &format!("limits.{field}"),
                    format!("{value} is outside 1…{ceiling}"),
                ));
            }
        }
        if let Some(capture) = &self.capture {
            if self.mode != Some(MqttMode::Capture) {
                return Err(invalid(
                    "capture",
                    "is read only by a broker with spec.mode: capture".into(),
                ));
            }
            if capture.messages_per_topic == 0 || capture.messages_per_topic > MAX_CAPTURE_MESSAGES
            {
                return Err(invalid(
                    "capture.messagesPerTopic",
                    format!(
                        "{} is outside 1…{MAX_CAPTURE_MESSAGES}",
                        capture.messages_per_topic
                    ),
                ));
            }
            if !capture
                .window_seconds()
                .is_some_and(|seconds| (1..=MAX_CAPTURE_SECONDS).contains(&seconds))
            {
                return Err(invalid(
                    "capture.window",
                    format!("`{}` is not a duration from PT1S to PT24H", capture.window),
                ));
            }
        }
        if let Some(port) = self.port {
            if !DEDICATED_PORTS.contains(&port) {
                return Err(invalid(
                    "port",
                    format!(
                        "{port} is outside the reserved range {}–{}",
                        DEDICATED_PORTS.start(),
                        DEDICATED_PORTS.end()
                    ),
                ));
            }
        }
        Ok(())
    }

    /// The mode in force: `ingest` when the manifest names none.
    pub fn effective_mode(&self) -> MqttMode {
        self.mode.unwrap_or(MqttMode::Ingest)
    }

    /// The roles a ServiceAccount holds, in declaration order.
    pub fn roles_of<'a>(&'a self, account: &'a str) -> impl Iterator<Item = &'a MqttRole> {
        self.roles
            .iter()
            .filter(move |role| role.service_accounts.iter().any(|a| a == account))
    }
}

/// An MQTT topic filter as MQTT 3.1.1 §4.7 writes it, without `#` alone, without a `$` system
/// topic, and with `${clientid}` as the one placeholder, standing for a whole level.
fn check_filter(field: &str, filter: &str) -> Result<()> {
    let refuse = |why: &str| Err(invalid(field, format!("`{filter}` {why}")));
    if filter.is_empty() || filter.len() > MAX_FILTER_BYTES {
        return refuse(&format!("is not 1 to {MAX_FILTER_BYTES} bytes long"));
    }
    if filter == "#" {
        return refuse("is every topic");
    }
    if filter.starts_with('$') && !filter.starts_with(CLIENT_ID) {
        return refuse("names a broker system topic");
    }
    let levels: Vec<&str> = filter.split('/').collect();
    for (i, level) in levels.iter().enumerate() {
        if level.contains('#') && (*level != "#" || i + 1 != levels.len()) {
            return refuse("uses `#` other than as the whole last level");
        }
        if level.contains('+') && *level != "+" {
            return refuse("uses `+` other than as a whole level");
        }
        if level.contains("${") && *level != CLIENT_ID {
            return refuse(&format!(
                "uses a placeholder other than {CLIENT_ID} as a whole level"
            ));
        }
        if level.contains('\0') {
            return refuse("holds a NUL");
        }
    }
    Ok(())
}

/// Whether a filter reaches only topics under the client's own id: a `${clientid}` level comes
/// before any wildcard.
fn under_client_id(filter: &str) -> bool {
    for level in filter.split('/') {
        match level {
            CLIENT_ID => return true,
            "+" | "#" => return false,
            _ => {}
        }
    }
    false
}

fn invalid(field: &str, reason: String) -> Error {
    Error::Invalid {
        field: format!("spec.{field}"),
        reason: format!("{reason} (MF-54)"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn role(name: &str, publish: &[&str], subscribe: &[&str]) -> MqttRole {
        MqttRole {
            name: name.into(),
            service_accounts: vec!["cam-north".into()],
            publish: publish.iter().map(|s| (*s).to_owned()).collect(),
            subscribe: subscribe.iter().map(|s| (*s).to_owned()).collect(),
        }
    }

    fn spec(roles: Vec<MqttRole>) -> MqttBrokerSpec {
        MqttBrokerSpec {
            roles,
            limits: MqttLimits::default(),
            port: None,
            mode: Some(MqttMode::Relay),
            capture: None,
        }
    }

    fn refusal(spec: &MqttBrokerSpec) -> String {
        spec.validate().expect_err("refused").to_string()
    }

    #[test]
    fn a_camera_role_publishing_under_its_own_id_is_valid() {
        let camera = spec(vec![role(
            "camera",
            &["cameras/${clientid}/#", "status/+/up"],
            &["commands/${clientid}/+"],
        )]);
        assert_eq!(camera.validate().map_err(|e| e.to_string()), Ok(()));
        assert_eq!(camera.roles_of("cam-north").count(), 1);
        assert_eq!(camera.roles_of("cam-south").count(), 0);
    }

    #[test]
    fn the_manifest_reads_with_its_defaults() {
        let read: MqttBrokerSpec = serde_json::from_value(serde_json::json!({
            "roles": [{"name": "camera", "serviceAccounts": ["cam-north"], "publish": ["cameras/${clientid}/#"]}]
        }))
        .expect("reads");
        assert_eq!(read.limits, MqttLimits::default());
        assert_eq!(read.port, None);
        let unknown = serde_json::from_value::<MqttBrokerSpec>(serde_json::json!({
            "roles": [], "password": "x"
        }));
        assert!(unknown.is_err(), "no field can carry a credential");
    }

    #[test]
    fn roles_are_named_once_and_each_grants_something_to_someone() {
        assert!(refusal(&spec(Vec::new())).contains("at least one role"));
        let twice = spec(vec![
            role("camera", &["a"], &[]),
            role("camera", &["b"], &[]),
        ]);
        assert!(refusal(&twice).contains("declared twice"));
        let mut nobody = role("camera", &["a"], &[]);
        nobody.service_accounts.clear();
        assert!(refusal(&spec(vec![nobody])).contains("names no ServiceAccount"));
        assert!(refusal(&spec(vec![role("camera", &[], &[])])).contains("grants no topic"));
        let mut bad_account = role("camera", &["a"], &[]);
        bad_account.service_accounts = vec!["Cam_North".into()];
        assert!(spec(vec![bad_account]).validate().is_err());
    }

    #[test]
    fn filters_follow_the_mqtt_grammar_and_never_reach_everything() {
        for (filter, why) in [
            ("#", "every topic"),
            ("$SYS/#", "system topic"),
            ("", "bytes long"),
            ("a/#/b", "`#`"),
            ("a/b#", "`#`"),
            ("a/b+/c", "`+`"),
            ("a/${username}", "placeholder"),
            ("a/x${clientid}", "placeholder"),
            ("a/\0", "NUL"),
        ] {
            let message = refusal(&spec(vec![role("camera", &[filter], &[])]));
            assert!(message.contains(why), "{filter:?}: {message}");
        }
        let long = "a/".repeat(200);
        assert!(refusal(&spec(vec![role("camera", &[&long], &[])])).contains("bytes long"));
    }

    #[test]
    fn a_subscription_stays_under_the_clients_own_id() {
        for filter in ["commands/#", "commands/+/${clientid}", "commands/x"] {
            let message = refusal(&spec(vec![role("camera", &[], &[filter])]));
            assert!(
                message.contains("beyond the client's own topics"),
                "{filter}: {message}"
            );
        }
        assert!(spec(vec![role("camera", &[], &["${clientid}/#"])])
            .validate()
            .is_ok());
    }

    #[test]
    fn limits_hold_between_one_and_the_ceiling() {
        for (limits, field) in [
            (
                MqttLimits {
                    max_message_bytes: MAX_MESSAGE_BYTES + 1,
                    ..MqttLimits::default()
                },
                "maxMessageBytes",
            ),
            (
                MqttLimits {
                    messages_per_second: 0,
                    ..MqttLimits::default()
                },
                "messagesPerSecond",
            ),
            (
                MqttLimits {
                    max_connections: MAX_CONNECTIONS + 1,
                    ..MqttLimits::default()
                },
                "maxConnections",
            ),
        ] {
            let mut broker = spec(vec![role("camera", &["a"], &[])]);
            broker.limits = limits;
            assert!(refusal(&broker).contains(field), "{field}");
        }
    }

    #[test]
    fn a_broker_is_named_once_for_its_release_and_its_url() {
        assert_eq!(release_name("helsinki", "cameras"), "mqtt-helsinki-cameras");
        assert_eq!(release_name(&"p".repeat(40), &"n".repeat(40)).len(), 63);
        assert_eq!(
            internal_url("helsinki", "cameras"),
            "tcp://mqtt-helsinki-cameras.${JC_MQTT_NAMESPACE}.svc.cluster.local:1883"
        );
    }

    #[test]
    fn a_dedicated_port_comes_from_the_reserved_range() {
        let mut broker = spec(vec![role("camera", &["a"], &[])]);
        for port in [*DEDICATED_PORTS.start(), *DEDICATED_PORTS.end()] {
            broker.port = Some(port);
            assert!(broker.validate().is_ok(), "{port}");
        }
        for port in [1883, 8883, DEDICATED_PORTS.end() + 1] {
            broker.port = Some(port);
            assert!(refusal(&broker).contains("reserved range"), "{port}");
        }
    }

    #[test]
    fn devices_subscribe_only_on_a_relay_broker() {
        let mut broker = spec(vec![role("camera", &["a"], &["commands/${clientid}/+"])]);
        broker.mode = None;
        assert!(refusal(&broker).contains("set spec.mode: relay"));
        broker.mode = Some(MqttMode::Ingest);
        assert!(refusal(&broker).contains("in ingest mode devices get no subscribe right"));
        broker.mode = Some(MqttMode::Capture);
        assert!(refusal(&broker).contains("in capture mode"));
        let mut ingest = spec(vec![role("camera", &["a"], &[])]);
        ingest.mode = None;
        assert_eq!(ingest.validate().map_err(|e| e.to_string()), Ok(()));
        assert_eq!(ingest.effective_mode(), MqttMode::Ingest);
    }

    #[test]
    fn a_capture_is_bounded_and_belongs_to_the_capture_mode() {
        let mut broker = spec(vec![role("camera", &["a"], &[])]);
        broker.mode = Some(MqttMode::Ingest);
        broker.capture = Some(MqttCapture::default());
        assert!(refusal(&broker).contains("spec.mode: capture"));
        broker.mode = Some(MqttMode::Capture);
        assert_eq!(broker.validate().map_err(|e| e.to_string()), Ok(()));
        for (capture, field) in [
            (
                MqttCapture {
                    messages_per_topic: MAX_CAPTURE_MESSAGES + 1,
                    ..MqttCapture::default()
                },
                "messagesPerTopic",
            ),
            (
                MqttCapture {
                    window: "PT25H".into(),
                    ..MqttCapture::default()
                },
                "window",
            ),
            (
                MqttCapture {
                    window: "P1D".into(),
                    ..MqttCapture::default()
                },
                "window",
            ),
        ] {
            broker.capture = Some(capture);
            assert!(refusal(&broker).contains(field), "{field}");
        }
        let read: MqttBrokerSpec = serde_json::from_value(serde_json::json!({
            "mode": "capture", "capture": {},
            "roles": [{"name": "camera", "serviceAccounts": ["cam-north"], "publish": ["a"]}]
        }))
        .expect("reads");
        assert_eq!(read.capture.and_then(|c| c.window_seconds()), Some(3600));
    }
}
