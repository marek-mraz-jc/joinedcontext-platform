//! T-0528: `kind: Dashboard` and `kind: Layer` (UI-17, UI-18, MF-09).

use jc_core::error::Error;
use jc_core::kinds::grid::GridFormat;
use jc_core::kinds::{Dashboard, DashboardVisibility, Layer, LayerStyle};

const DASHBOARD: &str = include_str!("golden/025-Dashboard-10-dashboards-and-visualization.yaml");
const LAYER: &str = include_str!("golden/026-Layer-10-dashboards-and-visualization.yaml");

fn dashboard(replace: &str, with: &str) -> Result<Dashboard, Error> {
    let manifest = Dashboard::from_yaml(&DASHBOARD.replace(replace, with))
        .map_err(|e| Error::Parse(e.to_string()))?;
    manifest.validate()?;
    Ok(manifest)
}

fn layer(replace: &str, with: &str) -> Result<Layer, Error> {
    let manifest =
        Layer::from_yaml(&LAYER.replace(replace, with)).map_err(|e| Error::Parse(e.to_string()))?;
    manifest.validate()?;
    Ok(manifest)
}

fn reason(result: Result<impl std::fmt::Debug, Error>) -> String {
    match result {
        Err(Error::Name { field, reason, .. }) => format!("{field}: {reason}"),
        other => panic!("expected a Name error, got {other:?}"),
    }
}

#[test]
fn the_documented_dashboard_round_trips() {
    let dashboard = dashboard("", "").expect("the golden dashboard validates");
    assert_eq!(dashboard.spec.visibility, DashboardVisibility::Public);
    assert_eq!(dashboard.spec.title.get("en"), Some("Air Quality Overview"));
    assert_eq!(
        dashboard.spec.layer_refs(),
        vec!["air-quality-stations", "organisational-districts"]
    );
    assert_eq!(
        dashboard.spec.pages[1].widgets[0].widget_type,
        "temporal-chart"
    );
    let yaml = serde_norway::to_string(&dashboard).expect("serializes");
    assert_eq!(Dashboard::from_yaml(&yaml).expect("parses back"), dashboard);
}

#[test]
fn the_documented_layer_round_trips() {
    let layer = layer("", "").expect("the golden layer validates");
    assert_eq!(layer.spec.style, LayerStyle::Circle);
    assert!(layer.spec.visible);
    assert_eq!(
        layer.spec.filter.as_ref().and_then(|f| f.q.as_deref()),
        Some("pm10>0")
    );
    assert_eq!(
        layer.spec.color_by.as_ref().and_then(|c| c.domain),
        Some([0.0, 100.0])
    );
    assert_eq!(
        layer.spec.popup_properties,
        vec!["stationName", "pm10", "temperature"]
    );
    let yaml = serde_norway::to_string(&layer).expect("serializes");
    assert!(
        !yaml.contains("visible"),
        "the default is not written: {yaml}"
    );
    assert_eq!(Layer::from_yaml(&yaml).expect("parses back"), layer);
}

#[test]
fn visibility_and_style_default_to_the_narrow_choice() {
    let dashboard = dashboard(
        "  visibility: public        # private | project | organization | public\n",
        "",
    )
    .expect("validates");
    assert_eq!(dashboard.spec.visibility, DashboardVisibility::Private);
    let layer = layer(
        "  style: circle             # circle | heatmap | hexagon | icon | line | fill\n  visible: true\n",
        "",
    )
    .expect("validates");
    assert_eq!(layer.spec.style, LayerStyle::Circle);
    assert!(layer.spec.visible);
}

#[test]
fn a_dashboard_refuses_no_pages_an_empty_page_and_a_bad_layer_name() {
    let no_pages = dashboard("  pages:\n", "  pages: []\n  zz:\n");
    assert!(no_pages.is_err());
    let empty_page = DASHBOARD
        .lines()
        .take_while(|l| !l.starts_with("      layers:"))
        .collect::<Vec<_>>()
        .join("\n");
    let refused = Dashboard::from_yaml(&empty_page)
        .expect("parses")
        .validate();
    assert!(reason(refused).contains("at least one layer or widget"));
    assert!(reason(dashboard("- air-quality-stations", "- Air_Quality")).contains("metadata.name"));
    assert!(
        reason(dashboard("endpointRef: ep-air-quality", "endpointRef: EP")).contains("endpointRef")
    );
    assert!(
        reason(dashboard("widgetType: temporal-chart", "widgetType: ''")).contains("widgetType")
    );
}

#[test]
fn a_layer_refuses_a_bad_endpoint_type_range_or_popup() {
    assert!(reason(layer("ep-air-quality", "Ep Air")).contains("sourceEndpointRef"));
    assert!(reason(layer("AirQualityObserved", "airQualityObserved")).contains("entityType"));
    assert!(reason(layer("domain: [0, 100]", "domain: [100, 0]")).contains("colorBy.domain"));
    assert!(reason(layer("range: [4, 18]", "range: [-1, 18]")).contains("sizeBy.range"));
    assert!(reason(layer("- temperature", "- ''")).contains("popupProperties"));
    assert!(reason(layer("q: 'pm10>0'", "q: '  '")).contains("filter.q"));
}

#[test]
fn unknown_members_are_refused() {
    assert!(layer("  visible: true", "  visible: true\n  opacity: 0.5").is_err());
    assert!(dashboard(
        "  visibility: public",
        "  visibility: public\n  theme: dark"
    )
    .is_err());
}

/// T-1440, UI-71, SDK-30: a `grid` widget is the entity grid the explorer and a generated
/// application render. It names the Endpoint and the type it reads — without either it would open
/// on nothing — and its configuration is checked here as well as in the browser.
#[test]
fn a_grid_widget_names_its_endpoint_its_type_and_a_configuration_that_holds() {
    let golden = dashboard("", "").expect("the golden dashboard validates");
    let grid = &golden.spec.pages[1].widgets[1];
    assert_eq!(grid.widget_type, "grid");
    assert_eq!(grid.entity_type.as_deref(), Some("AirQualityObserved"));
    let config = grid.grid.as_ref().expect("a configuration");
    assert_eq!(config.page_size, Some(25));
    assert_eq!(config.columns[0].attr, "pm10");
    assert_eq!(config.columns[0].format, Some(GridFormat::Number));
    assert_eq!(config.history.as_ref().and_then(|h| h.enabled), Some(true));

    // What the same configuration is refused for, in a manifest as in a browser.
    assert!(reason(dashboard("pageSize: 25", "pageSize: 5000")).contains("between 1 and 1000"));
    assert!(reason(dashboard("pageSize: 25", "mode: edit")).contains("a correction"));
    assert!(
        reason(dashboard("          entityType: AirQualityObserved\n", ""))
            .contains("names the entity type")
    );
    assert!(reason(dashboard(
        "entityType: AirQualityObserved",
        "entityType: airQuality"
    ))
    .contains("PascalCase"));
    // The two fields belong to this widget type alone: a chart with a grid config is a mistake
    // nobody should have to debug in a browser.
    assert!(
        reason(dashboard("widgetType: grid", "widgetType: temporal-chart"))
            .contains("belong to a widget of type `grid`")
    );
}
