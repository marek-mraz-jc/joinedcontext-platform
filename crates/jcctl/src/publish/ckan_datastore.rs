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
use jc_core::kinds::ckan::CkanPublication;
use jc_core::kinds::Representation;
use serde_json::{json, Map, Value};
use std::collections::{BTreeMap, BTreeSet};

/// The primary key of every mirrored table: the entity's own URN.
pub const PRIMARY_KEY: &str = "entity_id";

/// The tabular column the primary key is read from (EP-08).
pub const ID_COLUMN: &str = "id";

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
                .unwrap_or_else(|| observed(rows, index));
            let mut field = json!({ "id": id, "type": kind });
            if let Some(notes) = declared.and_then(|declaration| declaration.description.clone()) {
                field["info"] = json!({ "notes": notes });
            }
            field
        })
        .collect()
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
    let id = columns
        .iter()
        .position(|column| column == ID_COLUMN)
        .ok_or(MirrorError::NoIdColumn)?;
    rows.iter()
        .enumerate()
        .map(|(index, row)| {
            if row.len() != columns.len() {
                return Err(MirrorError::RaggedRow {
                    row: index,
                    cells: row.len(),
                    columns: columns.len(),
                });
            }
            let mut record = Map::new();
            for (column, cell) in columns.iter().zip(row) {
                record.insert(field_name(column), cell.clone());
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
        })
        .collect()
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
/// all, so a publication run converges rather than churning the catalogue (CC-18). A field
/// is only ever added — dropping one would delete a column of data because an Endpoint
/// answered one page without it.
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
    let known: BTreeSet<&str> = live
        .get("fields")
        .and_then(Value::as_array)
        .map(|fields| fields.iter().filter_map(|f| f["id"].as_str()).collect())
        .unwrap_or_default();
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
    if !records.is_empty() {
        api.action(
            "datastore_upsert",
            &json!({
                "resource_id": resource_id,
                "method": "upsert",
                "records": records,
                "force": true,
            }),
        )?;
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
    Ok(Synced { upserted, deleted })
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
    let mut kind: Option<&str> = None;
    for row in rows {
        let cell = match row.get(index) {
            Some(Value::Null) | None => continue,
            Some(cell) => cell,
        };
        let observed = match cell {
            Value::Bool(_) => "bool",
            Value::Number(number) if number.is_i64() || number.is_u64() => "int",
            Value::Number(_) => "float",
            Value::Object(_) | Value::Array(_) => "json",
            Value::String(_) | Value::Null => "text",
        };
        kind = match kind {
            None => Some(observed),
            Some(known) if known == observed => Some(known),
            // An int column that meets a float is a float column; anything else is text.
            Some(known) if matches!((known, observed), ("int", "float") | ("float", "int")) => {
                Some("float")
            }
            Some(_) => return "text".to_owned(),
        };
    }
    kind.unwrap_or("text").to_owned()
}
