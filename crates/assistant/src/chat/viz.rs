//! Charts and maps of a data answer (T-4657, ADR-N-053 §2.9, API/05 §1.3 and §1.4). Code builds
//! every spec from the rows the answer read: the model only names a table and its columns with
//! [`TOOL`], and a table whose rows carry a location gets a map without being asked. The specs
//! are drawn by the renderer on `viz.{domain}`, which checks them again.

use serde::Deserialize;
use serde_json::{json, Map, Value};

use super::agent::Table;

/// The tool the data loop offers when the deployment has a `viz.{domain}`.
pub const TOOL: &str = "show_visualization";
/// The map spec's `$schema` (ADR-N-053 §2.9).
pub const MAP_SCHEMA: &str = "urn:joinedcontext:map:v1";
const VEGA_LITE: &str = "https://vega.github.io/schema/vega-lite/v5.json";
/// The values a default map's popup shows beside the label and the time.
const POPUP: usize = 4;
const TITLE_CHARS: usize = 80;

/// The domain a viz origin serves: `https://viz.dev.example.org` → `dev.example.org`.
pub fn domain(viz: &url::Url) -> Option<&str> {
    viz.host_str()?.strip_prefix("viz.")
}

/// A GeoJSON geometry the map draws, every position in range and rounded to 6 decimals;
/// `None` for any other value.
pub fn geometry(value: &Value) -> Option<Value> {
    let kind = value.get("type")?.as_str()?;
    let depth = match kind {
        "Point" => 0,
        "MultiPoint" | "LineString" => 1,
        "MultiLineString" | "Polygon" => 2,
        "MultiPolygon" => 3,
        _ => return None,
    };
    Some(json!({"type": kind, "coordinates": positions(value.get("coordinates")?, depth)?}))
}

fn positions(value: &Value, depth: u8) -> Option<Value> {
    let list = value.as_array()?;
    if depth == 0 {
        // An altitude, when there is one, is left out.
        let (lon, lat) = (list.first()?.as_f64()?, list.get(1)?.as_f64()?);
        let ok =
            list.len() <= 3 && (-180.0..=180.0).contains(&lon) && (-90.0..=90.0).contains(&lat);
        return ok.then(|| json!([round(lon), round(lat)]));
    }
    if list.is_empty() {
        return None;
    }
    list.iter()
        .map(|item| positions(item, depth - 1))
        .collect::<Option<Vec<_>>>()
        .map(Value::Array)
}

fn round(x: f64) -> f64 {
    (x * 1e6).round() / 1e6
}

/// Each shown entity's location as [`geometry`] makes it, in the order of the table's rows
/// (`routed::table_of` reads the same first `most` entities).
pub fn geometries(text: &str, most: usize) -> Vec<Option<Value>> {
    let Ok(value) = serde_json::from_str::<Value>(text) else {
        return Vec::new();
    };
    let Some(entities) = value.get("entities").unwrap_or(&value).as_array() else {
        return Vec::new();
    };
    entities
        .iter()
        .take(most)
        .map(|entity| {
            let location = entity.get("location")?;
            geometry(location.get("value").unwrap_or(location))
        })
        .collect()
}

/// A cell as the map carries it: a structured value becomes its JSON text.
fn plain(value: &Value) -> Value {
    match value {
        Value::Array(_) | Value::Object(_) => Value::String(value.to_string()),
        other => other.clone(),
    }
}

/// The dataset's page when it is on our own hosts; the renderer refuses any other link.
fn link<'a>(table: &'a Table, domain: &str) -> Option<&'a str> {
    let page = table.dataset.as_deref()?;
    let host = url::Url::parse(page)
        .ok()
        .filter(|u| u.scheme() == "https")?;
    let host = host.host_str()?;
    (host == domain || host.ends_with(&format!(".{domain}"))).then_some(page)
}

/// The map spec of `table`'s located rows: the `label` column, the `popup` columns with their
/// units and the `time` column, nothing else of a row. `None` when no row has a location.
fn map_of(
    table: &Table,
    label: usize,
    popup: &[usize],
    time: Option<usize>,
    title: Option<&str>,
    domain: &str,
) -> Option<Value> {
    let shown: Vec<usize> = std::iter::once(label)
        .chain(popup.iter().copied())
        .chain(time)
        .collect();
    let features: Vec<Value> = table
        .rows
        .iter()
        .zip(&table.geometries)
        .filter_map(|(row, geometry)| {
            let properties: Map<String, Value> = shown
                .iter()
                .map(|&i| {
                    let cell = row.get(i).map_or(Value::Null, plain);
                    (table.columns[i].clone(), cell)
                })
                .collect();
            Some(json!({"type": "Feature", "geometry": geometry.as_ref()?, "properties": properties}))
        })
        .collect();
    if features.is_empty() {
        return None;
    }
    let popup: Vec<Value> = popup
        .iter()
        .map(|&i| {
            let mut field = json!({"field": table.columns[i], "label": table.columns[i]});
            if let Some(unit) = table.units.get(i).cloned().flatten() {
                field["unit"] = json!(unit);
            }
            field
        })
        .collect();
    let mut spec = json!({
        "$schema": MAP_SCHEMA,
        "label": table.columns[label],
        "popup": popup,
        "data": {"type": "FeatureCollection", "features": features},
    });
    if let Some(time) = time {
        spec["time"] = json!(table.columns[time]);
    }
    if let Some(title) = title {
        spec["title"] = json!(title);
    }
    if let Some(page) = link(table, domain) {
        spec["link"] = json!(page);
    }
    Some(spec)
}

/// Whether a column only points elsewhere (a relationship, a URN, a URL): no value for a popup.
fn points_elsewhere(table: &Table, column: usize) -> bool {
    table.columns[column].starts_with("ref")
        || table.rows.iter().all(|row| match row.get(column) {
            Some(Value::String(s)) => s.starts_with("urn:") || s.starts_with("http"),
            Some(Value::Null) | None => true,
            Some(_) => false,
        })
}

/// The map a table with located rows gets unasked (API/05 §1.3): its label, the first values
/// the table leads with and `observedAt`.
pub fn default_map(table: &Table, domain: &str) -> Option<Value> {
    let time = table.columns.iter().position(|c| c == "observedAt");
    // The model's key attributes where it marks them (DM-99, T-4672), else the first columns.
    let key = |i: usize| table.summary.get(i).copied().unwrap_or(false);
    let keyed = table.summary.iter().skip(1).any(|k| *k);
    let popup: Vec<usize> = (1..table.columns.len())
        .filter(|&i| Some(i) != time && !points_elsewhere(table, i) && (!keyed || key(i)))
        .take(POPUP)
        .collect();
    map_of(table, 0, &popup, time, None, domain)
}

/// The tool the model may call, as the loop offers it.
pub fn tool() -> Value {
    json!({"type": "function", "function": {
        "name": TOOL,
        "description": "Show the person one table of this answer as a map, a line chart or a bar chart, drawn by code from that table's own rows. At most once per answer, and only when a picture says more than the table; then answer in text as usual, naming the few values that matter.",
        "parameters": {
            "type": "object",
            "additionalProperties": false,
            "required": ["kind", "data", "label", "value", "title"],
            "properties": {
                "kind": {"type": "string", "enum": ["map", "line", "bar"], "description": "map needs a table whose rows have a location; line needs a time column"},
                "data": {"type": "integer", "description": "the number n of a table you were given"},
                "label": {"type": "string", "description": "the column that names each row, such as name"},
                "value": {"type": "string", "description": "a numeric column of that table"},
                "time": {"type": "string", "description": "a time column of that table, such as observedAt"},
                "title": {"type": "string", "maxLength": TITLE_CHARS},
            },
        },
    }})
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
enum Kind {
    Map,
    Line,
    Bar,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    kind: Kind,
    data: usize,
    label: String,
    value: String,
    #[serde(default)]
    time: Option<String>,
    title: String,
}

/// The `visualization` event of one [`TOOL`] call: `{"spec"}` for a map, `{"spec", "rows"}` for
/// a chart. `tables` are the answer's tables by their number; the error is told to the model.
pub fn build(arguments: &str, tables: &[(usize, Table)], domain: &str) -> Result<Value, String> {
    let request: Request = serde_json::from_str(arguments).map_err(|why| {
        format!("The call was not made: {why}. Correct the arguments, or answer without a chart.")
    })?;
    let numbers = || {
        tables
            .iter()
            .map(|(n, _)| n.to_string())
            .collect::<Vec<_>>()
            .join(", ")
    };
    let Some((_, table)) = tables.iter().find(|(n, _)| *n == request.data) else {
        return Err(format!(
            "There is no table {}; the tables of this answer are: {}.",
            request.data,
            numbers()
        ));
    };
    let column = |name: &str| {
        table.columns.iter().position(|c| c == name).ok_or_else(|| {
            format!(
                "Table {} has no column `{name}`; its columns are: {}.",
                request.data,
                table.columns.join(", ")
            )
        })
    };
    let label = column(&request.label)?;
    let value = column(&request.value)?;
    let time = request.time.as_deref().map(column).transpose()?;
    let numeric = table
        .rows
        .iter()
        .filter_map(|row| row.get(value))
        .filter(|v| !v.is_null())
        .all(Value::is_number)
        && table
            .rows
            .iter()
            .any(|row| row.get(value).is_some_and(Value::is_number));
    if !numeric {
        return Err(format!(
            "Column `{}` holds no numbers; a chart needs a numeric value.",
            request.value
        ));
    }
    if request.title.chars().count() > TITLE_CHARS {
        return Err(format!(
            "The title is longer than {TITLE_CHARS} characters."
        ));
    }
    let title = request.title.trim();
    match request.kind {
        Kind::Map => map_of(table, label, &[value], time, Some(title), domain)
            .map(|spec| json!({"spec": spec}))
            .ok_or_else(|| {
                format!(
                    "Table {} has no location; show it as a bar or a line chart.",
                    request.data
                )
            }),
        Kind::Line if time.is_none() => {
            Err("A line chart needs `time`, a time column of the table.".into())
        }
        kind => Ok(chart(table, kind, label, value, time, title)),
    }
}

/// A Vega-Lite bar or line chart over the table's rows, fields named `label`, `value` and
/// `time` whatever the columns are called (a dot in a column name would read as a path).
fn chart(
    table: &Table,
    kind: Kind,
    label: usize,
    value: usize,
    time: Option<usize>,
    title: &str,
) -> Value {
    let rows: Vec<Value> = table
        .rows
        .iter()
        .map(|row| {
            let mut out = json!({"label": row.get(label).map_or(Value::Null, plain), "value": row.get(value).cloned().unwrap_or(Value::Null)});
            if let Some(time) = time {
                out["time"] = row.get(time).cloned().unwrap_or(Value::Null);
            }
            out
        })
        .collect();
    let value_title = match table.units.get(value).cloned().flatten() {
        Some(unit) => format!("{} ({unit})", table.columns[value]),
        None => table.columns[value].clone(),
    };
    let encoding = if kind == Kind::Line {
        json!({
            "x": {"field": "time", "type": "temporal", "title": null},
            "y": {"field": "value", "type": "quantitative", "title": value_title, "scale": {"zero": false}},
            "color": {"field": "label", "type": "nominal", "title": null},
        })
    } else {
        json!({
            "y": {"field": "label", "type": "nominal", "sort": "-x", "title": null},
            "x": {"field": "value", "type": "quantitative", "title": value_title},
        })
    };
    let mark = if kind == Kind::Line {
        json!({"type": "line", "point": true, "tooltip": true})
    } else {
        json!({"type": "bar", "tooltip": true})
    };
    let mut spec = json!({"$schema": VEGA_LITE, "data": {"name": "source"}, "mark": mark, "encoding": encoding});
    // A title the renderer's refusal list would refuse is left out, so the chart still draws.
    let lower = title.to_lowercase();
    if !title.is_empty()
        && !["__proto__", "constructor", "prototype", "javascript:"]
            .iter()
            .any(|w| lower.contains(w))
    {
        spec["title"] = json!(title);
    }
    json!({"spec": spec, "rows": rows})
}

#[cfg(test)]
mod tests {
    use super::*;

    fn air() -> Table {
        Table {
            endpoint: "ovzdusie".into(),
            entity_type: "AirQualityObserved".into(),
            columns: vec![
                "name".into(),
                "no2".into(),
                "pm10".into(),
                "refPointOfInterest".into(),
                "observedAt".into(),
            ],
            rows: vec![
                vec![
                    json!("SK0214A <img src=x onerror=alert(1)>"),
                    json!(14.2),
                    json!(21.5),
                    json!("urn:ngsi-ld:PointOfInterest:1"),
                    json!("2026-10-18T15:00:00Z"),
                ],
                vec![
                    json!("Zvolen"),
                    json!(9.25),
                    Value::Null,
                    json!("urn:ngsi-ld:PointOfInterest:2"),
                    json!("2026-10-18T14:00:00Z"),
                ],
                vec![
                    json!("Bez polohy"),
                    json!(3),
                    json!(4),
                    Value::Null,
                    Value::Null,
                ],
            ],
            units: vec![None, Some("µg/m³".into()), Some("µg/m³".into()), None, None],
            labels: Vec::new(),
            summary: Vec::new(),
            total: None,
            dataset: Some("https://data.dev.example.org/dataset/ovzdusie".into()),
            download: None,
            map: None,
            geometries: vec![
                Some(json!({"type": "Point", "coordinates": [19.146234, 48.736318]})),
                Some(json!({"type": "Point", "coordinates": [19.12, 48.57]})),
                None,
            ],
            profile: None,
        }
    }

    #[test]
    fn a_geometry_is_kept_only_when_valid_and_in_range_and_rounded() {
        let point = geometry(
            &json!({"type": "Point", "coordinates": [19.1462345678, 48.7363181234, 350.0]}),
        );
        assert_eq!(
            point,
            Some(json!({"type": "Point", "coordinates": [19.146235, 48.736318]}))
        );
        let polygon = json!({"type": "Polygon", "coordinates": [[[14.4, 50.0], [14.5, 50.0], [14.5, 50.1], [14.4, 50.0]]]});
        assert!(geometry(&polygon).is_some());
        for bad in [
            json!({"type": "Point", "coordinates": [190.0, 48.0]}),
            json!({"type": "Point", "coordinates": [19.0, -91.0]}),
            json!({"type": "Point", "coordinates": ["19", "48"]}),
            json!({"type": "Point", "coordinates": [19.0]}),
            json!({"type": "LineString", "coordinates": []}),
            json!({"type": "Polygon", "coordinates": [[19.0, 48.0]]}),
            json!({"type": "GeometryCollection", "geometries": []}),
            json!({"type": "Feature", "geometry": {"type": "Point", "coordinates": [19.0, 48.0]}}),
        ] {
            assert_eq!(geometry(&bad), None, "{bad}");
        }
    }

    #[test]
    fn the_locations_follow_the_rows_normalized_or_plain() {
        let text = json!([
            {"id": "a", "location": {"type": "GeoProperty", "value": {"type": "Point", "coordinates": [19.1, 48.7]}}},
            {"id": "b", "location": {"type": "Point", "coordinates": [500, 48.7]}},
            {"id": "c"},
            {"id": "d", "location": {"type": "Point", "coordinates": [17.1, 48.1]}},
        ])
        .to_string();
        let found = geometries(&text, 3);
        assert_eq!(found.len(), 3);
        assert!(found[0].is_some() && found[1].is_none() && found[2].is_none());
        assert!(geometries("not json", 3).is_empty());
    }

    #[test]
    fn the_default_map_holds_the_label_the_values_and_the_time_and_nothing_else() {
        let spec = default_map(&air(), "dev.example.org").expect("a map");
        assert_eq!(spec["$schema"], MAP_SCHEMA);
        let features = spec["data"]["features"].as_array().expect("features");
        assert_eq!(features.len(), 2, "the row without a location is left out");
        let properties = features[0]["properties"].as_object().expect("properties");
        let mut keys: Vec<_> = properties.keys().cloned().collect();
        keys.sort();
        assert_eq!(
            keys,
            ["name", "no2", "observedAt", "pm10"],
            "no reference, no id"
        );
        assert_eq!(
            properties["name"], "SK0214A <img src=x onerror=alert(1)>",
            "markup stays text for the renderer"
        );
        assert_eq!(
            spec["popup"][0],
            json!({"field": "no2", "label": "no2", "unit": "µg/m³"})
        );
        assert_eq!(spec["time"], "observedAt");
        assert_eq!(
            spec["link"],
            "https://data.dev.example.org/dataset/ovzdusie"
        );
        let mut elsewhere = air();
        elsewhere.dataset = Some("https://data.gov.sk/dataset/x".into());
        assert!(default_map(&elsewhere, "dev.example.org")
            .expect("a map")
            .get("link")
            .is_none());
        let mut nowhere = air();
        nowhere.geometries = vec![None, None, None];
        assert_eq!(default_map(&nowhere, "dev.example.org"), None);
    }

    /// T-4672 (DM-99): a model that marks key attributes puts only those in the popup.
    #[test]
    fn the_default_maps_popup_shows_the_models_key_attributes() {
        let mut keyed = air();
        keyed.summary = vec![true, false, true, false, false];
        let spec = default_map(&keyed, "dev.example.org").expect("a map");
        let fields: Vec<&Value> = spec["popup"]
            .as_array()
            .expect("a popup")
            .iter()
            .map(|f| &f["field"])
            .collect();
        assert_eq!(fields, [&json!("pm10")]);
        let spec = default_map(&air(), "dev.example.org").expect("a map");
        assert_eq!(
            spec["popup"].as_array().map(Vec::len),
            Some(2),
            "no key attributes: the first columns"
        );
    }

    #[test]
    fn a_valid_call_builds_its_spec_from_exactly_that_tables_rows() {
        let tables = [(3, air())];
        let map = build(r#"{"kind":"map","data":3,"label":"name","value":"pm10","time":"observedAt","title":"PM10 v kraji"}"#, &tables, "dev.example.org")
            .expect("a map");
        assert_eq!(map["spec"]["title"], "PM10 v kraji");
        assert_eq!(
            map["spec"]["popup"],
            json!([{"field": "pm10", "label": "pm10", "unit": "µg/m³"}])
        );
        assert_eq!(
            map["spec"]["data"]["features"].as_array().map(Vec::len),
            Some(2)
        );
        assert!(map.get("rows").is_none());
        let bar = build(
            r#"{"kind":"bar","data":3,"label":"name","value":"no2","title":"NO2"}"#,
            &tables,
            "dev.example.org",
        )
        .expect("a bar");
        assert_eq!(bar["rows"].as_array().map(Vec::len), Some(3));
        assert_eq!(bar["rows"][1], json!({"label": "Zvolen", "value": 9.25}));
        assert_eq!(bar["spec"]["encoding"]["x"]["title"], "no2 (µg/m³)");
        assert_eq!(bar["spec"]["$schema"], VEGA_LITE);
        let line = build(r#"{"kind":"line","data":3,"label":"name","value":"pm10","time":"observedAt","title":"constructor"}"#, &tables, "dev.example.org")
            .expect("a line");
        assert_eq!(line["rows"][0]["time"], "2026-10-18T15:00:00Z");
        assert!(
            line["spec"].get("title").is_none(),
            "a title the renderer would refuse is dropped"
        );
    }

    #[test]
    fn a_call_naming_what_the_table_lacks_is_refused_to_the_model() {
        let tables = [(3, air())];
        let refused = |args: &str| build(args, &tables, "dev.example.org").expect_err("refused");
        assert!(
            refused(r#"{"kind":"bar","data":4,"label":"name","value":"no2","title":"x"}"#)
                .contains("tables of this answer are: 3")
        );
        assert!(
            refused(r#"{"kind":"bar","data":3,"label":"name","value":"so2","title":"x"}"#)
                .contains("no column `so2`")
        );
        assert!(
            refused(r#"{"kind":"bar","data":3,"label":"name","value":"name","title":"x"}"#)
                .contains("holds no numbers")
        );
        assert!(
            refused(r#"{"kind":"line","data":3,"label":"name","value":"no2","title":"x"}"#)
                .contains("needs `time`")
        );
        assert!(
            refused(r#"{"kind":"pie","data":3,"label":"name","value":"no2","title":"x"}"#)
                .contains("unknown variant")
        );
        assert!(refused(
            r#"{"kind":"bar","data":3,"label":"name","value":"no2","title":"x","spec":{}}"#
        )
        .contains("unknown field"));
        assert!(refused(&format!(
            r#"{{"kind":"bar","data":3,"label":"name","value":"no2","title":"{}"}}"#,
            "x".repeat(81)
        ))
        .contains("80"));
        let mut flat = air();
        flat.geometries = vec![None, None, None];
        let refused = build(
            r#"{"kind":"map","data":3,"label":"name","value":"no2","title":"x"}"#,
            &[(3, flat)],
            "dev.example.org",
        )
        .expect_err("refused");
        assert!(refused.contains("no location"));
    }

    #[test]
    fn the_tool_offers_three_kinds_and_admits_no_other_member() {
        let offered = tool();
        assert_eq!(offered["function"]["name"], TOOL);
        let parameters = &offered["function"]["parameters"];
        assert_eq!(
            parameters["properties"]["kind"]["enum"],
            json!(["map", "line", "bar"])
        );
        assert_eq!(parameters["additionalProperties"], json!(false));
        assert_eq!(
            parameters["properties"]["title"]["maxLength"],
            json!(TITLE_CHARS)
        );
        assert_eq!(
            parameters["required"],
            json!(["kind", "data", "label", "value", "title"])
        );
    }

    #[test]
    fn the_domain_is_the_viz_hosts_parent() {
        let viz = url::Url::parse("https://viz.dev.example.org").expect("url");
        assert_eq!(domain(&viz), Some("dev.example.org"));
        let other = url::Url::parse("https://charts.dev.example.org").expect("url");
        assert_eq!(domain(&other), None);
    }
}
