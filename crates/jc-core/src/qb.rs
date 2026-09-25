//! RDF Data Cube: the Data Structure Definition a model may declare (DM-60, Architecture/03 §2).
//!
//! A DSD is an ordinary LinkML class annotated `qb_dsd: true`; its slots annotated
//! `qb_component: dimension` or `measure` are the cube's components. Model Tools checks the rules
//! and carries the roles into the JSON Schema as [`DSD_KEYWORD`] on the class and
//! [`COMPONENT_KEYWORD`] on each slot, which is what every reader of a projected schema keys on.
//! An observation is an ordinary entity of that class, one cell of the table.

use crate::error::{Error, Result};
use crate::names;
use serde_json::Value;

/// The JSON Schema keyword that marks a class definition as a Data Structure Definition.
pub const DSD_KEYWORD: &str = "x-qb-dsd";
/// The JSON Schema keyword that names a slot's role in the cube.
pub const COMPONENT_KEYWORD: &str = "x-qb-component";
/// What joins the parts of an observation's `{localId}`. Not `-`: an age band `15-19` carries
/// one, and two different cells must never share an id.
pub const LOCAL_ID_SEPARATOR: char = '~';

/// The role of one slot of a Data Structure Definition.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Component {
    /// A question the table is sliced by: district, age band, year (`qb:DimensionProperty`).
    Dimension,
    /// The number the cell holds (`qb:MeasureProperty`).
    Measure,
}

impl Component {
    /// The role one annotation or keyword value names, or `None` for any other word.
    pub fn parse(word: &str) -> Option<Self> {
        match word {
            "dimension" => Some(Component::Dimension),
            "measure" => Some(Component::Measure),
            _ => None,
        }
    }

    /// The role a projected slot definition carries, if it carries one.
    pub fn of(definition: &Value) -> Option<Self> {
        definition
            .get(COMPONENT_KEYWORD)
            .and_then(Value::as_str)
            .and_then(Self::parse)
    }
}

/// Whether a projected class definition is a Data Structure Definition.
pub fn is_dsd(definition: &Value) -> bool {
    definition.get(DSD_KEYWORD).and_then(Value::as_bool) == Some(true)
}

/// The `{localId}` of one observation: the DSD's class name and each dimension value, in the
/// DSD's order, joined by [`LOCAL_ID_SEPARATOR`] (Architecture/03 §2).
///
/// The same cell always gets the same id, so a pipeline that writes the table again updates
/// it rather than duplicating it. A dimension value that is itself an entity id contributes
/// that entity's own `{localId}`.
pub fn observation_local_id(dsd: &str, dimensions: &[&str]) -> Result<String> {
    names::validate_entity_type(dsd)?;
    if dimensions.is_empty() {
        return Err(Error::Name {
            field: "dimensions",
            value: dsd.to_owned(),
            reason: "an observation carries one value of every dimension of its DSD, and this one names none",
        });
    }
    let mut parts = vec![dsd.to_owned()];
    for value in dimensions {
        let part = match value.parse::<crate::urn::Urn>() {
            Ok(urn) => urn.local_id().to_owned(),
            Err(_) => (*value).to_owned(),
        };
        if part.is_empty() || part.contains(LOCAL_ID_SEPARATOR) {
            return Err(Error::Name {
                field: "dimension",
                value: (*value).to_owned(),
                reason: "a dimension value in an observation id is non-empty and carries no `~`, which separates the dimensions",
            });
        }
        parts.push(part);
    }
    let local_id = parts.join(&LOCAL_ID_SEPARATOR.to_string());
    names::validate_local_id(&local_id)?;
    Ok(local_id)
}
