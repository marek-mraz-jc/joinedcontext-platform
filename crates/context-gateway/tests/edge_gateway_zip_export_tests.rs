//! Edge cases of `translators::zip_export::bundle` (T-1962; EP-41, EP-43, EP-44, EP-51).
//!
//! Contract, one sentence: one projected answer becomes one archive whose three shapes,
//! schemas and manifest describe the same entities, and an answer past a ceiling is refused
//! whole rather than written short.
//!
//! `zip_export_tests.rs` drives the download through the HTTP surface: the members, the three
//! shapes agreeing, a hidden attribute, and the two ceilings. These are the shapes underneath —
//! what is not an answer at all, a name a member could escape the archive with, and the bound
//! and the bound plus one.

use context_gateway::translators::tabular::Limits;
use context_gateway::translators::zip_export::{bundle, file_name, Bundle, BundleError};
use serde_json::{json, Value};

const SLUG: &str = "n4t8xq2vhm6zc9wrb5sdj3kfp7";
const EXPORTED: &str = "2026-09-07T08:00:00Z";

fn descriptor() -> Bundle<'static> {
    Bundle {
        slug: SLUG,
        space: "ovzdusie",
        query: "type=AirQualityObserved&limit=10",
        exported_at: EXPORTED,
    }
}

fn station(local: &str) -> Value {
    json!({
        "id": format!("urn:ngsi-ld:AirQualityObserved:banskabystrica.sk:ovzdusie:{local}"),
        "type": "AirQualityObserved",
        "pm10": { "type": "Property", "value": 34.2, "unitCode": "GQ" },
        "location": {
            "type": "GeoProperty",
            "value": { "type": "Point", "coordinates": [19.15, 48.73] }
        }
    })
}

fn dcat() -> Value {
    json!({ "@type": "dcat:Dataset", "dct:title": "Ovzdušie" })
}

fn limits(max_rows: u32, max_bytes: u64) -> Limits {
    Limits {
        max_rows,
        max_bytes,
    }
}

/// The archive's members by path, with their bytes.
fn members(archive: &[u8]) -> Vec<(String, Vec<u8>)> {
    use std::io::Read;
    let mut zip =
        zip::ZipArchive::new(std::io::Cursor::new(archive.to_vec())).expect("a zip archive");
    (0..zip.len())
        .map(|index| {
            let mut entry = zip.by_index(index).expect("an entry");
            let name = entry.name().to_owned();
            let mut body = Vec::new();
            entry.read_to_end(&mut body).expect("the member");
            (name, body)
        })
        .collect()
}

fn text(archive: &[u8], suffix: &str) -> String {
    let (_, body) = members(archive)
        .into_iter()
        .find(|(name, _)| name.ends_with(suffix))
        .unwrap_or_else(|| panic!("no member ending in {suffix}"));
    String::from_utf8(body).expect("utf-8")
}

#[test]
fn an_answer_with_no_entities_is_still_a_complete_bundle() {
    // A query that matched nothing is an answer, and a colleague opening the archive has to see
    // that: the shapes are empty, the manifest says zero rows and names no type.
    let archive =
        bundle(&json!([]), &[], &dcat(), &descriptor(), &Limits::DEFAULT).expect("a bundle");
    let names: Vec<String> = members(&archive)
        .into_iter()
        .map(|(name, _)| name)
        .collect();
    assert_eq!(names.len(), 5, "{names:?}");
    let manifest: Value = serde_json::from_str(&text(&archive, "manifest.json")).expect("json");
    assert_eq!(manifest["rows"], json!(0));
    assert_eq!(manifest["types"], json!([]));
    assert_eq!(
        serde_json::from_str::<Value>(&text(&archive, "entities.geojson")).expect("json")
            ["features"],
        json!([])
    );
}

#[test]
fn an_answer_that_is_not_entities_bundles_as_nothing_rather_than_failing() {
    // The download surface may be asked with a filter that made the broker answer a document
    // rather than a list. There is nothing to export; an archive saying so is more use than a
    // 500 the caller cannot act on.
    for not_entities in [
        json!(null),
        json!("no"),
        json!(7),
        json!({ "title": "Not Found" }),
    ] {
        let archive =
            bundle(&not_entities, &[], &dcat(), &descriptor(), &Limits::DEFAULT).expect("a bundle");
        let manifest: Value = serde_json::from_str(&text(&archive, "manifest.json")).expect("json");
        assert_eq!(manifest["types"], json!([]), "{not_entities}");
    }
}

#[test]
fn a_single_entity_bundles_like_a_list_of_one() {
    let alone = bundle(
        &station("s-1"),
        &[],
        &dcat(),
        &descriptor(),
        &Limits::DEFAULT,
    )
    .expect("a bundle");
    let manifest: Value = serde_json::from_str(&text(&alone, "manifest.json")).expect("json");
    assert_eq!(manifest["rows"], json!(1));
    assert_eq!(manifest["types"], json!(["AirQualityObserved"]));
}

#[test]
fn every_type_in_the_answer_is_named_once_and_in_the_same_order_each_run() {
    // The manifest is what a bundle on a disk says about itself, so it has to be stable: two
    // exports of the same answer produce the same bytes, which is what an ETag means.
    let answer = json!([
        station("s-1"),
        { "id": "urn:ngsi-ld:Depot:banskabystrica.sk:ovzdusie:d-1", "type": "Depot" },
        station("s-2"),
        { "id": "urn:ngsi-ld:Depot:banskabystrica.sk:ovzdusie:d-2", "type": "Depot" },
        { "id": "urn:ngsi-ld:X:banskabystrica.sk:ovzdusie:x-1" }
    ]);
    let first = bundle(&answer, &[], &dcat(), &descriptor(), &Limits::DEFAULT).expect("a bundle");
    let manifest: Value = serde_json::from_str(&text(&first, "manifest.json")).expect("json");
    assert_eq!(manifest["types"], json!(["AirQualityObserved", "Depot"]));
    assert_eq!(
        manifest["rows"],
        json!(5),
        "an entity with no type is still a row"
    );

    let again = bundle(&answer, &[], &dcat(), &descriptor(), &Limits::DEFAULT).expect("a bundle");
    assert_eq!(
        members(&first)
            .iter()
            .map(|(name, body)| (name.clone(), body.len()))
            .collect::<Vec<_>>(),
        members(&again)
            .iter()
            .map(|(name, body)| (name.clone(), body.len()))
            .collect::<Vec<_>>()
    );
}

#[test]
fn every_member_sits_under_the_one_directory_the_archive_is_named_for() {
    // An archive that writes outside its own directory is one that overwrites a file next to
    // where it was unpacked.
    let archive = bundle(
        &json!([station("s-1")]),
        &[("index.json".to_owned(), b"{}".to_vec())],
        &dcat(),
        &descriptor(),
        &Limits::DEFAULT,
    )
    .expect("a bundle");
    let root = format!("{SLUG}-20260907");
    for (name, _) in members(&archive) {
        assert!(
            name.starts_with(&format!("{root}/")),
            "{name} leaves the bundle"
        );
        assert!(!name.contains(".."), "{name}");
        assert!(!name.starts_with('/'), "{name}");
    }
}

#[test]
fn a_schema_path_is_taken_as_the_caller_of_this_function_wrote_it() {
    // Recorded with its evidence: `zip_export.rs:108` joins the schema's own path under
    // `schema/` without checking it. The names come from the gateway's schema surface, which
    // renders them from the endpoint's models, never from a request — so this is a note for
    // whoever adds a second caller, not a reachable defect.
    let archive = bundle(
        &json!([]),
        &[("v1/index.json".to_owned(), b"{}".to_vec())],
        &dcat(),
        &descriptor(),
        &Limits::DEFAULT,
    )
    .expect("a bundle");
    assert!(members(&archive)
        .iter()
        .any(|(name, _)| name.ends_with("/schema/v1/index.json")));
}

#[test]
fn the_row_ceiling_is_the_row_ceiling_and_the_answer_below_it_still_bundles() {
    let answer: Vec<Value> = (0..10)
        .map(|index| station(&format!("s-{index}")))
        .collect();
    assert!(bundle(
        &json!(answer),
        &[],
        &dcat(),
        &descriptor(),
        &limits(10, u64::MAX)
    )
    .is_ok());
    let refused = bundle(
        &json!(answer),
        &[],
        &dcat(),
        &descriptor(),
        &limits(9, u64::MAX),
    )
    .expect_err("one over the ceiling");
    assert!(matches!(refused, BundleError::TooLarge(_)), "{refused}");
}

#[test]
fn the_byte_ceiling_is_measured_on_what_goes_in_and_refuses_before_the_archive_is_written() {
    // EP-44: the ceiling bounds the memory one download may claim, so it is counted on the
    // members rather than on the compressed archive, which is always smaller.
    let answer: Vec<Value> = (0..200)
        .map(|index| station(&format!("s-{index}")))
        .collect();
    let refused = bundle(
        &json!(answer),
        &[],
        &dcat(),
        &descriptor(),
        &limits(u32::MAX, 1024),
    )
    .expect_err("past the ceiling");
    assert!(matches!(refused, BundleError::TooLarge(_)), "{refused}");
    assert!(bundle(
        &json!(answer),
        &[],
        &dcat(),
        &descriptor(),
        &Limits::DEFAULT
    )
    .is_ok());
}

#[test]
fn a_schema_is_counted_towards_the_ceiling_like_the_data_is() {
    // The archive holds it, so it costs what it weighs: a bundle whose schemas are larger than
    // the ceiling is refused, not sent because the entities happened to fit.
    let heavy = vec![("index.json".to_owned(), vec![b'x'; 200_000])];
    let refused = bundle(
        &json!([station("s-1")]),
        &heavy,
        &dcat(),
        &descriptor(),
        &limits(u32::MAX, 100_000),
    )
    .expect_err("past the ceiling");
    assert!(matches!(refused, BundleError::TooLarge(_)), "{refused}");
}

#[test]
fn an_answer_with_no_geometry_carries_an_empty_collection_and_not_a_failure() {
    // A non-spatial dataset is ordinary. The bundle carries every shape, and the honest GeoJSON
    // of a table with no coordinates is a collection of nothing.
    let flat = json!([{
        "id": "urn:ngsi-ld:Obyvatelstvo:banskabystrica.sk:demografia:2026",
        "type": "Obyvatelstvo",
        "pocet": { "type": "Property", "value": 76000 }
    }]);
    let archive = bundle(&flat, &[], &dcat(), &descriptor(), &Limits::DEFAULT).expect("a bundle");
    let features: Value = serde_json::from_str(&text(&archive, "entities.geojson")).expect("json");
    assert_eq!(features["type"], json!("FeatureCollection"));
    assert_eq!(features["features"], json!([]));
    assert!(text(&archive, "entities.csv").contains("pocet"));
}

#[test]
fn the_manifest_repeats_the_query_verbatim_and_says_where_the_answer_came_from() {
    // A bundle found on a disk a year later has to say what it is. The query is repeated as the
    // caller sent it, so the same request can be made again.
    let odd = Bundle {
        query: "q=pm10>30;pm25<10&attrs=pm10,location",
        ..descriptor()
    };
    let archive = bundle(
        &json!([station("s-1")]),
        &[],
        &dcat(),
        &odd,
        &Limits::DEFAULT,
    )
    .expect("a bundle");
    let manifest: Value = serde_json::from_str(&text(&archive, "manifest.json")).expect("json");
    assert_eq!(
        manifest["query"],
        json!("q=pm10>30;pm25<10&attrs=pm10,location")
    );
    assert_eq!(manifest["space"], json!("ovzdusie"));
    assert_eq!(manifest["endpoint"], json!(SLUG));
    assert_eq!(manifest["exportedAt"], json!(EXPORTED));
}

#[test]
fn the_file_name_is_a_name_every_filesystem_keeps() {
    // EP-43: the day and not the instant, because a colon does not survive every disk a
    // download lands on, and the date is what sorts in a folder.
    assert_eq!(file_name(SLUG, EXPORTED), format!("{SLUG}-20260907.zip"));
    assert_eq!(
        file_name(SLUG, "2026-09-07"),
        format!("{SLUG}-20260907.zip")
    );
    assert_eq!(
        file_name(SLUG, ""),
        format!("{SLUG}-.zip"),
        "an instant nobody passed leaves the name without a day rather than inventing one"
    );
    for name in [
        file_name(SLUG, EXPORTED),
        file_name(SLUG, "2026-09-07T08:00:00+02:00"),
    ] {
        assert!(!name.contains(':'), "{name}");
        assert!(!name.contains('/'), "{name}");
    }
}

#[test]
fn the_three_shapes_hold_the_same_entities_as_each_other() {
    // One answer in one pass: the shapes cannot disagree, and a caller checking one against
    // another is checking the same projection twice.
    let answer = json!([station("s-1"), station("s-2")]);
    let archive = bundle(&answer, &[], &dcat(), &descriptor(), &Limits::DEFAULT).expect("a bundle");
    let entities: Value = serde_json::from_str(&text(&archive, "entities.jsonld")).expect("json");
    let features: Value = serde_json::from_str(&text(&archive, "entities.geojson")).expect("json");
    let csv = text(&archive, "entities.csv");
    assert_eq!(entities.as_array().map(Vec::len), Some(2));
    assert_eq!(features["features"].as_array().map(Vec::len), Some(2));
    assert_eq!(csv.lines().filter(|line| !line.is_empty()).count(), 3);
    for local in ["s-1", "s-2"] {
        assert!(csv.contains(local), "{csv}");
        assert!(serde_json::to_string(&features)
            .expect("json")
            .contains(local));
    }
}
