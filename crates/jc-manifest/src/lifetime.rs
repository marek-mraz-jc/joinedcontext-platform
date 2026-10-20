//! `spec.lifetime` (PL-80…PL-83) compiled into one Bloblang step and checked against the space's
//! model: the step runs after the author's steps, immediately before the write, and sets the
//! entity's `expiresAt` or the `expiresAt` member of each named attribute (PL-82).
//!
//! The text is generated from checked parts only: types match the entity-type pattern, names
//! the slot pattern (no quote, dot or newline), durations are whole seconds and an `at` is
//! reformatted from its parsed instant. Every string still enters the text as a JSON literal or
//! a quoted path segment, never as code.

use chrono::{DateTime, SecondsFormat, Utc};
use jc_core::error::{Error, Result};
use jc_core::kinds::pipeline_lifetime::{is_slot_name, End, OBSERVED_AT};
use jc_core::kinds::{Lifetime, LifetimeMode, PipelineSpec};

use crate::model_schema::ModelSchema;

/// The metadata key a record whose `from` field is missing or no date-time carries (PL-82).
pub const PROBLEM_META: &str = "jc_problem";
/// Its value: the run's outcome lines count the record under this reason (PL-62).
pub const PROBLEM_FROM: &str = "lifetime-from";

/// The marker the per-entity map sets on a record whose `from` failed; removed before the write.
/// It starts with `_`, which no slot name does.
const MARKER: &str = "_jc_lifetime_from";

/// The entity members that are no attribute, skipped when the newest `observedAt` is looked for.
const ENTITY_MEMBERS: &str =
    r#""id", "type", "scope", "@context", "createdAt", "modifiedAt", "deletedAt", "expiresAt""#;

const FORMAT: &str = r#".ts_format("2006-01-02T15:04:05Z", "UTC")"#;

/// The model checks of PL-81 at proposal time: each type a class of the space's model, each
/// name one of its slots, each `from` a date-time slot or `observedAt`, no `at` already past.
pub fn check(spec: &PipelineSpec, model: &ModelSchema, now: DateTime<Utc>) -> Result<()> {
    for (index, entry) in spec.lifetime.iter().enumerate() {
        let path = format!("spec.lifetime[{index}]");
        let Some(class) = model.attributes(&entry.entity_type) else {
            return Err(Error::Invalid {
                field: format!("{path}.type"),
                reason: format!(
                    "the space's model declares no class `{}` (PL-81)",
                    entry.entity_type
                ),
            });
        };
        let slot = |name: &str| class.attributes.iter().find(|a| a.name == name);
        let mut targets: Vec<(String, LifetimeMode)> = Vec::new();
        if let Some(mode) = &entry.entity {
            targets.push((format!("{path}.entity"), mode.clone()));
        }
        for (at, attribute) in entry.attributes.iter().enumerate() {
            let at = format!("{path}.attributes[{at}]");
            if let Some(name) = attribute.names.iter().find(|name| slot(name).is_none()) {
                return Err(Error::Invalid {
                    field: format!("{at}.names"),
                    reason: format!(
                        "`{name}` is not a slot of `{}` in the space's model {} {} (PL-81)",
                        entry.entity_type, class.model, class.version
                    ),
                });
            }
            targets.push((at, attribute.mode()));
        }
        for (at, mode) in targets {
            match mode.end(&at)? {
                End::From { field, .. } if field != OBSERVED_AT => {
                    if slot(&field).and_then(|a| a.format.as_deref()) != Some("date-time") {
                        return Err(Error::Invalid {
                            field: format!("{at}.from"),
                            reason: format!(
                                "`{field}` is not a date-time slot of `{}`; name one, or \
                                 `observedAt` (PL-80)",
                                entry.entity_type
                            ),
                        });
                    }
                }
                End::At(instant) if instant <= now => {
                    return Err(Error::Invalid {
                        field: format!("{at}.at"),
                        reason: format!(
                            "`{}` has already passed; choose a later instant (PL-81)",
                            mode.at.unwrap_or_default()
                        ),
                    });
                }
                _ => {}
            }
        }
    }
    Ok(())
}

/// The Bloblang step that writes the lifetimes, `None` when the pipeline has none. It reads a
/// message holding one entity or an array of entities; a type without an entry passes unchanged.
/// The output is the same for the same manifest: entries by type, names sorted.
pub fn step(spec: &PipelineSpec) -> Result<Option<String>> {
    if spec.lifetime.is_empty() {
        return Ok(None);
    }
    let mut entries: Vec<(usize, &Lifetime)> = spec.lifetime.iter().enumerate().collect();
    entries.sort_by(|a, b| a.1.entity_type.cmp(&b.1.entity_type));
    let mut text = String::from(
        "# spec.lifetime (PL-80…PL-82), generated by jcctl: the end times, after the author's steps.\n\
         map jc_lifetime {\n  root = this\n",
    );
    for (index, entry) in entries {
        let path = format!("spec.lifetime[{index}]");
        text.push_str(&format!(
            "  if this.type == {} {{\n",
            literal(&entry.entity_type)
        ));
        if let Some(mode) = &entry.entity {
            text.push_str(&entity_block(&mode.end(&format!("{path}.entity"))?)?);
        }
        for (name, end) in attribute_targets(&path, entry)? {
            text.push_str(&attribute_block(&name, &end)?);
        }
        text.push_str("  }\n");
    }
    text.push_str(&tail());
    Ok(Some(text))
}

/// Sets the entity's `expiresAt`, or marks the record when its `from` gives no time.
fn entity_block(end: &End) -> Result<String> {
    let mut text = format!(
        "    let end = {}\n    if $end != null {{\n      root.expiresAt = $end{FORMAT}\n    }}\n",
        end_expression(end)?
    );
    if matches!(end, End::From { .. }) {
        text.push_str(&format!(
            "    if $end == null {{\n      root.{MARKER} = true\n    }}\n"
        ));
    }
    Ok(text)
}

/// Each named attribute with its end, sorted by name; a name off the slot pattern is refused.
fn attribute_targets(path: &str, entry: &Lifetime) -> Result<Vec<(String, End)>> {
    let mut attributes: Vec<(String, End)> = Vec::new();
    for (at, attribute) in entry.attributes.iter().enumerate() {
        let end = attribute.mode().end(&format!("{path}.attributes[{at}]"))?;
        if let Some(name) = attribute.names.iter().find(|name| !is_slot_name(name)) {
            return Err(Error::Invalid {
                field: format!("{path}.attributes[{at}].names"),
                reason: format!("`{}` is not a slot name (PL-81)", name.escape_debug()),
            });
        }
        attributes.extend(
            attribute
                .names
                .iter()
                .map(|name| (name.clone(), end.clone())),
        );
    }
    attributes.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(attributes)
}

/// Sets the `expiresAt` member of every instance of `name`, when the record carries it.
fn attribute_block(name: &str, end: &End) -> Result<String> {
    let mut text = format!(
        "    if this.exists({}) {{\n      let end = {}\n",
        literal(name),
        end_expression(end)?
    );
    text.push_str(&format!(
        "      if $end != null {{\n        let at = $end{FORMAT}\n        root.{name} = \
         this.{name}.(a -> if a.type() == \"array\" {{ a.map_each(i -> if i.type() == \
         \"object\" {{ i.assign({{\"expiresAt\": $at}}) }} else {{ i }}) }} else if a.type() \
         == \"object\" {{ a.assign({{\"expiresAt\": $at}}) }} else {{ a }})\n      }}\n"
    ));
    if matches!(end, End::From { .. }) {
        text.push_str(&format!(
            "      if $end == null {{\n        root.{MARKER} = true\n      }}\n"
        ));
    }
    text.push_str("    }\n");
    Ok(text)
}

/// Applies the map to one entity or each of an array, sets the problem and drops the marker.
fn tail() -> String {
    format!(
        "}}\n\
         let jc_out = if this.type() == \"array\" {{ this.map_each(e -> if e.type() == \"object\" {{ e.apply(\"jc_lifetime\") }} else {{ e }}) }} else if this.type() == \"object\" {{ this.apply(\"jc_lifetime\") }} else {{ this }}\n\
         let jc_failed = if $jc_out.type() == \"array\" {{ $jc_out.any(e -> e.type() == \"object\" && e.{MARKER}.or(false) == true) }} else {{ $jc_out.type() == \"object\" && $jc_out.{MARKER}.or(false) == true }}\n\
         if $jc_failed {{\n  meta {PROBLEM_META} = {}\n}}\n\
         root = if $jc_out.type() == \"array\" {{ $jc_out.map_each(e -> if e.type() == \"object\" {{ e.without({MARKER:?}) }} else {{ e }}) }} else if $jc_out.type() == \"object\" {{ $jc_out.without({MARKER:?}) }} else {{ $jc_out }}\n",
        literal(PROBLEM_FROM)
    )
}

/// The end as unix seconds, or `null` when a `from` field gives no date-time.
fn end_expression(end: &End) -> Result<String> {
    Ok(match end {
        End::Rolling(seconds) => format!("timestamp_unix() + {seconds}"),
        End::At(instant) => format!(
            "{}.ts_unix()",
            literal(
                &instant
                    .with_timezone(&Utc)
                    .to_rfc3339_opts(SecondsFormat::Secs, true)
            )
        ),
        End::From {
            field,
            plus,
            not_after,
        } => {
            let start = if field == OBSERVED_AT {
                format!(
                    "this.without({ENTITY_MEMBERS}).values().map_each(a -> if a.type() == \"array\" \
                     {{ a }} else {{ [a] }}).flatten().map_each(i -> if i.type() == \"object\" {{ \
                     i.observedAt.or(\"\").ts_unix().catch(null) }} else {{ null }}).filter(t -> t \
                     != null).max().catch(null)"
                )
            } else if is_slot_name(field) {
                format!(
                    "this.{field}.value.(v -> if v.type() == \"object\" {{ v.\"@value\" }} else \
                     {{ v }}).ts_unix().catch(null)"
                )
            } else {
                return Err(Error::Invalid {
                    field: "spec.lifetime".to_owned(),
                    reason: format!("`{}` is not a slot name (PL-80)", field.escape_debug()),
                });
            };
            let shifted = format!("({start}).(t -> if t != null {{ t + {plus} }} else {{ null }})");
            match not_after {
                None => shifted,
                Some(cap) => format!(
                    "{shifted}.(t -> if t != null {{ [t, timestamp_unix() + {cap}].min() }} else {{ null }})"
                ),
            }
        }
    })
}

/// A Bloblang string literal: JSON's quoting is Bloblang's.
fn literal(text: &str) -> String {
    serde_json::Value::String(text.to_owned()).to_string()
}
