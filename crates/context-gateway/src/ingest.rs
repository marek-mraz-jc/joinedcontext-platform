//! Minimisation at ingest (ADR-N-054 2.6, DM-121): a personal slot the model marks `drop` never
//! reaches the broker, and one it marks `keyedHash` reaches it as the HMAC-SHA-256 of its value
//! under the space's key, in lowercase hex, so equal values stay linkable and the value is gone.
//!
//! The value hashed is a Property's `value`: a string's own UTF-8 bytes, anything else its
//! compact JSON. The Portal finds a data subject by hashing the string it is given the same way
//! (API/01, the data subject's rights). The key is the space's `spec.personalData.keyRef`, which
//! the reconciler mounts as the file `{JC_GATEWAY_PERSONAL_KEYS}/{space}`; a write that needs a
//! key the gateway does not hold is refused, never stored as sent.

use crate::units::{types_of, Shape};
use jc_manifest::model_schema::ModelSchema;
use ring::hmac;
use serde_json::Value;
use std::path::Path;

const NGSI_LD_NULL: &str = "urn:ngsi-ld:null";
/// The shortest key accepted: HMAC-SHA-256's own block of security.
pub const MIN_KEY_BYTES: usize = 32;

/// Why a write cannot reach the broker minimised. None of them names a value.
#[derive(Debug, PartialEq, Eq)]
pub enum Refusal {
    /// A `keyedHash` attribute is written and the space has no usable key.
    NoKey {
        /// The attribute that wanted it.
        attribute: String,
    },
    /// The model names an `ingest` the gateway does not know.
    Unknown {
        /// The attribute it is named on.
        attribute: String,
        /// The mode as the schema writes it.
        mode: String,
    },
    /// A `keyedHash` attribute written as something with no `value` to hash (a Relationship, a
    /// LanguageProperty), or a `drop` attribute written as the whole body.
    Unfit {
        /// The attribute written so.
        attribute: String,
    },
}

/// Whether any class of `schema` hashes an attribute, so a write over it needs the key.
pub fn wants_key(schema: &ModelSchema) -> bool {
    schema.classes.keys().any(|class| {
        schema
            .ingest(class)
            .iter()
            .any(|(_, mode)| mode == "keyedHash")
    })
}

/// The space's key, or `None` when the gateway has no directory, the space's name is no plain
/// file name, or the file is missing or shorter than [`MIN_KEY_BYTES`]. Read at each write
/// that needs it, so a rotated Secret takes effect without a restart.
// ponytail: a blocking read of one small file per hashed write; cache it if a profile shows it.
pub fn key_of(dir: Option<&Path>, space: &str) -> Option<jc_core::Secret> {
    let plain = !space.is_empty()
        && space
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
    if !plain {
        return None;
    }
    let raw = std::fs::read_to_string(dir?.join(space)).ok()?;
    let key = raw.trim_end_matches(['\n', '\r']);
    (key.len() >= MIN_KEY_BYTES).then(|| jc_core::Secret::new(key))
}

/// Applies every `ingest` of the payload's classes in place. `Ok(true)` when the body changed.
pub fn apply(
    payload: &mut Value,
    shape: Shape<'_>,
    schema: &ModelSchema,
    key: Option<&jc_core::Secret>,
) -> Result<bool, Refusal> {
    if let Some(attribute) = shape.targeted {
        let mut wrapped = serde_json::json!({ attribute: std::mem::take(payload) });
        let outcome = entity(&mut wrapped, shape, schema, key);
        let kept = wrapped
            .as_object_mut()
            .and_then(|object| object.remove(attribute));
        let changed = outcome?;
        *payload = kept.ok_or_else(|| Refusal::Unfit {
            attribute: attribute.to_owned(),
        })?;
        return Ok(changed);
    }
    match payload {
        Value::Array(entities) => {
            let mut changed = false;
            for one in entities {
                changed |= entity(one, shape, schema, key)?;
            }
            Ok(changed)
        }
        one => entity(one, shape, schema, key),
    }
}

fn entity(
    entity: &mut Value,
    shape: Shape<'_>,
    schema: &ModelSchema,
    key: Option<&jc_core::Secret>,
) -> Result<bool, Refusal> {
    let Some(object) = entity.as_object_mut() else {
        return Ok(false);
    };
    let classes: Vec<String> = types_of(object, shape.addressed)
        .into_iter()
        .map(|t| t.rsplit(['/', '#', ':']).next().unwrap_or(t).to_owned())
        .collect();
    let mut changed = false;
    for class in classes {
        for (attribute, mode) in schema.ingest(&class) {
            let Some(value) = object.get_mut(&attribute) else {
                continue;
            };
            match mode.as_str() {
                "drop" => {
                    object.remove(&attribute);
                    changed = true;
                }
                "keyedHash" => {
                    let key = key.ok_or_else(|| Refusal::NoKey {
                        attribute: attribute.clone(),
                    })?;
                    changed |= hashed(
                        value,
                        &attribute,
                        &hmac::Key::new(hmac::HMAC_SHA256, key.expose().as_bytes()),
                    )?;
                }
                _ => return Err(Refusal::Unknown { attribute, mode }),
            }
        }
    }
    Ok(changed)
}

fn hashed(value: &mut Value, attribute: &str, key: &hmac::Key) -> Result<bool, Refusal> {
    let unfit = || Refusal::Unfit {
        attribute: attribute.to_owned(),
    };
    match value {
        // A multi-attribute (several `datasetId`s) or a temporal write's instances.
        Value::Array(instances) => {
            let mut changed = false;
            for instance in instances {
                changed |= hashed(instance, attribute, key)?;
            }
            Ok(changed)
        }
        Value::Object(member) => {
            let kind = member.get("type").and_then(Value::as_str);
            if kind.is_some_and(|kind| kind != "Property") {
                return Err(unfit());
            }
            match member.get_mut("value") {
                Some(inner) if !is_null(inner) => {
                    *inner = digest(key, inner);
                    Ok(true)
                }
                Some(_) => Ok(false),
                None => Err(unfit()),
            }
        }
        null if is_null(null) => Ok(false),
        // Concise form: the bare value of a Property.
        bare => {
            *bare = digest(key, bare);
            Ok(true)
        }
    }
}

fn is_null(value: &Value) -> bool {
    value.is_null() || value.as_str() == Some(NGSI_LD_NULL)
}

fn digest(key: &hmac::Key, value: &Value) -> Value {
    let text = match value {
        Value::String(text) => text.clone(),
        other => other.to_string(),
    };
    let tag = hmac::sign(key, text.as_bytes());
    Value::String(tag.as_ref().iter().map(|b| format!("{b:02x}")).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn schema() -> ModelSchema {
        let model = json!({ "definitions": { "BikeTrip": { "type": "object", "properties": {
            "rider": { "type": "string", "x-personal-data": true, "x-ingest": "keyedHash" },
            "location": { "type": "object", "x-personal-data": true, "x-ingest": "drop" },
            "startStation": { "type": "string", "x-ingest": "keep" }
        } } } });
        ModelSchema::compile(
            "bike-trips",
            "1.0.0",
            &model,
            &["BikeTrip".to_owned()],
            true,
        )
    }

    const KEY: &str = "Lq2-a-space-personal-data-key-of-32-bytes";

    fn whole() -> Shape<'static> {
        Shape {
            addressed: None,
            targeted: None,
        }
    }

    fn run(payload: &mut Value, shape: Shape<'_>) -> Result<bool, Refusal> {
        apply(payload, shape, &schema(), Some(&jc_core::Secret::new(KEY)))
    }

    fn anna() -> String {
        let key = hmac::Key::new(hmac::HMAC_SHA256, KEY.as_bytes());
        digest(&key, &json!("anna"))
            .as_str()
            .unwrap_or_default()
            .to_owned()
    }

    #[test]
    fn a_hashed_slot_keeps_no_value_and_a_dropped_one_is_gone() {
        let mut trip = json!({ "id": "urn:ngsi-ld:BikeTrip:1", "type": "BikeTrip",
            "rider": { "type": "Property", "value": "anna" },
            "location": { "type": "GeoProperty", "value": { "type": "Point", "coordinates": [19.1, 48.7] } },
            "startStation": { "type": "Property", "value": "Námestie SNP" } });
        assert_eq!(run(&mut trip, whole()), Ok(true));
        let rider = trip["rider"]["value"]
            .as_str()
            .unwrap_or_default()
            .to_owned();
        assert_eq!(rider.len(), 64);
        assert!(rider
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()));
        assert_eq!(rider, anna(), "equal values stay linkable");
        assert!(trip.get("location").is_none());
        assert_eq!(trip["startStation"]["value"], "Námestie SNP");
        assert!(!trip.to_string().contains("anna"));
    }

    #[test]
    fn the_digest_is_the_standard_hmac_sha256() {
        // RFC 4231 test case 2: key "Jefe", data "what do ya want for nothing?".
        let key = hmac::Key::new(hmac::HMAC_SHA256, b"Jefe");
        assert_eq!(
            digest(&key, &json!("what do ya want for nothing?")),
            json!("5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843")
        );
    }

    #[test]
    fn every_shape_of_a_write_is_minimised() {
        // concise, a batch, instances, a fragment of the path's entity, the attribute alone
        let mut concise =
            json!({ "id": "urn:ngsi-ld:BikeTrip:1", "type": "BikeTrip", "rider": "anna" });
        run(&mut concise, whole()).expect("concise");
        assert_eq!(concise["rider"], json!(anna()));

        let mut batch = json!([
            { "id": "urn:ngsi-ld:BikeTrip:1", "type": "BikeTrip", "rider": "anna" },
            { "id": "urn:ngsi-ld:BikeTrip:2", "type": "https://smartdatamodels.org/BikeTrip",
              "location": { "type": "GeoProperty", "value": { "type": "Point", "coordinates": [0, 0] } } }
        ]);
        run(&mut batch, whole()).expect("batch");
        assert_eq!(batch[0]["rider"], json!(anna()));
        assert!(
            batch[1].get("location").is_none(),
            "an expanded type is the same class"
        );

        let mut instances = json!({ "id": "urn:ngsi-ld:BikeTrip:1", "type": "BikeTrip",
            "rider": [{ "type": "Property", "value": "anna", "observedAt": "2026-10-19T00:00:00Z" },
                      { "type": "Property", "value": "anna", "datasetId": "urn:ngsi-ld:Dataset:b" }] });
        run(&mut instances, whole()).expect("instances");
        assert_eq!(instances["rider"][1]["value"], json!(anna()));

        let fragment_shape = Shape {
            addressed: Some("urn:ngsi-ld:BikeTrip:1"),
            targeted: None,
        };
        let mut fragment = json!({ "rider": { "value": "anna" } });
        run(&mut fragment, fragment_shape).expect("fragment");
        assert_eq!(fragment["rider"]["value"], json!(anna()));

        let targeted = Shape {
            addressed: Some("urn:ngsi-ld:BikeTrip:1"),
            targeted: Some("rider"),
        };
        let mut alone = json!({ "type": "Property", "value": "anna" });
        run(&mut alone, targeted).expect("the attribute alone");
        assert_eq!(alone["value"], json!(anna()));
    }

    #[test]
    fn a_deletion_and_another_class_pass_untouched() {
        let mut deleted = json!({ "id": "urn:ngsi-ld:BikeTrip:1", "type": "BikeTrip",
            "rider": { "type": "Property", "value": "urn:ngsi-ld:null" } });
        assert_eq!(run(&mut deleted, whole()), Ok(false));
        assert_eq!(deleted["rider"]["value"], "urn:ngsi-ld:null");
        let mut station =
            json!({ "id": "urn:ngsi-ld:Station:1", "type": "Station", "rider": "anna" });
        assert_eq!(run(&mut station, whole()), Ok(false));
        assert_eq!(station["rider"], "anna");
    }

    #[test]
    fn what_cannot_be_minimised_is_refused_naming_only_the_attribute() {
        let rider = |value: Value| json!({ "id": "urn:ngsi-ld:BikeTrip:1", "type": "BikeTrip", "rider": value });
        let mut trip = rider(json!("anna"));
        assert_eq!(
            apply(&mut trip, whole(), &schema(), None),
            Err(Refusal::NoKey {
                attribute: "rider".into()
            })
        );
        for unfit in [
            json!({ "type": "Relationship", "object": "urn:ngsi-ld:Person:anna" }),
            json!({ "type": "Property" }),
        ] {
            assert_eq!(
                run(&mut rider(unfit), whole()),
                Err(Refusal::Unfit {
                    attribute: "rider".into()
                })
            );
        }
        let dropped = Shape {
            addressed: Some("urn:ngsi-ld:BikeTrip:1"),
            targeted: Some("location"),
        };
        let mut alone =
            json!({ "type": "GeoProperty", "value": { "type": "Point", "coordinates": [0, 0] } });
        assert_eq!(
            run(&mut alone, dropped),
            Err(Refusal::Unfit {
                attribute: "location".into()
            })
        );
        let odd = ModelSchema::compile(
            "m",
            "1.0.0",
            &json!({ "definitions": { "BikeTrip": { "properties": { "rider": { "x-ingest": "hash" } } } } }),
            &["BikeTrip".to_owned()],
            true,
        );
        assert_eq!(
            apply(&mut rider(json!("anna")), whole(), &odd, None),
            Err(Refusal::Unknown {
                attribute: "rider".into(),
                mode: "hash".into()
            })
        );
    }

    #[test]
    fn the_key_needs_a_plain_space_name_and_32_bytes() {
        assert!(wants_key(&schema()));
        let dir = std::env::temp_dir().join(format!("jc-ingest-keys-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("a directory");
        std::fs::write(dir.join("trips"), format!("{KEY}\n")).expect("a key");
        std::fs::write(dir.join("short"), "too-short").expect("a short key");
        let key = key_of(Some(&dir), "trips").expect("the key");
        assert_eq!(key.expose(), KEY, "the file's newline is not the key's");
        assert!(key_of(Some(&dir), "short").is_none());
        assert!(key_of(Some(&dir), "missing").is_none());
        assert!(key_of(Some(&dir), "../trips").is_none());
        assert!(key_of(None, "trips").is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
