//! `kind: UiSchema` — how the Portal arranges the form of another kind (T-0451, UI-02, MF-06).
//!
//! A `UiSchema` arranges a form; it never declares a field. Everything renderable comes from
//! the JSON Schema of the kind named in [`UiSchemaSpec::for_kind`], so this manifest only says
//! what order the fields come in, which widget draws each one, what help sits beside it and how
//! they are grouped. That is why nothing here has a type: a manifest cannot add a field, and a
//! field the manifest forgets is still rendered after the ones it names.
//!
//! The kind exists in the registry for a reason beyond tidiness. `jcctl`'s loader refuses a
//! manifest whose `kind` the registry does not know, and it refuses the whole repository rather
//! than the one file, so without this module the first `portal/forms/*.uischema.yaml` anybody
//! commits breaks `jcctl validate` for everything.

use crate::envelope::{Kind, ObjectMeta, Scope};
use crate::error::{Error, Result};
use crate::i18n::Text;
use crate::names;
use crate::registry;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

/// Widest and narrowest a field may be, in twelfths of the form row.
const COLUMNS: std::ops::RangeInclusive<u8> = 1..=12;

/// Desired specification of a [`UiSchema`][crate::kinds::UiSchema] resource (UI-02).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct UiSchemaSpec {
    /// The kind whose form this arranges, written as the manifest writes it: `Endpoint`.
    #[serde(rename = "for")]
    pub for_kind: String,
    /// The fields in the order the form shows them; the rest follow in schema order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub order: Vec<String>,
    /// Visual grouping of a flat schema, drawn as one fieldset each.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub groups: Vec<UiSchemaGroup>,
    /// Per-field arrangement, keyed by the field's name in the schema.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub fields: BTreeMap<String, UiSchemaField>,
}

/// One fieldset of a form (UI-02).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct UiSchemaGroup {
    /// The legend: one plain string, like every other human-facing string in a manifest
    /// (UI-50); the legacy language map still reads.
    pub title: Text,
    /// Optional sentence under the legend.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<Text>,
    /// The fields this group holds, in the order it holds them.
    pub fields: Vec<String>,
}

/// How one field is drawn (UI-02, CC-29).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct UiSchemaField {
    /// A widget the Portal has registered; an unknown one degrades to the default input.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub widget: Option<String>,
    /// Help text shown with the field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub help: Option<Text>,
    /// Placeholder text of an empty input.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub placeholder: Option<String>,
    /// Width in twelfths of the row, so `6` is half.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub columns: Option<u8>,
    /// Shown but not editable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub read_only: Option<bool>,
    /// Kept off the default form until the reader asks for the advanced mode (CC-29).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub advanced: Option<bool>,
}

impl Kind for UiSchemaSpec {
    const KIND: &'static str = "UiSchema";
    const PLURAL: &'static str = "uischemas";
    const SCOPE: Scope = Scope::Organization;
    const PATH_TEMPLATE: &'static str = "portal/forms/{name}.uischema.yaml";

    fn validate_spec(&self, meta: &ObjectMeta) -> Result<()> {
        names::validate_dns1123_label(&meta.name)?;
        // The spec first, so that a manifest whose `for` is a typo is told about the typo
        // rather than about the name it derives from it.
        self.validate()?;
        // The path a reader predicts from the kind has to be the path the file is at, and
        // `{name}` is the only placeholder this template has. A label carries no capitals, so
        // the name is the kind lowercased rather than the kind itself
        // (Architecture/09-portal.md, Development/04-manifest-kinds.md).
        if meta.name != self.for_kind.to_lowercase() {
            return Err(Error::Name {
                field: "metadata.name",
                value: meta.name.clone(),
                reason: "a UiSchema is named after the kind it arranges, lowercased, \
                         so that portal/forms/{name}.uischema.yaml is where the Portal looks",
            });
        }
        Ok(())
    }
}

impl UiSchemaSpec {
    /// Validates the target kind, the column counts and the grouping (UI-02).
    pub fn validate(&self) -> Result<()> {
        // A form for a kind that does not exist arranges nothing, and the mistake is a typo in
        // a file a person edits, so it is worth saying at `jcctl validate` rather than in a
        // browser that renders an empty dialog.
        if registry::by_kind(&self.for_kind).is_none() {
            return Err(Error::Name {
                field: "spec.for",
                value: self.for_kind.clone(),
                reason: "spec.for must name a kind this platform has, written as the manifest \
                         writes it",
            });
        }

        for (name, field) in &self.fields {
            if let Some(columns) = field.columns {
                if !COLUMNS.contains(&columns) {
                    return Err(Error::Name {
                        field: "spec.fields.columns",
                        value: format!("{name}: {columns}"),
                        reason: "a column count is a twelfth of the row and lies in 1..=12",
                    });
                }
            }
        }

        // A field in two groups would be drawn twice or dropped, depending on which fieldset
        // the renderer reaches first, and neither is what the manifest says.
        let mut seen = BTreeSet::new();
        for group in &self.groups {
            for field in &group.fields {
                if !seen.insert(field.as_str()) {
                    return Err(Error::Name {
                        field: "spec.groups.fields",
                        value: field.clone(),
                        reason: "a field belongs to one group; this one is named by two",
                    });
                }
            }
        }
        Ok(())
    }
}
