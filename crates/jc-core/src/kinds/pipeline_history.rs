//! `spec.history` of a Pipeline (PL-106…PL-110): per written attribute, which history rows the
//! broker keeps (every reading, only changes, the latest value only) and where the time of a row
//! comes from. The rendered stream sets the planned `observedAt` (`jc_manifest::history`), the
//! reconciler folds `keep` into the space's retention document, and the record check refuses a
//! plan the space or the broker cannot keep; the choice lives in the manifest, never only in a
//! mapping.
//!
//! The checks here need neither the space's model nor its retention; the ones that do (a name
//! that is not a slot, an `observedAt` slot that is no date-time) are `jc_manifest::history::check`.

use crate::error::{Error, Result};
use crate::names;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

use super::pipeline_lifetime::{
    is_slot_name, seconds, LIFETIME_MAX_ATTRIBUTES, LIFETIME_MAX_TYPES,
};

/// The `observedAt` that leaves the time of a row to the broker's receive time (DM-107).
pub const RECEIVED: &str = "received";

/// The history plan of one written type (PL-106).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct History {
    /// The entity type, one an output of the pipeline writes.
    #[serde(rename = "type")]
    pub entity_type: String,
    /// One to 64 entries, each attribute in one of them.
    pub attributes: Vec<AttributeHistory>,
}

/// What the broker keeps of an attribute's writes (PL-106).
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "camelCase")]
pub enum Keep {
    /// A row per measurement: the space rule `record: every`.
    Every,
    /// A row per change and once per `heartbeat`: `record: onChange` (DM-109).
    Changes,
    /// The current value only, no rows: holds where the type's history is `off` (DM-97).
    Latest,
}

impl Keep {
    /// The space rule's `record` this plan folds into, `None` for `latest` (PL-108).
    pub fn record(self) -> Option<&'static str> {
        match self {
            Keep::Every => Some(super::RECORD_EVERY),
            Keep::Changes => Some(super::RECORD_ON_CHANGE),
            Keep::Latest => None,
        }
    }
}

/// The plan of chosen attributes of a type (PL-106).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct AttributeHistory {
    /// One to 64 slots of the type.
    pub names: Vec<String>,
    /// Which rows the broker keeps.
    pub keep: Keep,
    /// With `changes` only: a row at least this often, `PT1M` to `P3650D`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub heartbeat: Option<String>,
    /// A date-time slot of the type whose value is the row's time, or [`RECEIVED`]; absent, the
    /// mapping decides.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_at: Option<String>,
}

/// The admission checks of PL-106 that need no model: `written` is the types the outputs write,
/// `None` when an output names none.
pub(super) fn validate(history: &[History], written: Option<&BTreeSet<&str>>) -> Result<()> {
    let refused = |field: String, reason: String| Error::Invalid { field, reason };
    if history.len() > LIFETIME_MAX_TYPES {
        return Err(refused(
            "spec.history".to_owned(),
            format!("name one to {LIFETIME_MAX_TYPES} types (PL-106)"),
        ));
    }
    for (index, entry) in history.iter().enumerate() {
        let path = format!("spec.history[{index}]");
        names::validate_entity_type(&entry.entity_type)?;
        if history[..index]
            .iter()
            .any(|other| other.entity_type == entry.entity_type)
        {
            return Err(refused(
                format!("{path}.type"),
                format!(
                    "`{}` has a history plan already; write one entry per type (PL-106)",
                    entry.entity_type
                ),
            ));
        }
        if let Some(written) = written {
            if !written.contains(entry.entity_type.as_str()) {
                return Err(refused(
                    format!("{path}.type"),
                    format!(
                        "no output of this pipeline writes `{}`; the outputs write {} (PL-106)",
                        entry.entity_type,
                        written.iter().copied().collect::<Vec<_>>().join(", ")
                    ),
                ));
            }
        }
        if entry.attributes.is_empty() || entry.attributes.len() > LIFETIME_MAX_ATTRIBUTES {
            return Err(refused(
                format!("{path}.attributes"),
                format!("write one to {LIFETIME_MAX_ATTRIBUTES} attribute entries (PL-106)"),
            ));
        }
        let mut seen = BTreeSet::new();
        for (at, attribute) in entry.attributes.iter().enumerate() {
            validate_attribute(&format!("{path}.attributes[{at}]"), attribute, &mut seen)?;
        }
    }
    Ok(())
}

fn validate_attribute<'a>(
    at: &str,
    attribute: &'a AttributeHistory,
    seen: &mut BTreeSet<&'a str>,
) -> Result<()> {
    let refused = |field: String, reason: String| Error::Invalid { field, reason };
    if attribute.names.is_empty() || attribute.names.len() > LIFETIME_MAX_ATTRIBUTES {
        return Err(refused(
            format!("{at}.names"),
            format!("name one to {LIFETIME_MAX_ATTRIBUTES} attributes (PL-106)"),
        ));
    }
    for name in &attribute.names {
        if !is_slot_name(name) {
            return Err(refused(
                format!("{at}.names"),
                format!(
                    "`{}` is not an attribute name: a letter, then letters, digits or `_`, and \
                     no NGSI-LD system member (DM-130, PL-106)",
                    name.escape_debug()
                ),
            ));
        }
        if !seen.insert(name.as_str()) {
            return Err(refused(
                format!("{at}.names"),
                format!("`{name}` is planned twice; give each attribute one plan (PL-106)"),
            ));
        }
    }
    match (attribute.keep, attribute.heartbeat.as_deref()) {
        (Keep::Changes, None) => {
            return Err(refused(
                format!("{at}.heartbeat"),
                "keep: changes needs a heartbeat, the longest gap between two rows, such as \
                 PT15M or P1D (DM-109, PL-106)"
                    .to_owned(),
            ))
        }
        (Keep::Changes, Some(text)) if seconds(text).is_none() => {
            return Err(refused(
                format!("{at}.heartbeat"),
                format!(
                    "`{}` is not a duration from `PT1M` to `P3650D`, such as `PT15M` or `P1D` \
                     (PL-106)",
                    text.escape_debug()
                ),
            ))
        }
        (Keep::Every | Keep::Latest, Some(_)) => {
            return Err(refused(
                format!("{at}.heartbeat"),
                "a heartbeat belongs to keep: changes only (PL-106)".to_owned(),
            ))
        }
        _ => {}
    }
    if let Some(field) = attribute.observed_at.as_deref() {
        if field != RECEIVED && !is_slot_name(field) {
            return Err(refused(
                format!("{at}.observedAt"),
                format!(
                    "`{}` is not a slot name or `received` (PL-106)",
                    field.escape_debug()
                ),
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plan(yaml: &str) -> Vec<History> {
        serde_norway::from_str(yaml).expect("plan parses")
    }

    fn refusal(yaml: &str) -> String {
        let written: BTreeSet<&str> = ["Vehicle"].into();
        match validate(&plan(yaml), Some(&written)) {
            Err(Error::Invalid { field, reason }) => format!("{field}: {reason}"),
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    #[test]
    fn the_three_choices_pass_with_their_settings() {
        let written: BTreeSet<&str> = ["Vehicle"].into();
        let ok = plan(
            "- type: Vehicle\n  attributes:\n\
             \x20 - { names: [location], keep: changes, heartbeat: PT15M, observedAt: observedOn }\n\
             \x20 - { names: [speed], keep: latest }\n\
             \x20 - { names: [delay], keep: every, observedAt: received }\n",
        );
        validate(&ok, Some(&written)).expect("valid plan");
        assert_eq!(Keep::Changes.record(), Some("onChange"));
        assert_eq!(Keep::Latest.record(), None);
    }

    #[test]
    fn each_rule_is_refused_with_its_path() {
        let cases = [
            ("- type: Bus\n  attributes: [{ names: [a], keep: every }]", "spec.history[0].type: no output"),
            ("- type: Vehicle\n  attributes: []", "spec.history[0].attributes: write one"),
            ("- type: Vehicle\n  attributes: [{ names: [], keep: every }]", "attributes[0].names: name one"),
            ("- type: Vehicle\n  attributes: [{ names: [expiresAt], keep: every }]", "system member"),
            ("- type: Vehicle\n  attributes: [{ names: [a], keep: every }, { names: [a], keep: latest }]", "planned twice"),
            ("- type: Vehicle\n  attributes: [{ names: [a], keep: changes }]", "needs a heartbeat"),
            ("- type: Vehicle\n  attributes: [{ names: [a], keep: changes, heartbeat: PT1S }]", "not a duration"),
            ("- type: Vehicle\n  attributes: [{ names: [a], keep: latest, heartbeat: PT1H }]", "changes only"),
            ("- type: Vehicle\n  attributes: [{ names: [a], keep: every, observedAt: \"a.b\" }]", "or `received`"),
            (
                "- type: Vehicle\n  attributes: [{ names: [a], keep: every }]\n- type: Vehicle\n  attributes: [{ names: [b], keep: every }]",
                "spec.history[1].type: `Vehicle` has a history plan already",
            ),
        ];
        for (yaml, expected) in cases {
            let said = refusal(yaml);
            assert!(said.contains(expected), "{yaml}\n  said: {said}");
        }
    }

    #[test]
    fn an_unknown_member_or_choice_does_not_parse() {
        assert!(serde_norway::from_str::<Vec<History>>(
            "- type: V\n  attributes: [{ names: [a], keep: sometimes }]"
        )
        .is_err());
        assert!(serde_norway::from_str::<Vec<History>>(
            "- type: V\n  attributes: [{ names: [a], keep: every, deadband: 1 }]"
        )
        .is_err());
    }
}
