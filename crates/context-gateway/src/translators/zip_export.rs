//! One query as a bundle a colleague can open without the platform (T-0161, EP-41, EP-43,
//! EP-44, EP-51).
//!
//! The bundle holds the same entities in three shapes, the schemas that describe them and a
//! catalogue record that says where they came from. Every file is produced from one projected
//! answer in one pass, so the shapes cannot disagree with each other or with the same query
//! asked directly (Architecture/04 section 4a).
//!
//! The size limit is measured on the bytes that go in, not on the bytes that come out. A ZIP
//! central directory is written last, so the archive is assembled in memory rather than streamed
//! member by member, and an endpoint's `maxFileBytes` is therefore also the memory one download
//! may claim. Counting the input keeps that bound honest: compression can only make the answer
//! smaller than the ceiling, never larger.

use std::io::{Cursor, Write};

use serde_json::{json, Value};

use super::geojson;
use super::tabular::{self, Limits, TooLarge};

/// The media type of the answer.
pub const MEDIA_TYPE: &str = "application/zip";

/// What the bundle says about itself, from the endpoint and the request that asked for it.
#[derive(Debug, Clone, Copy)]
pub struct Bundle<'a> {
    /// The endpoint's slug, which names the archive and its top-level directory.
    pub slug: &'a str,
    /// The context space the entities came from.
    pub space: &'a str,
    /// The query string the caller sent, verbatim and without its leading `?`.
    pub query: &'a str,
    /// The export instant, RFC 3339 in UTC.
    pub exported_at: &'a str,
}

/// Why a bundle could not be produced.
#[derive(Debug, thiserror::Error)]
pub enum BundleError {
    /// The answer is larger than the endpoint allows to be downloaded at once (EP-44).
    #[error(transparent)]
    TooLarge(#[from] TooLarge),
    /// The archive could not be written, which for an in-memory writer means out of memory.
    #[error("the archive could not be written: {0}")]
    Io(#[from] zip::result::ZipError),
    /// A member could not be serialized, which means the projection produced something that is
    /// not JSON: a bug here, never a caller's doing.
    #[error("a bundle member is not serializable: {0}")]
    Encoding(#[from] serde_json::Error),
}

/// The name the browser saves the download as (EP-43).
///
/// The date and not the instant: a file called `air-quality-20260907.zip` sorts and reads, where
/// one with a colon in it does not survive every filesystem the download may land on.
pub fn file_name(slug: &str, exported_at: &str) -> String {
    let day: String = exported_at
        .chars()
        .take_while(|character| *character != 'T')
        .filter(char::is_ascii_digit)
        .collect();
    format!("{slug}-{day}.zip")
}

/// The whole bundle, ready to be sent.
///
/// `entities` is what the broker answered and the projection narrowed. `schemas` are the
/// artifacts of every type in it, already narrowed to the same grant (EP-47), each named by its
/// path under `schema/`, version segment included; the caller renders them through the same code the `schema/`
/// surface serves, so the bundle and the surface cannot describe the data differently. `dcat` is
/// the endpoint's own DCAT-AP record, whose distributions point at the live representations this
/// bundle is a snapshot of.
pub fn bundle(
    entities: &Value,
    schemas: &[(String, Vec<u8>)],
    dcat: &Value,
    bundle: &Bundle,
    limits: &Limits,
) -> Result<Vec<u8>, BundleError> {
    let table = tabular::table(entities, limits)?;
    let csv = tabular::csv(&table, limits)?;

    // A dataset with no geometry is not an error here, unlike a request that asked for GeoJSON
    // and nothing else: the bundle carries every shape, and an empty collection is the honest
    // shape of a non-spatial answer.
    let features = geojson::feature_collection(entities)
        .unwrap_or_else(|_| json!({ "type": "FeatureCollection", "features": [] }));

    let types = types_in(entities);
    let root = file_name(bundle.slug, bundle.exported_at)
        .strip_suffix(".zip")
        .unwrap_or(bundle.slug)
        .to_owned();

    let mut members: Vec<(String, Vec<u8>)> = vec![
        (
            format!("{root}/data/entities.jsonld"),
            serde_json::to_vec_pretty(entities)?,
        ),
        (
            format!("{root}/data/entities.geojson"),
            serde_json::to_vec_pretty(&features)?,
        ),
        (format!("{root}/data/entities.csv"), csv.into_bytes()),
    ];
    for (name, document) in schemas {
        members.push((format!("{root}/schema/{name}"), document.clone()));
    }
    members.push((
        format!("{root}/dcat.jsonld"),
        serde_json::to_vec_pretty(dcat)?,
    ));
    members.push((
        format!("{root}/manifest.json"),
        serde_json::to_vec_pretty(&manifest(bundle, &types, table.len()))?,
    ));

    let written: u64 = members.iter().map(|(_, body)| body.len() as u64).sum();
    if written > limits.max_bytes {
        return Err(BundleError::TooLarge(TooLarge));
    }

    let options = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);
    let mut archive = zip::ZipWriter::new(Cursor::new(Vec::new()));
    for (path, body) in &members {
        archive.start_file(path, options)?;
        archive.write_all(body).map_err(zip::result::ZipError::Io)?;
    }
    Ok(archive.finish()?.into_inner())
}

/// Every entity type in the answer, once each, in a stable order.
fn types_in(entities: &Value) -> Vec<String> {
    let mut types: Vec<String> = match entities {
        Value::Array(entities) => entities.iter().collect(),
        entity if entity.is_object() => vec![entity],
        _ => Vec::new(),
    }
    .iter()
    .filter_map(|entity| entity.get("type").and_then(Value::as_str))
    .map(str::to_owned)
    .collect();
    types.sort_unstable();
    types.dedup();
    types
}

/// What was asked for and what came back, so a bundle found on a disk still says what it is.
fn manifest(bundle: &Bundle, types: &[String], rows: usize) -> Value {
    json!({
        "endpoint": bundle.slug,
        "space": bundle.space,
        "query": bundle.query,
        "exportedAt": bundle.exported_at,
        "types": types,
        "rows": rows,
        "files": [
            "data/entities.jsonld",
            "data/entities.geojson",
            "data/entities.csv",
            "schema/",
            "dcat.jsonld",
        ],
    })
}
