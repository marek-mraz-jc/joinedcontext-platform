//! T-4642 (EP-132, ADR-N-053 §2.7): each visualization of an Endpoint becomes a
//! `jc_visualization` view of its dataset; a run creates the missing, updates the changed and
//! deletes the views the list no longer names, never touches another view type, and an
//! unchanged list writes nothing.

use jc_core::kinds::ckan::CkanInstanceSpec;
use jcctl::loader::RawManifest;
use jcctl::publish::ckan::{
    publish, publish_views, CkanApi, InMemoryCkan, Settings, Views, VISUALIZATION_VIEW,
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
    let done = publish_views(&mut ckan, &endpoint, &settings()).expect("views");
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
    let data: Value =
        serde_json::from_str(now["data"].as_str().expect("data as text")).expect("JSON");
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
    publish_views(&mut ckan, &endpoint, &settings()).expect("first run");
    let before = writes(&ckan).len();
    let again = publish_views(&mut ckan, &endpoint, &settings()).expect("second run");
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
    publish_views(&mut ckan, &first_list, &settings()).expect("first run");
    let id = ours(&ckan, &first)
        .iter()
        .find(|v| v["jc_name"] == "no2-now")
        .map(|v| v["id"].clone())
        .expect("an id");

    let renamed = endpoint(&[chart("no2-now", "NO2 práve teraz")]);
    let done = publish_views(&mut ckan, &renamed, &settings()).expect("second run");
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

    let done = publish_views(&mut ckan, &endpoint(&[]), &settings()).expect("emptied");
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

    let done = publish_views(&mut ckan, &endpoint, &settings()).expect("views");
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
        publish_views(&mut empty, &endpoint, &settings()).expect("no dataset"),
        Views::default()
    );
    assert!(writes(&empty).is_empty());

    let unpublished: RawManifest = serde_norway::from_str(
        "apiVersion: joinedcontext.com/v1alpha1\nkind: Endpoint\nmetadata: { name: x, namespace: ovzdusie }\nspec: { contextSpaceRef: ovzdusie, slug: zt4qm7ge2xdv6ksb3ncf5arw2y, audience: public }\n",
    )
    .expect("parses");
    assert_eq!(
        publish_views(&mut empty, &unpublished, &settings()).expect("none"),
        Views::default()
    );
}
