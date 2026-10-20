//! `spec.lifetime` of a Pipeline (PL-80…PL-83): when an entity of a written type, or chosen
//! attributes of it, end. The rendered stream turns it into NGSI-LD `expiresAt` members after the
//! author's steps (`jc_manifest::lifetime`), so the choice lives in the manifest and never only in
//! a mapping comment.
//!
//! The checks here need neither the space's model nor a clock; the ones that do (a name that is
//! not a slot of the type, an `at` already past) are `jc_manifest::lifetime::check`.

use crate::error::{Error, Result};
use crate::names;
use chrono::{DateTime, FixedOffset};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::sync::LazyLock;

/// The most lifetime entries, one per type, a pipeline carries (PL-80).
pub const LIFETIME_MAX_TYPES: usize = 20;
/// The most attribute entries, and the most names in one entry (PL-80).
pub const LIFETIME_MAX_ATTRIBUTES: usize = 64;
/// The shortest duration: a minute (PL-80).
pub const DURATION_MIN_SECONDS: u64 = 60;
/// The longest duration: 3650 days (PL-80).
pub const DURATION_MAX_SECONDS: u64 = 3650 * 86_400;
/// The `from` that means the newest `observedAt` among the record's attributes (PL-80).
pub const OBSERVED_AT: &str = "observedAt";

/// The entity members of NGSI-LD that are no Attribute (DM-130): never a lifetime name.
const ENTITY_SYSTEM_MEMBERS: &[&str] = &[
    "id",
    "type",
    "scope",
    "createdAt",
    "modifiedAt",
    "deletedAt",
    "expiresAt",
];

/// A slot name a generated step may write unquoted: no quote, dot, space or newline.
static SLOT_RE: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"^[A-Za-z][A-Za-z0-9_]{0,63}$").expect("valid regex"));

/// ISO 8601 days, hours, minutes and seconds.
static DURATION_RE: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(
        r"^P(?:([0-9]{1,7})D)?(?:T(?:([0-9]{1,8})H)?(?:([0-9]{1,10})M)?(?:([0-9]{1,12})S)?)?$",
    )
    .expect("valid regex")
});

/// The lifetime of one written type (PL-80): the whole entity, chosen attributes, or both.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Lifetime {
    /// The entity type, a class of the target space's model that an output writes.
    #[serde(rename = "type")]
    pub entity_type: String,
    /// When the whole entity ends: its `expiresAt`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entity: Option<LifetimeMode>,
    /// When chosen attributes end: the `expiresAt` member of each named attribute.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attributes: Vec<AttributeLifetime>,
}

/// One of three modes (PL-80): `from` a field (with `plus` and `notAfter`), `rolling`, or `at`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct LifetimeMode {
    /// A date-time slot of the type, or `observedAt`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
    /// Added to the `from` time, an ISO 8601 duration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plus: Option<String>,
    /// Caps a `from` end at the write time plus this duration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub not_after: Option<String>,
    /// The write time plus this duration, renewed by each write.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rolling: Option<String>,
    /// One fixed RFC 3339 instant with an offset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub at: Option<String>,
}

/// The lifetime of chosen attributes of a type (PL-80): `names` and one mode.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct AttributeLifetime {
    /// One to 64 slots of the type.
    pub names: Vec<String>,
    /// See [`LifetimeMode::from`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
    /// See [`LifetimeMode::plus`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plus: Option<String>,
    /// See [`LifetimeMode::not_after`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub not_after: Option<String>,
    /// See [`LifetimeMode::rolling`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rolling: Option<String>,
    /// See [`LifetimeMode::at`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub at: Option<String>,
}

impl AttributeLifetime {
    /// The entry's mode, without its names.
    pub fn mode(&self) -> LifetimeMode {
        LifetimeMode {
            from: self.from.clone(),
            plus: self.plus.clone(),
            not_after: self.not_after.clone(),
            rolling: self.rolling.clone(),
            at: self.at.clone(),
        }
    }
}

/// A checked mode, durations in seconds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum End {
    /// The record's `field` plus `plus`, capped at the write time plus `not_after`.
    From {
        /// A slot name, or [`OBSERVED_AT`].
        field: String,
        /// Zero when absent.
        plus: u64,
        /// No cap when absent.
        not_after: Option<u64>,
    },
    /// The write time plus this.
    Rolling(u64),
    /// This instant.
    At(DateTime<FixedOffset>),
}

impl LifetimeMode {
    /// The checked mode; `path` names the target in a refusal.
    pub fn end(&self, path: &str) -> Result<End> {
        let refused = |reason: String| Error::Invalid {
            field: path.to_owned(),
            reason,
        };
        let modes = [
            self.from.is_some(),
            self.rolling.is_some(),
            self.at.is_some(),
        ];
        if modes.iter().filter(|m| **m).count() != 1 {
            return Err(refused(
                "write exactly one of `from`, `rolling` or `at` (PL-80)".to_owned(),
            ));
        }
        if self.from.is_none() && (self.plus.is_some() || self.not_after.is_some()) {
            return Err(refused(
                "`plus` and `notAfter` belong to `from` only (PL-81)".to_owned(),
            ));
        }
        let duration = |name: &str, text: &str| {
            seconds(text).ok_or_else(|| Error::Invalid {
                field: format!("{path}.{name}"),
                reason: format!(
                    "`{text}` is not a duration from `PT1M` to `P3650D`: write ISO 8601 days, \
                     hours, minutes and seconds, such as `PT5M`, `PT12H` or `P30D` (PL-80)"
                ),
            })
        };
        if let Some(field) = &self.from {
            if field != OBSERVED_AT && !is_slot_name(field) {
                return Err(Error::Invalid {
                    field: format!("{path}.from"),
                    reason: format!(
                        "`{}` is not a slot name: a letter, then letters, digits or `_`, or \
                         `observedAt` (PL-80)",
                        field.escape_debug()
                    ),
                });
            }
            return Ok(End::From {
                field: field.clone(),
                plus: self
                    .plus
                    .as_deref()
                    .map(|text| duration("plus", text))
                    .transpose()?
                    .unwrap_or(0),
                not_after: self
                    .not_after
                    .as_deref()
                    .map(|text| duration("notAfter", text))
                    .transpose()?,
            });
        }
        if let Some(text) = &self.rolling {
            return Ok(End::Rolling(duration("rolling", text)?));
        }
        let text = self.at.as_deref().unwrap_or_default();
        DateTime::parse_from_rfc3339(text)
            .map(End::At)
            .map_err(|_| Error::Invalid {
                field: format!("{path}.at"),
                reason: format!(
                    "`{}` is not an RFC 3339 instant with an offset, such as \
                     `2026-12-31T23:00:00Z` or `2026-12-31T23:00:00+01:00` (PL-80)",
                    text.escape_debug()
                ),
            })
    }
}

/// An ISO 8601 duration of days, hours, minutes and seconds in seconds, `None` when it is not
/// one or lies outside `PT1M`…`P3650D`.
pub fn seconds(text: &str) -> Option<u64> {
    let parts = DURATION_RE.captures(text)?;
    if text.ends_with('T') || text == "P" {
        return None;
    }
    let total = [(1, 86_400u64), (2, 3_600), (3, 60), (4, 1)]
        .iter()
        .try_fold(0u64, |sum, (group, unit)| {
            let count: u64 = parts
                .get(*group)
                .map_or(Some(0), |m| m.as_str().parse().ok())?;
            sum.checked_add(count.checked_mul(*unit)?)
        })?;
    (DURATION_MIN_SECONDS..=DURATION_MAX_SECONDS)
        .contains(&total)
        .then_some(total)
}

/// Whether a generated step may write `name` as an attribute: the slot pattern, no system member.
pub fn is_slot_name(name: &str) -> bool {
    SLOT_RE.is_match(name) && !ENTITY_SYSTEM_MEMBERS.contains(&name)
}

/// What [`validate`] reads of the rest of the Pipeline, so this module needs none of its types.
pub(super) struct Context<'a> {
    /// The types the outputs write; `None` when an output names none.
    pub written: Option<BTreeSet<&'a str>>,
    /// `spec.expiry.types`.
    pub expiry_types: &'a [String],
    /// Every Bloblang text of the author's steps and routes' steps.
    pub author_texts: &'a [String],
}

/// The admission checks of PL-81 and PL-83 that need no model and no clock.
pub(super) fn validate(lifetime: &[Lifetime], context: &Context<'_>) -> Result<()> {
    let refused = |field: String, reason: String| Error::Invalid { field, reason };
    if lifetime.len() > LIFETIME_MAX_TYPES {
        return Err(refused(
            "spec.lifetime".to_owned(),
            format!("name one to {LIFETIME_MAX_TYPES} types (PL-80)"),
        ));
    }
    let writes = scan_author_writes(context.author_texts);
    for (index, entry) in lifetime.iter().enumerate() {
        let path = format!("spec.lifetime[{index}]");
        names::validate_entity_type(&entry.entity_type)?;
        if lifetime[..index]
            .iter()
            .any(|other| other.entity_type == entry.entity_type)
        {
            return Err(refused(
                format!("{path}.type"),
                format!(
                    "`{}` has a lifetime already; write one entry per type (PL-80)",
                    entry.entity_type
                ),
            ));
        }
        // Every output names its type: the lifetime's type must be one of them. An output that
        // names none may write any type, which the model check judges.
        if let Some(written) = &context.written {
            if !written.contains(entry.entity_type.as_str()) {
                return Err(refused(
                    format!("{path}.type"),
                    format!(
                        "no output of this pipeline writes `{}`; the outputs write {} (PL-81)",
                        entry.entity_type,
                        written.iter().copied().collect::<Vec<_>>().join(", ")
                    ),
                ));
            }
        }
        if entry.entity.is_none() && entry.attributes.is_empty() {
            return Err(refused(
                path,
                "give `entity`, `attributes` or both; a type that keeps its entities needs no \
                 entry (PL-80)"
                    .to_owned(),
            ));
        }
        if let Some(mode) = &entry.entity {
            mode.end(&format!("{path}.entity"))?;
            if context.expiry_types.contains(&entry.entity_type) {
                return Err(refused(
                    format!("{path}.entity"),
                    format!(
                        "`{}` is also in spec.expiry.types: keep the stale sweep or the entity \
                         lifetime for it, not both (PL-83)",
                        entry.entity_type
                    ),
                ));
            }
            if writes.entity {
                return Err(refused(
                    format!("{path}.entity"),
                    "a step of this pipeline assigns the entity's `expiresAt` itself; remove that \
                     assignment from the mapping, the lifetime sets it (PL-83)"
                        .to_owned(),
                ));
            }
        }
        validate_attributes(&path, entry, &writes)?;
    }
    Ok(())
}

fn validate_attributes(path: &str, entry: &Lifetime, writes: &AuthorWrites) -> Result<()> {
    let refused = |field: String, reason: String| Error::Invalid { field, reason };
    if entry.attributes.len() > LIFETIME_MAX_ATTRIBUTES {
        return Err(refused(
            format!("{path}.attributes"),
            format!("write at most {LIFETIME_MAX_ATTRIBUTES} attribute entries (PL-80)"),
        ));
    }
    let mut seen = BTreeSet::new();
    for (index, attribute) in entry.attributes.iter().enumerate() {
        let at = format!("{path}.attributes[{index}]");
        if attribute.names.is_empty() || attribute.names.len() > LIFETIME_MAX_ATTRIBUTES {
            return Err(refused(
                format!("{at}.names"),
                format!("name one to {LIFETIME_MAX_ATTRIBUTES} attributes (PL-80)"),
            ));
        }
        for name in &attribute.names {
            if !SLOT_RE.is_match(name) {
                return Err(refused(
                    format!("{at}.names"),
                    format!(
                        "`{}` is not a slot name: a letter, then letters, digits or `_` (PL-81)",
                        name.escape_debug()
                    ),
                ));
            }
            if ENTITY_SYSTEM_MEMBERS.contains(&name.as_str()) {
                return Err(refused(
                    format!("{at}.names"),
                    format!(
                        "`{name}` is an NGSI-LD system member, not an attribute (DM-130, PL-81)"
                    ),
                ));
            }
            if !seen.insert(name.as_str()) {
                return Err(refused(
                    format!("{at}.names"),
                    format!("`{name}` is named twice; give each attribute one lifetime (PL-80)"),
                ));
            }
            if writes.any_key || writes.attributes.contains(name.as_str()) {
                return Err(refused(
                    format!("{at}.names"),
                    format!(
                        "a step of this pipeline writes an `expiresAt` that may be `{name}`'s; \
                         remove it from the mapping, the lifetime sets it (PL-83)"
                    ),
                ));
            }
        }
        attribute.mode().end(&at)?;
    }
    Ok(())
}

/// What the author's steps assign to `expiresAt`, by a static scan of their Bloblang (PL-83).
///
/// A scan cannot follow every expression; it does not have to be complete for safety, because
/// the generated step runs after the author's and its value is the one written.
#[derive(Debug, Default)]
struct AuthorWrites {
    /// `root.expiresAt = …`, or an `"expiresAt": …` key whose owner the scan cannot tell.
    entity: bool,
    /// `root.<name>.expiresAt = …`.
    attributes: BTreeSet<String>,
    /// An `"expiresAt": …` object key: it may sit on the entity or on any attribute.
    any_key: bool,
}

/// Where an assignment target starts: `root.`, or a line's start, where Bloblang reads a bare
/// path as `root.` too.
const ASSIGNED: &str = r"(?m)(?:^[ \t]*(?:root[ \t]*\.[ \t]*)?|\broot[ \t]*\.[ \t]*)";

static ROOT_EXPIRES_RE: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(&format!(r#"{ASSIGNED}"?expiresAt"?[ \t]*=(?:[^=]|$)"#)).expect("valid regex")
});
static ATTRIBUTE_EXPIRES_RE: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(&format!(
        r#"{ASSIGNED}"?([A-Za-z][A-Za-z0-9_]*)"?[ \t]*\.[ \t]*"?expiresAt"?[ \t]*=(?:[^=]|$)"#
    ))
    .expect("valid regex")
});
static KEY_EXPIRES_RE: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r#""expiresAt"\s*:"#).expect("valid regex"));

fn scan_author_writes(texts: &[String]) -> AuthorWrites {
    let mut writes = AuthorWrites::default();
    for text in texts {
        writes.attributes.extend(
            ATTRIBUTE_EXPIRES_RE
                .captures_iter(text)
                .map(|found| found[1].to_owned()),
        );
        let key = KEY_EXPIRES_RE.is_match(text);
        writes.any_key |= key;
        writes.entity |= key || ROOT_EXPIRES_RE.is_match(text);
    }
    writes
}
