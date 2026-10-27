//! The one MQTT topic validator (MF-54, ADR-N-048 §2.13): `jcctl validate`, the Portal and the
//! gateway judge a role's filters here and match a publish here, so the grammar a manifest is
//! refused by is the grammar a publish is decided by.

use crate::error::{Error, Result};
use crate::kinds::mqtt_broker::{MqttMode, MqttRole, CLIENT_ID};

/// The longest filter, in bytes.
pub const MAX_FILTER_BYTES: usize = 128;
/// The most levels a filter has.
pub const MAX_LEVELS: usize = 8;

/// One topic filter as MQTT 3.1.1 §4.7 writes it, narrowed: no filter of wildcards alone (`#`,
/// `+/#`), no `$` system topic, no empty level, no NUL, at most [`MAX_LEVELS`] levels and
/// [`MAX_FILTER_BYTES`] bytes, `${clientid}` the one placeholder, standing for a whole level.
pub fn check(field: &str, filter: &str) -> Result<()> {
    let refuse = |why: &str| Err(invalid(field, format!("`{filter}` {why}")));
    if filter.is_empty() || filter.len() > MAX_FILTER_BYTES {
        return refuse(&format!("is not 1 to {MAX_FILTER_BYTES} bytes long"));
    }
    if filter.starts_with('$') && !filter.starts_with(CLIENT_ID) {
        return refuse("names a broker system topic");
    }
    let levels: Vec<&str> = filter.split('/').collect();
    if levels.len() > MAX_LEVELS {
        return refuse(&format!("has more than {MAX_LEVELS} levels"));
    }
    if levels.iter().all(|level| matches!(*level, "+" | "#")) {
        return refuse("is wildcards alone and reaches every topic");
    }
    for (i, level) in levels.iter().enumerate() {
        if level.is_empty() {
            return refuse("has an empty level");
        }
        if level.contains('\0') {
            return refuse("holds a NUL");
        }
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
    }
    Ok(())
}

/// Whether a filter reaches only topics under the client's own id: a `${clientid}` level comes
/// before any wildcard.
pub fn under_client_id(filter: &str) -> bool {
    for level in filter.split('/') {
        match level {
            CLIENT_ID => return true,
            "+" | "#" => return false,
            _ => {}
        }
    }
    false
}

/// Every rule one role is judged by on a broker of `mode`.
pub fn check_role(role: &MqttRole, mode: Option<MqttMode>) -> Result<()> {
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
        check("roles[].publish", filter)?;
        if !role.shared_topics && !under_client_id(filter) {
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
        check("roles[].subscribe", filter)?;
        if !under_client_id(filter) {
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

/// What two roles of one broker grant twice: a filter both list, or one role's filter that
/// covers another's. Neither is refused (a role may well be a wider one), each is worth a look.
pub fn overlaps(roles: &[MqttRole]) -> Vec<String> {
    let mut found = Vec::new();
    for (i, a) in roles.iter().enumerate() {
        for b in roles.iter().skip(i.saturating_add(1)) {
            let pairs = [
                (&a.publish, &b.publish, "publish"),
                (&a.subscribe, &b.subscribe, "subscribe"),
            ];
            for (left, right, right_kind) in pairs {
                for x in left {
                    for y in right {
                        if x == y {
                            found.push(format!(
                                "roles `{}` and `{}` both grant {right_kind} `{x}`",
                                a.name, b.name
                            ));
                        } else if covers(x, y) {
                            found.push(format!(
                                "role `{}` {right_kind} `{x}` covers role `{}` `{y}`",
                                a.name, b.name
                            ));
                        } else if covers(y, x) {
                            found.push(format!(
                                "role `{}` {right_kind} `{y}` covers role `{}` `{x}`",
                                b.name, a.name
                            ));
                        }
                    }
                }
            }
        }
    }
    found
}

/// Whether every topic `narrow` reaches, `wide` reaches too (for the same client).
pub fn covers(wide: &str, narrow: &str) -> bool {
    let mut narrow = narrow.split('/');
    for level in wide.split('/') {
        if level == "#" {
            return true;
        }
        let Some(part) = narrow.next() else {
            return false;
        };
        let hit = match level {
            "+" => part != "#",
            _ => part == level,
        };
        if !hit {
            return false;
        }
    }
    narrow.next().is_none()
}

/// Whether the topic `topic` is under the filter `filter`, `${clientid}` standing for the level
/// `clientid` (MQTT 3.1.1 §4.7). A wildcard never reaches a `$` topic.
pub fn matches(filter: &str, topic: &str, clientid: &str) -> bool {
    if topic.starts_with('$') || topic.contains(['+', '#']) {
        return false;
    }
    let mut topic = topic.split('/');
    for level in filter.split('/') {
        if level == "#" {
            return true;
        }
        let Some(part) = topic.next() else {
            return false;
        };
        let hit = match level {
            "+" => true,
            CLIENT_ID => part == clientid,
            _ => part == level,
        };
        if !hit {
            return false;
        }
    }
    topic.next().is_none()
}

/// The refusal of `spec.{field}`, with the requirement it enforces.
pub(crate) fn invalid(field: &str, reason: String) -> Error {
    Error::Invalid {
        field: format!("spec.{field}"),
        reason: format!("{reason} (MF-54)"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kinds::MqttBrokerSpec;

    /// Every validator case as JSON: `{case, spec, refused}`, `refused` the field the refusal
    /// names or `null`. jc-core and the gateway (tests/mqtt_auth_tests.rs) each run the whole
    /// table, so the callers cannot drift apart.
    const CASES: &str = include_str!("topics_cases.json");

    #[derive(serde::Deserialize)]
    struct Case {
        case: String,
        spec: serde_json::Value,
        refused: Option<String>,
    }

    /// The field a refusal names, or `None` for a spec the validator admits.
    fn verdict(spec: serde_json::Value) -> Option<String> {
        let spec: MqttBrokerSpec = match serde_json::from_value(spec) {
            Ok(spec) => spec,
            Err(error) => return Some(format!("parse: {error}")),
        };
        match spec.validate() {
            Ok(()) => None,
            Err(Error::Invalid { field, .. }) => Some(field),
            Err(other) => Some(other.to_string()),
        }
    }

    #[test]
    fn every_case_of_the_shared_table_gets_its_verdict() {
        let cases: Vec<Case> = serde_json::from_str(CASES).expect("the table is JSON");
        assert!(cases.len() >= 10);
        for case in cases {
            assert_eq!(verdict(case.spec), case.refused, "{}", case.case);
        }
    }

    #[test]
    fn a_filter_covers_what_it_reaches_and_no_more() {
        assert!(covers("air/#", "air/${clientid}/pm10"));
        assert!(covers("air/+/pm10", "air/${clientid}/pm10"));
        assert!(!covers("air/+/pm10", "air/${clientid}/#"));
        assert!(!covers("air/${clientid}/pm10", "air/+/pm10"));
        assert!(!covers("air/x", "air/x/y"));
    }

    #[test]
    fn two_roles_granting_one_filter_or_a_wider_one_are_named() {
        let role = |name: &str, publish: &[&str]| MqttRole {
            name: name.into(),
            service_accounts: Vec::new(),
            publish: publish.iter().map(|s| (*s).into()).collect(),
            subscribe: Vec::new(),
            shared_topics: false,
            reason: None,
        };
        let found = overlaps(&[
            role("a", &["air/${clientid}/pm10"]),
            role("b", &["air/${clientid}/pm10"]),
            role("c", &["air/${clientid}/#"]),
            role("d", &["noise/${clientid}"]),
        ]);
        assert_eq!(
            found,
            vec![
                "roles `a` and `b` both grant publish `air/${clientid}/pm10`",
                "role `c` publish `air/${clientid}/#` covers role `a` `air/${clientid}/pm10`",
                "role `c` publish `air/${clientid}/#` covers role `b` `air/${clientid}/pm10`",
            ]
        );
    }

    #[test]
    fn a_topic_matches_its_filter_level_by_level() {
        assert!(matches("air/${clientid}/pm10", "air/cam-1/pm10", "cam-1"));
        assert!(!matches("air/${clientid}/pm10", "air/cam-2/pm10", "cam-1"));
        assert!(matches("air/${clientid}/#", "air/cam-1/a/b", "cam-1"));
        assert!(!matches("air/+", "$SYS/x", "cam-1"));
        assert!(!matches("air/+", "air/+", "cam-1"));
        assert!(!matches("air/x", "air/x/y", "cam-1"));
    }
}
