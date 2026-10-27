//! `kind: MqttBroker`, one MQTT edge broker of a project (MF-54, ADR-N-048).
//!
//! The broker is a release of the unmodified RMQTT image; the gateway answers its CONNECT checks
//! with the ACL of the role the connecting ServiceAccount holds (PL-75). Every rule one manifest
//! can be judged by is judged here; whether each named ServiceAccount exists and whether a
//! dedicated port is free across the repository is `jcctl validate`'s.

use crate::envelope::{Kind, ObjectMeta, Scope};
use crate::error::Result;
use crate::mqtt::topics::{self, invalid};
use crate::names;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::net::IpAddr;
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
/// The prefix of the username a pipeline runner connects with on a broker's internal listener;
/// no manifest may write a username that starts with it (PL-76).
pub const RUNNER_USERNAME: &str = "pipeline-runner";

/// The username the runner reads a broker of `project` with, `pipeline-runner-{project}`: a
/// broker admits only its own project's, so one project's stream never reads another's devices
/// (PL-76, T-5561).
pub fn runner_username(project: &str) -> String {
    format!("{RUNNER_USERNAME}-{project}")
}
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

/// The most networks `spec.allowedSources` may list (PF-129).
pub const MAX_ALLOWED_SOURCES: usize = 32;

/// One network of `spec.allowedSources`: an address with every host bit zero and its prefix.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SourceRange {
    /// The network address.
    pub network: IpAddr,
    /// The prefix length, at most 32 for IPv4 and 128 for IPv6.
    pub prefix: u8,
}

impl SourceRange {
    /// Reads `203.0.113.0/24` or `2001:db8::/48`; refuses a hostname, a missing or too long
    /// prefix, host bits set and a network of every address.
    pub fn parse(text: &str) -> std::result::Result<Self, String> {
        let (address, prefix) = text.split_once('/').ok_or_else(|| {
            format!("`{text}` is no CIDR range: write an address and a prefix, e.g. 203.0.113.0/24")
        })?;
        let network: IpAddr = address
            .parse()
            .map_err(|_| format!("`{text}` names no IP address (a hostname is not accepted)"))?;
        let bits: u8 = if network.is_ipv4() { 32 } else { 128 };
        let prefix: u8 = prefix
            .parse()
            .ok()
            .filter(|p| *p <= bits)
            .ok_or_else(|| format!("`{text}` has no prefix from 0 to {bits}"))?;
        if prefix == 0 {
            return Err(format!(
                "`{text}` is every address: leave allowedSources out instead"
            ));
        }
        let masked = mask(network, prefix);
        if masked != network {
            return Err(format!(
                "`{text}` has host bits set: the network is {masked}/{prefix}"
            ));
        }
        Ok(Self { network, prefix })
    }

    /// Whether `address` lies in the network; an IPv4-mapped IPv6 address counts as IPv4.
    pub fn contains(&self, address: IpAddr) -> bool {
        let address = address.to_canonical();
        address.is_ipv4() == self.network.is_ipv4() && mask(address, self.prefix) == self.network
    }
}

/// `address` with every bit past `prefix` cleared; `prefix` is at most the address's width.
fn mask(address: IpAddr, prefix: u8) -> IpAddr {
    let prefix = u32::from(prefix);
    match address {
        IpAddr::V4(ip) => {
            let keep = u32::MAX.checked_shl(32 - prefix).unwrap_or(0);
            IpAddr::V4((u32::from(ip) & keep).into())
        }
        IpAddr::V6(ip) => {
            let keep = u128::MAX.checked_shl(128 - prefix).unwrap_or(0);
            IpAddr::V6((u128::from(ip) & keep).into())
        }
    }
}

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
    /// The networks devices may connect from, in CIDR notation, at most 32; empty admits any
    /// address (PF-129). The edge and the gateway both hold a device to it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allowed_sources: Vec<String>,
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

/// A role: the topic filters it grants. Who holds it is a ServiceAccount RoleBinding
/// `mqtt/<broker>/<role>` (ADR-N-048 §2.13, [`crate::mqtt::roles_held`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct MqttRole {
    /// A DNS label, unique among the broker's roles.
    pub name: String,
    /// Deprecated (T-5872): ServiceAccounts of the broker's own project, by name. Still read for
    /// one release beside the bindings; `jcctl migrate` moves them to `mqtt/<broker>/<role>`
    /// bindings on each account.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub service_accounts: Vec<String>,
    /// Topic filters the role may publish to; `${clientid}` is the connecting client.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub publish: Vec<String>,
    /// Topic filters the role may subscribe to, each under the client's own `${clientid}`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub subscribe: Vec<String>,
    /// The role's devices share their `publish` topics, so each may publish as any other; without
    /// it every `publish` filter holds `${clientid}` before any wildcard (T-5561).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub shared_topics: bool,
    /// Why the role's devices share their topics; required with `sharedTopics`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
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
            for account in &role.service_accounts {
                names::validate_dns1123_label(account)?;
            }
            check_role(role, self.mode)?;
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
            self.check_capture(capture)?;
        }
        self.source_ranges()?;
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

    /// `spec.capture` of a capture broker, within its bounds.
    fn check_capture(&self, capture: &MqttCapture) -> Result<()> {
        if self.mode != Some(MqttMode::Capture) {
            return Err(invalid(
                "capture",
                "is read only by a broker with spec.mode: capture".into(),
            ));
        }
        if capture.messages_per_topic == 0 || capture.messages_per_topic > MAX_CAPTURE_MESSAGES {
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
        Ok(())
    }

    /// `spec.allowedSources` read, each entry a [`SourceRange`]; any unreadable entry, a repeat
    /// or more than [`MAX_ALLOWED_SOURCES`] refuses the whole list (PF-129).
    pub fn source_ranges(&self) -> Result<Vec<SourceRange>> {
        if self.allowed_sources.len() > MAX_ALLOWED_SOURCES {
            return Err(invalid(
                "allowedSources",
                format!(
                    "{} networks are more than {MAX_ALLOWED_SOURCES}: merge neighbouring ranges",
                    self.allowed_sources.len()
                ),
            ));
        }
        let mut ranges = Vec::with_capacity(self.allowed_sources.len());
        for text in &self.allowed_sources {
            let range = SourceRange::parse(text).map_err(|why| invalid("allowedSources", why))?;
            if ranges.contains(&range) {
                return Err(invalid(
                    "allowedSources",
                    format!("`{text}` is listed twice"),
                ));
            }
            ranges.push(range);
        }
        Ok(ranges)
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

/// Every rule one role is judged by on a broker of `mode`.
fn check_role(role: &MqttRole, mode: Option<MqttMode>) -> Result<()> {
    if role.publish.is_empty() && role.subscribe.is_empty() {
        return Err(invalid(
            "roles[]",
            format!("role `{}` grants no topic", role.name),
        ));
    }
    if role.shared_topics && role.reason.as_deref().is_none_or(|r| r.trim().is_empty()) {
        return Err(invalid(
            "roles[].reason",
            format!(
                "role `{}` sets sharedTopics, so each of its devices may publish as any other: \
                 write why in `reason`",
                role.name
            ),
        ));
    }
    for filter in &role.publish {
        topics::check("roles[].publish", filter)?;
        if !role.shared_topics && !topics::under_client_id(filter) {
            return Err(invalid(
                "roles[].publish",
                format!(
                    "`{filter}` lets each device of role `{}` publish as any other: write a \
                     {CLIENT_ID} level before any wildcard, or set sharedTopics: true with a \
                     reason on the role",
                    role.name
                ),
            ));
        }
    }
    if !role.subscribe.is_empty() && mode != Some(MqttMode::Relay) {
        let reason = match mode {
            None => format!(
                "role `{}` lets devices subscribe, which only a relay broker allows: set \
                 spec.mode: relay (ADR-N-048 §2.10)",
                role.name
            ),
            Some(mode) => format!(
                "role `{}` lets devices subscribe, and in {} mode devices get no subscribe \
                 right; use spec.mode: relay",
                role.name,
                mode.word()
            ),
        };
        return Err(invalid("roles[].subscribe", reason));
    }
    for filter in &role.subscribe {
        topics::check("roles[].subscribe", filter)?;
        if !topics::under_client_id(filter) {
            return Err(invalid(
                "roles[].subscribe",
                format!(
                    "`{filter}` reaches beyond the client's own topics: a device subscribes only \
                     under {CLIENT_ID}, with no wildcard before it"
                ),
            ));
        }
    }
    Ok(())
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
            shared_topics: false,
            reason: None,
        }
    }

    fn spec(roles: Vec<MqttRole>) -> MqttBrokerSpec {
        MqttBrokerSpec {
            roles,
            limits: MqttLimits::default(),
            port: None,
            mode: Some(MqttMode::Relay),
            capture: None,
            allowed_sources: Vec::new(),
        }
    }

    fn refusal(spec: &MqttBrokerSpec) -> String {
        spec.validate().expect_err("refused").to_string()
    }

    #[test]
    fn a_camera_role_publishing_under_its_own_id_is_valid() {
        let camera = spec(vec![role(
            "camera",
            &["cameras/${clientid}/#", "status/${clientid}/up"],
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
            role("camera", &["${clientid}/a"], &[]),
            role("camera", &["${clientid}/b"], &[]),
        ]);
        assert!(refusal(&twice).contains("declared twice"));
        // Who holds a role is a ServiceAccount binding now (T-5872): a role may list nobody.
        let mut nobody = role("camera", &["${clientid}/a"], &[]);
        nobody.service_accounts.clear();
        assert_eq!(spec(vec![nobody]).validate(), Ok(()));
        assert!(refusal(&spec(vec![role("camera", &[], &[])])).contains("grants no topic"));
        let mut bad_account = role("camera", &["${clientid}/a"], &[]);
        bad_account.service_accounts = vec!["Cam_North".into()];
        assert!(spec(vec![bad_account]).validate().is_err());
    }

    #[test]
    fn filters_follow_the_mqtt_grammar_and_never_reach_everything() {
        for (filter, why) in [
            ("#", "wildcards alone"),
            ("+/#", "wildcards alone"),
            ("a//${clientid}", "empty level"),
            ("a/b/c/d/e/f/g/h/${clientid}", "8 levels"),
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
        let long = format!("${{clientid}}/{}", "a".repeat(130));
        assert!(refusal(&spec(vec![role("camera", &[&long], &[])])).contains("bytes long"));
    }

    #[test]
    fn a_device_publish_filter_without_its_own_id_is_refused() {
        for filter in ["cameras/+/#", "cameras/x", "cameras/+/${clientid}"] {
            let message = refusal(&spec(vec![role("camera", &[filter], &[])]));
            assert!(
                message.contains("publish as any other"),
                "{filter}: {message}"
            );
        }
        for filter in ["cameras/${clientid}/#", "${clientid}", "a/${clientid}/+/b"] {
            assert_eq!(
                spec(vec![role("camera", &[filter], &[])]).validate(),
                Ok(())
            );
        }
        let mut shared = role("camera", &["cameras/+/#"], &[]);
        shared.shared_topics = true;
        assert!(refusal(&spec(vec![shared.clone()])).contains("write why in `reason`"));
        shared.reason = Some("a gateway forwards for every camera".into());
        assert_eq!(spec(vec![shared]).validate(), Ok(()));
    }

    #[test]
    fn the_runner_name_carries_the_project() {
        assert_eq!(runner_username("pamiatky"), "pipeline-runner-pamiatky");
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
            let mut broker = spec(vec![role("camera", &["${clientid}/a"], &[])]);
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
        let mut broker = spec(vec![role("camera", &["${clientid}/a"], &[])]);
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
        let mut broker = spec(vec![role(
            "camera",
            &["${clientid}/a"],
            &["commands/${clientid}/+"],
        )]);
        broker.mode = None;
        assert!(refusal(&broker).contains("set spec.mode: relay"));
        broker.mode = Some(MqttMode::Ingest);
        assert!(refusal(&broker).contains("in ingest mode devices get no subscribe right"));
        broker.mode = Some(MqttMode::Capture);
        assert!(refusal(&broker).contains("in capture mode"));
        let mut ingest = spec(vec![role("camera", &["${clientid}/a"], &[])]);
        ingest.mode = None;
        assert_eq!(ingest.validate().map_err(|e| e.to_string()), Ok(()));
        assert_eq!(ingest.effective_mode(), MqttMode::Ingest);
    }

    #[test]
    fn a_capture_is_bounded_and_belongs_to_the_capture_mode() {
        let mut broker = spec(vec![role("camera", &["${clientid}/a"], &[])]);
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
            "roles": [{"name": "camera", "serviceAccounts": ["cam-north"], "publish": ["${clientid}/a"]}]
        }))
        .expect("reads");
        assert_eq!(read.capture.and_then(|c| c.window_seconds()), Some(3600));
    }

    fn sources(list: &[&str]) -> MqttBrokerSpec {
        let mut broker = spec(vec![role("camera", &["${clientid}/a"], &[])]);
        broker.allowed_sources = list.iter().map(|s| (*s).to_owned()).collect();
        broker
    }

    #[test]
    fn allowed_sources_take_v4_and_v6_networks_and_admit_only_inside() {
        let broker = sources(&["203.0.113.0/24", "2001:db8::/48", "198.51.100.7/32"]);
        assert_eq!(broker.validate().map_err(|e| e.to_string()), Ok(()));
        let ranges = broker.source_ranges().expect("readable");
        let admits = |ip: &str| {
            let ip = ip.parse().expect("an address");
            ranges.iter().any(|range| range.contains(ip))
        };
        assert!(admits("203.0.113.200"));
        assert!(admits("::ffff:203.0.113.9"));
        assert!(admits("2001:db8:0:ffff::1"));
        assert!(admits("198.51.100.7"));
        assert!(!admits("198.51.100.8"));
        assert!(!admits("203.0.114.1"));
        assert!(!admits("2001:db9::1"));
        assert_eq!(
            sources(&[]).source_ranges().map_err(|e| e.to_string()),
            Ok(Vec::new())
        );
    }

    #[test]
    fn a_bad_source_list_is_refused_whole_with_the_field_named() {
        for (list, why) in [
            (
                vec!["203.0.113.1/24"],
                "host bits set: the network is 203.0.113.0/24",
            ),
            (vec!["2001:db8::1/48"], "host bits set"),
            (
                vec!["sensors.city.example/24"],
                "a hostname is not accepted",
            ),
            (vec!["203.0.113.0"], "no CIDR range"),
            (vec!["203.0.113.0/33"], "no prefix from 0 to 32"),
            (vec!["0.0.0.0/0"], "every address"),
            (vec!["::/0"], "every address"),
            (vec!["203.0.113.0/24", "203.0.113.0/24"], "listed twice"),
        ] {
            let refused = refusal(&sources(&list));
            assert!(refused.contains("spec.allowedSources"), "{refused}");
            assert!(refused.contains(why), "{list:?}: {refused}");
        }
        let many: Vec<String> = (0..33).map(|i| format!("10.0.{i}.0/24")).collect();
        let many: Vec<&str> = many.iter().map(String::as_str).collect();
        assert!(refusal(&sources(&many)).contains("more than 32"));
        assert_eq!(
            sources(&many[..32]).validate().map_err(|e| e.to_string()),
            Ok(())
        );
    }
}
