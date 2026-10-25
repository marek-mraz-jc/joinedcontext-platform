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
//! [`Table::settle`] fixes the columns before the bytes are written (T-5209): `id`, `type`, then
//! each attribute in the order of the model's slots with the columns its kind always has, then the
//! attributes the model does not name, sorted. A filter that narrows the rows, or leaves none,
//! keeps the header; the same answer gives the same bytes, which is what the download's `ETag`
//! (EP-43) relies on. A text cell a spreadsheet would run as a formula is made inert ([`csv`]).
//!
//! A download reads the broker one page at a time into a [`Collector`] (T-5507). The gateway
//! imports this crate: format code lives outside the policy edge (ADR-N-051), as the relationship
//! rules do in `jc-relationships` (T-4771).

mod collector;
pub use collector::{page_entities, BadWindow, Collect, Collector, Window};

use serde_json::{Map, Value};
use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};

/// The media type of the CSV answer, with the parameters RFC 4180 section 3 names.
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
    "ListRelationship",
];

/// The members only an NGSI-LD attribute holds beside its `type` (CIM 009 clause 4.5): an object
/// with a `type` and one of these is an attribute even of a kind this list does not know yet.
const ATTRIBUTE_MEMBERS: &[&str] = &[
    "value",
    "object",
    "languageMap",
    "vocab",
    "json",
    "valueList",
    "objectList",
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
    /// The column names, in the order the rows first mentioned them until [`Table::settle`].
    pub columns: Vec<String>,
    /// One row per entity; a cell the entity does not carry is [`Value::Null`].
    pub rows: Vec<Vec<Value>>,
    /// The ids already in `rows`: a page read at a shifted offset repeats none (T-5209).
    ids: HashSet<String>,
}

/// One attribute of the model as a column group (T-5209): its name, its NGSI-LD kind, whether
/// its value is one scalar, and whether the model gives it a unit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Slot {
    /// The attribute's name.
    pub name: String,
    /// `Property`, `Relationship`, `GeoProperty`, `LanguageProperty`, ...
    pub kind: String,
    /// Whether the value is a string, number or boolean rather than an object or a list.
    pub scalar: bool,
    /// Whether the model measures it in a unit, so it has a `unitCode`.
    pub unit: bool,
}

impl Slot {
    /// The columns this slot has in every download, whatever the rows hold. An object or a list
    /// value has none fixed: its columns are the paths the rows carry.
    // ponytail: a nested object the model leaves open still widens with the data; a model that
    // declares its members could fix those too.
    fn columns(&self) -> Vec<String> {
        let name = &self.name;
        match (self.kind.as_str(), self.scalar) {
            ("GeoProperty", _) => vec![
                format!("{name}.value.type"),
                format!("{name}.value.coordinates[0]"),
                format!("{name}.value.coordinates[1]"),
            ],
            ("Relationship", true) => vec![format!("{name}.object")],
            ("VocabProperty", true) => vec![format!("{name}.vocab")],
            ("Property", true) if self.unit => {
                vec![format!("{name}.value"), format!("{name}.unitCode")]
            }
            ("Property", true) => vec![format!("{name}.value")],
            _ => Vec::new(),
        }
    }
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
    /// Adds the rows of `entities` (an array, a single entity, or nothing), widening every row for a new column.
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
            if let Some(id) = entity.get("id").and_then(Value::as_str) {
                if !self.ids.insert(id.to_owned()) {
                    continue;
                }
            }
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

impl Table {
    /// Puts the columns in their fixed order and adds each column `slots` always has (T-5209):
    /// `id`, `type`, the slots' attributes in the model's order, then every other attribute by
    /// name; inside one attribute the slot's own columns first, then its paths in natural order
    /// (`[2]` before `[10]`).
    pub fn settle(&mut self, slots: &[Slot]) {
        for column in ["id".to_owned(), "type".to_owned()]
            .into_iter()
            .chain(slots.iter().flat_map(Slot::columns))
        {
            if !self.columns.contains(&column) {
                self.columns.push(column);
            }
        }
        let fixed: Vec<String> = slots.iter().flat_map(Slot::columns).collect();
        let key = |column: &str| {
            let attribute = column.split(['.', '[']).next().unwrap_or(column);
            let group = match column {
                "id" => (0, 0),
                "type" => (1, 0),
                _ => match slots.iter().position(|slot| slot.name == attribute) {
                    Some(rank) => (2, rank),
                    None => (3, 0),
                },
            };
            let place = fixed.iter().position(|name| name == column);
            (group, attribute.to_owned(), place.unwrap_or(usize::MAX))
        };
        let mut order: Vec<usize> = (0..self.columns.len()).collect();
        order.sort_by(|&a, &b| {
            let (left, right) = (&self.columns[a], &self.columns[b]);
            key(left)
                .cmp(&key(right))
                .then_with(|| natural(left, right))
        });
        self.columns = order.iter().map(|&i| self.columns[i].clone()).collect();
        for row in &mut self.rows {
            row.resize(self.columns.len(), Value::Null);
            let mut old = std::mem::take(row);
            *row = order.iter().map(|&i| old[i].take()).collect();
        }
    }
}

/// Compares two column names with each run of digits read as a number.
fn natural(left: &str, right: &str) -> Ordering {
    let (mut a, mut b) = (left.chars().peekable(), right.chars().peekable());
    loop {
        match (a.peek().copied(), b.peek().copied()) {
            (None, None) => return Ordering::Equal,
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(x), Some(y)) if x.is_ascii_digit() && y.is_ascii_digit() => {
                let run = |chars: &mut std::iter::Peekable<std::str::Chars>| {
                    let mut digits = String::new();
                    while let Some(digit) = chars.next_if(char::is_ascii_digit) {
                        digits.push(digit);
                    }
                    digits.trim_start_matches('0').to_owned()
                };
                let (x, y) = (run(&mut a), run(&mut b));
                let order = x.len().cmp(&y.len()).then_with(|| x.cmp(&y));
                if order != Ordering::Equal {
                    return order;
                }
            }
            (Some(x), Some(y)) => {
                if x != y {
                    return x.cmp(&y);
                }
                a.next();
                b.next();
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
        Value::Object(members) if !members.is_empty() => walk_members(path, members, cells),
        // Several instances of one attribute (CIM 009 4.5.5): the default instance keeps the
        // attribute's columns, each other one gets its own under `[datasetId=…]` (T-5209).
        Value::Array(items)
            if !items.is_empty()
                && items
                    .iter()
                    .all(|item| item.as_object().is_some_and(is_attribute)) =>
        {
            for members in items.iter().filter_map(Value::as_object) {
                match members.get("datasetId").and_then(Value::as_str) {
                    Some(dataset) => {
                        let mut members = members.clone();
                        members.remove("datasetId");
                        walk_members(&format!("{path}[datasetId={dataset}]"), &members, cells);
                    }
                    None => walk_members(path, members, cells),
                }
            }
        }
        // Only an array of scalars spreads over indexed columns (a Point's two coordinates).
        // One holding arrays or objects, such as a polygon's rings, stays one JSON cell, so
        // no geometry widens the table past what a spreadsheet holds.
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

/// The members of one object, each under its own path; an attribute's `type` gets no column.
fn walk_members(path: &str, members: &Map<String, Value>, cells: &mut Vec<(String, Value)>) {
    let structural = is_attribute(members);
    for (name, member) in members {
        if name.starts_with('@') || (structural && name == "type") {
            continue;
        }
        walk(&format!("{path}.{name}"), member, cells);
    }
}

/// Whether this object is an NGSI-LD attribute, whose `type` says what shape it has rather than
/// what it holds: a kind this crate names, or any `type` beside an attribute's own members.
fn is_attribute(members: &Map<String, Value>) -> bool {
    members
        .get("type")
        .and_then(Value::as_str)
        .is_some_and(|kind| {
            ATTRIBUTE_TYPES.contains(&kind)
                || ATTRIBUTE_MEMBERS
                    .iter()
                    .any(|member| members.contains_key(*member))
        })
}

/// A column name for a person (EP-45): the members NGSI-LD wraps a value in drop out, so
/// `address.value.addressCountry` reads `address.addressCountry` and `name.languageMap.sk`
/// reads `name.sk`.
fn readable(column: &str) -> String {
    let mut segments = column.split('.');
    let mut out = segments.next().unwrap_or_default().to_owned();
    for segment in segments {
        let (name, index) = segment.split_at(segment.find('[').unwrap_or(segment.len()));
        if ATTRIBUTE_MEMBERS.contains(&name) {
            out.push_str(index);
        } else {
            out.push('.');
            out.push_str(segment);
        }
    }
    out
}

/// Rewrites the header for a human reader: the `.value` wrappers drop off, and a
/// unit moves out of its own column into the header of the value it measures as the symbol a
/// person reads, `no2 [µg/m³]`, or the code when Rec 20 gives no symbol (EP-45, DM-06).
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
            let symbol = jc_core::units::lookup(code)
                .map(|unit| unit.symbol)
                .filter(|symbol| !symbol.is_empty())
                .unwrap_or(code);
            units.push((readable(base), symbol.to_owned()));
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
        let stem = readable(column);
        // The unit was declared on the attribute, so it is the stem it belongs to:
        // `temperature.unitCode` describes `temperature.value`, not a column of its own.
        match units.iter().find(|(base, _)| *base == stem) {
            Some((_, symbol)) => columns.push(format!("{stem} [{symbol}]")),
            None => columns.push(stem),
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

/// How the bytes are laid out for the program that opens them (T-5209).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Dialect {
    /// The field separator: `,` by default, `;` for a spreadsheet in a locale whose decimal
    /// mark is the comma (Slovak Excel), or a tab.
    pub delimiter: char,
    /// A UTF-8 byte order mark first, which Excel needs to read the text as UTF-8 at all.
    pub bom: bool,
}

/// A `delimiter` or `bom` parameter the download does not know.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct BadDialect(&'static str);

impl Dialect {
    /// The dialect a download asks for: `delimiter` `comma` (default), `semicolon` or `tab`;
    /// `bom` `true` or `false`, by default on for the spreadsheet audience of `humanHeaders=true`
    /// and off for a program, which would read the mark as part of the first column's name.
    pub fn of(delimiter: Option<&str>, bom: Option<&str>, human: bool) -> Result<Self, BadDialect> {
        let delimiter = match delimiter {
            None | Some("comma") => ',',
            Some("semicolon") => ';',
            Some("tab") => '\t',
            Some(_) => {
                return Err(BadDialect(
                    "delimiter must be comma, semicolon or tab, e.g. delimiter=semicolon",
                ))
            }
        };
        let bom = match bom {
            None => human,
            Some("true") => true,
            Some("false") => false,
            Some(_) => return Err(BadDialect("bom must be true or false, e.g. bom=true")),
        };
        Ok(Self { delimiter, bom })
    }
}

impl Default for Dialect {
    fn default() -> Self {
        Self {
            delimiter: ',',
            bom: false,
        }
    }
}

/// The table as RFC 4180 CSV, refused the moment it would cross the byte ceiling (EP-44).
pub fn csv(table: &Table, limits: &Limits, dialect: Dialect) -> Result<String, TooLarge> {
    let mut out = String::new();
    if dialect.bom {
        out.push('\u{feff}');
    }
    write_row(
        &mut out,
        table.columns.iter().map(String::as_str),
        dialect.delimiter,
    );
    for row in &table.rows {
        let cells: Vec<String> = row.iter().map(cell).collect();
        write_row(
            &mut out,
            cells.iter().map(String::as_str),
            dialect.delimiter,
        );
        // Checked per row, so the answer is refused rather than sent half-written.
        if out.len() as u64 > limits.max_bytes {
            return Err(TooLarge);
        }
    }
    Ok(out)
}

/// One CSV record, with CRLF as RFC 4180 wants it.
fn write_row<'a>(out: &mut String, cells: impl Iterator<Item = &'a str>, delimiter: char) {
    let mut first = true;
    for cell in cells {
        if !first {
            out.push(delimiter);
        }
        first = false;
        out.push_str(&quoted(cell, delimiter));
    }
    out.push_str("\r\n");
}

/// A field, quoted when it carries the delimiter, a quote, a newline or leading whitespace.
///
/// A field a spreadsheet would run as a formula, one starting with `=`, `+`, `-`, `@`, a tab or
/// a carriage return, gets a `'` in front and quotes around it (OWASP CSV Injection, T-5209):
/// the data comes from the web, and `=HYPERLINK(...)` in an event title must open as text. A
/// number such as `-12.5` stays a number.
fn quoted(cell: &str, delimiter: char) -> String {
    let formula = cell.starts_with(['=', '+', '-', '@', '\t', '\r'])
        && !cell.parse::<f64>().is_ok_and(f64::is_finite);
    let needs = formula
        || cell.contains([delimiter, '"', '\n', '\r'])
        || cell.starts_with(' ')
        || cell.ends_with(' ');
    if !needs {
        return cell.to_owned();
    }
    let inert = if formula { "'" } else { "" };
    format!("\"{inert}{}\"", cell.replace('"', "\"\""))
}

/// One cell as text: a string as it stands, anything else as its JSON.
fn cell(value: &Value) -> String {
    match value {
        Value::Null => String::new(),
        Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}
