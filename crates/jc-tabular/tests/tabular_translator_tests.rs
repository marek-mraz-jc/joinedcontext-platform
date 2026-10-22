//! The tabular representations of an NGSI-LD answer (T-0157, EP-08, EP-44, EP-45).

use jc_tabular::{csv, flatten, humanize, table, Dialect, Limits, Table, TooLarge};
use serde_json::{json, Value};

fn station() -> Value {
    json!({
        "id": "urn:ngsi-ld:WeatherObserved:banskabystrica.sk:ovzdusie:station-01",
        "type": "WeatherObserved",
        "@context": "https://uri.etsi.org/ngsi-ld/v1/ngsi-ld-core-context-v1.8.jsonld",
        "temperature": { "type": "Property", "value": 22.4, "unitCode": "CEL",
                         "observedAt": "2026-08-15T12:00:00Z" },
        "location": { "type": "GeoProperty",
                      "value": { "type": "Point", "coordinates": [19.146, 48.736] } },
        "refDistrict": { "type": "Relationship",
                         "object": "urn:ngsi-ld:District:banskabystrica.sk:ovzdusie:sasova" }
    })
}

/// EP-08: a nested property becomes a dot-notated column, an array index is bracketed,
/// and the shape word NGSI-LD puts on an attribute is not a column at all.
#[test]
fn nested_properties_flatten_to_dot_notation() {
    let cells = flatten(&station());
    let named = |name: &str| {
        cells
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.clone())
    };

    assert_eq!(
        named("id"),
        Some(json!(
            "urn:ngsi-ld:WeatherObserved:banskabystrica.sk:ovzdusie:station-01"
        ))
    );
    assert_eq!(named("type"), Some(json!("WeatherObserved")));
    assert_eq!(named("temperature.value"), Some(json!(22.4)));
    assert_eq!(
        named("temperature.observedAt"),
        Some(json!("2026-08-15T12:00:00Z"))
    );
    assert_eq!(named("location.value.coordinates[0]"), Some(json!(19.146)));
    assert_eq!(named("location.value.coordinates[1]"), Some(json!(48.736)));
    // The geometry's own type is data: it says which geometry this is.
    assert_eq!(named("location.value.type"), Some(json!("Point")));

    // The attribute discriminators are structure, so they get no column of their own.
    assert_eq!(named("temperature.type"), None);
    assert_eq!(named("location.type"), None);
    assert_eq!(named("refDistrict.type"), None);
    assert_eq!(named("@context"), None);

    // `id` and `type` come first whatever order the broker wrote the members in.
    assert_eq!(cells[0].0, "id");
    assert_eq!(cells[1].0, "type");
}

/// EP-08, EP-65: a polygon's rings are one JSON cell, so a layer of polygons stays a few
/// columns wide; a Point in the same answer keeps its two indexed coordinates.
#[test]
fn an_array_of_arrays_is_one_json_cell() {
    let ring: Vec<Value> = (0..1000)
        .map(|step| json!([24.9 + f64::from(step) * 0.0001, 60.1]))
        .collect();
    let area = json!({
        "id": "urn:a", "type": "Announcement",
        "location": { "type": "GeoProperty",
                      "value": { "type": "Polygon", "coordinates": [ring] } },
        "tags": { "type": "Property", "value": [{ "k": "road" }, { "k": "closure" }] }
    });
    let cells = flatten(&area);
    assert!(
        cells.len() < 10,
        "{:?}",
        cells.iter().map(|(key, _)| key).collect::<Vec<_>>()
    );
    assert_eq!(
        cells
            .iter()
            .find(|(key, _)| key == "location.value.coordinates")
            .map(|(_, value)| value),
        Some(&json!([ring]))
    );
    assert!(cells
        .iter()
        .any(|(key, value)| key == "tags.value" && value.is_array()));

    let answer = json!([area, station()]);
    let table = table(&answer, &Limits::default()).expect("within the limits");
    for column in [
        "location.value.coordinates",
        "location.value.coordinates[0]",
        "location.value.coordinates[1]",
    ] {
        assert!(
            table.columns.iter().any(|name| name == column),
            "{:?}",
            table.columns
        );
    }
    let text = csv(&table, &Limits::default(), Dialect::default()).expect("within the limits");
    assert!(
        text.contains("\"[[[24.9,60.1],"),
        "the rings are JSON in one quoted cell"
    );
}

/// EP-08: a relationship is its target URN, as a string a spreadsheet can read.
#[test]
fn a_relationship_is_the_target_urn() {
    let cells = flatten(&station());
    assert_eq!(
        cells
            .iter()
            .find(|(key, _)| key == "refDistrict.object")
            .map(|(_, value)| value.clone()),
        Some(json!(
            "urn:ngsi-ld:District:banskabystrica.sk:ovzdusie:sasova"
        ))
    );
}

/// Two entities that carry different attributes still make one rectangle: a column one of
/// them lacks is an empty cell, not a shifted row.
#[test]
fn rows_are_aligned_to_the_union_of_their_columns() {
    let answer = json!([
        { "id": "urn:a", "type": "T", "a": { "type": "Property", "value": 1 } },
        { "id": "urn:b", "type": "T", "b": { "type": "Property", "value": 2 } }
    ]);
    let table = table(&answer, &Limits::default()).expect("within the limits");
    assert_eq!(table.columns, vec!["id", "type", "a.value", "b.value"]);
    assert_eq!(
        table.rows[0],
        vec![json!("urn:a"), json!("T"), json!(1), Value::Null]
    );
    assert_eq!(
        table.rows[1],
        vec![json!("urn:b"), json!("T"), Value::Null, json!(2)]
    );

    let text = csv(&table, &Limits::default(), Dialect::default()).expect("within the limits");
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines[0], "id,type,a.value,b.value");
    assert_eq!(lines[1], "urn:a,T,1,");
    assert_eq!(lines[2], "urn:b,T,,2");
}

/// EP-45: a human header drops `.value` and carries the unit the property declares.
#[test]
fn human_headers_carry_the_unit_symbol() {
    let mut table = table(&json!([station()]), &Limits::default()).expect("within the limits");
    humanize(&mut table);

    assert!(
        table.columns.contains(&"temperature [°C]".to_owned()),
        "{:?}",
        table.columns
    );
    assert!(
        !table
            .columns
            .iter()
            .any(|column| column.contains("unitCode")),
        "the unit moved into the header: {:?}",
        table.columns
    );
    assert!(table.columns.contains(&"temperature.observedAt".to_owned()));
    // Dropping a column drops its cell, or every row after it would be shifted.
    assert_eq!(table.columns.len(), table.rows[0].len());

    let text = csv(&table, &Limits::default(), Dialect::default()).expect("within the limits");
    let value_at = |header: &str| {
        let index = table
            .columns
            .iter()
            .position(|c| c == header)
            .expect("column");
        text.lines()
            .nth(1)
            .expect("a row")
            .split(',')
            .nth(index)
            .map(str::to_owned)
    };
    assert_eq!(value_at("temperature [°C]"), Some("22.4".to_owned()));
}

/// RFC 4180: a value carrying the separator, a quote or a newline is quoted, and an inner
/// quote is doubled. Without this the header and the rows stop lining up.
#[test]
fn csv_quotes_the_fields_that_need_it() {
    let answer = json!([{
        "id": "urn:a",
        "type": "T",
        "note": { "type": "Property", "value": "one, two \"three\"\nfour" }
    }]);
    let table = table(&answer, &Limits::default()).expect("within the limits");
    let text = csv(&table, &Limits::default(), Dialect::default()).expect("within the limits");
    assert!(text.contains("\"one, two \"\"three\"\"\nfour\""), "{text}");
    // The record separator is CRLF, so a naive reader does not merge two rows.
    assert!(text.starts_with("id,type,note.value\r\n"), "{text}");
}

/// EP-44: too many rows is a refusal, never a file that is silently short.
#[test]
fn a_row_ceiling_refuses_rather_than_truncates() {
    let answer = Value::Array(
        (0..5)
            .map(|n| json!({ "id": format!("urn:{n}"), "type": "T" }))
            .collect(),
    );
    let limits = Limits {
        max_rows: 3,
        max_bytes: Limits::DEFAULT.max_bytes,
    };
    assert_eq!(table(&answer, &limits), Err(TooLarge));

    // The same answer under a byte ceiling is refused the same way.
    let table = table(&answer, &Limits::default()).expect("within the row limit");
    let tight = Limits {
        max_rows: Limits::DEFAULT.max_rows,
        max_bytes: 8,
    };
    assert_eq!(csv(&table, &tight, Dialect::default()), Err(TooLarge));
}

/// An answer with no entity is a header-only file, not an error: nothing to show is a
/// legitimate result of a narrowed query (R22).
#[test]
fn an_empty_answer_is_an_empty_table() {
    let table = table(&json!([]), &Limits::default()).expect("within the limits");
    assert!(table.rows.is_empty());
    assert_eq!(
        csv(&table, &Limits::default(), Dialect::default()),
        Ok("\r\n".to_owned())
    );
}

/// A table built twice from the same answer is byte-identical, which is what makes an
/// `ETag` on a download mean anything (EP-43).
#[test]
fn the_same_answer_produces_the_same_bytes() {
    let answer = json!([station(), station()]);
    let first: Table = table(&answer, &Limits::default()).expect("within the limits");
    let second: Table = table(&answer, &Limits::default()).expect("within the limits");
    assert_eq!(first, second);
    assert_eq!(
        csv(&first, &Limits::default(), Dialect::default()),
        csv(&second, &Limits::default(), Dialect::default())
    );
}
