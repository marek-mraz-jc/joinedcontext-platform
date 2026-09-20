//! NGSI-LD entities as a flat table, served as CSV and XLSX (T-0157, EP-08, EP-44, EP-45).
//!
//! The translation runs after the policy projection, never before, so a column can only
//! ever name an attribute the grant already allowed through: a second format is a second
//! way to read the same data, not a second set of rules (EP-06, EP-07).
//!
//! One entity is one row and one leaf value is one column, named by the dot-joined path
//! that reaches it (`temperature.value`, `location.value.coordinates[0]`); an array that
//! holds arrays or objects is one cell carrying its JSON. The structural
//! discriminator NGSI-LD puts on an attribute (`"type": "Property"`) is shape rather than
//! data and gets no column; the `type` of a GeoJSON geometry is data and keeps one. A
//! Relationship arrives as the target URN under `.object` (EP-08).
//!
//! Columns appear in the order the rows first mention them, so two runs over the same
//! answer produce the same header and the same bytes, which is what makes an `ETag` on a
//! download mean anything.

use serde_json::{Map, Value};
use std::io::{Cursor, Write};

/// The media type of the CSV answer, with the parameters RFC 7111 wants.
pub const CSV_MEDIA_TYPE: &str = "text/csv; charset=utf-8; header=present";

/// The media type of the XLSX answer.
pub const XLSX_MEDIA_TYPE: &str =
    "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet";

/// The `type` values NGSI-LD uses to say what shape an attribute has, per CIM 009 clause
/// 4.5. They describe the document, so they are not columns.
const ATTRIBUTE_TYPES: &[&str] = &[
    "Property",
    "GeoProperty",
    "Relationship",
    "LanguageProperty",
    "VocabProperty",
    "JsonProperty",
    "ListProperty",
];

/// The download crossed one of the endpoint's ceilings (EP-44).
///
/// A refusal rather than a short file: a truncated CSV looks exactly like a complete one,
/// and a caller that cannot tell will act on half the data.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("the answer is larger than this endpoint allows to be downloaded at once")]
pub struct TooLarge;

/// What one download may return (EP-44), from the endpoint manifest or the gateway's own
/// ceiling when the manifest declares neither.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// Rows the download may return.
    pub max_rows: u32,
    /// Bytes the download may return.
    pub max_bytes: u64,
}

impl Limits {
    /// The ceiling the gateway applies when the endpoint declares none.
    // ponytail: one pair of numbers, not a config file. An endpoint that needs another
    // pair writes them into its manifest, which is where every other limit lives.
    pub const DEFAULT: Self = Self {
        max_rows: 100_000,
        max_bytes: 64 * 1024 * 1024,
    };

    /// The endpoint's own ceilings, falling back to [`Limits::DEFAULT`] per field.
    pub fn of(declared: Option<&jc_core::kinds::FileLimits>) -> Self {
        let Some(declared) = declared else {
            return Self::DEFAULT;
        };
        Self {
            max_rows: declared.max_file_rows.unwrap_or(Self::DEFAULT.max_rows),
            max_bytes: declared.max_file_bytes.unwrap_or(Self::DEFAULT.max_bytes),
        }
    }
}

impl Default for Limits {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// The answer as a rectangle: a header and one row per entity, aligned to it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Table {
    /// The column names, in the order the rows first mentioned them.
    pub columns: Vec<String>,
    /// One row per entity; a cell the entity does not carry is [`Value::Null`].
    pub rows: Vec<Vec<Value>>,
}

impl Table {
    /// How many entities the table holds.
    pub fn len(&self) -> usize {
        self.rows.len()
    }

    /// Whether the answer had no entity at all.
    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }
}

/// The table of one broker answer, refused when it holds more rows than allowed (EP-44).
///
/// `entities` is what the broker returned and the projection narrowed: an array, a single
/// entity, or nothing at all. An empty answer is an empty table with no columns, which is
/// a header-only CSV: nothing to show is not an error.
pub fn table(entities: &Value, limits: &Limits) -> Result<Table, TooLarge> {
    let entities: Vec<&Value> = match entities {
        Value::Array(entities) => entities.iter().collect(),
        entity if entity.is_object() => vec![entity],
        _ => Vec::new(),
    };
    if entities.len() as u64 > u64::from(limits.max_rows) {
        return Err(TooLarge);
    }

    let mut columns: Vec<String> = Vec::new();
    let mut flattened: Vec<Vec<(String, Value)>> = Vec::with_capacity(entities.len());
    for entity in entities {
        let cells = flatten(entity);
        for (name, _) in &cells {
            if !columns.iter().any(|known| known == name) {
                columns.push(name.clone());
            }
        }
        flattened.push(cells);
    }

    let rows = flattened
        .into_iter()
        .map(|cells| {
            columns
                .iter()
                .map(|column| {
                    cells
                        .iter()
                        .find(|(name, _)| name == column)
                        .map_or(Value::Null, |(_, value)| value.clone())
                })
                .collect()
        })
        .collect();
    Ok(Table { columns, rows })
}

/// One entity flattened to its leaf values, in document order (EP-08).
///
/// `id` and `type` come first because a reader looks for them first, and because a table
/// whose first two columns move with the entity's key order is a table no two exports
/// agree on.
pub fn flatten(entity: &Value) -> Vec<(String, Value)> {
    let mut cells = Vec::new();
    let Some(members) = entity.as_object() else {
        return cells;
    };
    for key in ["id", "type"] {
        if let Some(value) = members.get(key).filter(|value| !value.is_object()) {
            cells.push((key.to_owned(), value.clone()));
        }
    }
    for (name, value) in members {
        if name == "id" || name == "type" || name.starts_with('@') {
            continue;
        }
        walk(name, value, &mut cells);
    }
    cells
}

/// Appends every leaf under `value` to `cells`, named by the path that reaches it.
fn walk(path: &str, value: &Value, cells: &mut Vec<(String, Value)>) {
    match value {
        Value::Object(members) if !members.is_empty() => {
            let structural = is_attribute(members);
            for (name, member) in members {
                if name.starts_with('@') || (structural && name == "type") {
                    continue;
                }
                walk(&format!("{path}.{name}"), member, cells);
            }
        }
        // Only an array of scalars spreads over indexed columns (a Point's two coordinates).
        // One holding arrays or objects, such as a polygon's rings, stays one JSON cell, so
        // no geometry widens the table past what a spreadsheet or a DataStore holds.
        Value::Array(items)
            if !items.is_empty()
                && !items.iter().any(|item| item.is_array() || item.is_object()) =>
        {
            for (index, item) in items.iter().enumerate() {
                walk(&format!("{path}[{index}]"), item, cells);
            }
        }
        // A scalar, an empty object, or an array that is not all scalars: the path ends here.
        leaf => cells.push((path.to_owned(), leaf.clone())),
    }
}

/// Whether this object is an NGSI-LD attribute, whose `type` says what shape it has
/// rather than what it holds.
fn is_attribute(members: &Map<String, Value>) -> bool {
    members
        .get("type")
        .and_then(Value::as_str)
        .is_some_and(|kind| ATTRIBUTE_TYPES.contains(&kind))
}

/// Rewrites the header for a human reader: `.value` drops off, and a unit code moves out
/// of its own column into the header of the value it measures (EP-45, DM-06).
///
/// The unit is read from the first row that carries one, because a `unitCode` describes
/// the attribute in the data model rather than the individual observation.
pub fn humanize(table: &mut Table) {
    let mut units: Vec<(String, String)> = Vec::new();
    for (index, column) in table.columns.iter().enumerate() {
        let Some(base) = column.strip_suffix(".unitCode") else {
            continue;
        };
        let code = table
            .rows
            .iter()
            .filter_map(|row| row.get(index))
            .find_map(|value| value.as_str())
            .unwrap_or_default();
        if !code.is_empty() {
            units.push((base.to_owned(), code.to_owned()));
        }
    }

    let dropped: Vec<usize> = table
        .columns
        .iter()
        .enumerate()
        .filter(|(_, column)| column.ends_with(".unitCode"))
        .map(|(index, _)| index)
        .collect();

    let mut columns = Vec::with_capacity(table.columns.len());
    for (index, column) in table.columns.iter().enumerate() {
        if dropped.contains(&index) {
            continue;
        }
        let stem = column.strip_suffix(".value").unwrap_or(column);
        // The unit was declared on the attribute, so it is the stem it belongs to:
        // `temperature.unitCode` describes `temperature.value`, not a column of its own.
        match units.iter().find(|(base, _)| base == stem) {
            Some((_, code)) => columns.push(format!("{stem} [{code}]")),
            None => columns.push(stem.to_owned()),
        }
    }
    for row in &mut table.rows {
        let mut index = 0;
        row.retain(|_| {
            let keep = !dropped.contains(&index);
            index += 1;
            keep
        });
    }
    table.columns = columns;
}

/// The table as RFC 4180 CSV, refused the moment it would cross the byte ceiling (EP-44).
pub fn csv(table: &Table, limits: &Limits) -> Result<String, TooLarge> {
    let mut out = String::new();
    write_row(&mut out, table.columns.iter().map(String::as_str));
    for row in &table.rows {
        let cells: Vec<String> = row.iter().map(cell).collect();
        write_row(&mut out, cells.iter().map(String::as_str));
        // Checked per row, so the answer is refused rather than sent half-written.
        if out.len() as u64 > limits.max_bytes {
            return Err(TooLarge);
        }
    }
    Ok(out)
}

/// One CSV record, with CRLF as RFC 4180 wants it.
fn write_row<'a>(out: &mut String, cells: impl Iterator<Item = &'a str>) {
    let mut first = true;
    for cell in cells {
        if !first {
            out.push(',');
        }
        first = false;
        out.push_str(&quoted(cell));
    }
    out.push_str("\r\n");
}

/// A field, quoted when it carries a comma, a quote, a newline or leading whitespace.
fn quoted(cell: &str) -> String {
    let needs =
        cell.contains([',', '"', '\n', '\r']) || cell.starts_with(' ') || cell.ends_with(' ');
    if !needs {
        return cell.to_owned();
    }
    format!("\"{}\"", cell.replace('"', "\"\""))
}

/// One cell as text: a string as it stands, anything else as its JSON.
fn cell(value: &Value) -> String {
    match value {
        Value::Null => String::new(),
        Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

/// The table as an Office Open XML workbook of two sheets: the data, and what produced it.
///
/// Written by hand rather than through a spreadsheet library, because the whole of what
/// is needed here is a grid of inline strings and numbers, and SpreadsheetML says how to
/// write one in a page. A JSON number becomes a numeric cell so a reader can sum a
/// measurement without retyping it; everything else is an inline string.
pub fn xlsx(
    table: &Table,
    metadata: &[(String, String)],
    limits: &Limits,
) -> Result<Vec<u8>, XlsxError> {
    use zip::write::SimpleFileOptions;
    use zip::CompressionMethod;

    let options = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
    let mut archive = zip::ZipWriter::new(Cursor::new(Vec::new()));

    for (name, body) in [
        ("[Content_Types].xml", CONTENT_TYPES.to_owned()),
        ("_rels/.rels", ROOT_RELS.to_owned()),
        ("xl/workbook.xml", WORKBOOK.to_owned()),
        ("xl/_rels/workbook.xml.rels", WORKBOOK_RELS.to_owned()),
        ("xl/worksheets/sheet1.xml", data_sheet(table, limits)?),
        ("xl/worksheets/sheet2.xml", metadata_sheet(metadata)),
    ] {
        archive.start_file(name, options)?;
        archive.write_all(body.as_bytes())?;
    }
    let workbook = archive.finish()?.into_inner();
    // The sheet is checked while it is written and the file itself when it is finished: the
    // one bounds what the gateway holds, the other what it would send (EP-44).
    match workbook.len() as u64 > limits.max_bytes {
        true => Err(XlsxError::TooLarge),
        false => Ok(workbook),
    }
}

/// Why no workbook was produced (T-0810).
#[derive(Debug, thiserror::Error)]
pub enum XlsxError {
    /// The workbook is past the endpoint's `maxFileBytes`. A download is refused, never
    /// truncated: a short file is indistinguishable from a complete one (EP-44).
    #[error("the workbook is larger than the endpoint allows")]
    TooLarge,
    /// The zip writer failed, which for an in-memory cursor means the machine did.
    #[error("the workbook does not serialize: {0}")]
    Io(#[from] std::io::Error),
    /// The archive itself could not be written.
    #[error("the workbook archive does not close: {0}")]
    Zip(#[from] zip::result::ZipError),
}

/// The `data` sheet: the header row, then one row per entity.
///
/// Checked per row like the CSV writer's, so a sheet past the ceiling is refused rather than
/// built whole and measured afterwards — the XML is the largest thing a workbook holds.
fn data_sheet(table: &Table, limits: &Limits) -> Result<String, XlsxError> {
    let mut rows = String::new();
    let header: Vec<Value> = table
        .columns
        .iter()
        .map(|column| Value::String(column.clone()))
        .collect();
    sheet_row(&mut rows, 1, &header);
    for (index, row) in table.rows.iter().enumerate() {
        sheet_row(&mut rows, index as u32 + 2, row);
        if rows.len() as u64 > limits.max_bytes {
            return Err(XlsxError::TooLarge);
        }
    }
    Ok(sheet(&rows))
}

/// The `metadata` sheet: one `key,value` pair per row, so the file says what produced it.
fn metadata_sheet(metadata: &[(String, String)]) -> String {
    let mut rows = String::new();
    for (index, (key, value)) in metadata.iter().enumerate() {
        sheet_row(
            &mut rows,
            index as u32 + 1,
            &[Value::String(key.clone()), Value::String(value.clone())],
        );
    }
    sheet(&rows)
}

/// A worksheet part around its rows.
fn sheet(rows: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><sheetData>{rows}</sheetData></worksheet>"#
    )
}

/// One worksheet row of cells, numbers as numbers and everything else as an inline string.
fn sheet_row(out: &mut String, number: u32, cells: &[Value]) {
    out.push_str(&format!(r#"<row r="{number}">"#));
    for (index, value) in cells.iter().enumerate() {
        let reference = format!("{}{number}", column_name(index));
        match value {
            Value::Null => continue,
            Value::Number(number) => {
                out.push_str(&format!(r#"<c r="{reference}"><v>{number}</v></c>"#));
            }
            other => out.push_str(&format!(
                r#"<c r="{reference}" t="inlineStr"><is><t xml:space="preserve">{}</t></is></c>"#,
                escaped(&cell(other))
            )),
        }
    }
    out.push_str("</row>");
}

/// The spreadsheet name of a zero-based column: `A`, `Z`, `AA`, `AB`, …
fn column_name(mut index: usize) -> String {
    let mut name = Vec::new();
    loop {
        name.push(b'A' + (index % 26) as u8);
        if index < 26 {
            break;
        }
        index = index / 26 - 1;
    }
    name.reverse();
    String::from_utf8(name).unwrap_or_else(|_| unreachable!("ASCII letters"))
}

/// XML text: the five predefined entities, and the control characters XML 1.0 forbids
/// dropped rather than emitted, because a workbook carrying one does not open at all.
fn escaped(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for character in text.chars() {
        match character {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            '\t' | '\n' | '\r' => out.push(character),
            control if control < ' ' || control == '\u{7f}' => {}
            plain => out.push(plain),
        }
    }
    out
}

const CONTENT_TYPES: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="xml" ContentType="application/xml"/><Override PartName="/xl/workbook.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml"/><Override PartName="/xl/worksheets/sheet1.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml"/><Override PartName="/xl/worksheets/sheet2.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml"/></Types>"#;

const ROOT_RELS: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="xl/workbook.xml"/></Relationships>"#;

const WORKBOOK: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><sheets><sheet name="data" sheetId="1" r:id="rId1"/><sheet name="metadata" sheetId="2" r:id="rId2"/></sheets></workbook>"#;

const WORKBOOK_RELS: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet1.xml"/><Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet2.xml"/></Relationships>"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn column_names_carry_past_z() {
        assert_eq!(column_name(0), "A");
        assert_eq!(column_name(25), "Z");
        assert_eq!(column_name(26), "AA");
        assert_eq!(column_name(27), "AB");
        assert_eq!(column_name(51), "AZ");
        assert_eq!(column_name(52), "BA");
        assert_eq!(column_name(701), "ZZ");
        assert_eq!(column_name(702), "AAA");
    }

    #[test]
    fn a_control_character_never_reaches_the_workbook() {
        assert_eq!(escaped("a\u{1}b"), "ab");
        assert_eq!(escaped("<a & b>"), "&lt;a &amp; b&gt;");
        assert_eq!(escaped("line\nbreak"), "line\nbreak");
    }
}
