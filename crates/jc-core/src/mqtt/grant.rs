//! The RoleBinding role that grants an MQTT broker role (ADR-N-048 §2.13): parsed here, so
//! the ServiceAccount kind can judge a binding without the decision in [`super::roles_held`].

use crate::error::{Error, Result};
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
