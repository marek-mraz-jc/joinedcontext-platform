use context_gateway::translators::geojson::{feature, feature_collection, Untranslatable};
use serde_json::{json, Value};

fn station(location: Value) -> Value {
    json!({
        "id": "urn:ngsi-ld:AirQualityObserved:banskabystrica.sk:ovzdusie:station-01",
        "type": "AirQualityObserved",
        "@context": "https://uri.etsi.org/ngsi-ld/v1/ngsi-ld-core-context-v1.8.jsonld",
        "pm10": { "type": "Property", "value": 34.2 },
        "refDistrict": { "type": "Relationship", "object": "urn:ngsi-ld:District:bb:sasova" },
        "location": { "type": "GeoProperty", "value": location }
    })
}

/// EP-09: every geometry NGSI-LD can carry survives the translation unchanged.
#[test]
fn point_polygon_and_multipolygon_geometries_map_verbatim() {
    for geometry in [
        json!({ "type": "Point", "coordinates": [19.146, 48.736] }),
        json!({
            "type": "Polygon",
            "coordinates": [[[19.10, 48.70], [19.20, 48.70], [19.20, 48.76], [19.10, 48.70]]]
        }),
        json!({
            "type": "MultiPolygon",
            "coordinates": [
                [[[19.10, 48.70], [19.20, 48.70], [19.20, 48.76], [19.10, 48.70]]],
                [[[19.30, 48.80], [19.40, 48.80], [19.40, 48.86], [19.30, 48.80]]]
            ]
        }),
        json!({ "type": "LineString", "coordinates": [[19.1, 48.7], [19.2, 48.8]] }),
    ] {
        let feature = feature(&station(geometry.clone())).expect("a feature");
        assert_eq!(feature["type"], json!("Feature"));
        assert_eq!(feature["geometry"], geometry);
        assert_eq!(
            feature["id"],
            json!("urn:ngsi-ld:AirQualityObserved:banskabystrica.sk:ovzdusie:station-01")
        );
    }
}

/// The properties a GeoJSON client reads are flat: a Property's value, a Relationship's
/// object, and the entity type. The geometry is not repeated in them.
#[test]
fn attributes_are_flattened_and_the_geometry_is_not_a_property() {
    let feature = feature(&station(
        json!({ "type": "Point", "coordinates": [19.146, 48.736] }),
    ))
    .expect("a feature");
    let properties = &feature["properties"];

    assert_eq!(properties["type"], json!("AirQualityObserved"));
    assert_eq!(properties["pm10"], json!(34.2));
    assert_eq!(
        properties["refDistrict"],
        json!("urn:ngsi-ld:District:bb:sasova")
    );
    assert!(
        properties.get("location").is_none(),
        "the geometry is not a property"
    );
    assert!(properties.get("id").is_none(), "the id is the Feature's id");
    assert!(
        properties.get("@context").is_none(),
        "JSON-LD keywords are not properties"
    );
}

/// EP-10: a caller that asks a non-spatial type for GeoJSON gets a 400, not an empty
/// collection that looks like an empty answer.
#[test]
fn entities_without_a_geometry_are_a_bad_request() {
    let flat = json!([{
        "id": "urn:ngsi-ld:Invoice:banskabystrica.sk:uctovnictvo:2026-01",
        "type": "Invoice",
        "amount": { "type": "Property", "value": 120.0 }
    }]);
    assert_eq!(feature_collection(&flat), Err(Untranslatable));

    // A `location` that is not a geometry is not a geometry.
    for impostor in [
        json!({ "type": "Property", "value": "behind the town hall" }),
        json!({ "type": "GeoProperty", "value": { "type": "Point" } }),
        json!({ "type": "GeoProperty", "value": { "type": "Teleport", "coordinates": [0, 0] } }),
    ] {
        let mut entity = station(json!(null));
        entity["location"] = impostor.clone();
        assert_eq!(
            feature_collection(&json!([entity])),
            Err(Untranslatable),
            "{impostor} was read as a geometry"
        );
    }
}

/// Nothing to show is not a type error: an empty answer is an empty collection.
#[test]
fn an_empty_answer_is_an_empty_collection() {
    let empty = feature_collection(&json!([])).expect("an empty collection");
    assert_eq!(empty["type"], json!("FeatureCollection"));
    assert_eq!(empty["features"], json!([]));
}

/// A mixed answer keeps what it can: the spatial entities become features, the rest are
/// simply not on a map.
#[test]
fn a_mixed_answer_keeps_the_entities_that_have_a_geometry() {
    let mixed = json!([
        station(json!({ "type": "Point", "coordinates": [19.146, 48.736] })),
        json!({ "id": "urn:ngsi-ld:Invoice:a:b:c", "type": "Invoice" }),
    ]);
    let collection = feature_collection(&mixed).expect("a collection");
    let features = collection["features"].as_array().expect("features");

    assert_eq!(features.len(), 1);
    assert_eq!(
        features[0]["properties"]["type"],
        json!("AirQualityObserved")
    );
}

/// A single entity translates too: `retrieveEntity` answers one object, not a list.
#[test]
fn one_entity_translates_to_a_collection_of_one() {
    let collection = feature_collection(&station(
        json!({ "type": "Point", "coordinates": [19.1, 48.7] }),
    ))
    .expect("a collection");
    assert_eq!(collection["features"].as_array().map(Vec::len), Some(1));
}
