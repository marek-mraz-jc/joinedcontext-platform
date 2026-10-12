//! A space's quality rules judged on one NGSI-LD entity (DM-87..DM-92, ADR-N-046 2.4-2.6).
//!
//! The one evaluation the gateway's write path, the Portal's rejected route and its `maxAge`
//! sweep share: no network, no store, nothing but the entity, the rules and the clock the caller
//! passes. An attribute the entity does not carry, and a value the rule's kind does not read
//! (a string under a `range` rule), pass.

use crate::kinds::{duration_seconds, QualityKind, QualityOutcome, QualityRule};
use chrono::{DateTime, Duration, Utc};
use serde_json::{json, Map, Value};

/// The sub-Property a flagged attribute carries (DM-89).
pub const QUALITY_FLAGS: &str = "qualityFlags";
/// The Property a quarantined entity carries (DM-90).
pub const QUARANTINED_AT: &str = "quarantinedAt";
/// The Property naming the rules that quarantined an entity (DM-90).
pub const QUARANTINE_RULES: &str = "quarantineRules";

/// One rule's verdict on one entity: `None` passed.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Verdict<'a> {
    /// The rule judged.
    pub rule: &'a QualityRule,
    /// The outcome the rule asks for when the entity broke it.
    pub broken: Option<QualityOutcome>,
}

/// Whether `entity` is of `ty`, by its short name or an expanded IRI ending in it.
pub fn is_of_type(entity: &Value, ty: &str) -> bool {
    let named = |t: &str| {
        t == ty
            || t.rsplit_once(['/', '#', ':'])
                .is_some_and(|(_, short)| short == ty)
    };
    match entity.get("type") {
        Some(Value::String(t)) => named(t),
        Some(Value::Array(ts)) => ts.iter().filter_map(Value::as_str).any(named),
        _ => false,
    }
}

/// The instances of `attribute` in `entity`: one, or each of a multi-attribute array.
fn instances<'v>(entity: &'v Value, attribute: &str) -> Vec<&'v Value> {
    match entity.get(attribute) {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Array(all)) if all.iter().all(Value::is_object) && !all.is_empty() => {
            all.iter().collect()
        }
        Some(one) => vec![one],
    }
}

/// An instance's value: `value` of a normalized or concise attribute, else the plain value.
fn value_of(instance: &Value) -> &Value {
    match instance {
        Value::Object(o) if o.contains_key("value") => &o["value"],
        other => other,
    }
}

fn observed_at(instance: &Value) -> Option<DateTime<Utc>> {
    let raw = instance.get("observedAt")?.as_str()?;
    DateTime::parse_from_rfc3339(raw)
        .ok()
        .map(|t| t.with_timezone(&Utc))
}

/// Every `[lon, lat, ..]` position of a GeoJSON geometry, at any depth.
fn positions(coordinates: &Value, out: &mut Vec<(f64, f64)>) {
    if let Value::Array(items) = coordinates {
        match (
            items.first().and_then(Value::as_f64),
            items.get(1).and_then(Value::as_f64),
        ) {
            (Some(lon), Some(lat)) => out.push((lon, lat)),
            _ => items.iter().for_each(|item| positions(item, out)),
        }
    }
}

fn seconds(raw: Option<&str>) -> Option<Duration> {
    raw.and_then(duration_seconds)
        .and_then(|s| Duration::try_seconds(i64::try_from(s).ok()?))
}

/// Whether one instance breaks the write-time half of `rule`.
fn breaks_on_write(rule: &QualityRule, instance: &Value, now: DateTime<Utc>) -> bool {
    match rule.kind {
        QualityKind::Range => {
            let Some(v) = value_of(instance).as_f64() else {
                return false;
            };
            rule.min
                .as_ref()
                .and_then(|m| m.as_f64())
                .is_some_and(|min| v < min)
                || rule
                    .max
                    .as_ref()
                    .and_then(|m| m.as_f64())
                    .is_some_and(|max| v > max)
        }
        QualityKind::Area => {
            let (Some(bbox), Some(coordinates)) =
                (rule.bbox.as_ref(), value_of(instance).get("coordinates"))
            else {
                return false;
            };
            let b: Vec<f64> = bbox.iter().filter_map(|n| n.as_f64()).collect();
            let mut points = Vec::new();
            positions(coordinates, &mut points);
            b.len() == 4
                && points
                    .iter()
                    .any(|&(lon, lat)| lon < b[0] || lat < b[1] || lon > b[2] || lat > b[3])
        }
        QualityKind::Freshness => match (seconds(rule.max_ahead.as_deref()), observed_at(instance))
        {
            (Some(ahead), Some(at)) => at > now + ahead,
            _ => false,
        },
    }
}

/// The verdict of each rule of `entity`'s type on a write, the `maxAge` half aside (DM-87).
pub fn on_write<'a>(
    entity: &Value,
    rules: &'a [QualityRule],
    now: DateTime<Utc>,
) -> Vec<Verdict<'a>> {
    rules
        .iter()
        .filter(|rule| is_of_type(entity, &rule.entity_type))
        .filter(|rule| rule.kind != QualityKind::Freshness || rule.max_ahead.is_some())
        .map(|rule| Verdict {
            rule,
            broken: instances(entity, &rule.attribute)
                .into_iter()
                .any(|instance| breaks_on_write(rule, instance, now))
                .then(|| rule.write_outcome()),
        })
        .collect()
}

/// The sweep's verdict of each `maxAge` rule of `entity`'s type (DM-91): broken when an
/// instance's `observedAt` is older than `maxAge`.
pub fn on_sweep<'a>(
    entity: &Value,
    rules: &'a [QualityRule],
    now: DateTime<Utc>,
) -> Vec<Verdict<'a>> {
    rules
        .iter()
        .filter(|rule| rule.kind == QualityKind::Freshness && is_of_type(entity, &rule.entity_type))
        .filter_map(|rule| Some((rule, seconds(rule.max_age.as_deref())?)))
        .map(|(rule, age)| Verdict {
            rule,
            broken: instances(entity, &rule.attribute)
                .into_iter()
                .filter_map(observed_at)
                .any(|at| at < now - age)
                .then(|| rule.sweep_outcome()),
        })
        .collect()
}

/// The first rejecting verdict, the one a refusal names (DM-88).
pub fn rejection<'a>(verdicts: &[Verdict<'a>]) -> Option<&'a QualityRule> {
    verdicts
        .iter()
        .find(|v| v.broken == Some(QualityOutcome::Reject))
        .map(|v| v.rule)
}

/// Writes the flags and the quarantine of `verdicts` into `entity` before it is stored
/// (DM-89, DM-90). Every judged attribute loses the `qualityFlags` it came with, so a write
/// that breaks nothing stores none and a writer never sets a flag of its own. Nothing is
/// written when a verdict rejects: the write is refused instead.
pub fn mark(entity: &mut Value, verdicts: &[Verdict<'_>], now: DateTime<Utc>) {
    if rejection(verdicts).is_some() {
        return;
    }
    let Some(object) = entity.as_object_mut() else {
        return;
    };
    let mut flags: Map<String, Value> = Map::new();
    for verdict in verdicts {
        let attribute = &verdict.rule.attribute;
        for instance in instances_mut(object, attribute) {
            if let Value::Object(o) = instance {
                o.remove(QUALITY_FLAGS);
            }
        }
        if verdict.broken == Some(QualityOutcome::Flag) {
            if let Value::Array(names) = flags
                .entry(attribute.clone())
                .or_insert_with(|| Value::Array(Vec::new()))
            {
                names.push(Value::String(verdict.rule.name.clone()));
            }
        }
    }
    for (attribute, names) in flags {
        for instance in instances_mut(object, &attribute) {
            if !instance.is_object()
                || instance.get("value").is_none() && instance.get("type").is_none()
            {
                *instance = json!({ "type": "Property", "value": instance.take() });
            }
            if let Value::Object(o) = instance {
                o.insert(
                    QUALITY_FLAGS.into(),
                    json!({ "type": "Property", "value": names.clone() }),
                );
            }
        }
    }
    let quarantined: Vec<&str> = verdicts
        .iter()
        .filter(|v| v.broken == Some(QualityOutcome::Quarantine))
        .map(|v| v.rule.name.as_str())
        .collect();
    if !quarantined.is_empty() {
        quarantine(object, &quarantined, now);
    }
}

/// The Properties that hide an entity from all but its space's owner and steward (DM-90).
pub fn quarantine(entity: &mut Map<String, Value>, rules: &[&str], now: DateTime<Utc>) {
    entity.insert(
        QUARANTINED_AT.into(),
        json!({
            "type": "Property",
            "value": { "@type": "DateTime", "@value": now.to_rfc3339_opts(chrono::SecondsFormat::Secs, true) }
        }),
    );
    entity.insert(
        QUARANTINE_RULES.into(),
        json!({ "type": "Property", "value": rules }),
    );
}

fn instances_mut<'v>(object: &'v mut Map<String, Value>, attribute: &str) -> Vec<&'v mut Value> {
    let Some(value) = object.get_mut(attribute) else {
        return Vec::new();
    };
    if matches!(value, Value::Array(all) if all.iter().all(Value::is_object) && !all.is_empty()) {
        if let Value::Array(all) = value {
            return all.iter_mut().collect();
        }
    }
    if value.is_null() {
        Vec::new()
    } else {
        vec![value]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rules() -> Vec<QualityRule> {
        serde_json::from_value(json!([
            {"name": "speed-plausible", "entityType": "Vehicle", "kind": "range", "attribute": "speed", "min": 0, "max": 33.4},
            {"name": "inside-hsl-area", "entityType": "Vehicle", "kind": "area", "attribute": "location", "bbox": [24.5, 60.0, 25.3, 60.45]},
            {"name": "position-current", "entityType": "Vehicle", "kind": "freshness", "attribute": "location", "maxAhead": "PT30S", "maxAge": "PT5M"},
            {"name": "stop-speed", "entityType": "Stop", "kind": "range", "attribute": "speed", "max": 1, "outcome": "reject"}
        ]))
        .expect("rules")
    }

    fn now() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-10-12T06:00:00Z")
            .expect("time")
            .with_timezone(&Utc)
    }

    fn vehicle(speed: Value, lon: f64, observed: &str) -> Value {
        json!({
            "id": "urn:ngsi-ld:Vehicle:1",
            "type": "Vehicle",
            "speed": {"type": "Property", "value": speed},
            "location": {"type": "GeoProperty", "value": {"type": "Point", "coordinates": [lon, 60.2]}, "observedAt": observed}
        })
    }

    fn broken<'a>(verdicts: &[Verdict<'a>]) -> Vec<(&'a str, QualityOutcome)> {
        verdicts
            .iter()
            .filter_map(|v| Some((v.rule.name.as_str(), v.broken?)))
            .collect()
    }

    #[test]
    fn a_clean_write_passes_every_rule_of_its_type_and_stores_no_flag() {
        let rules = rules();
        let mut entity = vehicle(json!(12.5), 24.9, "2026-10-12T05:59:50Z");
        entity["speed"]["qualityFlags"] = json!({"type": "Property", "value": ["forged"]});
        let verdicts = on_write(&entity, &rules, now());
        assert_eq!(verdicts.len(), 3, "the Stop rule is not the Vehicle's");
        assert!(broken(&verdicts).is_empty());
        mark(&mut entity, &verdicts, now());
        assert!(
            entity["speed"].get(QUALITY_FLAGS).is_none(),
            "a writer sets no flag"
        );
        assert!(entity.get(QUARANTINED_AT).is_none());
    }

    #[test]
    fn range_and_area_flag_and_freshness_ahead_rejects() {
        let rules = rules();
        let mut entity = vehicle(json!(40), 26.0, "2026-10-12T05:59:50Z");
        let verdicts = on_write(&entity, &rules, now());
        assert_eq!(
            broken(&verdicts),
            [
                ("speed-plausible", QualityOutcome::Flag),
                ("inside-hsl-area", QualityOutcome::Flag)
            ]
        );
        mark(&mut entity, &verdicts, now());
        assert_eq!(
            entity["speed"][QUALITY_FLAGS]["value"],
            json!(["speed-plausible"])
        );
        assert_eq!(
            entity["location"][QUALITY_FLAGS]["value"],
            json!(["inside-hsl-area"])
        );

        let ahead = vehicle(json!(-1), 24.9, "2026-10-12T06:01:00Z");
        let verdicts = on_write(&ahead, &rules, now());
        assert_eq!(
            rejection(&verdicts).map(|r| r.name.as_str()),
            Some("position-current")
        );
        let mut kept = ahead.clone();
        mark(&mut kept, &verdicts, now());
        assert_eq!(kept, ahead, "a refused write is not marked");
    }

    #[test]
    fn absent_attributes_and_values_of_another_kind_pass() {
        let rules = rules();
        let bare =
            json!({"id": "urn:ngsi-ld:Vehicle:2", "type": "https://smartdatamodels.org/Vehicle"});
        let verdicts = on_write(&bare, &rules, now());
        assert_eq!(verdicts.len(), 3, "an expanded type matches its short name");
        assert!(broken(&verdicts).is_empty());
        let odd = json!({"id": "x", "type": "Vehicle", "speed": "fast", "location": {"type": "GeoProperty", "value": "POINT(1 2)"}});
        assert!(broken(&on_write(&odd, &rules, now())).is_empty());
        assert!(on_write(&json!({"type": "Bus"}), &rules, now()).is_empty());
    }

    #[test]
    fn every_point_of_a_geometry_and_every_instance_is_checked() {
        let rules = rules();
        let line = json!({"type": "Vehicle", "location": {"type": "GeoProperty", "value": {"type": "LineString", "coordinates": [[24.9, 60.2], [25.4, 60.2]]}}});
        assert_eq!(
            broken(&on_write(&line, &rules, now())),
            [("inside-hsl-area", QualityOutcome::Flag)]
        );
        let mut multi = json!({"type": "Vehicle", "speed": [
            {"type": "Property", "value": 3, "datasetId": "urn:a"},
            {"type": "Property", "value": 99, "datasetId": "urn:b"}
        ]});
        let verdicts = on_write(&multi, &rules, now());
        mark(&mut multi, &verdicts, now());
        assert_eq!(
            multi["speed"][1][QUALITY_FLAGS]["value"],
            json!(["speed-plausible"])
        );
        let mut concise = json!({"type": "Vehicle", "speed": 99});
        let verdicts = on_write(&concise, &rules, now());
        mark(&mut concise, &verdicts, now());
        assert_eq!(
            concise["speed"]["value"],
            json!(99),
            "a plain value keeps its value"
        );
        assert_eq!(
            concise["speed"][QUALITY_FLAGS]["value"],
            json!(["speed-plausible"])
        );
    }

    #[test]
    fn the_sweep_quarantines_what_is_older_than_max_age_and_a_write_may_quarantine() {
        let rules = rules();
        let stale = vehicle(json!(1), 24.9, "2026-10-12T05:54:59Z");
        assert_eq!(
            broken(&on_sweep(&stale, &rules, now())),
            [("position-current", QualityOutcome::Quarantine)]
        );
        let fresh = vehicle(json!(1), 24.9, "2026-10-12T05:55:01Z");
        assert!(broken(&on_sweep(&fresh, &rules, now())).is_empty());
        let unobserved = json!({"type": "Vehicle", "location": {"type": "GeoProperty", "value": {"type": "Point", "coordinates": [24.9, 60.2]}}});
        assert!(broken(&on_sweep(&unobserved, &rules, now())).is_empty());

        let strict: Vec<QualityRule> = serde_json::from_value(json!([
            {"name": "speed-hide", "entityType": "Vehicle", "kind": "range", "attribute": "speed", "max": 1, "outcome": "quarantine"}
        ]))
        .expect("rule");
        let mut entity = vehicle(json!(5), 24.9, "2026-10-12T05:59:00Z");
        let verdicts = on_write(&entity, &strict, now());
        mark(&mut entity, &verdicts, now());
        assert_eq!(entity[QUARANTINE_RULES]["value"], json!(["speed-hide"]));
        assert_eq!(
            entity[QUARANTINED_AT]["value"]["@value"],
            "2026-10-12T06:00:00Z"
        );
    }
}
