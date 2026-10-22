//! Edge cases of `translators::tabular::{table, humanize}` (T-1958, T-1959; EP-08, EP-44,
//! EP-45, DM-06). The workbook writer went with T-4597.
//!
//! Contracts, one sentence each:
//!
//! - `table`: every row of the rectangle is aligned to the same header, and an answer past the
//!   row ceiling is refused whole rather than returned short.
//! - `humanize`: the header is rewritten for a reader without a cell moving out from under its
//!   column.
//!
//! `tabular_translator_tests.rs` covers the flattening, the CSV quoting, one unit header, the
//! row ceiling. These are the shapes around them: what is not an entity, the bound and the bound
//! plus one, and a header that collides.

use jc_tabular::{flatten, humanize, table, Limits, Table};
use serde_json::{json, Value};

fn reading(id: usize) -> Value {
    json!({
        "id": format!("urn:ngsi-ld:AirQualityObserved:banskabystrica.sk:ovzdusie:s-{id}"),
        "type": "AirQualityObserved",
        "pm10": { "type": "Property", "value": 34.2, "unitCode": "GQ" }
    })
}

fn limits(max_rows: u32, max_bytes: u64) -> Limits {
    Limits {
        max_rows,
        max_bytes,
    }
}

// --- table (T-1958) ---------------------------------------------------------------------------

#[test]
fn an_answer_that_is_not_entities_is_an_empty_table_and_not_an_error() {
    // A download of a problem document, a type list or a broker's plain string is nothing to
    // show. A header-only file says that; an error would say the download failed.
    for not_entities in [
        json!(null),
        json!("no entities here"),
        json!(7),
        json!(true),
        json!([]),
    ] {
        let table = table(&not_entities, &Limits::DEFAULT).expect("a table");
        assert!(table.columns.is_empty(), "{not_entities}");
        assert!(table.rows.is_empty(), "{not_entities}");
    }
}

#[test]
fn one_entity_is_one_row_whether_it_arrived_alone_or_in_a_list() {
    // A retrieve by id answers the entity itself and a query answers a list of one. The file a
    // caller downloads has to be the same either way.
    let alone = table(&reading(1), &Limits::DEFAULT).expect("a table");
    let listed = table(&json!([reading(1)]), &Limits::DEFAULT).expect("a table");
    assert_eq!(alone.columns, listed.columns);
    assert_eq!(alone.rows, listed.rows);
    assert_eq!(alone.rows.len(), 1);
}

#[test]
fn the_row_ceiling_is_the_ceiling_and_the_row_before_it_still_passes() {
    let rows: Vec<Value> = (0..10).map(reading).collect();
    assert_eq!(
        table(&json!(rows), &limits(10, u64::MAX))
            .expect("exactly the ceiling")
            .rows
            .len(),
        10
    );
    assert!(
        table(&json!(rows), &limits(9, u64::MAX)).is_err(),
        "one over"
    );
    // A ceiling of nothing refuses everything but the empty answer, rather than answering a
    // header with no rows, which a caller cannot tell from "there is no data".
    assert!(table(&json!([reading(1)]), &limits(0, u64::MAX)).is_err());
    assert_eq!(
        table(&json!([]), &limits(0, u64::MAX))
            .expect("nothing to refuse")
            .rows
            .len(),
        0
    );
}

#[test]
fn a_column_a_later_entity_brings_is_added_at_the_end_and_the_earlier_rows_hold_null() {
    // Columns are in first-mention order, so two runs over the same answer produce the same
    // header — which is what makes an ETag on a download mean anything.
    let answer = json!([
        { "id": "urn:ngsi-ld:X:o:s:1", "type": "X", "a": { "type": "Property", "value": 1 } },
        { "id": "urn:ngsi-ld:X:o:s:2", "type": "X", "b": { "type": "Property", "value": 2 } }
    ]);
    let table = table(&answer, &Limits::DEFAULT).expect("a table");
    assert_eq!(table.columns, vec!["id", "type", "a.value", "b.value"]);
    assert_eq!(table.rows[0][3], Value::Null, "the first entity has no b");
    assert_eq!(table.rows[1][2], Value::Null, "the second has no a");
    for row in &table.rows {
        assert_eq!(row.len(), table.columns.len(), "a row left the rectangle");
    }
}

#[test]
fn an_element_that_is_not_an_entity_is_a_row_of_nothing_and_never_a_shifted_row() {
    // A broker that answers an array inside its array is not correct, and the file must still be
    // a rectangle: the element brings no cells, so its row is null under every column.
    let answer = json!([reading(1), ["not an entity"], "nor this"]);
    let table = table(&answer, &Limits::DEFAULT).expect("a table");
    assert_eq!(table.rows.len(), 3);
    for row in &table.rows {
        assert_eq!(row.len(), table.columns.len());
    }
    assert!(table.rows[1].iter().all(Value::is_null));
    assert!(table.rows[2].iter().all(Value::is_null));
}

#[test]
fn id_and_type_lead_the_header_and_the_json_ld_keywords_are_not_columns() {
    let entity = json!({
        "@context": "https://uri.etsi.org/ngsi-ld/v1/ngsi-ld-core-context-v1.8.jsonld",
        "zulu": { "type": "Property", "value": 1 },
        "type": "AirQualityObserved",
        "id": "urn:ngsi-ld:AirQualityObserved:banskabystrica.sk:ovzdusie:s-1"
    });
    let table = table(&entity, &Limits::DEFAULT).expect("a table");
    assert_eq!(table.columns, vec!["id", "type", "zulu.value"]);
}

#[test]
fn an_id_that_is_not_a_scalar_takes_no_column_at_all() {
    // `id` and `type` lead the header because a reader looks for them first, and both are
    // skipped by the walk that follows so they are not written twice. An `id` that arrived as an
    // object therefore has no column anywhere — recorded with its evidence (`tabular.rs:158`
    // takes it only when it is not an object, `tabular.rs:164` skips it afterwards). CIM 009
    // says an entity id is a URI, so no correct broker sends one; a multi-type entity, which is
    // ordinary NGSI-LD, keeps its list in one cell.
    let table = table(
        &json!({
            "id": { "value": "urn:ngsi-ld:X:o:s:1" },
            "type": ["A", "B"],
            "a": { "type": "Property", "value": 1 }
        }),
        &Limits::DEFAULT,
    )
    .expect("a table");
    assert_eq!(table.columns, vec!["type", "a.value"]);
    assert_eq!(table.rows[0][0], json!(["A", "B"]));
}

#[test]
fn an_empty_container_ends_the_path_rather_than_disappearing() {
    // A cell that is an empty object or an empty array is data the entity carries. Walking into
    // it would produce no column at all, and the reader would not know the attribute was there.
    let cells = flatten(&json!({
        "id": "urn:ngsi-ld:X:o:s:1",
        "nothing": { "type": "Property", "value": {} },
        "none": { "type": "Property", "value": [] }
    }));
    assert!(
        cells.contains(&("nothing.value".to_owned(), json!({}))),
        "{cells:?}"
    );
    assert!(
        cells.contains(&("none.value".to_owned(), json!([])),),
        "{cells:?}"
    );
}

#[test]
fn two_paths_that_spell_the_same_column_leave_the_first_value_and_drop_the_second() {
    // Recorded with its evidence: the row is filled by `find`, the first cell of that name
    // (`tabular.rs:139`). An attribute whose own name carries a dot can collide with a nested
    // path, and the later value is then not in the file. A Smart Data Model name has no dot and
    // a broker answering one is already outside CIM 009, so this is written down rather than
    // guarded. Filed in chyby.md.
    let entity = json!({
        "id": "urn:ngsi-ld:X:o:s:1",
        "pm10": { "type": "Property", "value": 1 },
        "pm10.value": 2
    });
    let table = table(&entity, &Limits::DEFAULT).expect("a table");
    assert_eq!(
        table
            .columns
            .iter()
            .filter(|name| *name == "pm10.value")
            .count(),
        1
    );
    assert_eq!(
        table.rows[0][1],
        json!(1),
        "the first cell of that name wins"
    );
}

// --- humanize (T-1959) ------------------------------------------------------------------------

fn table_of(columns: &[&str], rows: Vec<Vec<Value>>) -> Table {
    let mut table = Table::default();
    table.columns = columns.iter().map(|name| (*name).to_owned()).collect();
    table.rows = rows;
    table
}

#[test]
fn the_unit_column_leaves_the_header_and_every_row_with_it() {
    // The cell has to go with its column, or every value after it reads under the wrong header —
    // which is a measurement attributed to the wrong thing, not a cosmetic defect.
    let mut table = table_of(
        &["id", "pm10.value", "pm10.unitCode", "pm25.value"],
        vec![
            vec![json!("s-1"), json!(34.2), json!("GQ"), json!(12.0)],
            vec![json!("s-2"), json!(30.0), json!("GQ"), json!(11.0)],
        ],
    );
    humanize(&mut table);
    assert_eq!(table.columns, vec!["id", "pm10 [µg/m³]", "pm25"]);
    assert_eq!(table.rows[0], vec![json!("s-1"), json!(34.2), json!(12.0)]);
    assert_eq!(table.rows[1], vec![json!("s-2"), json!(30.0), json!(11.0)]);
}

#[test]
fn the_unit_is_the_first_one_any_row_carries_and_no_unit_leaves_a_plain_header() {
    // A `unitCode` describes the attribute in the data model, not the individual observation, so
    // a row that happens to omit it does not make the column unitless.
    let mut first_row_silent = table_of(
        &["pm10.value", "pm10.unitCode"],
        vec![
            vec![json!(1), Value::Null],
            vec![json!(2), json!("GQ")],
            vec![json!(3), json!("XX")],
        ],
    );
    humanize(&mut first_row_silent);
    assert_eq!(first_row_silent.columns, vec!["pm10 [µg/m³]"]);

    for unitless in [Value::Null, json!(""), json!(7), json!({})] {
        let mut table = table_of(
            &["pm10.value", "pm10.unitCode"],
            vec![vec![json!(1), unitless.clone()]],
        );
        humanize(&mut table);
        assert_eq!(table.columns, vec!["pm10"], "{unitless} became a unit");
    }
}

#[test]
fn a_column_that_merely_ends_in_unit_code_without_a_dot_is_a_column_of_its_own() {
    // `unitCode` at the top level of an entity is an attribute, not the unit of another column.
    let mut table = table_of(
        &["unitCode", "myUnitCode", "pm10.value"],
        vec![vec![json!("GQ"), json!("XX"), json!(1)]],
    );
    humanize(&mut table);
    assert_eq!(table.columns, vec!["unitCode", "myUnitCode", "pm10"]);
    assert_eq!(table.rows[0].len(), 3, "no cell was dropped");
}

#[test]
fn a_unit_whose_value_column_is_not_there_takes_no_header_with_it() {
    // The grant may name `unitCode` and not `value`. The column still goes — it is the unit of
    // something the caller cannot read — and nothing else is renamed.
    let mut table = table_of(
        &["id", "pm10.unitCode"],
        vec![vec![json!("s-1"), json!("GQ")]],
    );
    humanize(&mut table);
    assert_eq!(table.columns, vec!["id"]);
    assert_eq!(table.rows[0], vec![json!("s-1")]);
}

#[test]
fn humanizing_a_header_twice_changes_nothing_the_second_time() {
    // A table is humanized once, but a second pass must not turn `pm10 [µg/m³]` into something else.
    let mut table = table_of(
        &["pm10.value", "pm10.unitCode"],
        vec![vec![json!(1), json!("GQ")]],
    );
    humanize(&mut table);
    let once = table.clone();
    humanize(&mut table);
    assert_eq!(table.columns, once.columns);
    assert_eq!(table.rows, once.rows);
}

#[test]
fn an_empty_table_survives_humanizing() {
    let mut nothing = Table::default();
    humanize(&mut nothing);
    assert!(nothing.columns.is_empty() && nothing.rows.is_empty());

    let mut header_only = table_of(&["pm10.value", "pm10.unitCode"], Vec::new());
    humanize(&mut header_only);
    assert_eq!(header_only.columns, vec!["pm10"], "no row carries a unit");
}

#[test]
fn several_units_each_find_their_own_column() {
    let mut table = table_of(
        &["pm10.value", "pm10.unitCode", "temp.value", "temp.unitCode"],
        vec![vec![json!(1), json!("GQ"), json!(-3), json!("CEL")]],
    );
    humanize(&mut table);
    assert_eq!(table.columns, vec!["pm10 [µg/m³]", "temp [°C]"]);
    assert_eq!(table.rows[0], vec![json!(1), json!(-3)]);
}
