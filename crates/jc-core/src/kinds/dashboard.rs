//! `kind: Dashboard` and `kind: Layer`: the declarative dashboards of the Portal (T-0528,
//! UI-17, UI-18, MF-09, Architecture/10 §1).
//!
//! A dashboard is pages of layers and widgets; a layer is one Endpoint's entities of one type
//! with a style, its encodings and a static filter. Whether a public dashboard reads only
//! through public Endpoints (UI-19) is a cross-manifest check the Portal makes: a Layer alone
//! does not know its Endpoint's audience.

use crate::envelope::{Kind, ObjectMeta, Scope};
use crate::error::{Error, Result};
use crate::i18n::Text;
use crate::kinds::grid::GridConfig;
use crate::names;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Who may open the dashboard (UI-19); `private` when undeclared.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum DashboardVisibility {
    /// Its author.
    #[default]
    Private,
    /// The members of its project.
    Project,
    /// Every signed-in user of the organization.
    Organization,
    /// Anyone; every layer must then read through an `audience: public` Endpoint.
    Public,
}

/// A widget of an analytics page (Architecture/10 §1).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Widget {
    /// The widget, e.g. `temporal-chart` or `grid`.
    pub widget_type: String,
    /// The Endpoint it reads through.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint_ref: Option<String>,
    /// The entity it shows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entity_id: Option<String>,
    /// The property it shows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub property: Option<String>,
    /// The entity type a `grid` widget shows; the type decides its columns (UI-71).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entity_type: Option<String>,
    /// The configuration of a `grid` widget: the same object the explorer and a generated
    /// application are configured by, without the source and the type those two fields decide
    /// (SDK-30, T-1440).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grid: Option<GridConfig>,
}

/// The widget type of the entity grid.
pub const GRID_WIDGET: &str = "grid";

/// One page: a map of layers, or a grid of widgets, or both.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Page {
    /// Shown as the page's tab.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// `full-map`, `grid-2x2`, …; the Portal's default when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub layout: Option<String>,
    /// Names of `Layer` manifests of the same project, drawn bottom to top.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub layers: Vec<String>,
    /// The widgets of the page.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub widgets: Vec<Widget>,
}

/// `spec` of a Dashboard (UI-17).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct DashboardSpec {
    /// The title (UI-50): one string, or the legacy map per locale (PF-24).
    pub title: Text,
    /// Who may open it.
    #[serde(default)]
    pub visibility: DashboardVisibility,
    /// At least one page.
    pub pages: Vec<Page>,
}

impl Kind for DashboardSpec {
    const KIND: &'static str = "Dashboard";
    const PLURAL: &'static str = "dashboards";
    const SCOPE: Scope = Scope::Project;
    const PATH_TEMPLATE: &'static str = "projects/{project}/dashboards/{name}.yaml";

    fn validate_spec(&self, meta: &ObjectMeta) -> Result<()> {
        names::validate_dns1123_label(&meta.name)?;
        self.validate()
    }
}

impl DashboardSpec {
    /// Every page shows something, and every layer it names is a name a Layer can have.
    pub fn validate(&self) -> Result<()> {
        if self.pages.is_empty() {
            return Err(Error::Name {
                field: "spec.pages",
                value: String::new(),
                reason: "a dashboard has at least one page (UI-17)",
            });
        }
        for page in &self.pages {
            if page.layers.is_empty() && page.widgets.is_empty() {
                return Err(Error::Name {
                    field: "spec.pages[]",
                    value: page.title.clone().unwrap_or_default(),
                    reason: "a page names at least one layer or widget",
                });
            }
            for layer in &page.layers {
                names::validate_dns1123_label(layer).map_err(|_| Error::Name {
                    field: "spec.pages[].layers[]",
                    value: layer.clone(),
                    reason: "a layer is named by its manifest's `metadata.name`",
                })?;
            }
            for widget in &page.widgets {
                if widget.widget_type.trim().is_empty() {
                    return Err(Error::Name {
                        field: "spec.pages[].widgets[].widgetType",
                        value: String::new(),
                        reason: "a widget names its type, e.g. `temporal-chart`",
                    });
                }
                if let Some(endpoint) = &widget.endpoint_ref {
                    names::validate_dns1123_label(endpoint).map_err(|_| Error::Name {
                        field: "spec.pages[].widgets[].endpointRef",
                        value: endpoint.clone(),
                        reason: "an endpoint is named by its manifest's `metadata.name`",
                    })?;
                }
                // The entity grid reads one Endpoint's entities of one type, like a layer does:
                // without both it would open on nothing, and its configuration is checked by the
                // same rules the SDK's parser applies in the browser (UI-71, SDK-30, T-1440).
                if widget.widget_type == GRID_WIDGET {
                    if widget.endpoint_ref.is_none() {
                        return Err(Error::Name {
                            field: "spec.pages[].widgets[].endpointRef",
                            value: String::new(),
                            reason: "a grid widget names the Endpoint it reads through",
                        });
                    }
                    match &widget.entity_type {
                        Some(entity_type) => {
                            names::validate_entity_type(entity_type).map_err(|_| Error::Name {
                                field: "spec.pages[].widgets[].entityType",
                                value: entity_type.clone(),
                                reason: "an entity type is PascalCase, as NGSI-LD writes it",
                            })?;
                        }
                        None => {
                            return Err(Error::Name {
                                field: "spec.pages[].widgets[].entityType",
                                value: String::new(),
                                reason: "a grid widget names the entity type it shows",
                            });
                        }
                    }
                    if let Some(config) = &widget.grid {
                        config.validate("spec.pages[].widgets[].grid")?;
                    }
                } else if widget.entity_type.is_some() || widget.grid.is_some() {
                    return Err(Error::Name {
                        field: "spec.pages[].widgets[].widgetType",
                        value: widget.widget_type.clone(),
                        reason: "`entityType` and `grid` belong to a widget of type `grid`",
                    });
                }
            }
        }
        Ok(())
    }

    /// Every layer name any page draws, in page order, once each.
    pub fn layer_refs(&self) -> Vec<&str> {
        let mut seen = Vec::new();
        for name in self.pages.iter().flat_map(|p| p.layers.iter()) {
            if !seen.contains(&name.as_str()) {
                seen.push(name.as_str());
            }
        }
        seen
    }
}

/// How a layer draws (UI-20, UI-21): the three MapLibre styles and the aggregations only
/// deck.gl renders; `icon` draws as `circle` until the Portal has an icon renderer.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum LayerStyle {
    /// A point per entity.
    #[default]
    Circle,
    /// A line per entity.
    Line,
    /// A filled polygon per entity.
    Fill,
    /// A density surface over the points.
    Heatmap,
    /// Hexagonal bins over the points.
    Hexagon,
    /// A symbol per entity.
    Icon,
}

/// The static NGSI-LD filter every request of the layer carries (UI-18, UI-22).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct LayerFilter {
    /// The `q` expression, e.g. `pm10>0`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub q: Option<String>,
    /// The `scopeQ` expression, e.g. `/geo/FI/HKI/#`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope_q: Option<String>,
    /// A fixed `geoQ`; the map adds its own viewport bbox on top (UI-22).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub geo_q: Option<String>,
}

/// Colour by one numeric property over a domain (UI-18).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct ColorBy {
    /// The property read from every feature.
    pub property: String,
    /// A ColorBrewer ramp name, e.g. `YlOrRd`; the Portal's default when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub palette: Option<String>,
    /// `[min, max]` of the property mapped onto the ramp.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub domain: Option<[f64; 2]>,
}

/// Size by one numeric property over a pixel range (UI-18).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct SizeBy {
    /// The property read from every feature.
    pub property: String,
    /// `[min, max]` radius in pixels.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub range: Option<[f64; 2]>,
}

fn is_true(value: &bool) -> bool {
    *value
}

fn yes() -> bool {
    true
}

/// `spec` of a Layer (UI-18).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct LayerSpec {
    /// The Endpoint of the same project the layer reads through, by `metadata.name`.
    pub source_endpoint_ref: String,
    /// The NGSI-LD entity type it shows.
    pub entity_type: String,
    /// How it draws.
    #[serde(default)]
    pub style: LayerStyle,
    /// Drawn when the dashboard opens; `false` leaves it in the legend, switched off.
    #[serde(default = "yes", skip_serializing_if = "is_true")]
    pub visible: bool,
    /// The static filter.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filter: Option<LayerFilter>,
    /// The colour encoding.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color_by: Option<ColorBy>,
    /// The size encoding.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size_by: Option<SizeBy>,
    /// The properties a click shows, in order; the first few of the feature when empty.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub popup_properties: Vec<String>,
}

impl Kind for LayerSpec {
    const KIND: &'static str = "Layer";
    const PLURAL: &'static str = "layers";
    const SCOPE: Scope = Scope::Project;
    const PATH_TEMPLATE: &'static str = "projects/{project}/dashboards/{name}.yaml";

    fn validate_spec(&self, meta: &ObjectMeta) -> Result<()> {
        names::validate_dns1123_label(&meta.name)?;
        self.validate()
    }
}

fn ordered(field: &'static str, pair: &[f64; 2], at_least: f64) -> Result<()> {
    let [min, max] = *pair;
    if !(min.is_finite() && max.is_finite()) || min < at_least || min >= max {
        return Err(Error::Name {
            field,
            value: format!("[{min}, {max}]"),
            reason: "`[min, max]` with min below max, both finite and not negative",
        });
    }
    Ok(())
}

impl LayerSpec {
    /// The endpoint is a manifest name, the type is an entity type, the encodings' ranges are
    /// ordered, and every popup property and filter expression is non-empty.
    pub fn validate(&self) -> Result<()> {
        names::validate_dns1123_label(&self.source_endpoint_ref).map_err(|_| Error::Name {
            field: "spec.sourceEndpointRef",
            value: self.source_endpoint_ref.clone(),
            reason: "an endpoint is named by its manifest's `metadata.name`",
        })?;
        names::validate_entity_type(&self.entity_type).map_err(|_| Error::Name {
            field: "spec.entityType",
            value: self.entity_type.clone(),
            reason: "an entity type is PascalCase, e.g. `AirQualityObserved` (PF-42)",
        })?;
        if let Some(filter) = &self.filter {
            for (field, value) in [
                ("spec.filter.q", &filter.q),
                ("spec.filter.scopeQ", &filter.scope_q),
                ("spec.filter.geoQ", &filter.geo_q),
            ] {
                if value.as_deref().is_some_and(|v| v.trim().is_empty()) {
                    return Err(Error::Name {
                        field,
                        value: String::new(),
                        reason: "a filter expression is not empty; leave the member out",
                    });
                }
            }
        }
        if let Some(color) = &self.color_by {
            if color.property.trim().is_empty() {
                return Err(Error::Name {
                    field: "spec.colorBy.property",
                    value: String::new(),
                    reason: "the property the colour reads is named",
                });
            }
            if let Some(domain) = &color.domain {
                ordered("spec.colorBy.domain", domain, f64::NEG_INFINITY)?;
            }
        }
        if let Some(size) = &self.size_by {
            if size.property.trim().is_empty() {
                return Err(Error::Name {
                    field: "spec.sizeBy.property",
                    value: String::new(),
                    reason: "the property the size reads is named",
                });
            }
            if let Some(range) = &size.range {
                ordered("spec.sizeBy.range", range, 0.0)?;
            }
        }
        if self.popup_properties.iter().any(|p| p.trim().is_empty()) {
            return Err(Error::Name {
                field: "spec.popupProperties[]",
                value: String::new(),
                reason: "a popup property is a property name",
            });
        }
        Ok(())
    }
}
