//! NGSI-LD entities as a flat table, served as CSV (T-0157, EP-08, EP-44, EP-45).
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
use std::collections::HashMap;

/// The media type of the CSV answer, with the parameters RFC 7111 wants.
pub const CSV_MEDIA_TYPE: &str = "text/csv; charset=utf-8; header=present";

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

/// The table of one broker answer, refused when it holds more rows than allowed (EP-44).
///
/// `entities` is what the broker returned and the projection narrowed: an array, a single
/// entity, or nothing at all. An empty answer is an empty table with no columns, which is
/// a header-only CSV: nothing to show is not an error.
pub fn table(entities: &Value, limits: &Limits) -> Result<Table, TooLarge> {
    if entities_of(entities).len() as u64 > u64::from(limits.max_rows) {
        return Err(TooLarge);
    }
    let mut table = Table::default();
    table.append(entities);
    Ok(table)
}

/// The entities of an answer: an array, a single entity, or nothing at all.
fn entities_of(entities: &Value) -> Vec<&Value> {
    match entities {
        Value::Array(entities) => entities.iter().collect(),
        entity if entity.is_object() => vec![entity],
        _ => Vec::new(),
    }
}

impl Table {
    /// Adds the rows of `entities` (as [`table`] reads them), widening every row for a new column.
    /// A download appends each page and drops it: its parsed entities, about fifteen times their
    /// JSON, held whole put the gateway past its 1 Gi limit (T-4782).
    pub fn append(&mut self, entities: &Value) {
        let width = self.columns.len();
        // Each column's position by name, so placing a cell is a lookup rather than a scan of
        // the header, and each value moves into its row instead of being cloned there (T-4107).
        let mut positions: HashMap<String, usize> = self
            .columns
            .iter()
            .enumerate()
            .map(|(index, name)| (name.clone(), index))
            .collect();
        for entity in entities_of(entities) {
            let cells: Vec<(usize, Value)> = flatten(entity)
                .into_iter()
                .map(|(name, value)| {
                    let next = self.columns.len();
                    let index = *positions.entry(name).or_insert_with_key(|name| {
                        self.columns.push(name.clone());
                        next
                    });
                    (index, value)
                })
                .collect();
            let mut row = vec![Value::Null; self.columns.len()];
            // Reversed, so of two leaves on one path the first one written is the one kept.
            for (index, value) in cells.into_iter().rev() {
                row[index] = value;
            }
            self.rows.push(row);
        }
        if self.columns.len() > width {
            for row in &mut self.rows {
                row.resize(self.columns.len(), Value::Null);
            }
        }
    }
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
        // A typed literal (`{"@type": "DateTime", "@value": …}`) is its value: skipping its
        // `@` members left the attribute with no cell at all (T-3141).
        Value::Object(members) if members.contains_key("@value") => {
            walk(path, &members["@value"], cells)
        }
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
