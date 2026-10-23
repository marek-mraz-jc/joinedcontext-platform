//! The publisher's defaults of the catalogue widget, `spec.representations.widget` (API/11 EP-159,
//! EP-163): per model class, the map's geometry, colour field, popup fields and clustering. The
//! Portal edits them, the gateway hands them to the widget at `widget.json`, and the widget starts
//! its Mapa panel from them; a reader's own choice in the address still wins (EP-162). Presentation
//! only: the broker never reads this member, and nothing here narrows or widens what a caller reads.
//! A source's `table` sets where the widget's table starts (EP-164): a default filter, order, page
//! length and columns, ranked above the model's `view_*` annotations; the reader can remove each.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::LazyLock;

use regex::Regex;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_norway::Value;

use super::ical_mapping::{
    describe, fits, is_name, is_path, is_plain, is_slug, is_tag, resolve, Accepts, Model,
    MAX_OUTPUTS, MAX_SOURCES,
};

/// The most popup fields one class names.
pub const MAX_POPUP: usize = 10;

/// The most columns a class's `table` names.
pub const MAX_COLUMNS: usize = 30;

/// The page lengths the widget offers (DM-134).
pub const PAGE_LENGTHS: [u16; 4] = [10, 20, 50, 100];

/// One named output of `spec.representations.widget`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WidgetOutput {
    /// A slug: lowercase letters, digits and `-`, 1 to 40, unique on the Endpoint.
    pub name: String,
    /// The output's title per language tag.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub title: BTreeMap<String, String>,
    /// The output's description per language tag.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub description: BTreeMap<String, String>,
    /// The model classes it sets defaults for; 1 to 10, each class once.
    pub sources: Vec<WidgetSource>,
}

/// One class of a [`WidgetOutput`] and its map defaults.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WidgetSource {
    /// The model class, by its local name (`Event`).
    #[serde(rename = "type")]
    pub entity_type: String,
    /// The roles: which attribute the map reads for each.
    #[serde(default)]
    pub mapping: WidgetRoles,
    /// Whether markers cluster; absent: the widget's own rule (above 200 in view, EP-161).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cluster: Option<bool>,
    /// Where the table starts; absent: the model's `view_*` annotations (EP-164).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub table: Option<WidgetTable>,
}

/// The table defaults of a class (EP-164): what the reader sees first and can remove.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct WidgetTable {
    /// The default filter, an NGSI-LD `q` whose dates may be `today`, `today+7d`, `today-1d`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filter: Option<String>,
    /// The default order: one attribute, `;desc` for descending (`startDate`, `name;desc`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub order_by: Option<String>,
    /// Rows per page: 10, 20, 50 or 100.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub page_length: Option<u16>,
    /// The columns shown, in order; 1 to 30, each once.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub columns: Vec<String>,
}

impl WidgetTable {
    /// The attribute `order_by` sorts by, without its direction.
    pub fn order_attribute(&self) -> Option<&str> {
        self.order_by.as_deref().map(|o| {
            o.strip_suffix(";desc")
                .or_else(|| o.strip_suffix(";asc"))
                .unwrap_or(o)
        })
    }

    fn shape_gaps(&self, at: &str) -> Vec<String> {
        let mut gaps = Vec::new();
        if let Some(q) = &self.filter {
            if !is_plain(q, 1000) || antares_ql::parse_q(&dated(q)).is_err() {
                gaps.push(format!(
                    "{at}: `table.filter` is not an NGSI-LD q of at most 1000 characters, such as \
                     `startDate>=today` or `category==\"concert\"`"
                ));
            }
        }
        if let Some(path) = self.order_attribute().filter(|p| !is_path(p)) {
            gaps.push(format!(
                "{at}: `table.orderBy: {path}` is not an attribute name with an optional `;desc`, \
                 such as `startDate;desc`"
            ));
        }
        if let Some(n) = self.page_length.filter(|n| !PAGE_LENGTHS.contains(n)) {
            gaps.push(format!(
                "{at}: `table.pageLength: {n}` is not one of 10, 20, 50 or 100"
            ));
        }
        if self.columns.len() > MAX_COLUMNS {
            gaps.push(format!(
                "{at}: `table.columns` lists {} attributes, at most {MAX_COLUMNS}",
                self.columns.len()
            ));
        }
        if let Some(path) = self.columns.iter().find(|p| !is_path(p)) {
            gaps.push(format!(
                "{at}: `table.columns` names `{path}`, which is not an attribute name such as `name`"
            ));
        }
        let mut shown = BTreeSet::new();
        if let Some(twice) = self.columns.iter().find(|p| !shown.insert(p.as_str())) {
            gaps.push(format!("{at}: `table.columns` names `{twice}` twice"));
        }
        gaps
    }

    fn model_gaps(&self, model: &Model, ty: &str, at: &str) -> Vec<String> {
        let named = self
            .order_attribute()
            .map(|p| ("orderBy", p))
            .into_iter()
            .chain(self.columns.iter().map(|p| ("columns", p.as_str())));
        let mut gaps = Vec::new();
        for (member, path) in named {
            match resolve(model, ty, path) {
                Err(why) => gaps.push(format!("{at}: `table.{member}` names {why}")),
                Ok(slot) if fits(model, &slot, Accepts::Geo) => gaps.push(format!(
                    "{at}: `table.{member}` names `{path}`, a GeoProperty; the map draws it, the \
                     table shows values"
                )),
                Ok(_) => {}
            }
        }
        gaps
    }
}

/// The widget's relative dates (`today`, `today+7d`, `today-1d`), as its `relativeDates` reads them.
static TODAY: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\btoday(?:[+-]\d{1,3}d)?\b").expect("valid regex"));

/// `q` with each relative date read as a date, as the widget reads it (DM-134).
fn dated(q: &str) -> std::borrow::Cow<'_, str> {
    TODAY.replace_all(q, "2026-01-01")
}

/// The map roles of a class; an absent role keeps the widget's default (EP-161).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WidgetRoles {
    /// The GeoProperty drawn (`location`, `operationSpace` or a model GeoProperty).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub geometry: Option<String>,
    /// The enum or boolean attribute the markers are coloured and the legend is drawn by.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub colour: Option<String>,
    /// The attributes the popup shows, in order; 1 to 10.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub popup: Vec<String>,
}

/// What the widget starts its map from for one class (`widget.json`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MapDefaults {
    /// [`WidgetRoles::geometry`].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub geometry: Option<String>,
    /// [`WidgetRoles::colour`].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub colour: Option<String>,
    /// [`WidgetRoles::popup`].
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub popup: Vec<String>,
    /// [`WidgetSource::cluster`].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cluster: Option<bool>,
    /// [`WidgetSource::table`]: served beside the map, under `widget.json`'s `table` (EP-164).
    #[serde(skip)]
    pub table: Option<WidgetTable>,
}

/// The map defaults per class: the first output that lists a class sets its defaults.
pub fn map_defaults(outputs: &[WidgetOutput]) -> BTreeMap<String, MapDefaults> {
    let mut map = BTreeMap::new();
    for source in outputs.iter().flat_map(|o| &o.sources) {
        map.entry(source.entity_type.clone())
            .or_insert_with(|| MapDefaults {
                geometry: source.mapping.geometry.clone(),
                colour: source.mapping.colour.clone(),
                popup: source.mapping.popup.clone(),
                cluster: source.cluster,
                table: source.table.clone(),
            });
    }
    map
}

impl WidgetOutput {
    fn shape_gaps(&self) -> Vec<String> {
        let name = &self.name;
        let mut gaps = Vec::new();
        if !is_slug(name) {
            gaps.push(format!(
                "widget output `{name}`: `name` takes lowercase letters, digits and `-`, 1 to 40, \
                 such as `mapa`"
            ));
        }
        for (member, texts) in [("title", &self.title), ("description", &self.description)] {
            if texts
                .iter()
                .any(|(tag, text)| !is_tag(tag) || !is_plain(text, 1000))
            {
                gaps.push(format!(
                    "widget output `{name}`: `{member}` takes a language tag (`sk`, `en-GB`) to 1 \
                     to 1000 characters without control characters"
                ));
            }
        }
        if self.sources.is_empty() || self.sources.len() > MAX_SOURCES {
            gaps.push(format!(
                "widget output `{name}`: `sources` lists 1 to {MAX_SOURCES} classes"
            ));
        }
        let mut seen = BTreeSet::new();
        for source in &self.sources {
            let ty = &source.entity_type;
            if !is_name(ty) {
                gaps.push(format!(
                    "widget output `{name}`: `type: {ty}` is not an NGSI-LD type name"
                ));
                continue;
            }
            if !seen.insert(ty.as_str()) {
                gaps.push(format!(
                    "widget output `{name}` lists {ty} twice; one class has one set of defaults"
                ));
            }
            let roles = &source.mapping;
            for (role, path) in [("geometry", &roles.geometry), ("colour", &roles.colour)] {
                if let Some(path) = path.as_deref().filter(|p| !is_path(p)) {
                    gaps.push(format!(
                        "widget output `{name}`, {ty}: `mapping.{role}: {path}` is not an \
                         attribute name such as `category`"
                    ));
                }
            }
            if roles.popup.len() > MAX_POPUP {
                gaps.push(format!(
                    "widget output `{name}`, {ty}: `mapping.popup` lists {} attributes, at most \
                     {MAX_POPUP}",
                    roles.popup.len()
                ));
            }
            if let Some(path) = roles.popup.iter().find(|p| !is_path(p)) {
                gaps.push(format!(
                    "widget output `{name}`, {ty}: `mapping.popup` names `{path}`, which is not \
                     an attribute path such as `address.addressLocality`"
                ));
            }
            let mut shown = BTreeSet::new();
            if let Some(twice) = roles.popup.iter().find(|p| !shown.insert(p.as_str())) {
                gaps.push(format!(
                    "widget output `{name}`, {ty}: `mapping.popup` names `{twice}` twice"
                ));
            }
            if let Some(table) = &source.table {
                gaps.extend(table.shape_gaps(&format!("widget output `{name}`, {ty}")));
            }
        }
        gaps
    }

    fn model_gaps(&self, model: &Model) -> Vec<String> {
        let name = &self.name;
        let mut gaps = Vec::new();
        for source in &self.sources {
            let ty = &source.entity_type;
            if model.class(ty).is_none() {
                gaps.push(format!(
                    "widget output `{name}` lists the type {ty}, which the data model does not have"
                ));
                continue;
            }
            let roles = &source.mapping;
            if let Some(path) = &roles.geometry {
                match resolve(model, ty, path) {
                    Err(why) => gaps.push(format!(
                        "widget output `{name}`, {ty}: `mapping.geometry` names {why}"
                    )),
                    Ok(slot) if !fits(model, &slot, Accepts::Geo) => gaps.push(format!(
                        "widget output `{name}`, {ty}: `mapping.geometry` reads `{path}`, {}; \
                         geometry takes a GeoProperty",
                        describe(model, &slot)
                    )),
                    Ok(_) => {}
                }
            }
            if let Some(path) = &roles.colour {
                match resolve(model, ty, path) {
                    Err(why) => gaps.push(format!(
                        "widget output `{name}`, {ty}: `mapping.colour` names {why}"
                    )),
                    Ok(slot) if !colours(model, &slot) => gaps.push(format!(
                        "widget output `{name}`, {ty}: `mapping.colour` reads `{path}`, {}; \
                         colour takes an enum or a boolean",
                        describe(model, &slot)
                    )),
                    Ok(_) => {}
                }
            }
            for path in &roles.popup {
                match resolve(model, ty, path) {
                    Err(why) => gaps.push(format!(
                        "widget output `{name}`, {ty}: `mapping.popup` names {why}"
                    )),
                    Ok(slot) if fits(model, &slot, Accepts::Geo) => gaps.push(format!(
                        "widget output `{name}`, {ty}: `mapping.popup` names `{path}`, a \
                         GeoProperty; the map draws it, the popup shows values"
                    )),
                    Ok(_) => {}
                }
            }
            if let Some(table) = &source.table {
                gaps.extend(table.model_gaps(model, ty, &format!("widget output `{name}`, {ty}")));
            }
        }
        gaps
    }
}

/// An enum or a boolean Property: the values a legend can list.
fn colours(model: &Model, slot: &super::ical_mapping::Slot) -> bool {
    matches!(slot.kind.as_deref(), None | Some("Property"))
        && (slot.range == "boolean" || model.enum_values(&slot.range).is_some())
}

/// Every gap of an Endpoint's widget outputs, one sentence each naming the output and the member:
/// the count and names, each output's shape, then, with a `linkml` model, each role against it.
/// `Err` when the model does not parse.
pub fn widget_gaps(outputs: &[WidgetOutput], linkml: Option<&str>) -> Result<Vec<String>, String> {
    let mut gaps = Vec::new();
    if outputs.len() > MAX_OUTPUTS {
        gaps.push(format!(
            "`widget` lists {} outputs, at most {MAX_OUTPUTS}",
            outputs.len()
        ));
    }
    let mut seen = BTreeSet::new();
    for output in outputs {
        if !seen.insert(output.name.as_str()) {
            gaps.push(format!(
                "widget output `{}` is listed twice; each name is listed once",
                output.name
            ));
        }
        gaps.extend(output.shape_gaps());
    }
    if !gaps.is_empty() {
        return Ok(gaps);
    }
    if let Some(text) = linkml {
        let schema: Value = serde_norway::from_str(text).map_err(|err| err.to_string())?;
        let model = Model { schema: &schema };
        for output in outputs {
            gaps.extend(output.model_gaps(&model));
        }
    }
    Ok(gaps)
}
