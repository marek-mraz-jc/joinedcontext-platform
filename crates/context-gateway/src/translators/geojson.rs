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
pub fn feature_collection(entities: &Value) -> Result<Value, Untranslatable> {
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
        .filter_map(|entity| feature(entity))
        .collect();
    if features.is_empty() {
        return Err(Untranslatable);
    }
    Ok(json!({ "type": "FeatureCollection", "features": features }))
}

/// One entity as a `Feature`, or nothing when it has no geometry.
pub fn feature(entity: &Value) -> Option<Value> {
    let geometry = geometry_of(entity)?;
    let mut feature = Map::new();
    feature.insert("type".to_owned(), json!("Feature"));
    if let Some(id) = entity.get("id").or_else(|| entity.get("@id")) {
        feature.insert("id".to_owned(), id.clone());
    }
    feature.insert("geometry".to_owned(), geometry);
    feature.insert("properties".to_owned(), Value::Object(properties(entity)));
    Some(Value::Object(feature))
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

/// The entity's attributes, flattened to the plain key-value pairs a GeoJSON client reads.
fn properties(entity: &Value) -> Map<String, Value> {
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
                properties.insert(name.clone(), flatten(value));
            }
        }
    }
    properties
}

/// One attribute as a plain value: a Property's `value`, a Relationship's `object`, a
/// concise attribute as it stands.
fn flatten(attribute: &Value) -> Value {
    attribute
        .get("value")
        .or_else(|| attribute.get("object"))
        .cloned()
        .unwrap_or_else(|| attribute.clone())
}
