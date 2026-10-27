//! The MQTT allowance of a project (PF-117…PF-119, ADR-N-048 §2.9–§2.11).
//!
//! `Organization.spec.limits.mqtt` is the allowance every project gets; `Project.spec.quotas.mqtt`
//! replaces it for one project as a whole object and may exceed it. Only an org admin writes
//! either. Inside the allowance a project member adds brokers without asking anyone; a broker
//! that would pass what is left is refused with the limit, its value, the usage and who can raise
//! it (PF-101).

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::envelope::SecretRef;
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
/// The most `customHostnames` one allowance lists (ADR-N-048 §2.9).
pub const MAX_CUSTOM_HOSTNAMES: usize = 10;

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
    /// A name of the allowance's `customHostnames`, one the city owns, on 8883.
    CustomHostname,
}

impl MqttAddress {
    /// The word the manifest spells it with.
    pub fn word(self) -> &'static str {
        match self {
            Self::Platform => "platform",
            Self::DedicatedPort => "dedicatedPort",
            Self::CustomHostname => "customHostname",
        }
    }
}

/// How a device proves who it is when it connects (ADR-N-048 §2.12).
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "camelCase")]
pub enum MqttDeviceAuth {
    /// A Keycloak access token of a ServiceAccount as the CONNECT password.
    Token,
    /// A client certificate the project's client CA signed, its common name the ServiceAccount.
    ClientCertificate,
}

impl MqttDeviceAuth {
    /// The word the manifest spells it with.
    pub fn word(self) -> &'static str {
        match self {
            Self::Token => "token",
            Self::ClientCertificate => "clientCertificate",
        }
    }
}

/// One hostname an org admin assigned to a project (ADR-N-048 §2.11).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct MqttHostname {
    /// A lowercase DNS name the city owns and points at the edge by CNAME, e.g.
    /// `mqtt.city.example`.
    pub name: String,
    /// The secret holding the certificate the edge serves for `name`; absent, cert-manager issues
    /// one by HTTP-01 once the name resolves to the edge.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub certificate_ref: Option<SecretRef>,
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
    /// Hostnames the city owns that an org admin assigned to this project, at most 10; only a
    /// project's own `spec.quotas.mqtt` lists them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub custom_hostnames: Vec<MqttHostname>,
    /// How devices may authenticate; absent is `[token]`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_auth: Option<Vec<MqttDeviceAuth>>,
    /// The secret holding the project's device client CA certificate (never its key), required
    /// when `deviceAuth` holds `clientCertificate`; only a project's own `spec.quotas.mqtt` sets
    /// it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_ca_ref: Option<SecretRef>,
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

    /// The device authentications in force.
    pub fn device_auth(&self) -> Vec<MqttDeviceAuth> {
        self.device_auth
            .clone()
            .unwrap_or_else(|| vec![MqttDeviceAuth::Token])
    }

    /// The shape alone: ports from the reserved range, each once, hostnames DNS names each once,
    /// a client CA exactly when certificates are allowed, and no empty list.
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
        self.validate_hostnames(field)?;
        let auth = self.device_auth.as_deref().unwrap_or_default();
        if self.device_auth.is_some() && auth.is_empty() {
            return Err(invalid(
                "deviceAuth names no authentication; leave it out for [token]".into(),
            ));
        }
        if let Some(twice) = listed_twice(auth) {
            return Err(invalid(format!(
                "deviceAuth: {} is listed twice",
                twice.word()
            )));
        }
        let certificates = auth.contains(&MqttDeviceAuth::ClientCertificate);
        match &self.client_ca_ref {
            Some(ca) if certificates => ca.validate(&format!("{field}.clientCaRef"))?,
            Some(_) => {
                return Err(invalid(
                    "clientCaRef is read only when deviceAuth holds clientCertificate".into(),
                ))
            }
            None if certificates => {
                return Err(invalid(
                    "deviceAuth clientCertificate needs clientCaRef, the secret holding the \
                     project's device client CA certificate"
                        .into(),
                ))
            }
            None => {}
        }
        Ok(())
    }

    /// `customHostnames`: at most [`MAX_CUSTOM_HOSTNAMES`], each a DNS name listed once, each
    /// certificate a reference to a secret.
    fn validate_hostnames(&self, field: &str) -> Result<()> {
        let invalid = |reason: String| Error::Invalid {
            field: format!("{field}.customHostnames"),
            reason: format!("{reason} (PF-119)"),
        };
        if self.custom_hostnames.len() > MAX_CUSTOM_HOSTNAMES {
            return Err(invalid(format!(
                "{} hostnames are more than {MAX_CUSTOM_HOSTNAMES}",
                self.custom_hostnames.len()
            )));
        }
        for (i, hostname) in self.custom_hostnames.iter().enumerate() {
            if crate::names::validate_org_domain(&hostname.name).is_err() {
                return Err(invalid(format!(
                    "`{}` is not a lowercase DNS name of at least two labels, such as \
                     mqtt.city.example, with no scheme, port, path or wildcard",
                    hostname.name
                )));
            }
            if self.custom_hostnames[..i]
                .iter()
                .any(|earlier| earlier.name == hostname.name)
            {
                return Err(invalid(format!("`{}` is listed twice", hostname.name)));
            }
            if let Some(certificate) = &hostname.certificate_ref {
                certificate.validate(&format!("{field}.customHostnames[{i}].certificateRef"))?;
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
        let address = spec.effective_address();
        let assigned = match address {
            MqttAddress::DedicatedPort => {
                spec.port
                    .filter(|port| !self.ports.contains(port))
                    .map(|port| {
                        let ports: Vec<String> = self.ports.iter().map(u16::to_string).collect();
                        format!(
                            "spec.port {port} is not assigned to project {project} (its ports: {})",
                            or_none(&ports)
                        )
                    })
            }
            MqttAddress::CustomHostname => spec
                .hostname
                .as_deref()
                .filter(|name| !self.custom_hostnames.iter().any(|h| h.name == *name))
                .map(|name| {
                    let names: Vec<String> = self
                        .custom_hostnames
                        .iter()
                        .map(|h| h.name.clone())
                        .collect();
                    format!(
                        "spec.hostname {name} is not assigned to project {project} (its \
                         hostnames: {})",
                        or_none(&names)
                    )
                }),
            MqttAddress::Platform => None,
        };
        if let Some(refused) = assigned {
            return Some(format!(
                "{refused}; an org admin assigns them in Organization settings > Projects > \
                 {project} (PF-119)"
            ));
        }
        if !self.addresses().contains(&address) {
            return Some(format!(
                "the {} address is not one the project may use; {raise} (PF-119)",
                address.word()
            ));
        }
        let allowed = self.device_auth();
        spec.effective_device_auth()
            .into_iter()
            .find(|auth| !allowed.contains(auth))
            .map(|auth| {
                let words: Vec<String> = allowed.iter().map(|a| a.word().to_owned()).collect();
                format!(
                    "spec.deviceAuth {} is not one the project may use ({}); {raise} (PF-119)",
                    auth.word(),
                    words.join(", ")
                )
            })
    }
}

/// The first device authentication a list names a second time.
pub(crate) fn listed_twice(auth: &[MqttDeviceAuth]) -> Option<MqttDeviceAuth> {
    auth.iter()
        .enumerate()
        .find(|(i, a)| auth[..*i].contains(a))
        .map(|(_, a)| *a)
}

/// A list joined with commas, or `none`.
fn or_none(items: &[String]) -> String {
    if items.is_empty() {
        "none".to_owned()
    } else {
        items.join(", ")
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
                reason: None,
            }],
            limits: MqttLimits {
                max_connections: connections,
                messages_per_second: rate,
                ..MqttLimits::default()
            },
            port: None,
            address: None,
            hostname: None,
            device_auth: None,
            mode: None,
            capture: None,
            allowed_sources: Vec::new(),
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

    fn hostname(name: &str) -> MqttHostname {
        MqttHostname {
            name: name.into(),
            certificate_ref: None,
        }
    }

    fn ca() -> Option<SecretRef> {
        Some(SecretRef {
            name: "city-device-ca".into(),
            key: None,
            env_var: None,
        })
    }

    #[test]
    fn a_custom_hostname_must_be_one_the_project_holds() {
        let mut named = broker(10, 1);
        named.address = Some(MqttAddress::CustomHostname);
        named.hostname = Some("mqtt.city.example".into());
        let message = refused(&on(), &MqttUsage::default(), &named);
        assert!(
            message.contains("spec.hostname mqtt.city.example is not assigned to project helsinki (its hostnames: none)"),
            "{message}"
        );
        let other = MqttAllowance {
            custom_hostnames: vec![hostname("mqtt.other.example")],
            ..on()
        };
        let message = refused(&other, &MqttUsage::default(), &named);
        assert!(
            message.contains("(its hostnames: mqtt.other.example)"),
            "{message}"
        );
        let held = MqttAllowance {
            custom_hostnames: vec![hostname("mqtt.city.example")],
            ..on()
        };
        let message = refused(&held, &MqttUsage::default(), &named);
        assert!(
            message.contains("customHostname address is not one the project may use"),
            "{message}"
        );
        let allowed = MqttAllowance {
            addresses: Some(vec![MqttAddress::CustomHostname]),
            ..held
        };
        assert_eq!(
            allowed.refusal("helsinki", &MqttUsage::default(), &named, None),
            None
        );
    }

    #[test]
    fn a_device_authentication_must_be_one_the_project_may_use() {
        let mut certified = broker(10, 1);
        certified.device_auth = Some(vec![
            MqttDeviceAuth::Token,
            MqttDeviceAuth::ClientCertificate,
        ]);
        let message = refused(&on(), &MqttUsage::default(), &certified);
        assert!(
            message.contains(
                "spec.deviceAuth clientCertificate is not one the project may use (token)"
            ),
            "{message}"
        );
        let allowed = MqttAllowance {
            device_auth: Some(vec![
                MqttDeviceAuth::Token,
                MqttDeviceAuth::ClientCertificate,
            ]),
            client_ca_ref: ca(),
            ..on()
        };
        assert_eq!(
            allowed.refusal("helsinki", &MqttUsage::default(), &certified, None),
            None
        );
        let only_certificates = MqttAllowance {
            device_auth: Some(vec![MqttDeviceAuth::ClientCertificate]),
            ..allowed
        };
        let message = refused(&only_certificates, &MqttUsage::default(), &broker(10, 1));
        assert!(
            message.contains(
                "spec.deviceAuth token is not one the project may use (clientCertificate)"
            ),
            "{message}"
        );
    }

    #[test]
    fn hostnames_and_the_client_ca_are_checked_in_shape() {
        let field = "spec.quotas.mqtt";
        let error =
            |allowance: MqttAllowance| allowance.validate(field).expect_err("refused").to_string();
        let many = MqttAllowance {
            custom_hostnames: (0..=MAX_CUSTOM_HOSTNAMES)
                .map(|i| hostname(&format!("m{i}.city.example")))
                .collect(),
            ..on()
        };
        assert!(error(many).contains("11 hostnames are more than 10"));
        for bad in [
            "*.city.example",
            "city",
            "mqtt.city.example/x",
            "Mqtt.City.example",
        ] {
            let message = error(MqttAllowance {
                custom_hostnames: vec![hostname(bad)],
                ..on()
            });
            assert!(
                message.contains("invalid spec.quotas.mqtt.customHostnames"),
                "{bad}: {message}"
            );
        }
        let twice = MqttAllowance {
            custom_hostnames: vec![hostname("mqtt.city.example"), hostname("mqtt.city.example")],
            ..on()
        };
        assert!(error(twice).contains("`mqtt.city.example` is listed twice"));
        // A certificate is a reference to a secret, never the certificate or a token.
        let pasted = MqttAllowance {
            custom_hostnames: vec![MqttHostname {
                name: "mqtt.city.example".into(),
                certificate_ref: Some(SecretRef {
                    name: "-----BEGIN".into(),
                    key: None,
                    env_var: None,
                }),
            }],
            ..on()
        };
        assert!(error(pasted).contains("customHostnames[0].certificateRef.name"));
        let no_ca = MqttAllowance {
            device_auth: Some(vec![MqttDeviceAuth::ClientCertificate]),
            ..on()
        };
        assert!(error(no_ca).contains("deviceAuth clientCertificate needs clientCaRef"));
        let stray_ca = MqttAllowance {
            client_ca_ref: ca(),
            ..on()
        };
        assert!(error(stray_ca)
            .contains("clientCaRef is read only when deviceAuth holds clientCertificate"));
        let none = MqttAllowance {
            device_auth: Some(Vec::new()),
            ..on()
        };
        assert!(error(none).contains("deviceAuth names no authentication"));
        let twice = MqttAllowance {
            device_auth: Some(vec![MqttDeviceAuth::Token, MqttDeviceAuth::Token]),
            ..on()
        };
        assert!(error(twice).contains("deviceAuth: token is listed twice"));
        let read: MqttAllowance = serde_json::from_value(serde_json::json!({
            "enabled": true, "addresses": ["platform", "customHostname"],
            "customHostnames": [{"name": "mqtt.city.example"}, {"name": "iot.city.example", "certificateRef": {"name": "iot-city-tls"}}],
            "deviceAuth": ["token", "clientCertificate"], "clientCaRef": {"name": "city-device-ca", "key": "ca.crt"}
        }))
        .expect("reads");
        assert_eq!(read.validate(field).map_err(|e| e.to_string()), Ok(()));
        // An inline certificate or key has no field to sit in.
        assert!(serde_json::from_value::<MqttAllowance>(serde_json::json!({
            "customHostnames": [{"name": "mqtt.city.example", "certificate": "-----BEGIN CERTIFICATE-----"}]
        }))
        .is_err());
        assert!(serde_json::from_value::<MqttAllowance>(serde_json::json!({
            "clientCaRef": {"name": "city-device-ca", "value": "-----BEGIN CERTIFICATE-----"}
        }))
        .is_err());
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
