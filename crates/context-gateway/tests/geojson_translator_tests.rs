use context_gateway::translators::geojson::{feature, feature_collection};
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
        let feature = feature(&station(geometry.clone()), None).expect("a feature");
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
    let feature = feature(
        &station(json!({ "type": "Point", "coordinates": [19.146, 48.736] })),
        None,
    )
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

/// EP-09, EP-10 (T-2939): a non-spatial answer is a collection of Features with a `null`
/// geometry (RFC 7946 §3.2), every entity kept with its properties, never a refusal.
#[test]
fn entities_without_a_geometry_are_features_with_a_null_geometry() {
    let flat = json!([{
        "id": "urn:ngsi-ld:Invoice:banskabystrica.sk:uctovnictvo:2026-01",
        "type": "Invoice",
        "amount": { "type": "Property", "value": 120.0 }
    }]);
    let collection = feature_collection(&flat, None);
    assert_eq!(collection["type"], json!("FeatureCollection"));
    assert_eq!(
        collection["features"],
        json!([{
            "type": "Feature",
            "id": "urn:ngsi-ld:Invoice:banskabystrica.sk:uctovnictvo:2026-01",
            "geometry": null,
            "properties": { "type": "Invoice", "amount": 120.0 }
        }])
    );

    // A `location` that is not a geometry is not a geometry.
    for impostor in [
        json!({ "type": "Property", "value": "behind the town hall" }),
        json!({ "type": "GeoProperty", "value": { "type": "Point" } }),
        json!({ "type": "GeoProperty", "value": { "type": "Teleport", "coordinates": [0, 0] } }),
    ] {
        let mut entity = station(json!(null));
        entity["location"] = impostor.clone();
        assert_eq!(
            feature_collection(&json!([entity]), None)["features"][0]["geometry"],
            json!(null),
            "{impostor} was read as a geometry"
        );
    }
}

/// Nothing to show is not a type error: an empty answer is an empty collection.
#[test]
fn an_empty_answer_is_an_empty_collection() {
    let empty = feature_collection(&json!([]), None);
    assert_eq!(empty["type"], json!("FeatureCollection"));
    assert_eq!(empty["features"], json!([]));
}

/// A mixed answer keeps every entity, in order: the spatial one with its geometry, the other
/// with a `null` one a map client skips (EP-09).
#[test]
fn a_mixed_answer_keeps_every_entity() {
    let mixed = json!([
        station(json!({ "type": "Point", "coordinates": [19.146, 48.736] })),
        json!({ "id": "urn:ngsi-ld:Invoice:a:b:c", "type": "Invoice" }),
    ]);
    let collection = feature_collection(&mixed, None);
    let features = collection["features"].as_array().expect("features");

    assert_eq!(features.len(), 2);
    assert_eq!(
        features[0]["properties"]["type"],
        json!("AirQualityObserved")
    );
    assert_eq!(features[0]["geometry"]["type"], json!("Point"));
    assert_eq!(features[1]["id"], json!("urn:ngsi-ld:Invoice:a:b:c"));
    assert_eq!(features[1]["geometry"], json!(null));
}

/// A single entity translates too: `retrieveEntity` answers one object, not a list.
#[test]
fn one_entity_translates_to_a_collection_of_one() {
    let collection = feature_collection(
        &station(json!({ "type": "Point", "coordinates": [19.1, 48.7] })),
        None,
    );
    assert_eq!(collection["features"].as_array().map(Vec::len), Some(1));
}

/// A station as the broker holds it: every shape of the flattening table of API/02 section 6
/// in one entity — a measured Property, a plain one, a Relationship, a LanguageProperty and a
/// second GeoProperty beside the primary one.
fn measured() -> Value {
    json!({
        "id": "urn:ngsi-ld:AirQualityObserved:banskabystrica.sk:ovzdusie:station-01",
        "type": "AirQualityObserved",
        "pm10": {
            "type": "Property",
            "value": 63.2,
            "unitCode": "GQ",
            "observedAt": "2026-09-05T11:50:00Z",
        },
        "status": { "type": "Property", "value": "ok" },
        "refDistrict": { "type": "Relationship", "object": "urn:ngsi-ld:District:bb:sasova" },
        "label": {
            "type": "LanguageProperty",
            "languageMap": { "sk": "Merací bod", "en": "Measuring point" },
        },
        "inletLocation": {
            "type": "GeoProperty",
            "value": { "type": "Point", "coordinates": [19.147, 48.737] },
        },
        "location": {
            "type": "GeoProperty",
            "value": { "type": "Point", "coordinates": [19.146, 48.736] },
        },
    })
}

/// EP-37: the unit and the instant travel with the value they describe. Without them a client
/// cannot tell micrograms from milligrams, nor a reading of an hour ago from one of last year.
#[test]
fn a_feature_carries_the_unit_and_the_instant_of_every_attribute_that_has_them() {
    let feature = feature(&measured(), None).expect("a feature");
    let properties = &feature["properties"];

    assert_eq!(properties["pm10"], json!(63.2));
    assert_eq!(properties["pm10_unitCode"], json!("GQ"));
    assert_eq!(properties["pm10_observedAt"], json!("2026-09-05T11:50:00Z"));
    assert_eq!(
        properties["refDistrict"],
        json!("urn:ngsi-ld:District:bb:sasova"),
        "a Relationship is still its object URN"
    );
    assert_eq!(
        properties["inletLocation"],
        json!({ "type": "Point", "coordinates": [19.147, 48.737] }),
        "a second GeoProperty stays a geometry object in properties"
    );
    assert_eq!(
        feature["geometry"],
        json!({ "type": "Point", "coordinates": [19.146, 48.736] }),
        "and the primary one is the feature's geometry"
    );
}

/// An attribute with neither gains no keys: a property list padded with nulls is harder to read
/// than one that only says what it knows.
#[test]
fn an_attribute_without_a_unit_or_an_instant_gains_no_empty_keys() {
    let feature = feature(&measured(), None).expect("a feature");
    let properties = feature["properties"].as_object().expect("an object");

    assert_eq!(properties["status"], json!("ok"));
    for absent in [
        "status_unitCode",
        "status_observedAt",
        "refDistrict_unitCode",
        "label_unitCode",
    ] {
        assert!(!properties.contains_key(absent), "{absent} was invented");
    }
}

/// EP-37, EP-45: a LanguageProperty is one text, the caller's. A language map in `properties`
/// is a JSON object where a GIS client expects a label.
#[test]
fn a_language_property_is_the_text_of_the_callers_language() {
    for (header, expected) in [
        (Some("sk"), "Merací bod"),
        (Some("en-GB,en;q=0.9"), "Measuring point"),
        // Nobody wrote Finnish: a label in the wrong language beats no label at all.
        (Some("fi"), "Measuring point"),
        (None, "Measuring point"),
    ] {
        let feature = feature(&measured(), header).expect("a feature");
        assert_eq!(
            feature["properties"]["label"],
            json!(expected),
            "for Accept-Language {header:?}"
        );
    }
}

/// A language map that holds no text at all travels as it is: losing the attribute would be a
/// worse answer than an unflattened one.
#[test]
fn a_language_map_with_nothing_to_pick_travels_whole() {
    let mut entity = measured();
    entity["label"]["languageMap"] = json!({ "sk": { "unexpected": true } });

    let feature = feature(&entity, Some("sk")).expect("a feature");
    assert_eq!(
        feature["properties"]["label"],
        json!({ "sk": { "unexpected": true } })
    );
}

/// R9, MP-02: the members come from the entity the projection already narrowed, so an
/// attribute the grant removed contributes no `_unitCode` either.
#[test]
fn an_attribute_the_projection_removed_contributes_nothing() {
    let mut entity = measured();
    entity.as_object_mut().expect("an object").remove("pm10");

    let feature = feature(&entity, None).expect("a feature");
    let properties = feature["properties"].as_object().expect("an object");

    for absent in ["pm10", "pm10_unitCode", "pm10_observedAt"] {
        assert!(
            !properties.contains_key(absent),
            "{absent} survived the projection"
        );
    }
}

/// EP-37, EP-08: two representations of one answer name the same facts. The CSV keeps the unit
/// and the instant in their own columns and the GeoJSON beside the value they describe; what
/// neither may do is lose one of them, because then the same query answers two different
/// things depending on the file extension.
#[test]
fn the_csv_and_the_geojson_of_one_entity_name_the_same_facts() {
    let entity = measured();
    let feature = feature(&entity, Some("en")).expect("a feature");
    let properties = feature["properties"].as_object().expect("an object");
    let columns: std::collections::BTreeMap<String, Value> =
        context_gateway::translators::tabular::flatten(&entity)
            .into_iter()
            .collect();

    assert_eq!(columns["pm10.value"], properties["pm10"]);
    assert_eq!(columns["pm10.unitCode"], properties["pm10_unitCode"]);
    assert_eq!(columns["pm10.observedAt"], properties["pm10_observedAt"]);
    assert_eq!(
        columns["refDistrict.object"], properties["refDistrict"],
        "a Relationship is its object URN in both"
    );
    assert_eq!(
        columns["label.languageMap.en"], properties["label"],
        "the English label of the CSV is the label the English caller reads"
    );
}
