//! The MQTT allowance of a project (PF-117…PF-119, ADR-N-048 §2.9–§2.11).
//!
//! `Organization.spec.limits.mqtt` is the allowance every project gets; `Project.spec.quotas.mqtt`
//! replaces it for one project as a whole object and may exceed it. Only an org admin writes
//! either. Inside the allowance a project member adds brokers without asking anyone; a broker
//! that would pass what is left is refused with the limit, its value, the usage and who can raise
//! it (PF-101).

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::kinds::mqtt_broker::{MqttBrokerSpec, MqttMode, DEDICATED_PORTS};

/// The default `brokers` once MQTT is on.
pub const DEFAULT_BROKERS: u32 = 1;
/// The default `connections`.
pub const DEFAULT_CONNECTIONS: u32 = 1000;
/// The default `messagesPerSecond`: one broker at its own defaults, 1000 connections × 10 a
/// second, fits.
pub const DEFAULT_MESSAGES_PER_SECOND: u32 = 10_000;
/// The default `maxMessageBytes`, the broker's own default.
pub const DEFAULT_MAX_MESSAGE_BYTES: u32 = 64 * 1024;

/// How a broker is addressed from outside (ADR-N-048 §2.11).
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "camelCase")]
pub enum MqttAddress {
    /// `<name>.<project>.mqtt.<domain>` on 8883.
    Platform,
    /// A port of the allowance's `ports`.
    DedicatedPort,
}

impl MqttAddress {
    fn word(self) -> &'static str {
        match self {
            Self::Platform => "platform",
            Self::DedicatedPort => "dedicatedPort",
        }
    }
}

/// `spec.limits.mqtt` of an Organization or `spec.quotas.mqtt` of a Project. Every number is
/// optional; an absent one is its catalog default once `enabled` is `true`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct MqttAllowance {
    /// Whether the project may declare brokers at all; off unless an org admin turns it on.
    #[serde(default)]
    pub enabled: bool,
    /// How many `MqttBroker` manifests the project may hold.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub brokers: Option<u32>,
    /// The sum of `spec.limits.maxConnections` over the project's brokers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub connections: Option<u32>,
    /// The sum of `maxConnections` × `messagesPerSecond` over the project's brokers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub messages_per_second: Option<u32>,
    /// The ceiling of each broker's `spec.limits.maxMessageBytes`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_message_bytes: Option<u32>,
    /// The broker modes the project may use; absent is `[ingest]`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub modes: Option<Vec<MqttMode>>,
    /// The address kinds the project may use; absent is `[platform]`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub addresses: Option<Vec<MqttAddress>>,
    /// Dedicated ports from 18830–18849 an org admin assigned to this project.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ports: Vec<u16>,
}

/// What a project's committed brokers use of its allowance (API/01 §47).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MqttUsage {
    /// Brokers declared.
    pub brokers: u32,
    /// The sum of `maxConnections`.
    pub connections: u64,
    /// The sum of `maxConnections` × `messagesPerSecond`.
    pub messages_per_second: u64,
}

impl MqttUsage {
    /// Counts one more broker.
    pub fn add(&mut self, spec: &MqttBrokerSpec) {
        self.brokers += 1;
        self.connections += u64::from(spec.limits.max_connections);
        self.messages_per_second += rate(spec);
    }
}

/// The most messages a broker may accept in one second.
fn rate(spec: &MqttBrokerSpec) -> u64 {
    u64::from(spec.limits.max_connections) * u64::from(spec.limits.messages_per_second)
}

/// The sentence every refusal ends with: who can raise the allowance, and where (PF-101).
pub fn raise(project: &str) -> String {
    format!("an org admin can raise this in Organization settings > Projects > {project}")
}

impl MqttAllowance {
    /// The numbers by their field name, as the catalog lists them under `spec.limits.mqtt`.
    pub fn numbers(&self) -> [(&'static str, Option<u32>); 4] {
        [
            ("brokers", self.brokers),
            ("connections", self.connections),
            ("messagesPerSecond", self.messages_per_second),
            ("maxMessageBytes", self.max_message_bytes),
        ]
    }

    /// The number in force for `field`: `0` while MQTT is off, else the value or the catalog's
    /// default.
    pub fn limit(&self, field: &str) -> u32 {
        if !self.enabled {
            return 0;
        }
        let default = match field {
            "brokers" => DEFAULT_BROKERS,
            "connections" => DEFAULT_CONNECTIONS,
            "messagesPerSecond" => DEFAULT_MESSAGES_PER_SECOND,
            "maxMessageBytes" => DEFAULT_MAX_MESSAGE_BYTES,
            _ => 0,
        };
        let set = self.numbers().into_iter().find(|(name, _)| *name == field);
        set.and_then(|(_, value)| value).unwrap_or(default)
    }

    /// The modes in force.
    pub fn modes(&self) -> Vec<MqttMode> {
        self.modes.clone().unwrap_or_else(|| vec![MqttMode::Ingest])
    }

    /// The address kinds in force.
    pub fn addresses(&self) -> Vec<MqttAddress> {
        self.addresses
            .clone()
            .unwrap_or_else(|| vec![MqttAddress::Platform])
    }

    /// The shape alone: ports from the reserved range, each once, and no empty list.
    pub fn validate(&self, field: &str) -> Result<()> {
        let invalid = |reason: String| Error::Invalid {
            field: field.to_owned(),
            reason: format!("{reason} (PF-119)"),
        };
        if self.modes.as_ref().is_some_and(Vec::is_empty) {
            return Err(invalid(
                "modes names no mode; leave it out for [ingest]".into(),
            ));
        }
        if self.addresses.as_ref().is_some_and(Vec::is_empty) {
            return Err(invalid(
                "addresses names no address kind; leave it out for [platform]".into(),
            ));
        }
        let mut seen = std::collections::BTreeSet::new();
        for port in &self.ports {
            if !DEDICATED_PORTS.contains(port) {
                return Err(invalid(format!(
                    "ports: {port} is outside the reserved range {}–{}",
                    DEDICATED_PORTS.start(),
                    DEDICATED_PORTS.end()
                )));
            }
            if !seen.insert(port) {
                return Err(invalid(format!("ports: {port} is listed twice")));
            }
        }
        Ok(())
    }

    /// Why `spec` may not join a project whose other brokers use `used`, or `None` when it fits
    /// what is left. `previous` is the broker's committed spec on an edit: an edit is never
    /// refused for the count, nor for a sum it does not raise, so a project over a lowered
    /// allowance can still fix a broker (PF-100). The sentence names the limit, its value, the
    /// usage and who can raise it (PF-101).
    pub fn refusal(
        &self,
        project: &str,
        used: &MqttUsage,
        spec: &MqttBrokerSpec,
        previous: Option<&MqttBrokerSpec>,
    ) -> Option<String> {
        let raise = raise(project);
        if !self.enabled {
            return Some(format!(
                "MQTT is off for project {project}; an org admin can turn it on in Organization \
                 settings > Projects > {project} (PF-117)"
            ));
        }
        let brokers = self.limit("brokers");
        if previous.is_none() && used.brokers + 1 > brokers {
            return Some(format!(
                "the project may run {brokers} MQTT brokers ({} in use); {raise} (PF-118)",
                used.brokers
            ));
        }
        let sums = [
            (
                "connections",
                "spec.limits.maxConnections",
                used.connections,
                u64::from(spec.limits.max_connections),
                previous.map(|p| u64::from(p.limits.max_connections)),
            ),
            (
                "messagesPerSecond",
                "maxConnections × messagesPerSecond",
                used.messages_per_second,
                rate(spec),
                previous.map(rate),
            ),
        ];
        for (field, what, in_use, asked, before) in sums {
            let limit = self.limit(field);
            if before.is_none_or(|before| asked > before) && in_use + asked > u64::from(limit) {
                return Some(format!(
                    "{what} of this broker is {asked}, and the project may hold {limit} MQTT \
                     {field} in all ({in_use} in use, {} left); {raise} (PF-118)",
                    u64::from(limit).saturating_sub(in_use)
                ));
            }
        }
        let bytes = self.limit("maxMessageBytes");
        if spec.limits.max_message_bytes > bytes {
            return Some(format!(
                "spec.limits.maxMessageBytes {} is above the project's {bytes}; {raise} (PF-118)",
                spec.limits.max_message_bytes
            ));
        }
        let modes = self.modes();
        let mode = spec.effective_mode();
        if !modes.contains(&mode) {
            let allowed: Vec<&str> = modes.iter().map(|m| m.word()).collect();
            return Some(format!(
                "spec.mode {} is not one the project may use ({}); {raise} (PF-119)",
                mode.word(),
                allowed.join(", ")
            ));
        }
        let addresses = self.addresses();
        let (address, refused) =
            match spec.port {
                Some(port) if !self.ports.contains(&port) => {
                    (
                        MqttAddress::DedicatedPort,
                        Some(format!(
                    "spec.port {port} is not assigned to project {project} (its ports: {}); \
                     an org admin assigns ports in Organization settings > Projects > {project} \
                     (PF-119)",
                    if self.ports.is_empty() {
                        "none".to_owned()
                    } else {
                        self.ports.iter().map(u16::to_string).collect::<Vec<_>>().join(", ")
                    }
                )),
                    )
                }
                Some(_) => (MqttAddress::DedicatedPort, None),
                None => (MqttAddress::Platform, None),
            };
        if refused.is_some() {
            return refused;
        }
        (!addresses.contains(&address)).then(|| {
            format!(
                "the {} address is not one the project may use; {raise} (PF-119)",
                address.word()
            )
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kinds::mqtt_broker::{MqttLimits, MqttRole};

    fn broker(connections: u32, rate: u32) -> MqttBrokerSpec {
        MqttBrokerSpec {
            roles: vec![MqttRole {
                name: "camera".into(),
                service_accounts: vec!["cam".into()],
                publish: vec!["cams/${clientid}/#".into()],
                subscribe: Vec::new(),
                shared_topics: false,
            }],
            limits: MqttLimits {
                max_connections: connections,
                messages_per_second: rate,
                ..MqttLimits::default()
            },
            port: None,
            mode: None,
            capture: None,
        }
    }

    fn on() -> MqttAllowance {
        MqttAllowance {
            enabled: true,
            ..MqttAllowance::default()
        }
    }

    fn refused(allowance: &MqttAllowance, used: &MqttUsage, spec: &MqttBrokerSpec) -> String {
        allowance
            .refusal("helsinki", used, spec, None)
            .expect("refused")
    }

    #[test]
    fn off_by_default_and_every_number_is_zero_then() {
        let off = MqttAllowance::default();
        assert_eq!(off.limit("brokers"), 0);
        let message = refused(&off, &MqttUsage::default(), &broker(10, 1));
        assert!(
            message.contains("MQTT is off for project helsinki"),
            "{message}"
        );
        assert!(message.contains("Organization settings > Projects > helsinki"));
    }

    #[test]
    fn once_on_the_catalog_defaults_hold() {
        let allowance = on();
        assert_eq!(allowance.limit("brokers"), 1);
        assert_eq!(allowance.limit("connections"), 1000);
        assert_eq!(allowance.limit("messagesPerSecond"), 10_000);
        assert_eq!(allowance.limit("maxMessageBytes"), 65536);
        assert_eq!(allowance.modes(), vec![MqttMode::Ingest]);
        // A broker at its own defaults fits the default allowance.
        assert_eq!(
            allowance.refusal("helsinki", &MqttUsage::default(), &broker(1000, 10), None),
            None
        );
    }

    #[test]
    fn the_broker_past_the_count_is_refused_with_the_count_and_the_usage() {
        let allowance = MqttAllowance {
            brokers: Some(2),
            connections: Some(20000),
            messages_per_second: Some(100_000),
            ..on()
        };
        let mut used = MqttUsage::default();
        used.add(&broker(10, 1));
        assert_eq!(
            allowance.refusal("helsinki", &used, &broker(10, 1), None),
            None
        );
        used.add(&broker(10, 1));
        let message = refused(&allowance, &used, &broker(10, 1));
        assert!(
            message.contains("the project may run 2 MQTT brokers (2 in use)"),
            "{message}"
        );
    }

    #[test]
    fn each_sum_is_refused_past_what_is_left() {
        let allowance = MqttAllowance {
            brokers: Some(5),
            connections: Some(1500),
            messages_per_second: Some(20_000),
            ..on()
        };
        let mut used = MqttUsage::default();
        used.add(&broker(1000, 10));
        let connections = refused(&allowance, &used, &broker(600, 1));
        assert!(
            connections.contains("1500 MQTT connections"),
            "{connections}"
        );
        assert!(
            connections.contains("1000 in use, 500 left"),
            "{connections}"
        );
        let rate = refused(&allowance, &used, &broker(500, 30));
        assert!(rate.contains("messagesPerSecond"), "{rate}");
        assert!(rate.contains("10000 in use, 10000 left"), "{rate}");
        // An edit that raises nothing passes in a project over a lowered allowance; one that
        // raises a sum past it does not, and an edit is never refused for the count.
        let held = broker(1000, 10);
        let mut over = used;
        over.add(&held);
        assert_eq!(
            allowance.refusal("helsinki", &over, &broker(900, 10), Some(&held)),
            None
        );
        assert!(allowance
            .refusal("helsinki", &over, &broker(1001, 10), Some(&held))
            .is_some_and(|m| m.contains("connections")));
        let full = MqttAllowance {
            brokers: Some(1),
            ..allowance
        };
        assert_eq!(
            full.refusal("helsinki", &used, &broker(10, 1), Some(&held)),
            None
        );
    }

    #[test]
    fn message_size_mode_and_address_come_from_the_allowance() {
        let allowance = on();
        let mut big = broker(10, 1);
        big.limits.max_message_bytes = 65537;
        assert!(refused(&allowance, &MqttUsage::default(), &big).contains("maxMessageBytes"));
        let mut relay = broker(10, 1);
        relay.mode = Some(MqttMode::Relay);
        let message = refused(&allowance, &MqttUsage::default(), &relay);
        assert!(message.contains("spec.mode relay is not one the project may use (ingest)"));
        let mut ported = broker(10, 1);
        ported.port = Some(18831);
        let message = refused(&allowance, &MqttUsage::default(), &ported);
        assert!(message.contains("18831 is not assigned to project helsinki (its ports: none)"));
        let assigned = MqttAllowance {
            ports: vec![18831],
            ..on()
        };
        let message = refused(&assigned, &MqttUsage::default(), &ported);
        assert!(message.contains("dedicatedPort address"), "{message}");
        let both = MqttAllowance {
            addresses: Some(vec![MqttAddress::DedicatedPort]),
            ..assigned
        };
        assert_eq!(
            both.refusal("helsinki", &MqttUsage::default(), &ported, None),
            None
        );
        assert!(refused(&both, &MqttUsage::default(), &broker(10, 1)).contains("platform address"));
    }

    #[test]
    fn the_shape_and_the_bounds_are_checked() {
        let bad_port = MqttAllowance {
            ports: vec![1883],
            ..on()
        };
        assert!(bad_port.validate("spec.quotas.mqtt").is_err());
        let twice = MqttAllowance {
            ports: vec![18830, 18830],
            ..on()
        };
        assert!(twice.validate("spec.quotas.mqtt").is_err());
        let no_mode = MqttAllowance {
            modes: Some(Vec::new()),
            ..on()
        };
        assert!(no_mode.validate("spec.quotas.mqtt").is_err());
        let read: MqttAllowance = serde_json::from_value(serde_json::json!({
            "enabled": true, "brokers": 2, "modes": ["ingest", "capture"],
            "addresses": ["platform", "dedicatedPort"], "ports": [18831]
        }))
        .expect("reads");
        assert_eq!(read.limit("brokers"), 2);
        assert!(serde_json::from_value::<MqttAllowance>(serde_json::json!({"x": 1})).is_err());
    }
}
