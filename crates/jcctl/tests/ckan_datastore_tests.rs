//! The CKAN DataStore mirror: one Endpoint's rows in a catalogue table (T-0317, EP-65, EP-66).
//!
//! The table is a copy and the Endpoint is the source, so the properties worth asserting
//! are about where a row can come from and where it cannot: every cell comes out of the
//! Endpoint's own tabular answer, and a notification contributes ids and nothing else.

use jc_core::kinds::ckan::CkanPublication;
use jcctl::publish::ckan::InMemoryCkan;
use jcctl::publish::ckan_datastore::{
    drop_table, ensure, fields, mirrored, records, sync, table, touched, MirrorError, Outcome,
    ID_COLUMN, PRIMARY_KEY,
};
use serde_json::{json, Value};

const PACKAGE: &str = "pkg-kvalita-ovzdusia";
const TABLE: &str = "kvalita-ovzdusia-rows";

/// The model of the entities this endpoint serves, as Model Tools renders it (DM-02).
fn schema() -> Value {
    json!({
        "definitions": {
            "AirQualityObserved": {
                "properties": {
                    "id": { "type": "string", "x-ngsi-ld-kind": "Property" },
                    "type": { "type": "string", "x-ngsi-ld-kind": "Property" },
                    "temperature": {
                        "type": "number",
                        "description": "Air temperature.",
                        "x-ngsi-ld-kind": "Property"
                    },
                    "readings": { "type": "integer", "x-ngsi-ld-kind": "Property" },
                    "calibrated": { "type": ["boolean", "null"], "x-ngsi-ld-kind": "Property" },
                    "observedAt": {
                        "type": ["string", "null"],
                        "format": "date-time",
                        "x-ngsi-ld-kind": "Property"
                    },
                    "refDevice": { "type": ["string", "null"], "x-ngsi-ld-kind": "Relationship" },
                    "location": { "type": ["object", "null"], "x-ngsi-ld-kind": "GeoProperty" }
                }
            }
        }
    })
}

/// One page of the endpoint's tabular representation: `id` first, then one column per leaf.
fn page() -> (Vec<String>, Vec<Vec<Value>>) {
    let columns = [
        ID_COLUMN,
        "type",
        "temperature.value",
        "temperature.unitCode",
        "readings.value",
        "observedAt.value",
        "refDevice.object",
    ]
    .iter()
    .map(|column| (*column).to_owned())
    .collect();
    let rows = vec![
        vec![
            json!("urn:ngsi-ld:AirQualityObserved:bb:ovzdusie:sever-01"),
            json!("AirQualityObserved"),
            json!(12.5),
            json!("CEL"),
            json!(7),
            json!("2026-09-07T01:00:00Z"),
            json!("urn:ngsi-ld:Device:bb:ovzdusie:d1"),
        ],
        vec![
            json!("urn:ngsi-ld:AirQualityObserved:bb:ovzdusie:juh-02"),
            json!("AirQualityObserved"),
            json!(-3.5),
            json!("CEL"),
            json!(4),
            Value::Null,
            Value::Null,
        ],
    ];
    (columns, rows)
}

fn field_types(fields: &[Value]) -> Vec<(&str, &str)> {
    fields
        .iter()
        .map(|field| {
            (
                field["id"].as_str().expect("a field id"),
                field["type"].as_str().expect("a field type"),
            )
        })
        .collect()
}

fn publication(datastore: &str) -> CkanPublication {
    serde_norway::from_str(&format!(
        r#"instanceRef: {{ kind: CkanInstance, name: open-data }}
organization: mesto-banska-bystrica
{datastore}"#
    ))
    .expect("the publication parses")
}

// --- the schema of the table ----------------------------------------------------------

#[test]
fn an_endpoint_without_a_datastore_block_mirrors_nothing() {
    assert_eq!(mirrored(&publication("")), None);
    assert!(mirrored(&publication("datastore: { representation: csv }")).is_some());
}

#[test]
fn the_id_column_becomes_the_primary_key() {
    let (columns, rows) = page();
    let fields = fields(&columns, &rows, &[schema()]);

    assert_eq!(fields[0]["id"], json!(PRIMARY_KEY));
    assert_eq!(fields[0]["type"], json!("text"));
    let payload = table(PACKAGE, TABLE, &fields);
    assert_eq!(payload["primary_key"], json!([PRIMARY_KEY]));
    assert_eq!(payload["resource"]["package_id"], json!(PACKAGE));
}

#[test]
fn the_models_type_the_columns_they_declare() {
    let (columns, rows) = page();

    // The type comes from the model, not from what this page happened to carry: `readings`
    // is an int because the model says integer, and `observedAt` a timestamp because of its
    // format, even though both arrive as JSON scalars indistinguishable from text.
    assert_eq!(
        field_types(&fields(&columns, &rows, &[schema()])),
        vec![
            (PRIMARY_KEY, "text"),
            ("type", "text"),
            ("temperature.value", "float"),
            ("temperature.unitCode", "text"),
            ("readings.value", "int"),
            ("observedAt.value", "timestamp"),
            ("refDevice.object", "text"),
        ]
    );
}

#[test]
fn a_column_no_model_declares_is_typed_from_the_values() {
    // DM-28: an open-world model serves attributes it never declared, and the mirror still
    // has to hold them.
    let columns: Vec<String> = [ID_COLUMN, "extra.value", "count.value", "flag.value"]
        .iter()
        .map(|c| (*c).to_owned())
        .collect();
    let rows = vec![
        vec![json!("urn:x:1"), json!("free text"), json!(3), json!(true)],
        vec![json!("urn:x:2"), Value::Null, json!(9), json!(false)],
    ];

    assert_eq!(
        field_types(&fields(&columns, &rows, &[schema()])),
        vec![
            (PRIMARY_KEY, "text"),
            ("extra.value", "text"),
            ("count.value", "int"),
            ("flag.value", "bool"),
        ]
    );
}

#[test]
fn a_column_of_mixed_values_is_text_rather_than_a_refused_upsert() {
    let columns: Vec<String> = [ID_COLUMN, "mixed.value", "widening.value"]
        .iter()
        .map(|c| (*c).to_owned())
        .collect();
    let rows = vec![
        vec![json!("urn:x:1"), json!(3), json!(3)],
        vec![json!("urn:x:2"), json!("three"), json!(3.5)],
    ];

    // CKAN refuses a whole upsert when one cell does not parse, so a mixed column is text.
    // An int column that meets a float only widens.
    assert_eq!(
        field_types(&fields(&columns, &rows, &[])),
        vec![
            (PRIMARY_KEY, "text"),
            ("mixed.value", "text"),
            ("widening.value", "float")
        ]
    );
}

#[test]
fn a_geo_or_relationship_attribute_keeps_its_ngsi_ld_meaning() {
    let columns: Vec<String> = [ID_COLUMN, "location.value", "refDevice.object"]
        .iter()
        .map(|c| (*c).to_owned())
        .collect();
    let rows = vec![vec![
        json!("urn:x:1"),
        json!({ "type": "Point" }),
        json!("urn:d:1"),
    ]];

    assert_eq!(
        field_types(&fields(&columns, &rows, &[schema()])),
        vec![
            (PRIMARY_KEY, "text"),
            ("location.value", "json"),
            ("refDevice.object", "text")
        ]
    );
}

#[test]
fn a_deeper_leaf_of_a_structure_is_left_to_the_values() {
    // `location` flattens to one column per coordinate; the model describes the geometry as
    // a whole and says nothing about `coordinates[0]`.
    let columns: Vec<String> = [
        ID_COLUMN,
        "location.value.type",
        "location.value.coordinates[0]",
    ]
    .iter()
    .map(|c| (*c).to_owned())
    .collect();
    let rows = vec![vec![json!("urn:x:1"), json!("Point"), json!(19.15)]];

    assert_eq!(
        field_types(&fields(&columns, &rows, &[schema()])),
        vec![
            (PRIMARY_KEY, "text"),
            ("location.value.type", "text"),
            ("location.value.coordinates[0]", "float"),
        ]
    );
}

#[test]
fn a_description_from_the_model_travels_into_the_field() {
    let (columns, rows) = page();
    let fields = fields(&columns, &rows, &[schema()]);
    let temperature = fields
        .iter()
        .find(|field| field["id"] == json!("temperature.value"))
        .expect("the temperature field");

    assert_eq!(temperature["info"]["notes"], json!("Air temperature."));
}

#[test]
fn two_models_disagreeing_about_one_attribute_leave_it_to_the_values() {
    let other = json!({
        "definitions": {
            "WaterQualityObserved": {
                "properties": {
                    "readings": { "type": "string", "x-ngsi-ld-kind": "Property" }
                }
            }
        }
    });
    let columns: Vec<String> = [ID_COLUMN, "readings.value"]
        .iter()
        .map(|c| (*c).to_owned())
        .collect();
    let rows = vec![vec![json!("urn:x:1"), json!(7)]];

    assert_eq!(
        field_types(&fields(&columns, &rows, &[schema(), other])),
        vec![(PRIMARY_KEY, "text"), ("readings.value", "int")]
    );
}

// --- the rows -------------------------------------------------------------------------

#[test]
fn every_column_of_the_answer_becomes_a_cell() {
    let (columns, rows) = page();
    let records = records(&columns, &rows).expect("the page becomes records");

    assert_eq!(records.len(), 2);
    assert_eq!(
        records[0][PRIMARY_KEY],
        json!("urn:ngsi-ld:AirQualityObserved:bb:ovzdusie:sever-01")
    );
    assert_eq!(records[0]["temperature.value"], json!(12.5));
    // A null cell is written rather than dropped: an attribute that stopped being answered
    // has to clear the column, not keep yesterday's value.
    assert_eq!(records[1]["observedAt.value"], Value::Null);
    assert!(records[1]
        .as_object()
        .expect("a record")
        .contains_key("refDevice.object"));
}

#[test]
fn an_answer_with_no_id_column_is_refused() {
    let columns = vec!["temperature.value".to_owned()];
    let rows = vec![vec![json!(12.5)]];

    assert_eq!(records(&columns, &rows), Err(MirrorError::NoIdColumn));
}

#[test]
fn a_row_that_does_not_match_the_header_is_refused() {
    let (columns, mut rows) = page();
    rows[1].pop();

    assert!(matches!(
        records(&columns, &rows),
        Err(MirrorError::RaggedRow { row: 1, .. })
    ));
}

#[test]
fn a_row_without_a_usable_id_is_refused() {
    let columns = vec![ID_COLUMN.to_owned(), "temperature.value".to_owned()];
    let rows = vec![vec![Value::Null, json!(12.5)]];

    assert!(records(&columns, &rows).is_err());
}

// --- creating and extending the table -------------------------------------------------

#[test]
fn the_first_publication_creates_the_table_and_loads_the_page() {
    let mut ckan = InMemoryCkan::new();
    let (columns, rows) = page();
    let fields = fields(&columns, &rows, &[schema()]);

    let (resource, outcome) = ensure(&mut ckan, PACKAGE, TABLE, &fields).expect("created");
    assert_eq!(outcome, Outcome::Created);

    let records = records(&columns, &rows).expect("records");
    let ids: Vec<String> = records
        .iter()
        .map(|record| record[PRIMARY_KEY].as_str().expect("an id").to_owned())
        .collect();
    let synced = sync(&mut ckan, &resource, &ids, &records).expect("synced");

    assert_eq!(synced.upserted.len(), 2);
    assert!(synced.deleted.is_empty());
    assert_eq!(ckan.rows(&resource).expect("the table").len(), 2);
    assert_eq!(ckan.actions(), vec!["datastore_create", "datastore_upsert"]);
}

#[test]
fn a_second_run_over_the_same_columns_writes_no_schema_call() {
    let mut ckan = InMemoryCkan::new();
    let (columns, rows) = page();
    let fields = fields(&columns, &rows, &[schema()]);
    let (resource, _) = ensure(&mut ckan, PACKAGE, TABLE, &fields).expect("created");

    let (again, outcome) = ensure(&mut ckan, PACKAGE, TABLE, &fields).expect("unchanged");

    // CC-18: apply converges rather than churning the catalogue.
    assert_eq!(outcome, Outcome::Unchanged);
    assert_eq!(again, resource);
    assert_eq!(ckan.actions(), vec!["datastore_create"]);
}

#[test]
fn a_new_column_extends_the_table_instead_of_failing_the_upsert() {
    let mut ckan = InMemoryCkan::new();
    let (columns, rows) = page();
    let (resource, _) = ensure(
        &mut ckan,
        PACKAGE,
        TABLE,
        &fields(&columns, &rows, &[schema()]),
    )
    .expect("created");

    let mut wider = columns.clone();
    wider.push("humidity.value".to_owned());
    let wider_rows = vec![vec![
        json!("urn:ngsi-ld:AirQualityObserved:bb:ovzdusie:sever-01"),
        json!("AirQualityObserved"),
        json!(12.5),
        json!("CEL"),
        json!(7),
        json!("2026-09-07T01:00:00Z"),
        json!("urn:ngsi-ld:Device:bb:ovzdusie:d1"),
        json!(61.0),
    ]];
    let (_, outcome) = ensure(
        &mut ckan,
        PACKAGE,
        TABLE,
        &fields(&wider, &wider_rows, &[schema()]),
    )
    .expect("extended");
    assert_eq!(outcome, Outcome::Extended);

    let records = records(&wider, &wider_rows).expect("records");
    sync(&mut ckan, &resource, &[], &records).expect("the wider page upserts");
    assert_eq!(
        ckan.rows(&resource).expect("the table")
            ["urn:ngsi-ld:AirQualityObserved:bb:ovzdusie:sever-01"]["humidity.value"],
        json!(61.0)
    );
    // A field is only ever added: dropping one would delete a column of data because an
    // endpoint answered one page without it.
    assert_eq!(ckan.table_fields(&resource).expect("fields").len(), 8);
}

#[test]
fn an_upsert_of_a_field_the_table_does_not_declare_is_refused() {
    // The property the extend path exists for. CKAN rejects the whole call, so a mirror
    // that skipped `ensure` would silently stop refreshing.
    let mut ckan = InMemoryCkan::new();
    let (columns, rows) = page();
    ensure(
        &mut ckan,
        PACKAGE,
        TABLE,
        &fields(&columns, &rows, &[schema()]),
    )
    .expect("created");

    let record = json!({ PRIMARY_KEY: "urn:x:1", "undeclared.value": 1 });
    let refused = sync(&mut ckan, TABLE, &[], &[record]);

    assert!(refused.is_err(), "an undeclared field must not be written");
}

// --- refreshing from a notification ---------------------------------------------------

/// A notification as the endpoint's own egress delivers it (T-0156, R46).
fn notification(entities: Value) -> Value {
    json!({
        "id": "urn:ngsi-ld:Notification:1",
        "type": "Notification",
        "subscriptionId": "urn:ngsi-ld:Subscription:ovzdusie",
        "notifiedAt": "2026-09-07T02:00:00Z",
        "data": entities
    })
}

#[test]
fn a_notification_contributes_ids_and_nothing_else() {
    let body = notification(json!([
        {
            "id": "urn:ngsi-ld:AirQualityObserved:bb:ovzdusie:sever-01",
            "type": "AirQualityObserved",
            "temperature": { "type": "Property", "value": 14.0 },
            "operatorNote": { "type": "Property", "value": "an attribute no grant allows" }
        },
        { "id": "urn:ngsi-ld:AirQualityObserved:bb:ovzdusie:juh-02", "type": "AirQualityObserved" }
    ]));

    let touched = touched(&body);

    // EP-66: the row is re-read through the Endpoint. Nothing but the id is taken from the
    // notification, so an attribute in the body that the Endpoint's projection would have
    // removed has no path into the table at all.
    assert_eq!(
        touched,
        vec![
            "urn:ngsi-ld:AirQualityObserved:bb:ovzdusie:juh-02".to_owned(),
            "urn:ngsi-ld:AirQualityObserved:bb:ovzdusie:sever-01".to_owned(),
        ]
    );
}

#[test]
fn a_notification_of_one_entity_and_an_empty_one_are_both_read() {
    let one = notification(json!({ "id": "urn:x:1", "type": "T" }));
    assert_eq!(touched(&one), vec!["urn:x:1".to_owned()]);
    assert!(touched(&notification(json!([]))).is_empty());
    assert!(touched(&json!({})).is_empty());
}

#[test]
fn a_changed_entity_is_upserted_and_a_vanished_one_is_deleted() {
    let mut ckan = InMemoryCkan::new();
    let (columns, rows) = page();
    let fields = fields(&columns, &rows, &[schema()]);
    let (resource, _) = ensure(&mut ckan, PACKAGE, TABLE, &fields).expect("created");
    let loaded = records(&columns, &rows).expect("records");
    let ids: Vec<String> = loaded
        .iter()
        .map(|record| record[PRIMARY_KEY].as_str().expect("an id").to_owned())
        .collect();
    sync(&mut ckan, &resource, &ids, &loaded).expect("the first load");

    // The notification names both entities; the Endpoint answers for only one of them,
    // because the other was deleted or narrowed out of the projection.
    let body = notification(json!([
        { "id": "urn:ngsi-ld:AirQualityObserved:bb:ovzdusie:sever-01", "type": "AirQualityObserved" },
        { "id": "urn:ngsi-ld:AirQualityObserved:bb:ovzdusie:juh-02", "type": "AirQualityObserved" }
    ]));
    let requested = touched(&body);
    let answered = records(&columns, &rows[..1]).expect("one row came back");

    let synced = sync(&mut ckan, &resource, &requested, &answered).expect("refreshed");

    assert_eq!(
        synced.upserted,
        vec!["urn:ngsi-ld:AirQualityObserved:bb:ovzdusie:sever-01".to_owned()]
    );
    assert_eq!(
        synced.deleted,
        vec!["urn:ngsi-ld:AirQualityObserved:bb:ovzdusie:juh-02".to_owned()]
    );
    // EP-65: rows that leave the endpoint's projection leave the table with them.
    let rows = ckan.rows(&resource).expect("the table");
    assert_eq!(rows.len(), 1);
    assert!(rows.contains_key("urn:ngsi-ld:AirQualityObserved:bb:ovzdusie:sever-01"));
}

#[test]
fn one_upsert_replaces_a_row_rather_than_adding_a_second() {
    let mut ckan = InMemoryCkan::new();
    let (columns, rows) = page();
    let (resource, _) = ensure(
        &mut ckan,
        PACKAGE,
        TABLE,
        &fields(&columns, &rows, &[schema()]),
    )
    .expect("created");
    let loaded = records(&columns, &rows).expect("records");
    sync(&mut ckan, &resource, &[], &loaded).expect("the first load");

    let mut warmer = rows.clone();
    warmer[0][2] = json!(21.0);
    let refreshed = records(&columns, &warmer[..1]).expect("records");
    sync(&mut ckan, &resource, &[], &refreshed).expect("the refresh");

    let table = ckan.rows(&resource).expect("the table");
    assert_eq!(
        table.len(),
        2,
        "the mirror is keyed by the entity, not appended to"
    );
    assert_eq!(
        table["urn:ngsi-ld:AirQualityObserved:bb:ovzdusie:sever-01"]["temperature.value"],
        json!(21.0)
    );
}

#[test]
fn a_refresh_that_changed_nothing_writes_nothing() {
    let mut ckan = InMemoryCkan::new();
    let (columns, rows) = page();
    let (resource, _) = ensure(
        &mut ckan,
        PACKAGE,
        TABLE,
        &fields(&columns, &rows, &[schema()]),
    )
    .expect("created");

    let synced = sync(&mut ckan, &resource, &[], &[]).expect("nothing to do");

    assert_eq!(synced, Default::default());
    assert_eq!(ckan.actions(), vec!["datastore_create"]);
}

// --- withdrawing ----------------------------------------------------------------------

#[test]
fn withdrawing_the_mirror_drops_the_table_and_is_re_runnable() {
    let mut ckan = InMemoryCkan::new();
    let (columns, rows) = page();
    let (resource, _) = ensure(
        &mut ckan,
        PACKAGE,
        TABLE,
        &fields(&columns, &rows, &[schema()]),
    )
    .expect("created");

    assert_eq!(drop_table(&mut ckan, &resource), Ok(Outcome::NotMirrored));
    assert!(ckan.rows(&resource).is_none());
    // CC-19: a run has to be re-runnable after a partial failure.
    assert_eq!(drop_table(&mut ckan, &resource), Ok(Outcome::Unchanged));
}

// --- the token ------------------------------------------------------------------------

#[test]
fn no_payload_this_module_builds_carries_the_api_token() {
    const TOKEN: &str = "ckan-api-token-that-must-never-be-published";
    let mut ckan = InMemoryCkan::new().with_token(TOKEN);
    let (columns, rows) = page();
    let (resource, _) = ensure(
        &mut ckan,
        PACKAGE,
        TABLE,
        &fields(&columns, &rows, &[schema()]),
    )
    .expect("created");
    let loaded = records(&columns, &rows).expect("records");
    sync(&mut ckan, &resource, &["urn:gone".to_owned()], &loaded).expect("synced");

    // EP-67: the token belongs to the transport. It is in the double so a test can look for
    // it in every payload that left this module.
    for (action, payload) in ckan.calls() {
        let sent = serde_json::to_string(payload).expect("the payload serialises");
        assert!(
            !sent.contains(ckan.token()),
            "{action} carried the API token"
        );
    }
}
