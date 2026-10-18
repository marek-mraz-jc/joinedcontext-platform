//! The CKAN DataStore mirror of one Endpoint (T-0317, EP-65, EP-66).
//!
//! A DataStore table gives the catalogue a preview, a filtered API and a SQL surface over
//! the rows. It is a copy, and the Endpoint stays the source: the mirror is filled through
//! the Endpoint's own tabular representation, as an ordinary consumer holding the
//! Endpoint's grants, so a row can only ever carry what the policy set already allowed
//! through (EP-65, EP-66). Nothing here reads a broker or a database.
//!
//! Like [`super::ckan`], this module builds payloads and decides which action to call. It
//! opens no socket: a page of the tabular answer arrives as columns and rows, and the CKAN
//! API token lives in the [`CkanApi`] implementation that carries the request (EP-67).
//!
//! ## Refreshing without a second projection
//!
//! A notification says *which* entities changed and nothing else is taken from it. The
//! rows are re-read through the Endpoint, and an entity the Endpoint no longer answers for
//! — deleted, or narrowed out of the projection — is deleted from the table by the same
//! rule. One row producer, one projection: reading the changed entity out of the
//! notification body instead would put a second, unpoliced path into the catalogue.

use super::ckan::{CkanApi, CkanError};
use jc_core::display_order::{display_order, Display};
use jc_core::field_order;
use jc_core::kinds::ckan::CkanPublication;
use jc_core::kinds::Representation;
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};

/// The primary key of every mirrored table: the entity's own URN.
pub const PRIMARY_KEY: &str = "entity_id";

/// The tabular column the primary key is read from (EP-08).
pub const ID_COLUMN: &str = "id";

/// The resource view that shows a DataStore table as a sortable, searchable grid.
pub const GRID_VIEW: &str = "datatables_view";

/// Rows per `datastore_upsert` call. CKAN writes an upsert row by row in one transaction,
/// so one call for a whole large table outlived the client's request timeout on every
/// run: the table kept an old partial load and the grid view, made after the rows, never
/// came (T-2931). A batch this size finishes in seconds and keeps each body small.
pub const UPSERT_BATCH: usize = 500;

/// What one run did to the table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// The Endpoint declares no `datastore` block, so there is no mirror.
    NotMirrored,
    /// The table did not exist and was created.
    Created,
    /// The table existed and gained the fields this answer brought.
    Extended,
    /// The table already had every field of this answer.
    Unchanged,
}

/// What one refresh did to the rows.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Synced {
    /// Entity ids written into the table.
    pub upserted: Vec<String>,
    /// Entity ids removed, because the Endpoint no longer answers for them.
    pub deleted: Vec<String>,
}

/// Why the mirror could not be built or carried out.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum MirrorError {
    /// The tabular answer carries no `id` column, so no row has a primary key (EP-08).
    #[error("the tabular answer has no '{ID_COLUMN}' column, so a row has no {PRIMARY_KEY}")]
    NoIdColumn,
    /// A row does not line up with the header of the answer it came in.
    #[error("row {row}: {cells} cells for {columns} columns")]
    RaggedRow {
        /// Index of the row in the answer.
        row: usize,
        /// Cells the row carries.
        cells: usize,
        /// Columns the header declares.
        columns: usize,
    },
    /// CKAN refused or could not be reached.
    #[error(transparent)]
    Api(#[from] CkanError),
}

/// The mirror one Endpoint declares, or `None` when it declares none (EP-65).
pub fn mirrored(publication: &CkanPublication) -> Option<Representation> {
    publication
        .datastore
        .as_ref()
        .map(|datastore| datastore.representation)
}

/// The DataStore fields one tabular answer becomes, typed from the Endpoint's models.
///
/// The column set comes from the answer rather than from the models, because the columns
/// an Endpoint actually serves are shaped by the data: a `GeoProperty` flattens to
/// `location.value.coordinates[0]` and an array of scalars to one column per index, and a table
/// created from predicted names would hold empty fields the rows never fill (EP-08).
///
/// The models supply what they can supply reliably. A column whose attribute one of them
/// declares takes that attribute's type and its description, so `temperature.value` is a
/// number in CKAN because the model says so and not because this page happened to carry
/// numbers. A column no model declares — an open-world attribute (DM-28), a nested leaf —
/// is typed from the values in front of us and falls back to text.
pub fn fields(columns: &[String], rows: &[Vec<Value>], schemas: &[Value]) -> Vec<Value> {
    fields_by(columns, schemas, |index| observed(rows, index))
}

/// [`fields`], with the type of a column no model settles read by `kind_of` from the column's
/// index: a caller holding the table in another shape than typed rows reads it from there,
/// without a typed copy of the whole table (T-2968).
pub fn fields_by(
    columns: &[String],
    schemas: &[Value],
    kind_of: impl Fn(usize) -> String,
) -> Vec<Value> {
    let declared = declarations(schemas);
    columns
        .iter()
        .enumerate()
        .map(|(index, column)| {
            let id = field_name(column);
            let attribute = column.split(['.', '[']).next().unwrap_or(column);
            let declared = declared.get(attribute);
            let kind = declared
                .and_then(|declaration| declaration.datastore_type(column))
                .unwrap_or_else(|| kind_of(index));
            let mut field = json!({ "id": id, "type": kind });
            if let Some(notes) = declared.and_then(|declaration| declaration.description.clone()) {
                field["info"] = json!({ "notes": notes });
            }
            field
        })
        .collect()
}

/// The DataStore fields of one entity type's table, whose columns come from its data and its
/// model (T-3012).
///
/// `observed` is the type of a column read off the cells it holds, `None` for a column no row of
/// the table fills. The cells win where there are any: CKAN refuses a whole upsert over one cell
/// that does not parse as its field's type, and data written before a model tightened must not
/// stop the mirror. An empty column, one only the model brought, takes the model's type, so a
/// consumer sees the schema the rows will have. Every column of a declared attribute carries the
/// attribute's description.
pub fn table_fields(
    columns: &[String],
    definition: Option<&Value>,
    language: &str,
    observed: impl Fn(usize) -> Option<String>,
) -> Vec<Value> {
    let declared = definition
        .map(|definition| declarations(&[json!({ "definitions": { "_": definition } })]))
        .unwrap_or_default();
    columns
        .iter()
        .enumerate()
        .map(|(index, column)| {
            let declared = declared.get(stem(column));
            let kind = observed(index)
                .or_else(|| declared.and_then(|declaration| declaration.datastore_type(column)))
                .unwrap_or_else(|| "text".to_owned());
            let mut field = json!({
                "id": field_name(column),
                "type": kind,
                "info": { "label": field_order::label(column, language) },
            });
            if let Some(notes) = declared.and_then(|declaration| declaration.description.clone()) {
                field["info"]["notes"] = json!(notes);
            }
            field
        })
        .collect()
}

/// The order in which a table's columns are read and what each is called (T-4661, T-4672): the
/// indexes of `fields` in the display order of `entity_type` in `model` (the Endpoint's LinkML,
/// [`display_order`]) narrowed by the Endpoint's `display` entry, the columns of one attribute
/// together in the heuristic's order. A column whose attribute the model hides or the `display`
/// leaves out is not among them; the primary key always is. Each kept field's `info` gets the
/// model's label in `language`, its labels in every language the model names as `labels`, and
/// `summary` on a key attribute's columns (DM-99…DM-101, T-4648).
pub fn arrange(
    fields: &mut [Value],
    model: Option<&str>,
    entity_type: &str,
    view: Option<&Display>,
    language: &str,
) -> Vec<usize> {
    let numeric = |field: &Value| {
        matches!(
            field["type"].as_str(),
            Some("int" | "int4" | "int8" | "float" | "float8" | "numeric")
        )
    };
    let column = |field: &Value| -> String {
        let id = field["id"].as_str().unwrap_or_default();
        if id == PRIMARY_KEY { ID_COLUMN } else { id }.to_owned()
    };
    let mut attributes: Vec<(String, bool)> = Vec::new();
    for field in fields.iter() {
        let name = field_order::attribute(&column(field)).to_owned();
        match attributes.iter_mut().find(|(a, _)| *a == name) {
            Some((_, number)) => *number |= numeric(field),
            None => attributes.push((name, numeric(field))),
        }
    }
    let served: Vec<(&str, bool)> = attributes.iter().map(|(a, n)| (a.as_str(), *n)).collect();
    let short = short_type(entity_type);
    // The gateway serves a model that validated; one that does not parse is ordered as if none.
    let shown = display_order(model, short, &served, view)
        .or_else(|_| display_order(None, short, &served, view))
        .unwrap_or_default();
    let place = |field: &Value| {
        let column = column(field);
        let attribute = field_order::attribute(&column);
        shown
            .iter()
            .position(|f| f.slot == attribute)
            .map(|at| (at, field_order::rank(&column, numeric(field))))
    };
    let mut order: Vec<usize> = (0..fields.len())
        .filter(|&i| place(&fields[i]).is_some())
        .collect();
    order.sort_by_key(|&i| place(&fields[i]));
    if let Some(key) = fields.iter().position(|f| f["id"] == PRIMARY_KEY) {
        if !order.contains(&key) {
            order.push(key);
        }
    }
    // Every language the model labels the type in, for a page in another one (T-4648).
    let mut languages: BTreeSet<&str> = shown
        .iter()
        .flat_map(|f| f.labels.keys().map(String::as_str))
        .collect();
    languages.extend([language, "en"]);
    for &index in &order {
        let column = column(&fields[index]);
        let attribute = field_order::attribute(&column);
        let Some(shown) = shown.iter().find(|f| f.slot == attribute) else {
            continue;
        };
        if field_order::leaf(&column) == attribute {
            fields[index]["info"]["label"] = json!(shown.label(language));
            let labels: Map<String, Value> = languages
                .iter()
                .map(|lang| ((*lang).to_owned(), json!(shown.label(lang))))
                .collect();
            fields[index]["info"]["labels"] = Value::Object(labels);
        }
        if shown.summary {
            fields[index]["info"]["summary"] = json!(true);
        }
    }
    order
}

/// An entity type by its short name: the last segment of an expanded IRI.
pub fn short_type(entity_type: &str) -> &str {
    entity_type
        .rsplit(['/', '#', ':'])
        .next()
        .unwrap_or(entity_type)
}

/// The definition of `entity_type` in an Endpoint's JSON Schema (T-3012).
///
/// The schema keys a class by its short name (`$defs`, or `definitions` in an older document);
/// the rows may carry the type expanded, so an IRI is matched by its last segment.
pub fn definition<'a>(schema: &'a Value, entity_type: &str) -> Option<&'a Value> {
    let short = short_type(entity_type);
    ["$defs", "definitions"]
        .iter()
        .filter_map(|key| schema.get(key).and_then(Value::as_object))
        .find_map(|definitions| {
            definitions
                .get(entity_type)
                .or_else(|| definitions.get(short))
        })
}

/// The columns one entity type's model says its table carries, named as the gateway's tabular
/// answer names them (T-3012, EP-08).
///
/// A Relationship is `<attr>.object`; a LanguageProperty one `<attr>.languageMap.<lang>` per
/// language in `languages`; a GeoProperty its `value.type` and `value.coordinates`; a Property
/// `<attr>.value`, one column per member when the model spells out a structured value, and
/// `<attr>.unitCode` when the model gives a unit. `id` and `type` are the table's own columns
/// and are not listed.
pub fn model_columns(definition: &Value, languages: &[String]) -> Vec<String> {
    let Some(properties) = definition.get("properties").and_then(Value::as_object) else {
        return Vec::new();
    };
    let mut columns = Vec::new();
    for (name, property) in properties {
        if matches!(name.as_str(), "id" | "type") || name.starts_with('@') {
            continue;
        }
        match property.get("x-ngsi-ld-kind").and_then(Value::as_str) {
            Some("Relationship") => columns.push(format!("{name}.object")),
            Some("LanguageProperty") if languages.is_empty() => {
                columns.push(format!("{name}.languageMap"));
            }
            Some("LanguageProperty") => columns.extend(
                languages
                    .iter()
                    .map(|language| format!("{name}.languageMap.{language}")),
            ),
            Some("GeoProperty") => {
                columns.push(format!("{name}.value.type"));
                columns.push(format!("{name}.value.coordinates"));
            }
            Some("JsonProperty") => columns.push(format!("{name}.json")),
            Some("ListProperty") => columns.push(format!("{name}.valueList")),
            Some("VocabProperty") => columns.push(format!("{name}.vocab")),
            _ => {
                match property.get("properties").and_then(Value::as_object) {
                    Some(members) if !members.is_empty() => columns.extend(
                        members
                            .keys()
                            .map(|member| format!("{name}.value.{member}")),
                    ),
                    _ => columns.push(format!("{name}.value")),
                }
                if property.get("x-unit").is_some() {
                    columns.push(format!("{name}.unitCode"));
                }
            }
        }
    }
    columns
}

/// Whether `column` is already carried by `columns`: by itself, or by the leaves the gateway
/// spread it over (`location.value.coordinates` by `location.value.coordinates[0]`).
pub fn covered(columns: &[String], column: &str) -> bool {
    columns.iter().any(|known| {
        known == column
            || known
                .strip_prefix(column)
                .is_some_and(|rest| rest.starts_with(['.', '[']))
    })
}

/// The attributes `definition` declares that no column of `columns` carries: the completeness
/// check of one table against its model (T-3012). Empty when the table covers the model.
pub fn missing_attributes(definition: &Value, columns: &[String]) -> Vec<String> {
    let carried: BTreeSet<&str> = columns.iter().map(|column| stem(column)).collect();
    definition
        .get("properties")
        .and_then(Value::as_object)
        .map(|properties| {
            properties
                .keys()
                .filter(|name| !matches!(name.as_str(), "id" | "type") && !name.starts_with('@'))
                .filter(|name| !carried.contains(name.as_str()))
                .cloned()
                .collect()
        })
        .unwrap_or_default()
}

/// The attribute a column belongs to: `no2` of `no2.value`, `location` of
/// `location.value.coordinates[0]`.
fn stem(column: &str) -> &str {
    column.split(['.', '[']).next().unwrap_or(column)
}

/// The `datastore_create` payload of a table that does not exist yet.
///
/// `force` is set because the resource belongs to a dataset the publisher owns and CKAN
/// refuses to touch a DataStore table behind a resource otherwise.
pub fn table(package_id: &str, name: &str, fields: &[Value]) -> Value {
    json!({
        "resource": { "package_id": package_id, "name": name, "format": "CSV" },
        "fields": fields,
        "primary_key": [PRIMARY_KEY],
        "force": true,
    })
}

/// One page of the tabular answer as DataStore records (EP-65).
///
/// The `id` column becomes [`PRIMARY_KEY`] and every other column keeps its own name, so
/// an upsert of the same entity replaces its row rather than adding a second one. A null
/// cell is written as null rather than dropped: an attribute that stopped being answered
/// must clear the column, not keep yesterday's value.
pub fn records(columns: &[String], rows: &[Vec<Value>]) -> Result<Vec<Value>, MirrorError> {
    id_column(columns)?;
    rows.iter()
        .enumerate()
        .map(|(index, row)| record(columns, index, row.clone()))
        .collect()
}

/// Row `index` of the tabular answer as one DataStore record, as [`records`] writes it; the
/// cells are moved into the record, so a caller building one batch at a time holds no second
/// copy of the row (T-2968).
pub fn record(columns: &[String], index: usize, row: Vec<Value>) -> Result<Value, MirrorError> {
    let id = id_column(columns)?;
    if row.len() != columns.len() {
        return Err(MirrorError::RaggedRow {
            row: index,
            cells: row.len(),
            columns: columns.len(),
        });
    }
    let mut record = Map::new();
    for (column, cell) in columns.iter().zip(row) {
        record.insert(field_name(column), cell);
    }
    // A row whose id is not a string has no key CKAN can address it by.
    if !record
        .get(PRIMARY_KEY)
        .is_some_and(|value| value.as_str().is_some_and(|id| !id.is_empty()))
    {
        return Err(MirrorError::RaggedRow {
            row: index,
            cells: id,
            columns: columns.len(),
        });
    }
    Ok(Value::Object(record))
}

/// Where the `id` column is: a table without one has no row CKAN can address, however few
/// rows it has.
pub fn id_column(columns: &[String]) -> Result<usize, MirrorError> {
    columns
        .iter()
        .position(|column| column == ID_COLUMN)
        .ok_or(MirrorError::NoIdColumn)
}

/// The entity ids one NGSI-LD notification asks to be refreshed (EP-65, EP-44).
///
/// Only the ids are taken. What each entity now looks like is read back through the
/// Endpoint, because the notification body is not the Endpoint's tabular projection and
/// treating it as one would let a row into the catalogue by a path no policy narrowed.
pub fn touched(notification: &Value) -> Vec<String> {
    let entities = match notification.get("data") {
        Some(Value::Array(entities)) => entities.iter().collect(),
        Some(entity) if entity.is_object() => vec![entity],
        _ => Vec::new(),
    };
    entities
        .into_iter()
        .filter_map(|entity| entity.get("id")?.as_str())
        .map(str::to_owned)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

/// Creates the table, or adds the fields this answer brought to the table that exists.
///
/// Idempotent: an answer whose columns the table already holds makes no writing call at
/// all, so a publication run converges rather than churning the catalogue (CC-18). New fields
/// that come after every field the table holds are added; a retyped, reordered or relabelled
/// field rebuilds the table, which the caller then reloads in full.
pub fn ensure(
    api: &mut impl CkanApi,
    package_id: &str,
    name: &str,
    fields: &[Value],
) -> Result<(String, Outcome), MirrorError> {
    let Some(live) = api.show("resource_show", name)? else {
        let created = api.action("datastore_create", &table(package_id, name, fields))?;
        let id = resource_id(&created).unwrap_or_else(|| name.to_owned());
        return Ok((id, Outcome::Created));
    };
    let id = resource_id(&live).unwrap_or_else(|| name.to_owned());
    let live_fields: Vec<&Value> = live
        .get("fields")
        .and_then(Value::as_array)
        .map(|fields| fields.iter().collect())
        .unwrap_or_default();
    // A column whose type can no longer hold this answer's cells (a number column the
    // Endpoint now answers text for) would make CKAN refuse every upsert, and the table would
    // stay as it was, silently, while the Endpoint moved on (T-3113). The mirror is a full
    // reload, so the table is built again with this answer's types and refilled.
    let retyped = fields.iter().any(|field| {
        let wanted = field["type"].as_str().unwrap_or("text");
        live_fields
            .iter()
            .find(|live| live["id"] == field["id"])
            .and_then(|live| live["type"].as_str())
            .is_some_and(|held| !holds(held, wanted))
    });
    // The fields are in reading order with a label each (T-4661), and CKAN only appends a field
    // and keeps a label as first written, so a table whose columns stand in another order or
    // carry other labels or key flags (T-4672), or that would get a new field before one it
    // holds, is built again.
    // A field the table holds and this answer lacks is left where it is.
    let held = |id: &Value| live_fields.iter().any(|live| &live["id"] == id);
    let common_live: Vec<&Value> = live_fields
        .iter()
        .map(|live| &live["id"])
        .filter(|id| fields.iter().any(|field| &&field["id"] == id))
        .collect();
    let common_wanted: Vec<&Value> = fields
        .iter()
        .map(|field| &field["id"])
        .filter(|id| held(id))
        .collect();
    let last_held = fields.iter().rposition(|field| held(&field["id"]));
    let inserted = last_held.is_some_and(|last| !fields[..last].iter().all(|f| held(&f["id"])));
    let reordered = common_live != common_wanted || inserted;
    let relabelled = fields.iter().any(|field| {
        live_fields
            .iter()
            .find(|live| live["id"] == field["id"])
            .is_some_and(|live| {
                ["label", "labels", "summary"]
                    .iter()
                    .any(|key| live["info"][key] != field["info"][key])
            })
    });
    if retyped || reordered || relabelled {
        api.action(
            "datastore_delete",
            &json!({ "resource_id": id, "force": true }),
        )?;
        api.action(
            "datastore_create",
            &json!({
                "resource_id": id,
                "fields": fields,
                "primary_key": [PRIMARY_KEY],
                "force": true,
            }),
        )?;
        return Ok((id, Outcome::Created));
    }
    let known: BTreeSet<&str> = live_fields
        .iter()
        .filter_map(|f| f["id"].as_str())
        .collect();
    let missing: Vec<Value> = fields
        .iter()
        .filter(|field| !known.contains(field["id"].as_str().unwrap_or_default()))
        .cloned()
        .collect();
    if missing.is_empty() {
        return Ok((id, Outcome::Unchanged));
    }
    let mut payload = json!({ "resource_id": id, "fields": missing, "force": true });
    // A resource that exists without a table yet gets its table here, and a table without
    // a primary key cannot be upserted into.
    if known.is_empty() {
        payload["primary_key"] = json!([PRIMARY_KEY]);
    }
    api.action("datastore_create", &payload)?;
    Ok((id, Outcome::Extended))
}

/// Whether a column CKAN holds as `held` takes the cells of a column this answer types as
/// `wanted`: the same kind, a whole number into a decimal one, anything into text (the mirror
/// writes a text column's cells as text). CKAN names its types `int4`, `float8`, `numeric`…
fn holds(held: &str, wanted: &str) -> bool {
    fn kind(name: &str) -> &str {
        match name {
            "int" | "int2" | "int4" | "int8" | "integer" | "bigint" => "int",
            "float" | "float4" | "float8" | "numeric" | "double precision" => "float",
            "timestamp" | "timestamptz" => "timestamp",
            "json" | "jsonb" => "json",
            "bool" | "boolean" => "bool",
            other => other,
        }
    }
    let (held, wanted) = (kind(held), kind(wanted));
    held == wanted || held == "text" || (held == "float" && wanted == "int")
}

/// Writes the rows the Endpoint answered with and deletes the ones it did not (EP-65).
///
/// `requested` is what was asked for — the ids of a notification, or the ids of the page
/// being reloaded. An id that was requested and did not come back is an entity the
/// Endpoint no longer serves, whether it was deleted or narrowed out of the projection,
/// and its row leaves the table with it.
pub fn sync(
    api: &mut impl CkanApi,
    resource_id: &str,
    requested: &[String],
    records: &[Value],
) -> Result<Synced, MirrorError> {
    let upserted: Vec<String> = records
        .iter()
        .filter_map(|record| record[PRIMARY_KEY].as_str())
        .map(str::to_owned)
        .collect();
    for batch in records.chunks(UPSERT_BATCH) {
        upsert(api, resource_id, batch.to_vec())?;
    }
    let answered: BTreeSet<&str> = upserted.iter().map(String::as_str).collect();
    let deleted: Vec<String> = requested
        .iter()
        .filter(|id| !answered.contains(id.as_str()))
        .cloned()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    if !deleted.is_empty() {
        api.action(
            "datastore_delete",
            &json!({
                "resource_id": resource_id,
                "filters": { PRIMARY_KEY: deleted },
                "force": true,
            }),
        )?;
    }
    if !upserted.is_empty() || !deleted.is_empty() {
        touch(api, resource_id, None)?;
    }
    Ok(Synced { upserted, deleted })
}

/// Rows per `datastore_search` page when the mirror reads which entities a table holds, below
/// CKAN's default `ckan.datastore.search.rows_max` of 32000.
pub const ID_PAGE: usize = 10_000;

/// Entities per `datastore_delete`: the filter is one SQL `IN` list.
pub const DELETE_BATCH: usize = 1_000;

/// Removes every row of the table whose entity is not in `keep`, the ids a complete answer of
/// the Endpoint holds for this table's type (EP-65, T-3118). A full reload that only upserts
/// keeps every entity the Endpoint ever served: a vehicle that left the feed stayed in the
/// table, so the table no longer equalled the answer. Returns the ids removed.
///
/// Only for a complete answer: `file.csv` is the whole dataset or an error, never a page, so an
/// id it does not hold is an entity the Endpoint no longer serves.
pub fn prune(
    api: &mut impl CkanApi,
    resource_id: &str,
    keep: &BTreeSet<&str>,
) -> Result<Vec<String>, MirrorError> {
    let mut gone = Vec::new();
    let mut offset = 0;
    loop {
        let page = api.action(
            "datastore_search",
            &json!({
                "resource_id": resource_id,
                "fields": [PRIMARY_KEY],
                "sort": "_id",
                "limit": ID_PAGE,
                "offset": offset,
            }),
        )?;
        let records = page
            .get("records")
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or_default();
        gone.extend(
            records
                .iter()
                .filter_map(|record| record[PRIMARY_KEY].as_str())
                .filter(|id| !keep.contains(id))
                .map(str::to_owned),
        );
        if records.len() < ID_PAGE {
            break;
        }
        offset += ID_PAGE;
    }
    for batch in gone.chunks(DELETE_BATCH) {
        api.action(
            "datastore_delete",
            &json!({
                "resource_id": resource_id,
                "filters": { PRIMARY_KEY: batch },
                "force": true,
            }),
        )?;
    }
    Ok(gone)
}

/// One `datastore_upsert` of `batch`, at most [`UPSERT_BATCH`] records (EP-65).
///
/// The batch is moved into the payload rather than copied into it: a payload built with
/// `json!` from a borrowed batch holds the batch twice for as long as the request is sent.
pub fn upsert(
    api: &mut impl CkanApi,
    resource_id: &str,
    batch: Vec<Value>,
) -> Result<(), MirrorError> {
    let mut payload = json!({ "resource_id": resource_id, "method": "upsert", "force": true });
    payload["records"] = Value::Array(batch);
    api.action("datastore_upsert", &payload)?;
    Ok(())
}

/// The resource field that holds [`digest`] of the rows the mirror last wrote.
pub const ROWS_DIGEST: &str = "rows_sha256";

/// The hash of one record, which [`digest`] folds: 32 bytes a row, so a large table is not
/// held a second time to be compared (T-2968).
pub fn row_hash(record: &Value) -> [u8; 32] {
    Sha256::digest(record.to_string()).into()
}

/// One digest of a table's rows from their [`row_hash`]es, whatever order the Endpoint
/// answered them in, so a reload of unchanged rows is recognised as unchanged (EP-109).
pub fn digest(mut each: Vec<[u8; 32]>) -> String {
    each.sort_unstable();
    let mut whole = Sha256::new();
    for record in &each {
        whole.update(record);
    }
    format!("{:x}", whole.finalize())
}

/// Marks the table's resource as changed now, which moves its dataset's `metadata_modified` in
/// CKAN: `datastore_upsert` and `datastore_delete` change rows and leave the dataset's dates
/// alone, so a harvester polling `metadata_modified` would never see new rows (EP-109, T-3886).
/// `rows` is the [`digest`] of the rows now held, when the caller knows all of them.
pub fn touch(
    api: &mut impl CkanApi,
    resource_id: &str,
    rows: Option<&str>,
) -> Result<(), MirrorError> {
    let mut payload = json!({
        "id": resource_id,
        "last_modified": chrono::Utc::now().format("%Y-%m-%dT%H:%M:%S%.6f").to_string(),
    });
    if let Some(rows) = rows {
        payload[ROWS_DIGEST] = json!(rows);
    }
    api.action("resource_patch", &payload)?;
    Ok(())
}

/// Gives the table's resource the grid its dataset page opens in (EP-62, EP-65).
///
/// CKAN makes no view for a resource the DataStore API created, so without this the page
/// shows a download link and nothing to read. Idempotent: a resource that already has a
/// [`GRID_VIEW`], whoever made it, gets no second one, and a run over a mirror published
/// before this existed adds the one it lacks. Returns whether a view was created.
pub fn ensure_view(api: &mut impl CkanApi, resource_id: &str) -> Result<bool, MirrorError> {
    let views = api.show("resource_view_list", resource_id)?;
    let has_grid = views
        .as_ref()
        .and_then(Value::as_array)
        .is_some_and(|views| {
            views
                .iter()
                .any(|view| view.get("view_type").and_then(Value::as_str) == Some(GRID_VIEW))
        });
    if has_grid {
        return Ok(false);
    }
    api.action(
        "resource_view_create",
        &json!({ "resource_id": resource_id, "view_type": GRID_VIEW, "title": "Table" }),
    )?;
    Ok(true)
}

/// Removes the whole table an Endpoint no longer mirrors (EP-62, CC-19).
///
/// Removing what is not there succeeds: a publication run has to be re-runnable after a
/// partial failure.
pub fn drop_table(api: &mut impl CkanApi, resource_id: &str) -> Result<Outcome, MirrorError> {
    if api.show("resource_show", resource_id)?.is_none() {
        return Ok(Outcome::Unchanged);
    }
    api.action(
        "datastore_delete",
        &json!({ "resource_id": resource_id, "force": true }),
    )?;
    Ok(Outcome::NotMirrored)
}

/// The DataStore field name of one tabular column: `id` is the primary key, the rest keep
/// the name the Endpoint gave them.
fn field_name(column: &str) -> String {
    if column == ID_COLUMN {
        PRIMARY_KEY.to_owned()
    } else {
        column.to_owned()
    }
}

fn resource_id(value: &Value) -> Option<String> {
    value
        .get("resource_id")
        .or_else(|| value.get("id"))
        .and_then(Value::as_str)
        .map(str::to_owned)
}

/// What one model says about one attribute.
struct Declaration {
    kind: Option<String>,
    format: Option<String>,
    ngsi_ld_kind: Option<String>,
    description: Option<String>,
}

impl Declaration {
    /// The CKAN DataStore type of one column of this attribute, where the model settles it.
    ///
    /// Only the column that carries the attribute's own value is typed from the model.
    /// A deeper leaf (`location.value.coordinates[0]`) is a piece of a structure the model
    /// describes as a whole, so it is left to the values.
    fn conflicts(&self, other: &Self) -> bool {
        self.kind != other.kind
            || self.format != other.format
            || self.ngsi_ld_kind != other.ngsi_ld_kind
    }

    fn datastore_type(&self, column: &str) -> Option<String> {
        let leaf = column.rsplit('.').next().unwrap_or(column);
        if !matches!(leaf, "value" | "object") && column.contains(['.', '[']) {
            return None;
        }
        // A Relationship is a URN and a GeoProperty is a geometry, whatever the schema
        // draws around them.
        match self.ngsi_ld_kind.as_deref() {
            Some("Relationship") => return Some("text".to_owned()),
            Some("GeoProperty" | "JsonProperty" | "ListProperty" | "LanguageProperty") => {
                return Some("json".to_owned())
            }
            _ => {}
        }
        if self.format.as_deref() == Some("date-time") {
            return Some("timestamp".to_owned());
        }
        Some(
            match self.kind.as_deref()? {
                "string" => "text",
                "integer" => "int",
                "number" => "float",
                "boolean" => "bool",
                "object" | "array" => "json",
                _ => return None,
            }
            .to_owned(),
        )
    }
}

/// Every attribute the Endpoint's models declare, by name.
///
/// The models of one Endpoint describe different entity types, and an attribute two of
/// them declare differently is left to the values rather than typed from whichever model
/// was read first.
fn declarations(schemas: &[Value]) -> BTreeMap<String, Declaration> {
    let mut declared: BTreeMap<String, Option<Declaration>> = BTreeMap::new();
    for schema in schemas {
        let Some(definitions) = schema.get("definitions").and_then(Value::as_object) else {
            continue;
        };
        for definition in definitions.values() {
            let Some(properties) = definition.get("properties").and_then(Value::as_object) else {
                continue;
            };
            for (name, property) in properties {
                let declaration = declaration(property);
                match declared.get(name) {
                    None => {
                        declared.insert(name.clone(), Some(declaration));
                    }
                    Some(Some(known)) if known.conflicts(&declaration) => {
                        declared.insert(name.clone(), None);
                    }
                    Some(_) => {}
                }
            }
        }
    }
    declared
        .into_iter()
        .filter_map(|(name, declaration)| Some((name, declaration?)))
        .collect()
}

fn declaration(property: &Value) -> Declaration {
    Declaration {
        // A nullable attribute is `["string", "null"]`; the null is the absence, not a type.
        kind: match property.get("type") {
            Some(Value::String(kind)) => Some(kind.clone()),
            Some(Value::Array(kinds)) => kinds
                .iter()
                .filter_map(Value::as_str)
                .find(|kind| *kind != "null")
                .map(str::to_owned),
            _ => None,
        },
        format: property
            .get("format")
            .and_then(Value::as_str)
            .map(str::to_owned),
        ngsi_ld_kind: property
            .get("x-ngsi-ld-kind")
            .and_then(Value::as_str)
            .map(str::to_owned),
        description: property
            .get("description")
            .and_then(Value::as_str)
            .map(str::to_owned),
    }
}

/// The CKAN type of a column no model settles, read off the values this answer carries.
///
/// `text` is the fallback and the answer for a mixed column: CKAN rejects the whole upsert
/// when one cell does not parse as the field's type, and a mirror that refuses a page
/// because one entity carried a string where the others carried numbers is worse than a
/// column of text.
fn observed(rows: &[Vec<Value>], index: usize) -> String {
    let mut kind: Option<&'static str> = None;
    for row in rows {
        match row.get(index) {
            Some(Value::Null) | None => continue,
            Some(cell) => match fold(kind, cell) {
                Some(next) => kind = Some(next),
                None => return "text".to_owned(),
            },
        }
    }
    kind.unwrap_or("text").to_owned()
}

/// The CKAN type of one column from its cells, as [`fields`] reads it off typed rows; for a
/// caller that produces each cell as it goes and keeps none of them (T-2968).
pub fn observed_kind(cells: impl IntoIterator<Item = Value>) -> String {
    let mut kind: Option<&'static str> = None;
    for cell in cells {
        if cell.is_null() {
            continue;
        }
        match fold(kind, &cell) {
            Some(next) => kind = Some(next),
            None => return "text".to_owned(),
        }
    }
    kind.unwrap_or("text").to_owned()
}

/// The column's kind once `cell` is seen, from what it was before; `None` is a mixed column,
/// which is text.
fn fold(kind: Option<&'static str>, cell: &Value) -> Option<&'static str> {
    let observed = match cell {
        Value::Bool(_) => "bool",
        Value::Number(number) if number.is_i64() || number.is_u64() => "int",
        Value::Number(_) => "float",
        Value::Object(_) | Value::Array(_) => "json",
        Value::String(_) | Value::Null => "text",
    };
    match kind {
        None => Some(observed),
        Some(known) if known == observed => Some(known),
        // An int column that meets a float is a float column; anything else is text.
        Some(known) if matches!((known, observed), ("int", "float") | ("float", "int")) => {
            Some("float")
        }
        Some(_) => None,
    }
}
