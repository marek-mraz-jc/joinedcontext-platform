//! T-4642 (ADR-N-053, EP-130): an Endpoint's `spec.catalog.visualizations` is a Vega-Lite 5 or
//! Vega 5 spec over one of its own sources; every refusal names the JSON pointer and the rule.

use jc_core::kinds::endpoint::Representation;
use jc_core::kinds::visualization::{check_spec, template, validate_list, Visualization};
use jc_core::kinds::Endpoint;
use jc_core::Error;
use serde_json::{json, Value};
use std::collections::BTreeMap;

/// The weather Endpoint of Architecture/04 §3a, with both of its charts.
const WEATHER: &str = r#"apiVersion: joinedcontext.com/v1alpha1
kind: Endpoint
metadata:
  name: weather-public
  namespace: banskabystrica
spec:
  contextSpaceRef: banskabystrica-verejne
  slug: zt4qm7ge2xdv6ksb3ncf5arw2y
  audience: public
  catalog:
    license: CC_BY_4_0
    visualizations:
      - name: temperature-now
        title: { sk: Teplota na staniciach teraz, en: Temperature at the stations now }
        description: { en: The latest air temperature at each SHMÚ station. }
        data:
          source: odata
          entitySet: WeatherObserved
          $select: name,temperature,dateObserved
          $filter: temperature ne null
          $orderby: temperature desc
        spec:
          $schema: https://vega.github.io/schema/vega-lite/v5.json
          data: { name: source }
          mark: bar
          encoding:
            y: { field: name, type: nominal, sort: "-x", title: null }
            x: { field: temperature, type: quantitative, title: "°C" }
            tooltip:
              - { field: name, type: nominal }
              - { field: temperature, type: quantitative, format: ".1f" }
              - { field: dateObserved, type: temporal, timeUnit: hoursminutes }
      - name: temperature-24h
        title: { en: Temperature over the last 24 hours }
        description: { en: Hourly mean air temperature per station. }
        data:
          source: temporal
          type: WeatherObserved
          attrs: [temperature]
          last: PT24H
          aggrMethod: avg
          aggrPeriod: PT1H
        spec:
          $schema: https://vega.github.io/schema/vega-lite/v5.json
          data: { name: source }
          params:
            - name: station
              select: { type: point, fields: [name] }
              bind: legend
          mark: { type: line, point: true }
          encoding:
            x: { field: observedAt, type: temporal, title: null }
            y: { field: temperature, type: quantitative, title: "°C", scale: { zero: false } }
            color: { field: name, type: nominal, title: null }
            opacity:
              condition: { param: station, value: 1 }
              value: 0.2
"#;

const ALL: &[Representation] = &[
    Representation::NgsiLd,
    Representation::Csv,
    Representation::OgcFeatures,
    Representation::Odata,
];

fn item(spec: Value) -> Visualization {
    serde_json::from_value(json!({
        "name": "chart",
        "title": { "en": "A chart" },
        "description": { "en": "What it shows." },
        "data": { "source": "csv", "type": "WeatherObserved" },
        "spec": spec,
    }))
    .expect("an item")
}

fn bar(extra: Value) -> Value {
    let mut spec = json!({
        "$schema": "https://vega.github.io/schema/vega-lite/v5.json",
        "data": { "name": "source" },
        "mark": "bar",
        "encoding": { "x": { "field": "a", "type": "nominal" }, "y": { "field": "b", "type": "quantitative" } }
    });
    merge(&mut spec, extra);
    spec
}

fn merge(into: &mut Value, extra: Value) {
    if let (Value::Object(a), Value::Object(b)) = (into, extra) {
        for (k, v) in b {
            match a.get_mut(&k) {
                Some(slot @ Value::Object(_)) if v.is_object() => merge(slot, v),
                _ => {
                    a.insert(k, v);
                }
            }
        }
    }
}

/// The refused field and rule of a list of one item.
fn refused(items: &[Visualization], served: &[Representation]) -> (String, String) {
    match validate_list(items, served) {
        Err(Error::Invalid { field, reason }) => {
            let rule = reason.split(':').next().unwrap_or_default().to_owned();
            (field, rule)
        }
        other => panic!("expected a refusal, got {other:?}"),
    }
}

fn refused_spec(spec: Value) -> (String, String) {
    refused(&[item(spec)], ALL)
}

const AT: &str = "/spec/catalog/visualizations/0";

#[test]
fn the_architecture_example_parses_validates_and_round_trips() {
    let endpoint = Endpoint::from_yaml(WEATHER).expect("parses");
    endpoint.validate().expect("validates");
    let charts = &endpoint
        .spec
        .catalog
        .as_ref()
        .expect("catalog")
        .visualizations;
    assert_eq!(charts.len(), 2);
    let again = Endpoint::from_yaml(&endpoint.to_yaml().expect("yaml")).expect("re-parses");
    assert_eq!(endpoint, again);
}

#[test]
fn a_source_the_endpoint_does_not_serve_is_refused() {
    let yaml = WEATHER.replace(
        "  audience: public\n",
        "  audience: public\n  disabledRepresentations: [odata]\n",
    );
    let endpoint = Endpoint::from_yaml(&yaml).expect("parses");
    match endpoint.validate() {
        Err(Error::Invalid { field, reason }) => {
            assert_eq!(field, "/spec/catalog/visualizations/0/data/source");
            assert!(reason.starts_with("source:"), "{reason}");
        }
        other => panic!("expected the odata source refused, got {other:?}"),
    }
}

#[test]
fn an_unknown_member_of_an_item_or_its_data_does_not_parse() {
    assert!(Endpoint::from_yaml(&WEATHER.replace(
        "aggrPeriod: PT1H",
        "aggrPeriod: PT1H\n          url: https://x.example/"
    ))
    .is_err());
    assert!(Endpoint::from_yaml(&WEATHER.replace("source: odata", "source: http")).is_err());
    assert!(Endpoint::from_yaml(&WEATHER.replace(
        "name: temperature-now",
        "name: temperature-now\n        script: x"
    ))
    .is_err());
}

#[test]
fn data_from_anywhere_but_source_is_refused() {
    for (spec, pointer) in [
        (
            bar(json!({ "data": { "url": "https://evil.example/rows.csv" } })),
            "/spec/data/url",
        ),
        (
            bar(json!({ "data": { "name": "other" } })),
            "/spec/data/name",
        ),
        (
            bar(
                json!({ "transform": [{ "lookup": "a", "from": { "data": { "url": "https://evil.example/" }, "key": "a" } }] }),
            ),
            "/spec/transform/0/from/data/url",
        ),
        (
            json!({
                "$schema": "https://vega.github.io/schema/vega/v5.json",
                "data": [{ "name": "source" }, { "name": "extra", "url": "https://evil.example/x.json" }]
            }),
            "/spec/data/1/url",
        ),
    ] {
        assert_eq!(
            refused_spec(spec),
            (format!("{AT}{pointer}"), "data-source".to_owned())
        );
    }
}

#[test]
fn usermeta_and_loader_options_are_refused() {
    assert_eq!(
        refused_spec(bar(
            json!({ "usermeta": { "embedOptions": { "loader": {} } } })
        )),
        (format!("{AT}/spec/usermeta"), "usermeta".to_owned())
    );
    assert_eq!(
        refused_spec(bar(
            json!({ "config": { "loader": { "baseURL": "https://evil.example/" } } })
        )),
        (format!("{AT}/spec/config/loader"), "loader".to_owned())
    );
}

#[test]
fn links_and_image_urls_that_could_leave_are_refused() {
    assert_eq!(
        refused_spec(bar(json!({ "encoding": { "href": { "field": "a" } } }))),
        (format!("{AT}/spec/encoding/href"), "href".to_owned())
    );
    assert_eq!(
        refused_spec(bar(
            json!({ "mark": { "type": "bar", "href": "https://evil.example/" } })
        )),
        (format!("{AT}/spec/mark/href"), "href".to_owned())
    );
    let image = |url: Value| {
        json!({
            "$schema": "https://vega.github.io/schema/vega-lite/v5.json",
            "data": { "values": [{ "a": 1, "b": 2 }] },
            "mark": { "type": "image", "width": 20, "height": 20 },
            "encoding": { "x": { "field": "a", "type": "quantitative" }, "y": { "field": "b", "type": "quantitative" }, "url": url }
        })
    };
    assert_eq!(
        refused_spec(image(json!({ "field": "picture" }))),
        (format!("{AT}/spec/encoding/url"), "url".to_owned())
    );
    assert_eq!(
        refused_spec(image(json!({ "value": "https://evil.example/pixel.png" }))),
        (format!("{AT}/spec/encoding/url"), "url".to_owned())
    );
    check_spec(&image(
        json!({ "value": "data:image/png;base64,iVBORw0KGgo=" }),
    ))
    .expect("a data: image is fine");
}

#[test]
fn inline_rows_with_a_url_column_and_expressions_stay_allowed() {
    let spec = json!({
        "$schema": "https://vega.github.io/schema/vega-lite/v5.json",
        "data": { "values": [{ "a": "x", "b": 1, "url": "https://anything.example/" }] },
        "transform": [{ "calculate": "datum.b * 2", "as": "c" }, { "filter": "datum.c > 0" }],
        "mark": "bar",
        "encoding": { "x": { "field": "a", "type": "nominal" }, "y": { "field": "c", "type": "quantitative" } }
    });
    check_spec(&spec).expect("inline values and expressions are allowed (ADR-N-053 §2.3, §2.4)");
}

#[test]
fn a_vega_spec_with_a_dataset_derived_from_source_validates() {
    let spec = json!({
        "$schema": "https://vega.github.io/schema/vega/v5.json",
        "width": 300, "height": 200,
        "data": [
            { "name": "source" },
            { "name": "warm", "source": "source", "transform": [{ "type": "filter", "expr": "datum.temperature > 20" }] }
        ],
        "scales": [
            { "name": "x", "type": "band", "domain": { "data": "warm", "field": "name" }, "range": "width" },
            { "name": "y", "type": "linear", "domain": { "data": "warm", "field": "temperature" }, "range": "height" }
        ],
        "marks": [{
            "type": "rect", "from": { "data": "warm" },
            "encode": { "enter": {
                "x": { "scale": "x", "field": "name" }, "width": { "scale": "x", "band": 1 },
                "y": { "scale": "y", "field": "temperature" }, "y2": { "scale": "y", "value": 0 }
            } }
        }]
    });
    check_spec(&spec).expect("a Vega 5 spec over source validates");
}

#[test]
fn the_schema_and_its_version_are_checked() {
    assert_eq!(
        refused_spec(bar(json!({ "mark": "pie-chart" }))),
        (format!("{AT}/spec/mark"), "schema".to_owned())
    );
    for schema in [
        json!("https://vega.github.io/schema/vega-lite/v4.json"),
        json!("https://evil.example/schema/vega-lite/v5.json"),
        json!(null),
    ] {
        let mut spec = bar(json!({}));
        spec["$schema"] = schema;
        assert_eq!(
            refused_spec(spec),
            (format!("{AT}/spec/$schema"), "schema".to_owned())
        );
    }
    let mut minor = bar(json!({}));
    minor["$schema"] = json!("https://vega.github.io/schema/vega-lite/v5.23.0.json");
    check_spec(&minor).expect("a minor version of 5 is the same grammar");
}

#[test]
fn the_limits_hold() {
    let big = bar(json!({ "description": "x".repeat(200 * 1024) }));
    assert_eq!(
        refused_spec(big),
        (format!("{AT}/spec"), "too-large".to_owned())
    );

    let many: Vec<Visualization> = (0..21)
        .map(|i| {
            let mut v = item(bar(json!({})));
            v.name = format!("chart-{i}");
            v
        })
        .collect();
    assert_eq!(
        refused(&many, ALL),
        (
            "/spec/catalog/visualizations".to_owned(),
            "too-many".to_owned()
        )
    );
    assert!(
        validate_list(&many[..20], ALL).is_ok(),
        "twenty is the limit, not past it"
    );

    let twice = [item(bar(json!({}))), item(bar(json!({})))];
    assert_eq!(
        refused(&twice, ALL),
        (
            "/spec/catalog/visualizations/1/name".to_owned(),
            "name".to_owned()
        )
    );
    let mut bad = item(bar(json!({})));
    bad.name = "Chart One".into();
    assert_eq!(
        refused(&[bad], ALL),
        (format!("{AT}/name"), "name".to_owned())
    );
    let mut untitled = item(bar(json!({})));
    untitled.title = BTreeMap::new();
    assert_eq!(
        refused(&[untitled], ALL),
        (format!("{AT}/title"), "text".to_owned())
    );
}

#[test]
fn a_temporal_source_reads_at_most_31_days_with_a_method_for_its_period() {
    let temporal = |data: Value| {
        let mut v = item(bar(json!({})));
        v.data = serde_json::from_value(data).expect("data");
        v
    };
    let ok = json!({ "source": "temporal", "type": "WeatherObserved", "attrs": ["temperature"], "last": "P31D" });
    assert!(validate_list(&[temporal(ok)], ALL).is_ok());
    for (data, field) in [
        (
            json!({ "source": "temporal", "type": "WeatherObserved", "attrs": ["temperature"], "last": "P32D" }),
            "last",
        ),
        (
            json!({ "source": "temporal", "type": "WeatherObserved", "attrs": ["temperature"], "last": "P1M" }),
            "last",
        ),
        (
            json!({ "source": "temporal", "type": "WeatherObserved", "attrs": ["temperature"], "last": "PT0S" }),
            "last",
        ),
        (
            json!({ "source": "temporal", "type": "WeatherObserved", "attrs": ["temperature"], "last": "24h" }),
            "last",
        ),
        (
            json!({ "source": "temporal", "type": "WeatherObserved", "attrs": [], "last": "P1D" }),
            "attrs",
        ),
        (
            json!({ "source": "temporal", "type": "WeatherObserved", "attrs": ["t"], "last": "P1D", "aggrPeriod": "PT1H" }),
            "aggrPeriod",
        ),
        (
            json!({ "source": "temporal", "type": "WeatherObserved", "attrs": ["t"], "last": "P1D", "aggrMethod": "avg", "aggrPeriod": "P" }),
            "aggrPeriod",
        ),
        (json!({ "source": "csv", "type": "not a type" }), "type"),
    ] {
        assert_eq!(
            refused(&[temporal(data)], ALL),
            (format!("{AT}/data/{field}"), "source".to_owned())
        );
    }
}

#[test]
fn every_template_fills_from_columns_and_validates() {
    // The five the Portal offers (crates/jc-core/data/visualization-templates.json).
    let names = ["bar", "line", "heatmap", "small-multiples", "linked-brush"];
    let columns: BTreeMap<String, String> = [
        ("x", "observedAt"),
        ("y", "temperature"),
        ("color", "name"),
        ("facet", "name"),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_owned(), v.to_owned()))
    .collect();
    for name in names {
        let spec = template(name, &columns).unwrap_or_else(|| panic!("{name} fills"));
        check_spec(&spec).unwrap_or_else(|r| panic!("{name}: {r:?}"));
        assert!(
            !spec.to_string().contains("\"{"),
            "{name} keeps no placeholder: {spec}"
        );
    }
}

/// T-6171: a week of readings drew 13-18k point markers per chart. The line keeps its points, so
/// each reading has its tooltip, but draws them transparent: the lines alone show.
#[test]
fn the_line_template_draws_no_marker_and_keeps_a_tooltip_per_reading() {
    let columns: BTreeMap<String, String> = [("x", "observedAt"), ("y", "temperature")]
        .into_iter()
        .map(|(k, v)| (k.to_owned(), v.to_owned()))
        .collect();
    let spec = template("line", &columns).expect("fills");
    assert_eq!(
        spec["mark"],
        serde_json::json!({ "type": "line", "point": { "opacity": 0 } })
    );
    assert_eq!(spec["encoding"]["tooltip"][0]["field"], "observedAt");
}

#[test]
fn an_optional_role_left_out_drops_its_encoding_and_a_required_one_fails() {
    let columns: BTreeMap<String, String> = [
        ("x".to_owned(), "observedAt".to_owned()),
        ("y".to_owned(), "temperature".to_owned()),
    ]
    .into();
    let line = template("line", &columns).expect("line without a colour");
    assert!(line["encoding"].get("color").is_none(), "{line}");
    assert_eq!(
        line["encoding"]["tooltip"].as_array().map(Vec::len),
        Some(2)
    );
    check_spec(&line).expect("still valid");
    assert!(
        template("heatmap", &columns).is_none(),
        "the heatmap needs a colour column"
    );
    assert!(template("pie", &columns).is_none());
}
