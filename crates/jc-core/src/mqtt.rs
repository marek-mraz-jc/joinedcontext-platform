//! Who holds a role of an MQTT broker (ADR-N-048 §2.13, MF-54, PL-75).
//!
//! The broker declares what each role may do; a ServiceAccount RoleBinding `mqtt/<broker>/<role>`
//! (or `mqtt/*/<role>`) says who holds it, scoped to the account's own project and within its
//! validity. [`roles_held`] is the one decision: the gateway's CONNECT and publish checks and the
//! Portal's explain route call it, and nothing else decides it.

pub mod topics;

use chrono::{DateTime, Utc};

use crate::error::{Error, Result};
use crate::kinds::mqtt_broker::MqttRole;
use crate::kinds::service_account::BindingValidity;
use crate::names;

/// Every MQTT grant's role starts with this.
pub const GRANT_PREFIX: &str = "mqtt/";
/// The broker part of a grant that names every broker of the project declaring the role.
pub const ANY_BROKER: &str = "*";

/// A RoleBinding role that grants a broker role: `mqtt/<broker>/<role>` or `mqtt/*/<role>`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Grant<'a> {
    /// The broker's name, `None` for every broker of the project (`*`).
    pub broker: Option<&'a str>,
    /// The role's name on the broker.
    pub role: &'a str,
}

impl<'a> Grant<'a> {
    /// The grant `text` names; `None` when it is no MQTT grant, an error when it starts like one
    /// and is not.
    pub fn parse(text: &'a str) -> Option<Result<Self>> {
        let rest = text.strip_prefix(GRANT_PREFIX)?;
        let refuse = || Error::Name {
            field: "spec.roles.role",
            value: text.to_owned(),
            reason: "an MQTT grant is `mqtt/<broker>/<role>` or `mqtt/*/<role>`, each name a DNS \
                     label (ADR-N-048 §2.13)",
        };
        let Some((broker, role)) = rest.split_once('/') else {
            return Some(Err(refuse()));
        };
        let broker_ok = broker == ANY_BROKER || names::validate_dns1123_label(broker).is_ok();
        if !broker_ok || names::validate_dns1123_label(role).is_err() {
            return Some(Err(refuse()));
        }
        Some(Ok(Self {
            broker: (broker != ANY_BROKER).then_some(broker),
            role,
        }))
    }

    /// Whether this grant names role `role` of broker `broker`.
    pub fn names(&self, broker: &str, role: &str) -> bool {
        self.role == role && self.broker.is_none_or(|b| b == broker)
    }
}

/// One RoleBinding of an account as [`roles_held`] reads it.
#[derive(Debug, Clone, Copy)]
pub struct Binding<'a> {
    /// The binding's `role`.
    pub role: &'a str,
    /// Its `scope.project`, `None` when it is scoped to anything else.
    pub project: Option<&'a str>,
    /// Its `validity`.
    pub validity: Option<&'a BindingValidity>,
}

/// One role of a broker an account holds, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Held<'a> {
    /// The role as the broker declares it.
    pub role: &'a MqttRole,
    /// Held through the deprecated `serviceAccounts` list of the role.
    pub legacy: bool,
    /// Held through at least one RoleBinding in force.
    pub bound: bool,
    /// When the last grant of the role ends; `None` while one has no end.
    pub until: Option<DateTime<Utc>>,
}

/// The account a broker's roles are decided for.
#[derive(Debug, Clone, Copy)]
pub struct Account<'a> {
    /// The ServiceAccount's name.
    pub name: &'a str,
    /// Its project.
    pub project: &'a str,
}

/// The broker the roles are decided on.
#[derive(Debug, Clone, Copy)]
pub struct Broker<'a> {
    /// Its name.
    pub name: &'a str,
    /// Its project.
    pub project: &'a str,
    /// Its roles, as declared.
    pub roles: &'a [MqttRole],
}

/// The roles of `broker` that `account` holds at `now`: each role the broker still declares
/// whose name a binding in force names (this broker or `*`, scoped to the broker's project), or
/// whose deprecated `serviceAccounts` lists the account. An account of another project holds
/// nothing, whatever its bindings say: deny by default.
pub fn roles_held<'a, 'b>(
    broker: Broker<'a>,
    account: Account<'_>,
    bindings: impl IntoIterator<Item = Binding<'b>>,
    now: DateTime<Utc>,
) -> Vec<Held<'a>> {
    if account.project != broker.project {
        return Vec::new();
    }
    let grants: Vec<(Grant<'b>, Option<DateTime<Utc>>)> = bindings
        .into_iter()
        .filter(|b| b.project == Some(broker.project))
        .filter(|b| b.validity.is_none_or(|v| v.contains(now)))
        .filter_map(|b| {
            let grant = Grant::parse(b.role)?.ok()?;
            Some((grant, b.validity.and_then(|v| v.not_after)))
        })
        .collect();
    broker
        .roles
        .iter()
        .filter_map(|role| {
            let legacy = role.service_accounts.iter().any(|a| a == account.name);
            let ends: Vec<Option<DateTime<Utc>>> = grants
                .iter()
                .filter(|(grant, _)| grant.names(broker.name, &role.name))
                .map(|(_, end)| *end)
                .collect();
            let bound = !ends.is_empty();
            if !legacy && !bound {
                return None;
            }
            let until = if legacy || ends.iter().any(Option::is_none) {
                None
            } else {
                ends.into_iter().flatten().max()
            };
            Some(Held {
                role,
                legacy,
                bound,
                until,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn role(name: &str, accounts: &[&str]) -> MqttRole {
        MqttRole {
            name: name.into(),
            service_accounts: accounts.iter().map(|s| (*s).into()).collect(),
            publish: vec!["air/${clientid}/pm10".into()],
            subscribe: Vec::new(),
            shared_topics: false,
            reason: None,
        }
    }

    fn at(day: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 11, day, 12, 0, 0)
            .single()
            .expect("a date")
    }

    fn names(held: &[Held<'_>]) -> Vec<String> {
        held.iter().map(|h| h.role.name.clone()).collect()
    }

    const FLEET: Account<'static> = Account {
        name: "air-fleet",
        project: "bratislava",
    };

    #[test]
    fn a_grant_names_a_broker_or_every_broker_and_a_role() {
        let one = Grant::parse("mqtt/air/air-sensor")
            .expect("mqtt")
            .expect("valid");
        assert!(one.names("air", "air-sensor"));
        assert!(!one.names("noise", "air-sensor"));
        let any = Grant::parse("mqtt/*/air-sensor")
            .expect("mqtt")
            .expect("valid");
        assert!(any.names("noise", "air-sensor"));
        assert!(!any.names("noise", "camera"));
        assert!(Grant::parse("reader").is_none());
        for bad in [
            "mqtt/air",
            "mqtt/Air/x",
            "mqtt/air/x/y",
            "mqtt//x",
            "mqtt/air/*",
        ] {
            assert!(matches!(Grant::parse(bad), Some(Err(_))), "{bad}");
        }
    }

    #[test]
    fn a_binding_admits_until_its_validity_ends_and_only_in_its_project() {
        let roles = [role("air-sensor", &[]), role("camera", &[])];
        let broker = Broker {
            name: "air",
            project: "bratislava",
            roles: &roles,
        };
        let window = BindingValidity {
            not_before: None,
            not_after: Some(at(10)),
        };
        let bound = Binding {
            role: "mqtt/air/air-sensor",
            project: Some("bratislava"),
            validity: Some(&window),
        };
        let held = roles_held(broker, FLEET, [bound], at(5));
        assert_eq!(names(&held), ["air-sensor"]);
        assert_eq!(held[0].until, Some(at(10)));
        assert!(held[0].bound);
        assert!(!held[0].legacy);
        assert!(roles_held(broker, FLEET, [bound], at(11)).is_empty());
        let other_scope = Binding {
            project: Some("kosice"),
            ..bound
        };
        assert!(roles_held(broker, FLEET, [other_scope], at(5)).is_empty());
        let other_account = Account {
            project: "kosice",
            ..FLEET
        };
        assert!(roles_held(broker, other_account, [bound], at(5)).is_empty());
        assert!(roles_held(broker, FLEET, [], at(5)).is_empty());
    }

    #[test]
    fn a_role_the_broker_no_longer_declares_grants_nothing() {
        let roles = [role("camera", &[])];
        let broker = Broker {
            name: "air",
            project: "bratislava",
            roles: &roles,
        };
        let bound = Binding {
            role: "mqtt/*/air-sensor",
            project: Some("bratislava"),
            validity: None,
        };
        assert!(roles_held(broker, FLEET, [bound], at(5)).is_empty());
    }

    #[test]
    fn the_deprecated_list_and_the_bindings_are_a_union_and_nothing_more() {
        let roles = [role("air-sensor", &["air-fleet"]), role("camera", &[])];
        let broker = Broker {
            name: "air",
            project: "bratislava",
            roles: &roles,
        };
        let held = roles_held(broker, FLEET, [], at(5));
        assert_eq!(names(&held), ["air-sensor"]);
        assert!(held[0].legacy);
        assert!(!held[0].bound);
        assert_eq!(held[0].until, None);
        let camera = Binding {
            role: "mqtt/*/camera",
            project: Some("bratislava"),
            validity: None,
        };
        let held = roles_held(broker, FLEET, [camera], at(5));
        assert_eq!(names(&held), ["air-sensor", "camera"]);
        assert!(held[1].bound);
    }
}
