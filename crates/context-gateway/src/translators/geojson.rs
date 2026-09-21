//! NGSI-LD entities as an RFC 7946 `FeatureCollection` (T-0158, EP-09, EP-10).
//!
//! The translation runs after the policy projection, never before, so a GeoJSON property
//! can only ever be an attribute the grant already allowed through: a second format is a
//! second way to read the same data, not a second set of rules (EP-07).
//!
//! An entity's geometry is its `location`, which is what NGSI-LD names the primary
//! `GeoProperty`; any other `GeoProperty` stays an ordinary property, because a Feature
//! has exactly one geometry.

use serde_json::{json, Map, Value};
use std::collections::BTreeMap;

/// The media type of the answer.
pub const MEDIA_TYPE: &str = "application/geo+json";

/// The name NGSI-LD gives the primary `GeoProperty`.
const PRIMARY: &str = "location";

/// Entities were asked for as GeoJSON and not one of them has a geometry (EP-10).
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("no entity in the answer carries a geometry")]
pub struct Untranslatable;

/// A `FeatureCollection` of every entity that has a geometry.
///
/// An empty answer translates to an empty collection: nothing to show is not a type
/// error. An answer that has entities and no geometry at all is one, because the caller
/// asked a non-spatial type for a spatial representation (EP-10).
pub fn feature_collection(entities: &Value, lang: Option<&str>) -> Result<Value, Untranslatable> {
    let entities: Vec<&Value> = match entities {
        Value::Array(entities) => entities.iter().collect(),
        entity if entity.is_object() => vec![entity],
        _ => Vec::new(),
    };
    if entities.is_empty() {
        return Ok(json!({ "type": "FeatureCollection", "features": [] }));
    }

    let features: Vec<Value> = entities
        .iter()
        .filter_map(|entity| feature(entity, lang))
        .collect();
    if features.is_empty() {
        return Err(Untranslatable);
    }
    Ok(json!({ "type": "FeatureCollection", "features": features }))
}

/// One entity as a `Feature`, or nothing when it has no geometry.
///
/// `lang` is the caller's `Accept-Language`, which decides the text of a `LanguageProperty`
/// (EP-37). An answer that depends on it says so in `Vary`.
pub fn feature(entity: &Value, lang: Option<&str>) -> Option<Value> {
    let geometry = geometry_of(entity)?;
    let mut feature = Map::new();
    feature.insert("type".to_owned(), json!("Feature"));
    if let Some(id) = entity.get("id").or_else(|| entity.get("@id")) {
        feature.insert("id".to_owned(), id.clone());
    }
    feature.insert("geometry".to_owned(), geometry);
    feature.insert(
        "properties".to_owned(),
        Value::Object(properties(entity, lang)),
    );
    Some(Value::Object(feature))
}

/// One entity as CIM 009 6.3.15 renders it for `Accept: application/geo+json` on the NGSI-LD
/// surface: a `Feature` always, with a `null` geometry when the entity has none, because there
/// the caller asked for this entity and not for a map layer (T-2583).
// ponytail: the default `location` only; `geometryProperty` when a caller asks for another.
pub fn ngsi_feature(entity: &Value, lang: Option<&str>) -> Value {
    feature(entity, lang).unwrap_or_else(|| {
        let mut feature = Map::new();
        feature.insert("type".to_owned(), json!("Feature"));
        if let Some(id) = entity.get("id").or_else(|| entity.get("@id")) {
            feature.insert("id".to_owned(), id.clone());
        }
        feature.insert("geometry".to_owned(), Value::Null);
        feature.insert(
            "properties".to_owned(),
            Value::Object(properties(entity, lang)),
        );
        Value::Object(feature)
    })
}

/// The entity's own geometry: `location`, in either the normalized or the concise form.
fn geometry_of(entity: &Value) -> Option<Value> {
    let location = entity.get(PRIMARY)?;
    let geometry = location.get("value").unwrap_or(location);
    // A geometry is a GeoJSON object with a type and coordinates; anything else is a
    // property that happens to be called `location`.
    geometry
        .get("type")
        .and_then(Value::as_str)
        .filter(|kind| {
            matches!(
                *kind,
                "Point"
                    | "MultiPoint"
                    | "LineString"
                    | "MultiLineString"
                    | "Polygon"
                    | "MultiPolygon"
                    | "GeometryCollection"
            )
        })
        .and(
            geometry
                .get("coordinates")
                .or_else(|| geometry.get("geometries")),
        )
        .map(|_| geometry.clone())
}

/// The entity's attributes, flattened to the plain key-value pairs a GeoJSON client reads
/// (EP-37, the flattening table of API/02 section 6).
fn properties(entity: &Value, lang: Option<&str>) -> Map<String, Value> {
    let mut properties = Map::new();
    let Some(members) = entity.as_object() else {
        return properties;
    };
    for (name, value) in members {
        match name.as_str() {
            // `id` is the Feature's id and `location` is its geometry; the JSON-LD
            // keywords describe the NGSI-LD document, not the feature.
            "id" | "@id" | PRIMARY | "@context" => continue,
            "type" | "@type" => {
                properties.insert("type".to_owned(), value.clone());
            }
            _ => {
                properties.insert(name.clone(), flatten(value, lang));
                // What the value alone does not say: what it is measured in, and when it was
                // observed. Both are members of the attribute the projection already allowed
                // through, so nothing new reaches the wire — and without them a client cannot
                // tell micrograms from milligrams, or an hour ago from last year (EP-37).
                for member in [UNIT_CODE, OBSERVED_AT] {
                    if let Some(beside) = value.get(member).filter(|it| !it.is_null()) {
                        properties.insert(format!("{name}_{member}"), beside.clone());
                    }
                }
            }
        }
    }
    properties
}

/// The member naming the unit an attribute's value is measured in (CIM 009 clause 4.5.4).
const UNIT_CODE: &str = "unitCode";
/// The member naming the instant an attribute's value was observed (CIM 009 clause 4.8).
const OBSERVED_AT: &str = "observedAt";

/// One attribute as a plain value: a Property's `value`, a Relationship's `object`, a
/// `LanguageProperty`'s text in the caller's language, a concise attribute as it stands.
fn flatten(attribute: &Value, lang: Option<&str>) -> Value {
    if let Some(texts) = attribute.get("languageMap") {
        // The map itself where nothing in it is text: a client reading one key too many is a
        // better answer than a feature that lost the attribute.
        return texts
            .as_object()
            .and_then(|texts| language_text(texts, lang))
            .unwrap_or_else(|| texts.clone());
    }
    attribute
        .get("value")
        .or_else(|| attribute.get("object"))
        .cloned()
        .unwrap_or_else(|| attribute.clone())
}

/// A `LanguageProperty` as the one text the caller asked for (EP-37, EP-45).
///
/// The same matching rule the landing page's title uses, so one endpoint answers one language
/// to one request. `None` where the map holds no text at all, and then the map travels whole
/// rather than the feature losing the attribute.
fn language_text(texts: &Map<String, Value>, lang: Option<&str>) -> Option<Value> {
    let by_locale: BTreeMap<String, String> = texts
        .iter()
        .filter_map(|(locale, text)| text.as_str().map(|text| (locale.clone(), text.to_owned())))
        .collect();
    if by_locale.is_empty() {
        return None;
    }
    Some(json!(super::ogc::localized(&by_locale, lang, None)))
}
