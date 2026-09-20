//! The configuration of the entity grid (UI-71, SDK-30, T-1440).
//!
//! One serializable object describes the grid wherever it is placed: the Portal's data explorer,
//! a `kind: "grid"` view of an application `spec.json`, and a `grid` widget of a Dashboard. The
//! SDK's `parseGridConfig` is the copy a browser applies and this is the copy every manifest and
//! every generated specification is checked against, so a configuration refused in one place is
//! refused in the other (`sdk/grid.config.schema.json` is the published schema of the same shape).
//!
//! `source` and `type` are deliberately absent: whoever places the grid decides what it reads —
//! a widget's `endpointRef` and `entityType`, an application's own endpoint and the view's source
//! — so a configuration can never point the grid at another endpoint's data.

use crate::error::{Error, Result};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// One column of a `grid` view (SDK-30, UI-71).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct GridColumn {
    /// The attribute shown, which has to be one the source asked for.
    pub attr: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    /// The heading, in place of the attribute's own name.
    pub label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    /// The column's width in pixels.
    pub width: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    /// Whether the column stays in view while the grid scrolls sideways.
    pub pinned: Option<bool>,
    /// Which of the value's metadata columns start open.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub show: Option<GridColumnShow>,
    /// Whether this column takes a correction, in `mode: edit`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub editable: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    /// How the value is drawn, when the rows alone do not say.
    pub format: Option<GridFormat>,
}

/// Which metadata columns of one attribute start open beside its value.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct GridColumnShow {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    /// When the value was observed.
    pub observed_at: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    /// What the value is measured in.
    pub unit: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    /// Which dataset the instance belongs to.
    pub dataset_id: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    /// When the attribute was first written.
    pub created_at: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    /// When the attribute last changed.
    pub modified_at: Option<bool>,
}

/// How a cell is drawn when the loaded rows do not say by themselves.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum GridFormat {
    /// Plain text.
    Text,
    /// A number, aligned and formatted by the locale.
    Number,
    /// A timestamp.
    Date,
    /// A link the cell opens.
    Link,
}

/// Whether the grid takes a correction at all.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum GridMode {
    /// Read only.
    View,
    /// A correction may be typed into the columns `editableAttrs` names.
    Edit,
}

/// How much room a row takes.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum GridDensity {
    /// More rows on screen.
    Compact,
    /// More room per row.
    Comfortable,
}

/// Whether one attribute's history may be opened, and how much of it is read.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct GridHistory {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    /// Whether an attribute's history may be opened at all.
    pub enabled: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    /// Points one history read returns at most.
    pub max_points: Option<u32>,
}

/// What the grid may be filtered by, and what it is filtered by from the start.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct GridFilters {
    /// The columns whose filter row is offered; every filterable one when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allowed: Option<Vec<String>>,
    /// What the grid asks the endpoint for before anyone filters on screen.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preset: Option<GridPreset>,
}

/// The query the grid always asks the endpoint with, before anyone filters on screen.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct GridPreset {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    /// An NGSI-LD `q` the grid always asks with.
    pub q: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    /// The attributes asked for, when not every one of them.
    pub attrs: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    /// A regular expression the ids must match.
    pub id_pattern: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    /// A scope expression the entities must be in.
    pub scope_q: Option<String>,
}

/// The configuration of a `grid` view: `EntityGridConfig` of the SDK without the two fields the
/// application decides for it (SDK-30). It reads through the app's own endpoint and shows the type
/// of the view's source, so a spec can neither point the grid at another endpoint nor at another
/// type; everything else — the columns, the page size, the filters, the edit — is the spec's.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct GridConfig {
    /// The columns shown, in order; every attribute of the loaded rows when empty.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub columns: Vec<GridColumn>,
    /// Whether the entity's own `createdAt` and `modifiedAt` are columns too.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entity_timestamps: Option<bool>,
    /// What may be filtered, and what is filtered from the start.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filters: Option<GridFilters>,
    /// Entities per page, 1 to 1000; 50 when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub page_size: Option<u32>,
    /// Whether a correction may be typed into a cell; read only when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<GridMode>,
    /// The attributes a correction may be typed into, which `mode: edit` requires.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub editable_attrs: Vec<String>,
    /// Whether an attribute's history may be opened from its column.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub history: Option<GridHistory>,
    /// How much room a row takes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub density: Option<GridDensity>,
    /// The row actions the host offers, by name; the host renders them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub row_actions: Vec<String>,
}

impl GridConfig {
    /// The rules the SDK's parser applies too: a page size inside the bounds, and an edit mode
    /// that names what may be corrected. The attributes themselves are checked by whoever knows
    /// the schema of the type — the application spec against its source, the Portal against the
    /// endpoint's own projection.
    pub fn validate(&self, field: &'static str) -> Result<()> {
        if let Some(size) = self.page_size {
            if !(1..=MAX_PAGE_SIZE).contains(&size) {
                return Err(Error::Name {
                    field,
                    value: size.to_string(),
                    reason: "a page holds between 1 and 1000 entities",
                });
            }
        }
        if self.mode == Some(GridMode::Edit) && self.editable_attrs.is_empty() {
            return Err(Error::Name {
                field,
                value: "edit".to_owned(),
                reason: "an editable grid names the attributes a correction may be typed into",
            });
        }
        if let Some(points) = self.history.as_ref().and_then(|h| h.max_points) {
            if !(1..=MAX_HISTORY_POINTS).contains(&points) {
                return Err(Error::Name {
                    field,
                    value: points.to_string(),
                    reason: "a history holds between 1 and 1000 points",
                });
            }
        }
        Ok(())
    }
}

/// Entities per page, as `DEFAULT_PAGE_SIZE`/`MAX_PAGE_SIZE` of the SDK have them.
pub const MAX_PAGE_SIZE: u32 = 1000;
/// Points of one attribute's history, as `MAX_POINTS` of the SDK has it.
pub const MAX_HISTORY_POINTS: u32 = 1000;
