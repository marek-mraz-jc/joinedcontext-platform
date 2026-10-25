//! T-5209: formula injection, stable columns, BOM and delimiter, repeated rows, attribute kinds.

mod support;

use jc_tabular::{csv, humanize, Dialect, Limits, Slot, Table};
use serde_json::{json, Value};
use support::table;

fn text_of(table: &Table, dialect: Dialect) -> String {
    csv(table, &Limits::default(), dialect).expect("within the limits")
}

fn event(n: u32, extra: Value) -> Value {
    let mut entity = json!({
        "id": format!("urn:ngsi-ld:Event:e{n}"),
        "type": "Event",
        "name": { "type": "Property", "value": format!("Event {n}") },
    });
    if let (Some(entity), Some(extra)) = (entity.as_object_mut(), extra.as_object()) {
        entity.extend(extra.clone());
    }
    entity
}

fn slots() -> Vec<Slot> {
    let slot = |name: &str, kind: &str, scalar: bool, unit: bool| Slot {
        name: name.to_owned(),
        kind: kind.to_owned(),
        scalar,
        unit,
    };
    vec![
        slot("name", "Property", true, false),
        slot("startDate", "Property", true, false),
        slot("location", "GeoProperty", false, false),
        slot("organizer", "Relationship", true, false),
        slot("no2", "Property", true, true),
    ]
}

fn header(entities: Value) -> String {
    let mut table = table(&entities, &Limits::default()).expect("within the limits");
    table.settle(&slots());
    text_of(&table, Dialect::default())
        .lines()
        .next()
        .unwrap_or_default()
        .to_owned()
}

/// The hole: a text cell starting with =, +, -, @, a tab or a CR opens as text, not a formula.
#[test]
fn a_formula_in_a_text_cell_opens_as_text() {
    for (n, payload) in [
        "=HYPERLINK(\"http://x\",\"click\")",
        "+cmd|' /C calc'!A0",
        "-2+3",
        "@SUM(A1:A2)",
        "\t=1+1",
        "\r=1+1",
    ]
    .into_iter()
    .enumerate()
    {
        let entities = json!([{ "id": "urn:a", "type": "T",
            "title": { "type": "Property", "value": payload } }]);
        let table = table(&entities, &Limits::default()).expect("within the limits");
        let text = text_of(&table, Dialect::default());
        let row = text.split("\r\n").nth(1).unwrap_or_default().to_owned();
        let quoted = format!("\"'{}\"", payload.replace('"', "\"\""));
        assert_eq!(row, format!("urn:a,T,{quoted}"), "payload {n}");
    }
}

#[test]
fn a_number_stays_a_number_and_a_header_is_neutralised_too() {
    let entities = json!([{ "id": "urn:a", "type": "T",
        "=x": { "type": "Property", "value": "-12.5" },
        "t": { "type": "Property", "value": -3 } }]);
    let table = table(&entities, &Limits::default()).expect("within the limits");
    let text = text_of(&table, Dialect::default());
    assert_eq!(
        text,
        "id,type,\"'=x.value\",t.value\r\nurn:a,T,-12.5,-3\r\n"
    );
}

/// Finding 15: the same header for the whole answer, a narrowing filter and an empty answer.
#[test]
fn the_header_does_not_move_with_the_filter() {
    let whole = json!([
        event(
            1,
            json!({ "organizer": { "type": "Relationship", "object": "urn:o" } })
        ),
        event(
            2,
            json!({ "location": { "type": "GeoProperty",
            "value": { "type": "Point", "coordinates": [19.1, 48.7] } },
            "zeta": { "type": "Property", "value": 1 } })
        ),
        event(3, json!({ "alpha": { "type": "Property", "value": 1 } })),
    ]);
    let all = header(whole.clone());
    assert_eq!(
        all,
        "id,type,name.value,startDate.value,location.value.type,location.value.coordinates[0],\
location.value.coordinates[1],organizer.object,no2.value,no2.unitCode,alpha.value,zeta.value"
    );
    let reversed: Vec<Value> = whole
        .as_array()
        .into_iter()
        .flatten()
        .rev()
        .cloned()
        .collect();
    assert_eq!(header(Value::Array(reversed)), all);
    let narrowed = json!([
        event(1, json!({})),
        event(
            3,
            json!({ "alpha":
        { "type": "Property", "value": 1 }, "zeta": { "type": "Property", "value": 1 } })
        )
    ]);
    assert_eq!(header(narrowed), all);
    assert_eq!(
        header(json!([])),
        all.replace(",alpha.value,zeta.value", "")
    );
}

#[test]
fn an_empty_answer_without_a_model_still_has_a_header() {
    let mut table = Table::default();
    table.settle(&[]);
    assert_eq!(text_of(&table, Dialect::default()), "id,type\r\n");
}

#[test]
fn indexed_columns_sort_as_numbers() {
    let values: Vec<u32> = (0..12).collect();
    let entities = json!([{ "id": "urn:a", "type": "T",
        "list": { "type": "Property", "value": values } }]);
    let mut table = table(&entities, &Limits::default()).expect("within the limits");
    table.settle(&[]);
    assert_eq!(table.columns[2], "list.value[0]");
    assert_eq!(table.columns[4], "list.value[2]");
    assert_eq!(table.columns[13], "list.value[11]");
}

/// Finding 16: the BOM and the Slovak Excel delimiter.
#[test]
fn bom_and_delimiter() {
    let entities = json!([{ "id": "urn:a", "type": "T",
        "name": { "type": "Property", "value": "Banská Bystrica; centrum" },
        "n": { "type": "Property", "value": 2.5 } }]);
    let table = table(&entities, &Limits::default()).expect("within the limits");

    let human = Dialect::of(Some("semicolon"), None, true).expect("a dialect");
    let text = text_of(&table, human);
    assert!(text.as_bytes().starts_with(&[0xEF, 0xBB, 0xBF]), "{text}");
    assert_eq!(
        text.trim_start_matches('\u{feff}'),
        "id;type;n.value;name.value\r\nurn:a;T;2.5;\"Banská Bystrica; centrum\"\r\n"
    );

    let program = Dialect::of(None, None, false).expect("a dialect");
    assert_eq!(program, Dialect::default());
    assert!(text_of(&table, program).starts_with("id,"));
    let tab = Dialect::of(Some("tab"), Some("false"), true).expect("a dialect");
    assert!(text_of(&table, tab).starts_with("id\ttype\t"));
    assert!(Dialect::of(Some("pipe"), None, false)
        .expect_err("refused")
        .to_string()
        .contains("delimiter"));
    assert!(Dialect::of(None, Some("yes"), false)
        .expect_err("refused")
        .to_string()
        .contains("bom"));
}

/// Finding 22: a page read at a shifted offset repeats an entity; the table holds it once.
#[test]
fn a_repeated_entity_is_one_row() {
    let mut table = table(
        &json!([event(1, json!({})), event(2, json!({}))]),
        &Limits::default(),
    )
    .expect("within the limits");
    table.append(&json!([event(2, json!({})), event(3, json!({}))]));
    let ids: Vec<&Value> = table.rows.iter().map(|row| &row[0]).collect();
    assert_eq!(
        ids,
        vec![
            "urn:ngsi-ld:Event:e1",
            "urn:ngsi-ld:Event:e2",
            "urn:ngsi-ld:Event:e3"
        ]
    );
}

/// Finding 21: ListRelationship and a kind this crate does not name keep their `type` out.
#[test]
fn list_relationship_and_unknown_kinds_are_structural() {
    let entities = json!([{ "id": "urn:a", "type": "T",
        "members": { "type": "ListRelationship", "objectList": ["urn:x", "urn:y"] },
        "future": { "type": "SomeNewProperty", "value": 4 },
        "where": { "type": "GeoProperty", "value": { "type": "Point", "coordinates": [1, 2] } } }]);
    let table = table(&entities, &Limits::default()).expect("within the limits");
    assert!(table.columns.contains(&"members.objectList[0]".to_owned()));
    assert!(table.columns.contains(&"future.value".to_owned()));
    assert!(
        table.columns.contains(&"where.value.type".to_owned()),
        "geometry type is data"
    );
    assert!(!table
        .columns
        .iter()
        .any(|c| c == "members.type" || c == "future.type"));
}

/// Finding 20: each datasetId instance in its own columns, the default one in the base columns.
#[test]
fn each_instance_has_its_own_columns() {
    let entities = json!([{ "id": "urn:a", "type": "T", "speed": [
        { "type": "Property", "value": 50 },
        { "type": "Property", "value": 48, "datasetId": "urn:ngsi-ld:Dataset:gps" }
    ] }]);
    let table = table(&entities, &Limits::default()).expect("within the limits");
    assert_eq!(
        table.columns,
        vec![
            "id",
            "type",
            "speed.value",
            "speed[datasetId=urn:ngsi-ld:Dataset:gps].value"
        ]
    );
    assert_eq!(table.rows[0][2], json!(50));
    assert_eq!(table.rows[0][3], json!(48));
}

/// Findings 18 and 19: readable headers with the unit symbol a person reads.
#[test]
fn human_headers_drop_every_wrapper_and_name_the_unit_symbol() {
    let entities = json!([{ "id": "urn:a", "type": "T",
        "address": { "type": "Property", "value": { "addressCountry": "SK" } },
        "name": { "type": "LanguageProperty", "languageMap": { "sk": "Námestie" } },
        "no2": { "type": "Property", "value": 12, "unitCode": "GQ" },
        "odd": { "type": "Property", "value": 1, "unitCode": "ZZZ" } }]);
    let mut table = table(&entities, &Limits::default()).expect("within the limits");
    humanize(&mut table);
    assert_eq!(
        table.columns,
        vec![
            "id",
            "type",
            "address.addressCountry",
            "name.sk",
            "no2 [µg/m³]",
            "odd [ZZZ]"
        ]
    );
}

/// The row ceiling holds for 100 000 rows built page by page, and the byte ceiling refuses.
#[test]
fn a_hundred_thousand_rows_stay_inside_the_ceilings() {
    let limits = Limits::default();
    let mut table = Table::default();
    for page in 0..100u32 {
        let entities: Vec<Value> = (0..1000u32)
            .map(|n| {
                json!({ "id": format!("urn:r{}", page * 1000 + n), "type": "R",
                "v": { "type": "Property", "value": n } })
            })
            .collect();
        table.append(&Value::Array(entities));
    }
    assert_eq!(table.rows.len(), 100_000);
    let text = csv(&table, &limits, Dialect::default()).expect("within the limits");
    assert!(text.len() < 4 * 1024 * 1024, "{}", text.len());
    let tight = Limits {
        max_rows: limits.max_rows,
        max_bytes: 1024,
    };
    assert!(csv(&table, &tight, Dialect::default()).is_err());
}
