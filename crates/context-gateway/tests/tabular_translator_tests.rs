//! The tabular representations of an NGSI-LD answer (T-0157, EP-08, EP-44, EP-45).

use context_gateway::translators::tabular::{
    csv, flatten, humanize, table, xlsx, Limits, Table, TooLarge, XlsxError,
};
use serde_json::{json, Value};
use std::io::Read;

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
    let text = csv(&table, &Limits::default()).expect("within the limits");
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

    let text = csv(&table, &Limits::default()).expect("within the limits");
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
        table.columns.contains(&"temperature [CEL]".to_owned()),
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

    let text = csv(&table, &Limits::default()).expect("within the limits");
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
    assert_eq!(value_at("temperature [CEL]"), Some("22.4".to_owned()));
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
    let text = csv(&table, &Limits::default()).expect("within the limits");
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
    assert_eq!(csv(&table, &tight), Err(TooLarge));
}

/// An answer with no entity is a header-only file, not an error: nothing to show is a
/// legitimate result of a narrowed query (R22).
#[test]
fn an_empty_answer_is_an_empty_table() {
    let table = table(&json!([]), &Limits::default()).expect("within the limits");
    assert!(table.is_empty());
    assert_eq!(csv(&table, &Limits::default()), Ok("\r\n".to_owned()));
}

/// The workbook is a real OOXML package: the parts Excel opens are all there, the numbers
/// are numbers, and the strings are inline rather than in a shared table.
#[test]
fn the_workbook_is_a_readable_ooxml_package() {
    let table = table(&json!([station()]), &Limits::default()).expect("within the limits");
    let bytes = xlsx(
        &table,
        &[("space".to_owned(), "ovzdusie".to_owned())],
        &Limits::DEFAULT,
    )
    .expect("a workbook");

    let mut archive = zip::ZipArchive::new(std::io::Cursor::new(bytes)).expect("a zip");
    let names: Vec<String> = archive.file_names().map(str::to_owned).collect();
    for required in [
        "[Content_Types].xml",
        "_rels/.rels",
        "xl/workbook.xml",
        "xl/_rels/workbook.xml.rels",
        "xl/worksheets/sheet1.xml",
        "xl/worksheets/sheet2.xml",
    ] {
        assert!(
            names.iter().any(|name| name == required),
            "{required} missing from {names:?}"
        );
    }

    let mut sheet = String::new();
    archive
        .by_name("xl/worksheets/sheet1.xml")
        .expect("the data sheet")
        .read_to_string(&mut sheet)
        .expect("utf-8");
    assert!(
        sheet.contains("<t xml:space=\"preserve\">temperature.value</t>"),
        "{sheet}"
    );
    // A JSON number is a numeric cell, in the column the header put it in, so a reader
    // can sum a measurement without retyping it.
    let column = table
        .columns
        .iter()
        .position(|name| name == "temperature.value")
        .expect("the measurement has a column");
    let reference = format!("{}2", (b'A' + column as u8) as char);
    assert!(
        sheet.contains(&format!("<c r=\"{reference}\"><v>22.4</v></c>")),
        "{sheet}"
    );

    let mut metadata = String::new();
    archive
        .by_name("xl/worksheets/sheet2.xml")
        .expect("the metadata sheet")
        .read_to_string(&mut metadata)
        .expect("utf-8");
    assert!(metadata.contains("ovzdusie"), "{metadata}");
}

/// A value that would close the XML it is written into is escaped, so a manifest or an
/// entity cannot make the workbook unopenable, or make it say something else.
#[test]
fn a_hostile_value_cannot_break_out_of_the_worksheet() {
    let answer = json!([{
        "id": "urn:a",
        "type": "T",
        "note": { "type": "Property", "value": "</t></is></c><c r=\"Z9\"><v>0</v></c>" }
    }]);
    let table = table(&answer, &Limits::default()).expect("within the limits");
    let bytes = xlsx(&table, &[], &Limits::DEFAULT).expect("a workbook");
    let mut archive = zip::ZipArchive::new(std::io::Cursor::new(bytes)).expect("a zip");
    let mut sheet = String::new();
    archive
        .by_name("xl/worksheets/sheet1.xml")
        .expect("the data sheet")
        .read_to_string(&mut sheet)
        .expect("utf-8");
    assert!(!sheet.contains("<c r=\"Z9\">"), "{sheet}");
    assert!(sheet.contains("&lt;/t&gt;&lt;/is&gt;"), "{sheet}");
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
        csv(&first, &Limits::default()),
        csv(&second, &Limits::default())
    );
}

// --- The representation as the gateway serves it -----------------------------------

/// T-0810: the workbook writer takes the same ceiling the CSV writer takes, and refuses
/// rather than truncating — a short file is indistinguishable from a complete one (EP-44).
#[test]
fn a_workbook_past_the_byte_ceiling_is_refused() {
    let many: Vec<Value> = (0..500).map(|_| station()).collect();
    let table = table(&json!(many), &Limits::DEFAULT).expect("a table");
    let tight = Limits {
        max_bytes: 1024,
        ..Limits::DEFAULT
    };
    assert!(
        matches!(xlsx(&table, &[], &tight), Err(XlsxError::TooLarge)),
        "a sheet larger than the ceiling is not a workbook"
    );
    xlsx(&table, &[], &Limits::DEFAULT).expect("the same table inside the default ceiling");
}

mod common;

use axum::body::Body;
use axum::http::{Request as HttpRequest, StatusCode};
use common::BrokerStub;
use context_gateway::app::{router, Gateway};
use context_gateway::pdp::PolicyPdp;
use context_gateway::proxy::Broker;
use context_gateway::resolver::Endpoint;
use jc_core::kinds::{Audience, FileLimits, Representation};
use std::sync::Arc;
use tower::ServiceExt;

const SLUG: &str = "k4y7pq2mzt6vhx3nbwrs5cjd8f";

/// The public grant: two readable attributes out of the three the broker returns.
fn endpoint(representations: Vec<Representation>, file_limits: Option<FileLimits>) -> Endpoint {
    Endpoint {
        slug: SLUG.to_owned(),
        title: std::collections::BTreeMap::new(),
        description: std::collections::BTreeMap::new(),
        space: "ovzdusie".to_owned(),
        project: "ovzdusie".to_owned(),
        audience: Audience::Public,
        allowed_projects: Vec::new(),
        representations,
        rate_limit: None,
        file_limits,
        hidden_attributes: Default::default(),
        projection: None,
        base_path: format!("/api/endpoint/{SLUG}"),
        models: Vec::new(),
        view_mapping: None,
        policies: vec![serde_norway::from_str(
            r#"contextSpaceRef: ovzdusie
assigner: did:web:banskabystrica.sk
assignee: { kind: role, id: public }
operations: [queryEntity, retrieveEntity]
information:
  - entities:
      - type: AirQualityObserved
    propertyNames: [pm10, location]
"#,
        )
        .expect("the policy spec parses")],
    }
}

fn gateway(broker: &str, endpoint: Endpoint) -> axum::Router {
    router(Arc::new(
        Gateway::new(
            Broker::new(broker),
            Box::new(PolicyPdp),
            "banskabystrica.sk",
        )
        .serve([endpoint]),
    ))
}

fn reading(n: usize) -> Value {
    json!({
        "id": format!("urn:ngsi-ld:AirQualityObserved:banskabystrica.sk:ovzdusie:s{n}"),
        "type": "AirQualityObserved",
        "pm10": { "type": "Property", "value": n },
        "location": { "type": "GeoProperty",
                      "value": { "type": "Point", "coordinates": [19.1, 48.7] } },
        "operatorNote": { "type": "Property", "value": "internal only" }
    })
}

async fn download(app: axum::Router, path: &str) -> (StatusCode, String, String) {
    let response = app
        .oneshot(
            HttpRequest::builder()
                .uri(path)
                .body(Body::empty())
                .expect("a request"),
        )
        .await
        .expect("the gateway answers");
    let status = response.status();
    let media = response
        .headers()
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    let body = axum::body::to_bytes(response.into_body(), 16 * 1024 * 1024)
        .await
        .expect("a readable body");
    (status, media, String::from_utf8_lossy(&body).into_owned())
}

/// EP-06: an attribute no grant names has no column, exactly as it has no member in the
/// JSON representation. A second format is not a second set of rules.
#[tokio::test]
async fn an_ungranted_attribute_is_not_a_column() {
    let broker = BrokerStub::start(vec![json!([reading(1)])]).await;
    let (status, media, body) = download(
        gateway(&broker.url, endpoint(vec![Representation::Csv], None)),
        &format!("/api/endpoint/{SLUG}/file.csv?type=AirQualityObserved"),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(media, "text/csv; charset=utf-8; header=present");
    let header = body.lines().next().expect("a header row");
    assert!(header.contains("pm10.value"), "{header}");
    assert!(header.contains("location.value.coordinates[0]"), "{header}");
    assert!(
        !body.contains("operatorNote") && !body.contains("internal only"),
        "an ungranted attribute leaked into the table: {body}"
    );
}

/// EP-44: a file representation returns the whole result set, not one broker page, and
/// the paging window is the gateway's own rather than the caller's.
#[tokio::test]
async fn the_gateway_pages_through_the_broker() {
    let full: Vec<Value> = (0..1000).map(reading).collect();
    let broker = BrokerStub::start(vec![json!(full), json!([reading(1000)])]).await;
    let (status, _, body) = download(
        gateway(&broker.url, endpoint(vec![Representation::Csv], None)),
        &format!("/api/endpoint/{SLUG}/file.csv?type=AirQualityObserved"),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    // 1001 rows plus the header.
    assert_eq!(body.lines().count(), 1002);

    let hops = broker.hops();
    assert_eq!(
        hops.len(),
        2,
        "a full page must be followed by another read"
    );
    assert!(hops[0].query.contains("limit=1000"), "{}", hops[0].query);
    assert!(hops[0].query.contains("offset=0"), "{}", hops[0].query);
    assert!(hops[1].query.contains("offset=1000"), "{}", hops[1].query);
    assert_eq!(hops[0].tenant, "ovzdusie");
}

/// EP-44: past the endpoint's own ceiling the download is refused, never truncated.
#[tokio::test]
async fn the_endpoints_row_ceiling_answers_413() {
    let full: Vec<Value> = (0..1000).map(reading).collect();
    let broker = BrokerStub::start(vec![json!(full)]).await;
    let limits = FileLimits {
        max_file_rows: Some(10),
        max_file_bytes: None,
    };
    let (status, _, _) = download(
        gateway(
            &broker.url,
            endpoint(vec![Representation::Csv], Some(limits)),
        ),
        &format!("/api/endpoint/{SLUG}/file.csv?type=AirQualityObserved"),
    )
    .await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
}

/// EP-05: an endpoint that does not enable the representation does not have the URL.
#[tokio::test]
async fn a_representation_the_endpoint_does_not_serve_is_404() {
    let broker = BrokerStub::start(vec![json!([reading(1)])]).await;
    let (status, _, _) = download(
        gateway(&broker.url, endpoint(vec![Representation::NgsiLd], None)),
        &format!("/api/endpoint/{SLUG}/file.csv"),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(broker.hops().is_empty(), "a 404 must not reach the broker");
}

/// EP-43: the workbook is sent as an attachment with its own media type.
#[tokio::test]
async fn the_workbook_is_served_as_an_attachment() {
    let broker = BrokerStub::start(vec![json!([reading(1)])]).await;
    let response = gateway(&broker.url, endpoint(vec![Representation::Xlsx], None))
        .oneshot(
            HttpRequest::builder()
                .uri(format!(
                    "/api/endpoint/{SLUG}/file.xlsx?type=AirQualityObserved"
                ))
                .body(Body::empty())
                .expect("a request"),
        )
        .await
        .expect("the gateway answers");

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get(axum::http::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok()),
        Some("application/vnd.openxmlformats-officedocument.spreadsheetml.sheet")
    );
    assert_eq!(
        response
            .headers()
            .get(axum::http::header::CONTENT_DISPOSITION)
            .and_then(|value| value.to_str().ok()),
        Some(format!("attachment; filename=\"{SLUG}.xlsx\"").as_str())
    );

    let body = axum::body::to_bytes(response.into_body(), 16 * 1024 * 1024)
        .await
        .expect("a readable body");
    // A workbook is a zip, and it starts with one.
    assert_eq!(&body[..2], b"PK");
}

/// T-0810, EP-44: the workbook was the one file representation no byte ceiling reached, so a
/// download that csv refuses was built whole in memory and answered 200.
#[tokio::test]
async fn the_endpoints_byte_ceiling_refuses_a_workbook_too() {
    let full: Vec<Value> = (0..1000).map(reading).collect();
    let limits = FileLimits {
        max_file_rows: None,
        max_file_bytes: Some(1024),
    };
    for (representation, file) in [
        (Representation::Xlsx, "file.xlsx"),
        (Representation::Csv, "file.csv"),
    ] {
        let broker = BrokerStub::start(vec![json!(full)]).await;
        let (status, _, _) = download(
            gateway(
                &broker.url,
                endpoint(vec![representation], Some(limits.clone())),
            ),
            &format!("/api/endpoint/{SLUG}/{file}?type=AirQualityObserved"),
        )
        .await;
        assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE, "{file}");
    }
}
