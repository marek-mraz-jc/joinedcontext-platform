//! `jcctl publish ckan` over a repository laid out the way `apply` reads it (T-0487,
//! EP-62…EP-67, CC-18).
//!
//! The walk, the publication of one target and the withdrawal run here against the
//! in-memory catalogue; the HTTP client has its own tests against a fake CKAN.

mod common;

use jcctl::commands::publish_ckan::{
    csv_table, publish_one, targets, token, typed_cell, withdraw_one, Error, Line, Mirror,
    TokenSource, DATASTORE_RESOURCE,
};
use jcctl::loader::Repository;
use jcctl::publish::ckan::{InMemoryCkan, Outcome, Settings};
use jcctl::publish::ckan_datastore;
use serde_json::{json, Value};
use std::path::Path;

const INSTANCE: &str = r#"apiVersion: joinedcontext.com/v1alpha1
kind: CkanInstance
metadata:
  name: open-data
  namespace: ovzdusie
spec:
  url: https://data.banskabystrica.sk
  organizationDefault: mesto-banska-bystrica
  apiTokenRef: { name: ckan-open-data, key: apiToken, envVar: CKAN_OPEN_DATA_TOKEN }
"#;

fn endpoint(name: &str, slug: &str, representations: &str, publish: &str) -> String {
    format!(
        r#"apiVersion: joinedcontext.com/v1alpha1
kind: Endpoint
metadata:
  name: {name}
  namespace: ovzdusie
spec:
  contextSpaceRef: ovzdusie
  slug: {slug}
  audience: public
  enabledRepresentations: {representations}
{publish}"#
    )
}

const PUBLISHED: &str = r#"  publish:
    ckan:
      instanceRef: open-data
      name: kvalita-ovzdusia
"#;

const MIRRORED: &str = r#"  publish:
    ckan:
      instanceRef: { kind: CkanInstance, name: open-data }
      datastore: { representation: csv, refresh: onReconcile }
"#;

/// The demo repository with the instance, two published endpoints and one that is not.
fn repo(test: &str) -> std::path::PathBuf {
    let dir = common::demo_repo(test);
    common::write(&dir, "projects/ovzdusie/ckan/open-data.yaml", INSTANCE);
    common::write(
        &dir,
        "projects/ovzdusie/spaces/ovzdusie/endpoints/air-public.yaml",
        &endpoint(
            "air-public",
            "zt4qm7ge2xdv6ksb3ncf5arw2y",
            "[ngsi-ld, csv]",
            PUBLISHED,
        ),
    );
    common::write(
        &dir,
        "projects/ovzdusie/spaces/ovzdusie/endpoints/air-rows.yaml",
        &endpoint("air-rows", "k4y7pq2mzt6vhx3nbwrs5cjd3f", "[csv]", MIRRORED),
    );
    dir
}

fn record(name: &str) -> Value {
    json!({
        "@context": "https://www.w3.org/ns/dcat.jsonld",
        "@type": "dcat:Dataset",
        "dct:identifier": name,
        "dct:title": [{ "@value": format!("Air quality ({name})"), "@language": "en" }],
        "dcat:keyword": ["ovzdusie"]
    })
}

const CSV: &str = "id,type,temperature.value,location.value,note\r\n\
urn:ngsi-ld:AirQualityObserved:bb.sk:ovzdusie:1,AirQualityObserved,12.5,\"{\"\"type\"\":\"\"Point\"\",\"\"coordinates\"\":[19.1,48.7]}\",\"a note, with a comma\"\r\n\
urn:ngsi-ld:AirQualityObserved:bb.sk:ovzdusie:2,AirQualityObserved,13,,\r\n";

fn settings() -> Settings {
    Settings::new("data.example.org")
}

// --- the walk --------------------------------------------------------------------------------

/// EP-62: every Endpoint of the project that declares a publication is a target, with its
/// instance resolved from the same project; the one without a publication is not.
#[test]
fn the_walk_finds_every_published_endpoint_of_the_project_with_its_instance() {
    let dir = repo("walk");
    let repo = Repository::load(&dir).expect("the repository loads");

    let found = targets(&repo, "ovzdusie").expect("the walk");
    let names: Vec<&str> = found.iter().map(|t| t.id.name.as_str()).collect();
    assert_eq!(names, vec!["air-public", "air-rows"]);
    assert!(found.iter().all(|t| t.instance_name == "open-data"));
    assert!(found
        .iter()
        .all(|t| t.instance.base_url() == "https://data.banskabystrica.sk"));
    assert_eq!(found[0].dataset_name(), "kvalita-ovzdusia");
    assert_eq!(found[1].dataset_name(), "air-rows");
    assert_eq!(found[1].slug, "k4y7pq2mzt6vhx3nbwrs5cjd3f");
    assert_eq!(
        found[1]
            .publication
            .datastore
            .as_ref()
            .map(|d| d.representation),
        Some(jc_core::kinds::Representation::Csv)
    );

    assert!(targets(&repo, "other-project")
        .expect("an empty walk")
        .is_empty());
}

/// A publication naming an instance the repository does not hold stops the walk with the
/// endpoint and the instance named.
#[test]
fn an_unknown_instance_is_an_error_naming_both_sides() {
    let dir = repo("unknown-instance");
    common::write(
        &dir,
        "projects/ovzdusie/spaces/ovzdusie/endpoints/air-lost.yaml",
        &endpoint(
            "air-lost",
            "aaaaaaaaaaaaaaaaaaaaaaaaaa",
            "[ngsi-ld]",
            "  publish:\n    ckan:\n      instanceRef: { kind: CkanInstance, name: nowhere, namespace: elsewhere }\n",
        ),
    );
    let repo = Repository::load(&dir).expect("the repository loads");

    let error = targets(&repo, "ovzdusie").expect_err("the instance is missing");
    let message = error.to_string();
    assert!(matches!(error, Error::UnknownInstance { .. }), "{message}");
    assert!(message.contains("air-lost"), "{message}");
    assert!(message.contains("elsewhere"), "{message}");
    assert!(message.contains("nowhere"), "{message}");
}

// --- publishing one target -------------------------------------------------------------

/// CC-18: the first run creates the dataset; the second, over the same repository and the
/// same record, reports it unchanged and makes no writing call.
#[test]
fn a_second_run_over_an_unchanged_repository_writes_nothing() {
    let dir = repo("unchanged");
    let repo = Repository::load(&dir).expect("the repository loads");
    let found = targets(&repo, "ovzdusie").expect("the walk");
    let target = &found[0];
    let mut api = InMemoryCkan::new().with_organization("mesto-banska-bystrica");

    let first = publish_one(&mut api, target, &record("air-public"), None, &settings())
        .expect("the first run");
    assert_eq!(
        first,
        Line {
            endpoint: target.id.clone(),
            dataset: "kvalita-ovzdusia".to_owned(),
            outcome: Outcome::Created,
            mirror: None,
        }
    );
    assert_eq!(
        first.to_string(),
        "Endpoint/ovzdusie/air-public: dataset kvalita-ovzdusia created"
    );
    assert_eq!(api.actions(), vec!["package_create"]);
    let dataset = api.package("kvalita-ovzdusia").expect("the dataset");
    assert_eq!(dataset["owner_org"], json!("mesto-banska-bystrica"));
    assert_eq!(
        dataset["url"],
        json!("https://data.example.org/api/endpoint/zt4qm7ge2xdv6ksb3ncf5arw2y/")
    );

    let second = publish_one(&mut api, target, &record("air-public"), None, &settings())
        .expect("the second run");
    assert_eq!(second.outcome, Outcome::Unchanged);
    assert_eq!(api.actions(), vec!["package_create"]);
}

/// EP-65: a target with a mirror gets its table created and filled from the CSV the
/// gateway answers, typed from the cells; a second run leaves the table's fields alone
/// and reloads the rows.
#[test]
fn a_mirrored_endpoint_fills_its_datastore_from_the_csv() {
    let dir = repo("mirror");
    let repo = Repository::load(&dir).expect("the repository loads");
    let found = targets(&repo, "ovzdusie").expect("the walk");
    let target = &found[1];
    let mut api = InMemoryCkan::new().with_organization("mesto-banska-bystrica");

    let line = publish_one(
        &mut api,
        target,
        &record("air-rows"),
        Some(CSV),
        &settings(),
    )
    .expect("the first run");
    assert_eq!(line.outcome, Outcome::Created);
    assert_eq!(
        line.mirror,
        Some(Mirror {
            table: ckan_datastore::Outcome::Created,
            rows: 2
        })
    );
    assert_eq!(
        line.to_string(),
        "Endpoint/ovzdusie/air-rows: dataset air-rows created, DataStore created (2 rows)"
    );
    assert_eq!(
        api.actions(),
        vec!["package_create", "datastore_create", "datastore_upsert"]
    );
    let fields = api.table_fields(DATASTORE_RESOURCE).expect("the table");
    let types: Vec<(&str, &str)> = fields
        .iter()
        .map(|f| (f["id"].as_str().unwrap(), f["type"].as_str().unwrap()))
        .collect();
    assert_eq!(
        types,
        vec![
            ("entity_id", "text"),
            ("type", "text"),
            ("temperature.value", "float"),
            ("location.value", "json"),
            ("note", "text"),
        ]
    );
    let rows = api.rows(DATASTORE_RESOURCE).expect("rows");
    let first = &rows["urn:ngsi-ld:AirQualityObserved:bb.sk:ovzdusie:1"];
    assert_eq!(first["temperature.value"], json!(12.5));
    assert_eq!(first["location.value"]["coordinates"][0], json!(19.1));
    assert_eq!(first["note"], json!("a note, with a comma"));
    let second = &rows["urn:ngsi-ld:AirQualityObserved:bb.sk:ovzdusie:2"];
    assert_eq!(second["temperature.value"], json!(13));
    assert_eq!(second["location.value"], Value::Null);
    assert_eq!(second["note"], Value::Null);

    let again = publish_one(
        &mut api,
        target,
        &record("air-rows"),
        Some(CSV),
        &settings(),
    )
    .expect("the second run");
    assert_eq!(again.outcome, Outcome::Unchanged);
    assert_eq!(
        again.mirror.map(|m| m.table),
        Some(ckan_datastore::Outcome::Unchanged)
    );
    assert_eq!(
        api.actions(),
        vec![
            "package_create",
            "datastore_create",
            "datastore_upsert",
            "datastore_upsert"
        ],
        "a reload writes rows and nothing else"
    );
}

/// A mirror without rows is an error, not a dataset without its table.
#[test]
fn a_mirror_declared_without_rows_is_refused() {
    let dir = repo("no-rows");
    let repo = Repository::load(&dir).expect("the repository loads");
    let found = targets(&repo, "ovzdusie").expect("the walk");
    let mut api = InMemoryCkan::new().with_organization("mesto-banska-bystrica");

    let error = publish_one(&mut api, &found[1], &record("air-rows"), None, &settings())
        .expect_err("no rows");
    assert!(matches!(error, Error::Rows(_)), "{error}");
}

/// CC-19: a withdrawal drops the table, then the dataset; a second one changes nothing.
#[test]
fn a_withdrawal_drops_the_table_and_the_dataset_once() {
    let dir = repo("withdraw");
    let repo = Repository::load(&dir).expect("the repository loads");
    let found = targets(&repo, "ovzdusie").expect("the walk");
    let target = &found[1];
    let mut api = InMemoryCkan::new().with_organization("mesto-banska-bystrica");
    publish_one(
        &mut api,
        target,
        &record("air-rows"),
        Some(CSV),
        &settings(),
    )
    .expect("published");

    let line = withdraw_one(&mut api, target).expect("withdrawn");
    assert_eq!(line.outcome, Outcome::Withdrawn);
    assert!(api.package("air-rows").is_none());
    assert_eq!(
        api.actions(),
        vec![
            "package_create",
            "datastore_create",
            "datastore_upsert",
            "package_delete"
        ]
    );

    let again = withdraw_one(&mut api, target).expect("nothing to withdraw");
    assert_eq!(again.outcome, Outcome::Unchanged);
    assert_eq!(api.actions().len(), 4);
}

// --- the CSV the gateway writes ------------------------------------------------------------

#[test]
fn the_csv_is_read_as_the_gateway_writes_it() {
    let (columns, rows) = csv_table(CSV).expect("the table");
    assert_eq!(
        columns,
        vec!["id", "type", "temperature.value", "location.value", "note"]
    );
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0][4], "a note, with a comma");
    assert_eq!(
        rows[0][3],
        "{\"type\":\"Point\",\"coordinates\":[19.1,48.7]}"
    );
    assert_eq!(rows[1][3], "");

    let (_, rows) = csv_table("\u{feff}id,text\n\"a\"\"quoted\"\"\",\"two\nlines\"\n\nb,\n")
        .expect("quotes and a blank line");
    assert_eq!(rows, vec![vec!["a\"quoted\"", "two\nlines"], vec!["b", ""]]);

    assert!(matches!(csv_table(""), Err(Error::Rows(_))));
    assert!(matches!(csv_table("id,id\n1,2\n"), Err(Error::Rows(_))));
    assert!(matches!(csv_table("id,x\n\"open"), Err(Error::Rows(_))));
}

#[test]
fn a_cell_is_typed_from_what_the_gateway_wrote() {
    assert_eq!(typed_cell(""), Value::Null);
    assert_eq!(typed_cell("12.5"), json!(12.5));
    assert_eq!(typed_cell("-3"), json!(-3));
    assert_eq!(typed_cell("true"), json!(true));
    assert_eq!(typed_cell("[1,2]"), json!([1, 2]));
    assert_eq!(
        typed_cell("2026-09-07T10:00:00Z"),
        json!("2026-09-07T10:00:00Z")
    );
    assert_eq!(typed_cell("007"), json!("007"));
    assert_eq!(
        typed_cell("urn:ngsi-ld:X:a:b:1"),
        json!("urn:ngsi-ld:X:a:b:1")
    );
}

// --- the token ---------------------------------------------------------------------------------

/// EP-67: the token comes out of the environment the command names, or the variable the
/// reference itself names; nothing is echoed, and with no source the error says what to
/// pass.
#[test]
fn the_token_is_read_from_the_named_environment_and_never_echoed() {
    let dir = repo("token");
    let repo = Repository::load(&dir).expect("the repository loads");
    let found = targets(&repo, "ovzdusie").expect("the walk");
    let instance = &found[0].instance;

    std::env::set_var("JCCTL_TEST_TOKEN_FLAG", "flag-token-value");
    let value = token(
        instance,
        &dir,
        TokenSource {
            env: Some("JCCTL_TEST_TOKEN_FLAG"),
            age_key_file: None,
        },
    )
    .expect("from the flag");
    assert_eq!(value.expose(), "flag-token-value");

    let error = token(
        instance,
        &dir,
        TokenSource {
            env: Some("JCCTL_TEST_TOKEN_UNSET"),
            age_key_file: None,
        },
    )
    .expect_err("unset");
    assert!(
        error.to_string().contains("JCCTL_TEST_TOKEN_UNSET"),
        "{error}"
    );

    // The reference's own `envVar`, as a Job would have it injected.
    std::env::set_var("CKAN_OPEN_DATA_TOKEN", "injected-token-value");
    let value = token(instance, &dir, TokenSource::default()).expect("from envVar");
    assert_eq!(value.expose(), "injected-token-value");
    std::env::remove_var("CKAN_OPEN_DATA_TOKEN");

    // No source at all: the error names the flags and nothing else.
    let error = token(
        instance,
        &dir,
        TokenSource {
            env: None,
            age_key_file: Some(Path::new("/nonexistent/age.key")),
        },
    )
    .expect_err("no key file");
    let message = error.to_string();
    assert!(matches!(error, Error::Token { .. }), "{message}");
    assert!(!message.contains("flag-token-value"), "{message}");
    assert!(!message.contains("injected-token-value"), "{message}");
}
