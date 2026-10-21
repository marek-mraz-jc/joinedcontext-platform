//! T-4642 (EP-132, ADR-N-053 §2.7): each visualization of an Endpoint becomes a
//! `jc_visualization` view of its dataset; a run creates the missing, updates the changed and
//! deletes the views the list no longer names, never touches another view type, and an
//! unchanged list writes nothing.

use jc_core::kinds::ckan::CkanInstanceSpec;
use jcctl::loader::RawManifest;
use jcctl::publish::ckan::{
    publish, publish_views, series_charts, CkanApi, InMemoryCkan, Settings, Views, SERIES_WINDOW,
    VIEW_PAYLOAD, VISUALIZATION_VIEW,
};
use serde_json::{json, Value};

const TOKEN: &str = "ckan-api-token-that-must-never-be-published";
const DATASET: &str = "kvalita-ovzdusia";

/// One visualization of the given name and title, over the Endpoint's `csv`.
fn chart(name: &str, title: &str) -> String {
    format!(
        r#"      - name: {name}
        title: {{ sk: "{title}", en: "{title} (en)" }}
        description: {{ sk: Popis. }}
        data: {{ source: csv, type: AirQualityObserved }}
        spec:
          $schema: https://vega.github.io/schema/vega-lite/v5.json
          data: {{ name: source }}
          mark: bar
          encoding:
            x: {{ field: name, type: nominal }}
            y: {{ field: no2, type: quantitative }}
"#
    )
}

fn endpoint(charts: &[String]) -> RawManifest {
    let list = if charts.is_empty() {
        String::new()
    } else {
        format!("  catalog:\n    visualizations:\n{}", charts.concat())
    };
    serde_norway::from_str(&format!(
        r#"apiVersion: joinedcontext.com/v1alpha1
kind: Endpoint
metadata:
  name: ovzdusie-public
  namespace: ovzdusie
spec:
  contextSpaceRef: ovzdusie
  slug: zt4qm7ge2xdv6ksb3ncf5arw2y
  audience: public
  enabledRepresentations: [csv, ogc-features]
  publish:
    ckan:
      instanceRef: {{ kind: CkanInstance, name: open-data }}
      organization: mesto-banska-bystrica
      name: {DATASET}
{list}"#
    ))
    .expect("the manifest parses")
}

fn instance() -> CkanInstanceSpec {
    serde_norway::from_str(
        "url: https://data.banskabystrica.sk\norganizationDefault: mesto-banska-bystrica\napiTokenRef: { name: ckan-open-data, key: apiToken }\n",
    )
    .expect("the instance spec parses")
}

fn settings() -> Settings {
    Settings::new("data.example.org").in_language("sk")
}

/// A CKAN holding the published dataset, and the id of its first resource.
fn published(endpoint: &RawManifest) -> (InMemoryCkan, String) {
    let mut ckan = InMemoryCkan::new()
        .with_organization("mesto-banska-bystrica")
        .with_token(TOKEN);
    let record = json!({ "dct:title": [{ "@value": "Kvalita ovzdušia", "@language": "sk" }] });
    publish(&mut ckan, endpoint, &instance(), &record, &settings()).expect("published");
    let first = ckan.package(DATASET).expect("the dataset")["resources"][0]["id"]
        .as_str()
        .expect("CKAN gave the resource an id")
        .to_owned();
    (ckan, first)
}

fn ours(ckan: &InMemoryCkan, resource: &str) -> Vec<Value> {
    ckan.views(resource)
        .iter()
        .filter(|v| v["view_type"] == VISUALIZATION_VIEW)
        .cloned()
        .collect()
}

fn writes(ckan: &InMemoryCkan) -> Vec<String> {
    ckan.actions()
        .into_iter()
        .filter(|a| a.starts_with("resource_view_"))
        .map(str::to_owned)
        .collect()
}

#[test]
fn each_visualization_becomes_a_view_holding_its_spec_source_and_slug() {
    let endpoint = endpoint(&[
        chart("no2-now", "NO2 teraz"),
        chart("no2-day", "NO2 za deň"),
    ]);
    let (mut ckan, first) = published(&endpoint);
    let done = publish_views(&mut ckan, &endpoint, &settings(), None).expect("views");
    assert_eq!(
        done,
        Views {
            created: 2,
            updated: 0,
            deleted: 0
        }
    );

    let views = ours(&ckan, &first);
    let now = views
        .iter()
        .find(|v| v["jc_name"] == "no2-now")
        .expect("no2-now");
    assert_eq!(now["title"], "NO2 teraz", "the publisher's language");
    assert_eq!(now["description"], "Popis.");
    assert_eq!(now["endpoint"], "zt4qm7ge2xdv6ksb3ncf5arw2y");
    let spec: Value =
        serde_json::from_str(now["spec"].as_str().expect("spec as text")).expect("JSON");
    assert_eq!(spec["data"], json!({ "name": "source" }));
    // `source`, the member the renderer and the catalogue's view schema read (T-4742).
    let data: Value =
        serde_json::from_str(now["source"].as_str().expect("source as text")).expect("JSON");
    assert_eq!(
        data,
        json!({ "source": "csv", "type": "AirQualityObserved" })
    );
    for (_, payload) in ckan.calls() {
        assert!(
            !payload.to_string().contains(TOKEN),
            "no payload carries the token (EP-67)"
        );
    }
}

#[test]
fn an_unchanged_list_writes_nothing() {
    let endpoint = endpoint(&[chart("no2-now", "NO2 teraz")]);
    let (mut ckan, _) = published(&endpoint);
    publish_views(&mut ckan, &endpoint, &settings(), None).expect("first run");
    let before = writes(&ckan).len();
    let again = publish_views(&mut ckan, &endpoint, &settings(), None).expect("second run");
    assert_eq!(again, Views::default());
    assert_eq!(writes(&ckan).len(), before, "no second write");
}

#[test]
fn a_changed_visualization_is_updated_in_place_and_a_removed_one_deleted() {
    let first_list = endpoint(&[
        chart("no2-now", "NO2 teraz"),
        chart("no2-day", "NO2 za deň"),
    ]);
    let (mut ckan, first) = published(&first_list);
    publish_views(&mut ckan, &first_list, &settings(), None).expect("first run");
    let id = ours(&ckan, &first)
        .iter()
        .find(|v| v["jc_name"] == "no2-now")
        .map(|v| v["id"].clone())
        .expect("an id");

    let renamed = endpoint(&[chart("no2-now", "NO2 práve teraz")]);
    let done = publish_views(&mut ckan, &renamed, &settings(), None).expect("second run");
    assert_eq!(
        done,
        Views {
            created: 0,
            updated: 1,
            deleted: 1
        }
    );
    let views = ours(&ckan, &first);
    assert_eq!(views.len(), 1);
    assert_eq!(
        views[0]["id"], id,
        "updated, not replaced: the embed keeps its id"
    );
    assert_eq!(views[0]["title"], "NO2 práve teraz");

    let done = publish_views(&mut ckan, &endpoint(&[]), &settings(), None).expect("emptied");
    assert_eq!(
        done,
        Views {
            created: 0,
            updated: 0,
            deleted: 1
        }
    );
    assert!(ours(&ckan, &first).is_empty());
}

#[test]
fn other_view_types_are_never_touched_and_a_forged_or_duplicate_view_goes() {
    let endpoint = endpoint(&[chart("no2-now", "NO2 teraz")]);
    let (mut ckan, first) = published(&endpoint);
    let second = ckan.package(DATASET).expect("dataset")["resources"][1]["id"]
        .as_str()
        .expect("a second resource")
        .to_owned();
    let grid = json!({ "resource_id": first, "view_type": "datatables_view", "title": "Table" });
    ckan.action("resource_view_create", &grid)
        .expect("a view of another type");
    for (resource, name) in [
        (&first, "no2-now"),
        (&first, "unknown"),
        (&second, "no2-now"),
    ] {
        let forged = json!({ "resource_id": resource, "view_type": VISUALIZATION_VIEW, "jc_name": name, "spec": "{}" });
        ckan.action("resource_view_create", &forged)
            .expect("a view made through the API");
    }

    let done = publish_views(&mut ckan, &endpoint, &settings(), None).expect("views");
    assert_eq!(
        done,
        Views {
            created: 0,
            updated: 1,
            deleted: 2
        }
    );
    let mine = ours(&ckan, &first);
    assert_eq!(mine.len(), 1, "{mine:?}");
    assert_ne!(
        mine[0]["spec"], "{}",
        "the forged spec is replaced by the manifest's"
    );
    assert!(
        ours(&ckan, &second).is_empty(),
        "a view on another resource goes"
    );
    assert!(
        ckan.views(&first)
            .iter()
            .any(|v| v["view_type"] == "datatables_view"),
        "the grid stays"
    );
}

#[test]
fn no_dataset_or_no_publication_does_nothing() {
    let endpoint = endpoint(&[chart("no2-now", "NO2 teraz")]);
    let mut empty = InMemoryCkan::new().with_organization("mesto-banska-bystrica");
    assert_eq!(
        publish_views(&mut empty, &endpoint, &settings(), None).expect("no dataset"),
        Views::default()
    );
    assert!(writes(&empty).is_empty());

    let unpublished: RawManifest = serde_norway::from_str(
        "apiVersion: joinedcontext.com/v1alpha1\nkind: Endpoint\nmetadata: { name: x, namespace: ovzdusie }\nspec: { contextSpaceRef: ovzdusie, slug: zt4qm7ge2xdv6ksb3ncf5arw2y, audience: public }\n",
    )
    .expect("parses");
    assert_eq!(
        publish_views(&mut empty, &unpublished, &settings(), None).expect("none"),
        Views::default()
    );
}

/// A `model.schema.json` as the gateway serves it: one type observed over time, one register.
fn schema() -> Value {
    json!({ "definitions": {
        "WaterQualityObserved": { "type": "object", "properties": {
            "dateObserved": { "type": "string", "format": "date-time", "x-ngsi-ld-kind": "Property" },
            "temperature": { "type": "number", "x-ngsi-ld-kind": "Property" },
            "location": { "type": "object", "x-ngsi-ld-kind": "GeoProperty" },
            "name": { "type": "string", "x-ngsi-ld-kind": "LanguageProperty" },
            "refPointOfInterest": { "type": "string", "x-ngsi-ld-kind": "Relationship" }
        }},
        "School": { "type": "object", "properties": {
            "name": { "type": "string", "x-ngsi-ld-kind": "Property" },
            "capacity": { "type": "integer", "x-ngsi-ld-kind": "Property" }
        }}
    }})
}

#[test]
fn a_value_observed_over_time_gets_a_chart_of_its_history_and_a_register_none() {
    // T-4742: helsinki-beach-water's temperature had a table and a map, and no chart.
    let endpoint = endpoint(&[]);
    let (mut ckan, first) = published(&endpoint);
    let done = publish_views(&mut ckan, &endpoint, &settings(), Some(&schema())).expect("views");
    assert_eq!(
        done.created, 1,
        "one numeric value over time; School has no dateObserved"
    );
    let [view] = ours(&ckan, &first).try_into().expect("one view");
    assert_eq!(view["jc_name"], "auto-waterqualityobserved-temperature");
    assert_eq!(view["endpoint"], "zt4qm7ge2xdv6ksb3ncf5arw2y");
    let source: Value = serde_json::from_str(view["source"].as_str().expect("text")).expect("JSON");
    assert_eq!(
        source,
        json!({ "source": "temporal", "type": "WaterQualityObserved", "attrs": ["temperature"], "last": SERIES_WINDOW })
    );
    let spec: Value = serde_json::from_str(view["spec"].as_str().expect("text")).expect("JSON");
    assert_eq!(spec["encoding"]["x"]["field"], "observedAt");
    assert_eq!(spec["encoding"]["y"]["field"], "temperature");
    jc_core::kinds::visualization::check_spec(&spec)
        .expect("the chart passes the manifest's own rules");

    // Stable: a second run writes nothing.
    let before = writes(&ckan).len();
    publish_views(&mut ckan, &endpoint, &settings(), Some(&schema())).expect("again");
    assert_eq!(writes(&ckan).len(), before);
}

#[test]
fn a_declared_list_replaces_the_automatic_charts_and_no_schema_gives_none() {
    let declared = endpoint(&[chart("no2-now", "NO2 teraz")]);
    let (mut ckan, first) = published(&declared);
    publish_views(&mut ckan, &declared, &settings(), Some(&schema())).expect("views");
    let names: Vec<Value> = ours(&ckan, &first)
        .iter()
        .map(|v| v["jc_name"].clone())
        .collect();
    assert_eq!(names, [json!("no2-now")]);

    let bare = endpoint(&[]);
    let (mut ckan, first) = published(&bare);
    assert_eq!(
        publish_views(&mut ckan, &bare, &settings(), None).expect("views"),
        Views::default()
    );
    assert!(ours(&ckan, &first).is_empty());
}

#[test]
fn the_automatic_charts_stop_at_the_visualization_limit() {
    let mut properties = serde_json::Map::new();
    properties.insert(
        "dateObserved".into(),
        json!({ "type": "string", "format": "date-time", "x-ngsi-ld-kind": "Property" }),
    );
    for i in 0..30 {
        properties.insert(
            format!("value{i:02}"),
            json!({ "type": "number", "x-ngsi-ld-kind": "Property" }),
        );
    }
    let wide = json!({ "definitions": { "AirQualityObserved": { "properties": properties } } });
    let charts = series_charts(&wide);
    assert_eq!(
        charts.len(),
        jc_core::kinds::visualization::MAX_VISUALIZATIONS
    );
    assert_eq!(charts[0].name, "auto-airqualityobserved-value00");
    assert!(
        series_charts(&json!({})).is_empty(),
        "a schema without definitions"
    );
    assert!(series_charts(&json!({ "definitions": { "T": {
        "properties": { "dateObserved": { "type": "string" }, "v": { "type": "number", "x-ngsi-ld-kind": "Property" } }
    }}})).is_empty(), "a dateObserved that is not a date-time");
}

/// EP-144, T-4846: CKAN answers `409 __extras` to a view field neither its core nor the view
/// type's schema declares, so a create and an update send only the pinned fields, which the
/// catalogue theme's `jc_visualization` schema lists (deployment tests/test_ckan_theme.py).
#[test]
fn a_view_create_or_update_sends_only_the_fields_the_view_schema_declares() {
    let (mut ckan, _) = published(&endpoint(&[chart("no2-now", "NO2 teraz")]));
    publish_views(
        &mut ckan,
        &endpoint(&[chart("no2-now", "NO2 teraz")]),
        &settings(),
        None,
    )
    .expect("created");
    publish_views(
        &mut ckan,
        &endpoint(&[chart("no2-now", "NO2 now")]),
        &settings(),
        None,
    )
    .expect("updated");
    let sent: Vec<&Value> = ckan
        .calls()
        .filter(|(action, _)| matches!(*action, "resource_view_create" | "resource_view_update"))
        .map(|(_, payload)| payload)
        .collect();
    assert_eq!(sent.len(), 2, "one create, one update");
    for payload in sent {
        let fields = payload.as_object().expect("an object");
        let unknown: Vec<&String> = fields
            .keys()
            .filter(|key| !VIEW_PAYLOAD.contains(&key.as_str()))
            .collect();
        assert!(
            unknown.is_empty(),
            "fields CKAN refuses as __extras: {unknown:?}"
        );
        assert!(
            fields.contains_key("jc_name"),
            "the name the next run matches it by"
        );
    }
}
