//! The unit of every quantity a write carries (DM-06, T-2810, Architecture/11 §1.0.2).
//!
//! NGSI-LD encodes a Property's `unitCode` with the UN/CEFACT Recommendation 20 codes, and a
//! space's model says which code each quantity is measured in: the generated JSON Schema carries
//! it as `x-unit.exactMappings` (`ucefact:GQ`). A write through an Endpoint that states another
//! code is refused, naming the attribute and both codes; one that states none is filled with the
//! model's code, or refused in a space whose `spec.missingUnitCode` is `refuse`. Reads are not
//! touched: `unitCode` comes back as stored (CIM 009).

use std::collections::BTreeMap;

use jc_core::kinds::MissingUnitCode;
use jc_core::ProblemDetails;
use serde_json::Value;

/// The header that names the quantities the gateway gave the model's code, `pm10=GQ, no2=GQ`.
pub const FILLED_HEADER: &str = "jc-unit-code-filled";

/// NGSI-LD's null, which deletes an attribute in a merge and carries no unit.
const NGSI_LD_NULL: &str = "urn:ngsi-ld:null";

/// The code each quantity Property of a space's model is measured in, by class and attribute.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UnitRules {
    /// Class, then attribute, then its Rec 20 code.
    pub by_class: BTreeMap<String, BTreeMap<String, String>>,
    /// What a quantity written without a code gets.
    pub missing: MissingUnitCode,
}

/// The CEFACT code among a unit's `exactMappings`, and the `unece:` spelling older models used.
fn cefact_code(unit: &Value) -> Option<&str> {
    unit.get("exactMappings")?
        .as_array()?
        .iter()
        .filter_map(Value::as_str)
        .find_map(|mapping| {
            mapping
                .strip_prefix("ucefact:")
                .or_else(|| mapping.strip_prefix("unece:"))
        })
        .filter(|code| !code.is_empty())
}

impl UnitRules {
    /// The rules a model's generated JSON Schema states: every Property of every class whose
    /// `x-unit` names a CEFACT code. A Relationship or a GeoProperty has no unit to check.
    pub fn from_schema(schema: &Value, missing: MissingUnitCode) -> Self {
        let mut by_class = BTreeMap::new();
        let classes = schema
            .get("definitions")
            .or_else(|| schema.get("$defs"))
            .and_then(Value::as_object);
        for (class, definition) in classes.into_iter().flatten() {
            let Some(properties) = definition.get("properties").and_then(Value::as_object) else {
                continue;
            };
            let units: BTreeMap<String, String> = properties
                .iter()
                .filter(|(_, property)| {
                    property
                        .get("x-ngsi-ld-kind")
                        .and_then(Value::as_str)
                        .is_none_or(|kind| kind == "Property")
                })
                .filter_map(|(name, property)| {
                    let code = cefact_code(property.get("x-unit")?)?;
                    Some((name.clone(), code.to_owned()))
                })
                .collect();
            if !units.is_empty() {
                by_class.insert(class.clone(), units);
            }
        }
        Self { by_class, missing }
    }

    /// Whether the model measures nothing in a unit, so a write needs no look.
    pub fn is_empty(&self) -> bool {
        self.by_class.is_empty()
    }

    fn of_class(&self, entity_type: &str) -> Option<&BTreeMap<String, String>> {
        let local = entity_type
            .rsplit(['/', '#', ':'])
            .next()
            .unwrap_or(entity_type);
        self.by_class
            .get(entity_type)
            .or_else(|| self.by_class.get(local))
    }
}

/// A quantity the gateway refuses for its unit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UnitRefusal {
    /// The write states a code other than the model's.
    Wrong {
        /// The entity type.
        class: String,
        /// The attribute.
        attribute: String,
        /// The model's code.
        expected: String,
        /// What the write said, as written.
        found: String,
    },
    /// The write states no code, in a space that refuses that.
    Missing {
        /// The entity type.
        class: String,
        /// The attribute.
        attribute: String,
        /// The model's code.
        expected: String,
    },
}

fn named(code: &str) -> String {
    match jc_core::units::lookup(code) {
        Some(unit) if !unit.symbol.is_empty() => {
            format!("`{code}` ({}, {})", unit.symbol, unit.name)
        }
        Some(unit) => format!("`{code}` ({})", unit.name),
        None => format!("`{code}`"),
    }
}

impl From<UnitRefusal> for ProblemDetails {
    fn from(refusal: UnitRefusal) -> Self {
        let detail = match refusal {
            UnitRefusal::Wrong { class, attribute, expected, found } => format!(
                "attribute `{attribute}` of `{class}` is measured in {} by the space's data model, \
                 and the write says `{found}`; convert the value and send unitCode `{expected}` (DM-06)",
                named(&expected)
            ),
            UnitRefusal::Missing { class, attribute, expected } => format!(
                "attribute `{attribute}` of `{class}` needs unitCode `{expected}`: this space refuses \
                 a quantity without its unit ({}) (DM-06)",
                named(&expected)
            ),
        };
        ProblemDetails::bad_request().with_detail(detail)
    }
}

/// Where in a write payload the entities are.
#[derive(Debug, Clone, Copy)]
pub struct Shape<'a> {
    /// The entity id of the path, for a fragment that names no type (`/entities/{id}/attrs`).
    pub addressed: Option<&'a str>,
    /// The attribute of the path, when the body is that attribute's bare value.
    pub targeted: Option<&'a str>,
}

/// The type an id names: `urn:ngsi-ld:{Type}:…` (PF-10).
fn type_of_id(id: &str) -> Option<&str> {
    let mut parts = id.splitn(4, ':');
    match (parts.next(), parts.next(), parts.next()) {
        (Some(urn), Some(ngsi), Some(kind))
            if urn.eq_ignore_ascii_case("urn") && ngsi.eq_ignore_ascii_case("ngsi-ld") =>
        {
            Some(kind)
        }
        _ => None,
    }
}

fn types_of<'a>(
    entity: &'a serde_json::Map<String, Value>,
    addressed: Option<&'a str>,
) -> Vec<&'a str> {
    match entity.get("type").or_else(|| entity.get("@type")) {
        Some(Value::String(one)) => vec![one.as_str()],
        Some(Value::Array(many)) => many.iter().filter_map(Value::as_str).collect(),
        _ => entity
            .get("id")
            .or_else(|| entity.get("@id"))
            .and_then(Value::as_str)
            .or(addressed)
            .and_then(type_of_id)
            .into_iter()
            .collect(),
    }
}

/// Checks the unit of one attribute value, filling it when the space fills. `Ok(true)` when it
/// was filled.
fn check_value(
    value: &mut Value,
    class: &str,
    attribute: &str,
    expected: &str,
    missing: MissingUnitCode,
) -> Result<bool, UnitRefusal> {
    let absent = || UnitRefusal::Missing {
        class: class.to_owned(),
        attribute: attribute.to_owned(),
        expected: expected.to_owned(),
    };
    match value {
        // A multi-attribute (several `datasetId`s) or a temporal write's instances: each one is a
        // quantity of its own.
        Value::Array(instances) => {
            let mut filled = false;
            for instance in instances {
                filled |= check_value(instance, class, attribute, expected, missing)?;
            }
            Ok(filled)
        }
        Value::Object(member) => {
            let kind = member.get("type").and_then(Value::as_str);
            if kind.is_some_and(|kind| kind != "Property") {
                return Ok(false);
            }
            // An object with neither `type` nor `value` is no Property in either form.
            if kind.is_none() && !member.contains_key("value") {
                return Ok(false);
            }
            if member.get("value").and_then(Value::as_str) == Some(NGSI_LD_NULL) {
                return Ok(false);
            }
            match member.get("unitCode") {
                Some(Value::String(code)) if code == expected => Ok(false),
                Some(other) => Err(UnitRefusal::Wrong {
                    class: class.to_owned(),
                    attribute: attribute.to_owned(),
                    expected: expected.to_owned(),
                    found: other
                        .as_str()
                        .map_or_else(|| other.to_string(), str::to_owned),
                }),
                None if missing == MissingUnitCode::Refuse => Err(absent()),
                None => {
                    member.insert("unitCode".to_owned(), Value::String(expected.to_owned()));
                    Ok(true)
                }
            }
        }
        Value::Null => Ok(false),
        Value::String(text) if text == NGSI_LD_NULL => Ok(false),
        // Concise form: the bare value of a Property, which states no unit.
        bare => {
            if missing == MissingUnitCode::Refuse {
                return Err(absent());
            }
            let taken = std::mem::take(bare);
            *bare = serde_json::json!({ "value": taken, "unitCode": expected });
            Ok(true)
        }
    }
}

fn check_entity(
    entity: &mut Value,
    shape: Shape<'_>,
    rules: &UnitRules,
    filled: &mut Vec<String>,
) -> Result<(), UnitRefusal> {
    let Some(object) = entity.as_object_mut() else {
        return Ok(());
    };
    let types: Vec<String> = types_of(object, shape.addressed)
        .into_iter()
        .map(str::to_owned)
        .collect();
    for class in types {
        let Some(units) = rules.of_class(&class) else {
            continue;
        };
        for (attribute, expected) in units {
            if let Some(value) = object.get_mut(attribute) {
                if check_value(value, &class, attribute, expected, rules.missing)? {
                    let note = format!("{attribute}={expected}");
                    if !filled.contains(&note) {
                        filled.push(note);
                    }
                }
            }
        }
    }
    Ok(())
}

/// Checks every quantity of a write payload against the model and fills the codes the space
/// fills. Answers the quantities it filled, `attribute=code`, each once.
pub fn enforce(
    payload: &mut Value,
    shape: Shape<'_>,
    rules: &UnitRules,
) -> Result<Vec<String>, UnitRefusal> {
    let mut filled = Vec::new();
    if rules.is_empty() {
        return Ok(filled);
    }
    if let Some(attribute) = shape.targeted {
        let mut wrapped = serde_json::json!({ attribute: std::mem::take(payload) });
        let outcome = check_entity(&mut wrapped, shape, rules, &mut filled);
        *payload = wrapped
            .as_object_mut()
            .and_then(|object| object.remove(attribute))
            .unwrap_or(Value::Null);
        outcome?;
        return Ok(filled);
    }
    match payload {
        Value::Array(entities) => {
            for entity in entities {
                check_entity(entity, shape, rules, &mut filled)?;
            }
        }
        entity => check_entity(entity, shape, rules, &mut filled)?,
    }
    Ok(filled)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn rules(missing: MissingUnitCode) -> UnitRules {
        UnitRules::from_schema(
            &json!({ "definitions": { "AirQualityObserved": { "properties": {
                "pm10": { "x-ngsi-ld-kind": "Property", "x-unit": { "exactMappings": ["ucefact:GQ", "qudt-unit:MicroGM-PER-M3"] } },
                "temperature": { "x-unit": { "exactMappings": ["qudt-unit:DEG_C", "unece:CEL"] } },
                "refDevice": { "x-ngsi-ld-kind": "Relationship", "x-unit": { "exactMappings": ["ucefact:C62"] } },
                "name": { "type": "string" }
            } } } }),
            missing,
        )
    }

    const ID: &str = "urn:ngsi-ld:AirQualityObserved:hel.fi:air:1";

    #[test]
    fn the_rules_are_read_from_the_schema_for_properties_only() {
        let read = rules(MissingUnitCode::Fill);
        let units = &read.by_class["AirQualityObserved"];
        assert_eq!(units.get("pm10").map(String::as_str), Some("GQ"));
        assert_eq!(units.get("temperature").map(String::as_str), Some("CEL"));
        assert!(!units.contains_key("refDevice") && !units.contains_key("name"));
        assert!(UnitRules::from_schema(&json!({}), MissingUnitCode::Fill).is_empty());
        assert!(
            UnitRules::from_schema(&json!({ "definitions": 3 }), MissingUnitCode::Fill).is_empty()
        );
    }

    #[test]
    fn a_wrong_code_is_refused_naming_the_attribute_and_both_codes() {
        let mut entity = json!({ "id": ID, "type": "AirQualityObserved",
            "pm10": { "type": "Property", "value": 0.02, "unitCode": "GP" } });
        let refusal = enforce(
            &mut entity,
            Shape {
                addressed: None,
                targeted: None,
            },
            &rules(MissingUnitCode::Fill),
        )
        .expect_err("GP is not GQ");
        assert_eq!(
            refusal,
            UnitRefusal::Wrong {
                class: "AirQualityObserved".into(),
                attribute: "pm10".into(),
                expected: "GQ".into(),
                found: "GP".into()
            }
        );
        let detail = ProblemDetails::from(refusal).detail.unwrap_or_default();
        assert!(
            detail.contains("`pm10`")
                && detail.contains("`GQ` (µg/m³, microgram per cubic metre)")
                && detail.contains("`GP`"),
            "{detail}"
        );

        let mut numeric = json!({ "id": ID, "type": "AirQualityObserved", "pm10": { "value": 3, "unitCode": 7 } });
        assert!(matches!(
            enforce(&mut numeric, Shape { addressed: None, targeted: None }, &rules(MissingUnitCode::Fill)),
            Err(UnitRefusal::Wrong { found, .. }) if found == "7"
        ));
    }

    #[test]
    fn a_missing_code_is_filled_in_both_forms_or_refused_when_the_space_is_strict() {
        let shape = Shape {
            addressed: None,
            targeted: None,
        };
        let mut entity = json!({ "id": ID, "type": "AirQualityObserved",
            "pm10": { "type": "Property", "value": 34 }, "temperature": 21.5,
            "refDevice": { "type": "Relationship", "object": "urn:ngsi-ld:Device:hel.fi:air:d" } });
        let filled = enforce(&mut entity, shape, &rules(MissingUnitCode::Fill)).expect("filled");
        assert_eq!(filled, vec!["pm10=GQ", "temperature=CEL"]);
        assert_eq!(
            entity["pm10"],
            json!({ "type": "Property", "value": 34, "unitCode": "GQ" })
        );
        assert_eq!(
            entity["temperature"],
            json!({ "value": 21.5, "unitCode": "CEL" })
        );
        assert!(entity["refDevice"].get("unitCode").is_none());

        let mut strict = json!({ "id": ID, "type": "AirQualityObserved", "pm10": { "type": "Property", "value": 34 } });
        assert!(matches!(
            enforce(&mut strict, shape, &rules(MissingUnitCode::Refuse)),
            Err(UnitRefusal::Missing { attribute, .. }) if attribute == "pm10"
        ));
        let mut concise = json!({ "id": ID, "type": "AirQualityObserved", "temperature": 3 });
        assert!(enforce(&mut concise, shape, &rules(MissingUnitCode::Refuse)).is_err());
    }

    #[test]
    fn fragments_batches_instances_and_targeted_attributes_are_judged_by_their_type() {
        let fill = rules(MissingUnitCode::Fill);
        let mut fragment = json!({ "pm10": { "type": "Property", "value": 1 } });
        let filled = enforce(
            &mut fragment,
            Shape {
                addressed: Some(ID),
                targeted: None,
            },
            &fill,
        )
        .expect("fragment");
        assert_eq!(filled, vec!["pm10=GQ"]);
        assert_eq!(fragment["pm10"]["unitCode"], "GQ");

        let mut bare = json!(12.5);
        enforce(
            &mut bare,
            Shape {
                addressed: Some(ID),
                targeted: Some("pm10"),
            },
            &fill,
        )
        .expect("targeted");
        assert_eq!(bare, json!({ "value": 12.5, "unitCode": "GQ" }));
        let mut wrong = json!({ "value": 1, "unitCode": "KGM" });
        assert!(enforce(
            &mut wrong,
            Shape {
                addressed: Some(ID),
                targeted: Some("pm10")
            },
            &fill
        )
        .is_err());
        assert_eq!(
            wrong,
            json!({ "value": 1, "unitCode": "KGM" }),
            "a refused body is left as it was"
        );

        let mut batch = json!([
            { "id": ID, "type": "AirQualityObserved", "pm10": [
                { "type": "Property", "value": 1, "datasetId": "urn:a" },
                { "type": "Property", "value": 2, "unitCode": "GQ", "datasetId": "urn:b" } ] },
            ID,
            { "id": "urn:ngsi-ld:Other:hel.fi:air:2", "type": "Other", "pm10": 3 }
        ]);
        let filled = enforce(
            &mut batch,
            Shape {
                addressed: None,
                targeted: None,
            },
            &fill,
        )
        .expect("batch");
        assert_eq!(filled, vec!["pm10=GQ"]);
        assert_eq!(batch[0]["pm10"][0]["unitCode"], "GQ");
        assert_eq!(
            batch[2]["pm10"], 3,
            "a type the model does not measure is not touched"
        );
    }

    #[test]
    fn a_delete_by_null_and_an_expanded_type_are_understood() {
        let strict = rules(MissingUnitCode::Refuse);
        let mut merge =
            json!({ "pm10": "urn:ngsi-ld:null", "temperature": { "value": "urn:ngsi-ld:null" } });
        assert_eq!(
            enforce(
                &mut merge,
                Shape {
                    addressed: Some(ID),
                    targeted: None
                },
                &strict
            ),
            Ok(vec![])
        );
        let mut expanded = json!({ "id": ID, "type": "https://hel.fi/terms/AirQualityObserved", "pm10": { "value": 1, "unitCode": "GQ" } });
        assert_eq!(
            enforce(
                &mut expanded,
                Shape {
                    addressed: None,
                    targeted: None
                },
                &strict
            ),
            Ok(vec![])
        );
        assert_eq!(type_of_id("not-a-urn"), None);
    }
}
