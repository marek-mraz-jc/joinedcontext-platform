//! `jcctl publish ckan` over a repository laid out the way `apply` reads it (T-0487,
//! EP-62…EP-67, CC-18).
//!
//! The walk, the publication of one target and the withdrawal run here against the
//! in-memory catalogue; the HTTP client has its own tests against a fake CKAN.

mod common;

use jcctl::commands::publish_ckan::{
    csv_table, publish_one, targets, token, typed_cell, withdraw_one, Error, Line, Mirror, Rows,
    TokenSource,
};
use jcctl::loader::Repository;
use jcctl::publish::ckan::{self, CkanApi, CkanError, InMemoryCkan, Outcome, Settings};
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

/// The table the air-quality rows land in: the one named by their entity type (T-3012).
const AIR: &str = "AirQualityObserved";

/// The gateway's answer for a mirror: `csv`, and no model schema.
fn answer(csv: &str) -> Rows {
    Rows {
        csv: csv.to_owned(),
        schema: None,
    }
}

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
        Some(&answer(CSV)),
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
        vec![
            "package_create",
            "datastore_create",
            "resource_view_create",
            "datastore_upsert"
        ]
    );
    // EP-62: the sheet opens as a grid on the dataset page.
    let views = api.views(AIR);
    assert_eq!(views.len(), 1);
    assert_eq!(views[0]["view_type"], json!(ckan_datastore::GRID_VIEW));
    let fields = api.table_fields(AIR).expect("the table");
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
    let rows = api.rows(AIR).expect("rows");
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
        Some(&answer(CSV)),
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
            "resource_view_create",
            "datastore_upsert",
            "datastore_upsert"
        ],
        "a reload writes rows and nothing else"
    );
    assert_eq!(api.views(AIR).len(), 1, "a second grid");
}

/// A catalogue that refuses one action and answers the rest as `InMemoryCkan` does.
struct Refusing {
    ckan: InMemoryCkan,
    action: &'static str,
}

impl CkanApi for Refusing {
    fn show(&self, action: &str, name: &str) -> Result<Option<Value>, CkanError> {
        self.ckan.show(action, name)
    }

    fn action(&mut self, action: &str, payload: &Value) -> Result<Value, CkanError> {
        if action == self.action {
            return Err(CkanError::Rejected {
                action: action.to_owned(),
                message: "refused by the test".to_owned(),
            });
        }
        self.ckan.action(action, payload)
    }
}

/// EP-62 (T-2962): the grid does not wait on the rows. A sync that fails part-way on a large
/// table left a filled DataStore with no view (T-2931); the view is asked for once the table
/// exists, and the sync's error is still the pass's answer.
#[test]
fn a_row_sync_that_fails_still_leaves_the_grid_view() {
    let dir = repo("sync-refused");
    let repo = Repository::load(&dir).expect("the repository loads");
    let found = targets(&repo, "ovzdusie").expect("the walk");
    let mut api = Refusing {
        ckan: InMemoryCkan::new().with_organization("mesto-banska-bystrica"),
        action: "datastore_upsert",
    };

    let error = publish_one(
        &mut api,
        &found[1],
        &record("air-rows"),
        Some(&answer(CSV)),
        &settings(),
    )
    .expect_err("the rows were refused");
    assert!(error.to_string().contains("datastore_upsert"), "{error}");
    let views = api.ckan.views(AIR);
    assert_eq!(views.len(), 1, "the grid is there although no row is");
    assert_eq!(views[0]["view_type"], json!(ckan_datastore::GRID_VIEW));
}

/// EP-62 (T-2962): the other way round, which is why the view once came second: a catalogue
/// that refuses the view still holds today's rows, and the pass says what it refused.
#[test]
fn a_refused_view_still_lands_the_rows() {
    let dir = repo("view-refused");
    let repo = Repository::load(&dir).expect("the repository loads");
    let found = targets(&repo, "ovzdusie").expect("the walk");
    let mut api = Refusing {
        ckan: InMemoryCkan::new().with_organization("mesto-banska-bystrica"),
        action: "resource_view_create",
    };

    let error = publish_one(
        &mut api,
        &found[1],
        &record("air-rows"),
        Some(&answer(CSV)),
        &settings(),
    )
    .expect_err("the view was refused");
    assert!(
        error.to_string().contains("resource_view_create"),
        "{error}"
    );
    assert_eq!(api.ckan.rows(AIR).map(|rows| rows.len()), Some(2));
    assert!(api.ckan.views(AIR).is_empty());
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

/// A CSV of `rows` air-quality rows; row `bad`, when given, has a cell too many.
fn rows_csv(rows: usize, bad: Option<usize>) -> String {
    let mut text = String::from("id,type,temperature.value\r\n");
    for row in 0..rows {
        let extra = if Some(row) == bad { ",surplus" } else { "" };
        text.push_str(&format!(
            "urn:ngsi-ld:AirQualityObserved:bb.sk:ovzdusie:{row},AirQualityObserved,{row}.5{extra}\r\n"
        ));
    }
    text
}

/// T-2968: a table larger than one upsert goes out batch by batch, every row of it, typed from
/// all its rows and not from the first batch alone.
#[test]
fn a_table_larger_than_one_upsert_is_written_in_batches_and_whole() {
    let dir = repo("mirror-batches");
    let repo = Repository::load(&dir).expect("the repository loads");
    let found = targets(&repo, "ovzdusie").expect("the walk");
    let mut api = InMemoryCkan::new().with_organization("mesto-banska-bystrica");

    let line = publish_one(
        &mut api,
        &found[1],
        &record("air-rows"),
        Some(&answer(&rows_csv(1201, None))),
        &settings(),
    )
    .expect("published");

    assert_eq!(line.mirror.map(|m| m.rows), Some(1201));
    let upserts: Vec<usize> = api
        .calls()
        .filter(|(action, _)| *action == "datastore_upsert")
        .map(|(_, payload)| payload["records"].as_array().map_or(0, Vec::len))
        .collect();
    assert_eq!(upserts, vec![500, 500, 201]);
    assert_eq!(api.rows(AIR).map(|rows| rows.len()), Some(1201));
    let fields = api.table_fields(AIR).expect("the table");
    assert_eq!(fields[2]["type"], json!("float"));
}

/// T-2968: a bad row in the last batch refuses the table before the first batch is written, so
/// a publication never leaves half a reload in the catalogue.
#[test]
fn a_bad_row_in_a_later_batch_writes_no_row_at_all() {
    let dir = repo("mirror-bad-row");
    let repo = Repository::load(&dir).expect("the repository loads");
    let found = targets(&repo, "ovzdusie").expect("the walk");
    let mut api = InMemoryCkan::new().with_organization("mesto-banska-bystrica");

    let error = publish_one(
        &mut api,
        &found[1],
        &record("air-rows"),
        Some(&answer(&rows_csv(1201, Some(1100)))),
        &settings(),
    )
    .expect_err("a ragged row is refused");

    assert!(error.to_string().contains("1100"), "{error}");
    assert!(
        !api.actions()
            .iter()
            .any(|action| action.starts_with("datastore_")),
        "{:?}",
        api.actions()
    );
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
        Some(&answer(CSV)),
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
            "resource_view_create",
            "datastore_upsert",
            "datastore_delete",
            "package_delete"
        ]
    );
    assert!(api.rows(AIR).is_none(), "the table left with the dataset");

    let again = withdraw_one(&mut api, target).expect("nothing to withdraw");
    assert_eq!(again.outcome, Outcome::Unchanged);
    assert_eq!(api.actions().len(), 6);
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

// --- the language of the space and the title of the instance (T-2465) ------------------------

/// The demo repository with a Slovak space and an instance that names itself in two languages.
fn slovak_repo(test: &str, instance_title: &str) -> std::path::PathBuf {
    let dir = repo(test);
    common::write(
        &dir,
        "projects/ovzdusie/spaces/ovzdusie/space.yaml",
        "apiVersion: joinedcontext.com/v1alpha1\nkind: ContextSpace\nmetadata:\n  name: ovzdusie\n  namespace: ovzdusie\nspec:\n  isSandbox: false\n  defaultLocale: sk\n",
    );
    common::write(
        &dir,
        "projects/ovzdusie/ckan/open-data.yaml",
        &INSTANCE.replacen(
            "  namespace: ovzdusie\n",
            &format!("  namespace: ovzdusie\n  title: {instance_title}\n"),
            1,
        ),
    );
    dir
}

/// EP-63: the walk reads the language off the Endpoint's space, so the dataset of a Slovak
/// body carries its Slovak title and not the English one listed beside it.
#[test]
fn a_slovak_space_publishes_its_dataset_under_the_slovak_title() {
    let dir = slovak_repo("slovak-title", "Mesto Banská Bystrica");
    let repo = Repository::load(&dir).expect("the repository loads");
    let found = targets(&repo, "ovzdusie").expect("the walk");
    assert!(found.iter().all(|t| t.language.as_deref() == Some("sk")));

    let record = json!({
        "@type": "dcat:Dataset",
        "dct:title": [
            { "@value": "Air quality", "@language": "en" },
            { "@value": "Kvalita ovzdušia", "@language": "sk" }
        ]
    });
    let mut api = InMemoryCkan::new().with_organization("mesto-banska-bystrica");
    publish_one(&mut api, &found[0], &record, None, &settings()).expect("the run");
    let dataset = api.package("kvalita-ovzdusia").expect("the dataset");
    assert_eq!(dataset["title"], json!("Kvalita ovzdušia"));
}

/// A space that names no locale leaves the language open, and the English title stands.
#[test]
fn a_space_without_a_locale_leaves_the_language_open() {
    let dir = repo("no-locale");
    let repo = Repository::load(&dir).expect("the repository loads");
    let found = targets(&repo, "ovzdusie").expect("the walk");
    assert!(found.iter().all(|t| t.language.is_none()));
    assert!(found.iter().all(|t| t.instance_title.is_none()));
}

/// An organization CKAN does not have yet is created with the instance's own title, in the
/// space's language, before the installation's branding name (which on dev names another city).
#[test]
fn a_created_organization_takes_the_instance_title() {
    let dir = slovak_repo(
        "instance-title",
        "{ sk: \"Mesto Banská Bystrica\", en: \"City of Banská Bystrica\" }",
    );
    let repo = Repository::load(&dir).expect("the repository loads");
    let found = targets(&repo, "ovzdusie").expect("the walk");
    assert_eq!(
        found[0].instance_title.as_deref(),
        Some("Mesto Banská Bystrica")
    );

    let mut api = InMemoryCkan::new();
    let branded = Settings::new("data.example.org").titled("Helsinki Region Context");
    publish_one(&mut api, &found[0], &record("air-public"), None, &branded).expect("the run");
    assert_eq!(api.actions(), vec!["organization_create", "package_create"]);
    let (_, organization) = api.calls().next().expect("the organization call");
    assert_eq!(organization["name"], json!("mesto-banska-bystrica"));
    assert_eq!(organization["title"], json!("Mesto Banská Bystrica"));
}

// --- T-2526: the edge cases of `targets` and `token` (EP-62…EP-67, CC-06) --------------------

use age::secrecy::ExposeSecret as _;
use common::sops::{sops_file, Node};
use jcctl::commands::publish_ckan::AGE_KEY_FILE_ENV;

/// The API token the SOPS cases encrypt; a test that finds it in an error has found a leak.
const SEALED_TOKEN: &str = "ckan-api-token-sealed-4f1c";

/// `repo(test)` whose instance reads its token from `envVar: {variable}`, so no two tests share
/// one process-wide variable, plus a SOPS file holding `entries` and the key file that opens it.
fn sealed_repo(
    test: &str,
    variable: &str,
    entries: &[(&str, Node)],
) -> (std::path::PathBuf, std::path::PathBuf) {
    let dir = repo(test);
    common::write(
        &dir,
        "projects/ovzdusie/ckan/open-data.yaml",
        &INSTANCE.replace("CKAN_OPEN_DATA_TOKEN", variable),
    );
    let identity = age::x25519::Identity::generate();
    common::write(
        &dir,
        "secrets/ckan.enc.yaml",
        &sops_file(entries, &identity.to_public()),
    );
    let key_dir = common::temp_dir(&format!("{test}-age"));
    let key_file = key_dir.join("keys.txt");
    std::fs::write(
        &key_file,
        format!("{}\n", identity.to_string().expose_secret()),
    )
    .expect("the key file");
    (dir, key_file)
}

fn instance_of(dir: &Path) -> jc_core::kinds::CkanInstanceSpec {
    let repo = Repository::load(dir).expect("the repository loads");
    targets(&repo, "ovzdusie").expect("the walk")[0]
        .instance
        .clone()
}

/// EP-67, CC-06: no error of the token path repeats the token, whichever source failed.
#[test]
fn the_resolved_token_never_appears_in_an_error() {
    let (dir, key_file) = sealed_repo(
        "token-no-echo",
        "JCCTL_T2526_NO_ECHO",
        &[("ckan-open-data", Node::Keys(&[("otherKey", SEALED_TOKEN)]))],
    );
    let error = token(
        &instance_of(&dir),
        &dir,
        TokenSource {
            env: None,
            age_key_file: Some(&key_file),
        },
    )
    .expect_err("the reference names a key the secret does not hold");
    assert!(matches!(error, Error::Token { .. }), "{error}");
    assert!(!error.to_string().contains(SEALED_TOKEN), "{error}");
    assert!(!format!("{error:?}").contains(SEALED_TOKEN), "{error:?}");
}

/// EP-67: an empty secret is not a token, and publishing with it would be anonymous.
#[test]
fn an_empty_secret_from_sops_is_refused_rather_than_published_as_a_blank_token() {
    let (dir, key_file) = sealed_repo(
        "token-empty",
        "JCCTL_T2526_EMPTY",
        &[("ckan-open-data", Node::Keys(&[("apiToken", "")]))],
    );
    let error = token(
        &instance_of(&dir),
        &dir,
        TokenSource {
            env: None,
            age_key_file: Some(&key_file),
        },
    )
    .expect_err("empty");
    assert!(error.to_string().contains("empty"), "{error}");
}

/// CC-06: with no variable set, the repository's own encrypted secret is the token.
#[test]
fn a_token_decrypted_from_the_repositorys_sops_files_is_used() {
    let (dir, key_file) = sealed_repo(
        "token-sops",
        "JCCTL_T2526_SOPS",
        &[("ckan-open-data", Node::Keys(&[("apiToken", SEALED_TOKEN)]))],
    );
    let value = token(
        &instance_of(&dir),
        &dir,
        TokenSource {
            env: None,
            age_key_file: Some(&key_file),
        },
    )
    .expect("decrypted");
    assert_eq!(value.expose(), SEALED_TOKEN);
}

/// EP-67: the variable named on the command line wins over the reference's own `envVar` and
/// over the repository's secret.
#[test]
fn env_var_named_on_the_command_line_wins_over_the_references_own_env_var() {
    let (dir, key_file) = sealed_repo(
        "token-order",
        "JCCTL_T2526_REFERENCE",
        &[("ckan-open-data", Node::Keys(&[("apiToken", SEALED_TOKEN)]))],
    );
    std::env::set_var("JCCTL_T2526_REFERENCE", "from-the-reference");
    std::env::set_var("JCCTL_T2526_FLAG", "from-the-flag");
    let instance = instance_of(&dir);
    let flagged = token(
        &instance,
        &dir,
        TokenSource {
            env: Some("JCCTL_T2526_FLAG"),
            age_key_file: Some(&key_file),
        },
    )
    .expect("the flag");
    assert_eq!(flagged.expose(), "from-the-flag");
    let referenced = token(
        &instance,
        &dir,
        TokenSource {
            env: None,
            age_key_file: Some(&key_file),
        },
    )
    .expect("the reference's variable");
    assert_eq!(referenced.expose(), "from-the-reference");
    std::env::remove_var("JCCTL_T2526_REFERENCE");
    std::env::remove_var("JCCTL_T2526_FLAG");
}

/// EP-67: a variable the operator named explicitly and left empty is an error naming it; it
/// never quietly falls through to another source the operator did not choose.
#[test]
fn an_explicitly_named_variable_that_is_empty_stops_with_its_name() {
    let (dir, key_file) = sealed_repo(
        "token-explicit-empty",
        "JCCTL_T2526_EXPLICIT_REF",
        &[("ckan-open-data", Node::Keys(&[("apiToken", SEALED_TOKEN)]))],
    );
    std::env::set_var("JCCTL_T2526_BLANK", "   ");
    let error = token(
        &instance_of(&dir),
        &dir,
        TokenSource {
            env: Some("JCCTL_T2526_BLANK"),
            age_key_file: Some(&key_file),
        },
    )
    .expect_err("blank");
    assert!(error.to_string().contains("JCCTL_T2526_BLANK"), "{error}");
    std::env::remove_var("JCCTL_T2526_BLANK");
}

/// EP-67: an unset reference variable is not a token; the walk goes on to the repository.
#[test]
fn an_unset_reference_variable_falls_through_to_the_repository() {
    let (dir, key_file) = sealed_repo(
        "token-fallthrough",
        "JCCTL_T2526_NEVER_SET",
        &[("ckan-open-data", Node::Keys(&[("apiToken", SEALED_TOKEN)]))],
    );
    let value = token(
        &instance_of(&dir),
        &dir,
        TokenSource {
            env: None,
            age_key_file: Some(&key_file),
        },
    )
    .expect("the repository");
    assert_eq!(value.expose(), SEALED_TOKEN);
}

/// EP-67: no source at all says how to give one, and names the age variable.
#[test]
fn no_age_key_file_and_no_env_source_is_refused_naming_how_to_supply_one() {
    let dir = repo("token-no-source");
    common::write(
        &dir,
        "projects/ovzdusie/ckan/open-data.yaml",
        &INSTANCE.replace("CKAN_OPEN_DATA_TOKEN", "JCCTL_T2526_NO_SOURCE"),
    );
    std::env::remove_var(AGE_KEY_FILE_ENV);
    let error = token(&instance_of(&dir), &dir, TokenSource::default()).expect_err("no source");
    let message = error.to_string();
    assert!(message.contains("--api-token-env"), "{message}");
    assert!(message.contains(AGE_KEY_FILE_ENV), "{message}");
}

/// EP-62: an instance reference into a namespace that holds none is an error naming it.
#[test]
fn an_instance_ref_naming_a_missing_namespace_is_refused_by_name() {
    let dir = repo("instance-namespace");
    common::write(
        &dir,
        "projects/ovzdusie/spaces/ovzdusie/endpoints/air-public.yaml",
        &endpoint(
            "air-public",
            "zt4qm7ge2xdv6ksb3ncf5arw2y",
            "[ngsi-ld, csv]",
            "  publish:\n    ckan:\n      instanceRef: { kind: CkanInstance, name: open-data, namespace: doprava }\n      name: kvalita-ovzdusia\n",
        ),
    );
    let repo = Repository::load(&dir).expect("the repository loads");
    let error = targets(&repo, "ovzdusie").expect_err("no instance in doprava");
    assert!(matches!(error, Error::UnknownInstance { .. }), "{error}");
    assert!(error.to_string().contains("doprava"), "{error}");
}

/// EP-67: an instance on plain HTTP never becomes a target: the token would travel in the clear.
#[test]
fn an_instance_on_plain_http_never_becomes_a_target() {
    let dir = repo("instance-http");
    common::write(
        &dir,
        "projects/ovzdusie/ckan/open-data.yaml",
        &INSTANCE.replace(
            "https://data.banskabystrica.sk",
            "http://data.banskabystrica.sk",
        ),
    );
    let walked = Repository::load(&dir)
        .map_err(|e| e.to_string())
        .and_then(|repo| targets(&repo, "ovzdusie").map_err(|e| e.to_string()));
    let error = walked.expect_err("refused at load or by the walk");
    assert!(error.contains("https://"), "{error}");
}

/// PF-28: a title with neither the space's language nor English takes its first entry.
#[test]
fn a_title_map_missing_every_requested_language_falls_back_to_english_then_the_first_entry() {
    let english = slovak_repo(
        "title-en",
        "{ en: \"City of Banská Bystrica\", de: \"Stadt\" }",
    );
    let repo = Repository::load(&english).expect("the repository loads");
    assert_eq!(
        targets(&repo, "ovzdusie").expect("the walk")[0]
            .instance_title
            .as_deref(),
        Some("City of Banská Bystrica")
    );
    let first = slovak_repo("title-first", "{ de: \"Stadt Banská Bystrica\" }");
    let repo = Repository::load(&first).expect("the repository loads");
    assert_eq!(
        targets(&repo, "ovzdusie").expect("the walk")[0]
            .instance_title
            .as_deref(),
        Some("Stadt Banská Bystrica")
    );
}

/// EP-63: a blank default locale is no locale.
#[test]
fn an_empty_default_locale_leaves_language_open() {
    let dir = repo("blank-locale");
    common::write(
        &dir,
        "projects/ovzdusie/spaces/ovzdusie/space.yaml",
        "apiVersion: joinedcontext.com/v1alpha1\nkind: ContextSpace\nmetadata:\n  name: ovzdusie\n  namespace: ovzdusie\nspec:\n  isSandbox: false\n  defaultLocale: \"  \"\n",
    );
    let walked = Repository::load(&dir)
        .map_err(|e| e.to_string())
        .and_then(|repo| targets(&repo, "ovzdusie").map_err(|e| e.to_string()));
    // A blank locale is either refused as a manifest or read as none; never as a language.
    if let Ok(found) = walked {
        assert!(found.iter().all(|t| t.language.is_none()), "{found:?}");
    }
}

// --- one table per entity type, its columns from the model (T-3012) --------------------------

/// Two entity types in one answer, as praha-mesto serves them: points spread by index, a
/// polygon as one JSON cell, a unit, a language map and a relationship.
const MIXED: &str = "id,type,no2.value,no2.unitCode,no2.observedAt,name.languageMap.cs,location.value.type,location.value.coordinates[0],location.value.coordinates[1],refDistrict.object,location.value.coordinates\r\n\
urn:ngsi-ld:AirQualityObserved:praha.eu:ovzdusie:1,AirQualityObserved,21.5,GQ,2026-09-26T08:00:00Z,Karlín,Point,14.45,50.09,,\r\n\
urn:ngsi-ld:AirQualityObserved:praha.eu:ovzdusie:2,AirQualityObserved,,,,,,,,,\r\n\
urn:ngsi-ld:WasteContainerIsle:praha.eu:ovzdusie:7,WasteContainerIsle,,,,,Polygon,,,urn:ngsi-ld:CityDistrict:praha.eu:ovzdusie:4,\"[[[14.1,50.1],[14.2,50.1],[14.2,50.2],[14.1,50.1]]]\"\r\n";

/// The Endpoint's `model.schema.json` as the gateway serves it: `$defs`, one class per type.
fn mixed_schema() -> Value {
    let geo = json!({
        "type": ["object", "null"],
        "x-ngsi-ld-kind": "GeoProperty",
        "properties": { "type": { "type": "string" }, "coordinates": { "type": "array" } }
    });
    json!({
        "$defs": {
            "AirQualityObserved": {
                "properties": {
                    "id": { "type": "string" },
                    "type": { "type": "string" },
                    "no2": { "type": ["number", "null"], "x-ngsi-ld-kind": "Property", "x-unit": { "ucumCode": "ug/m3" } },
                    "name": { "type": ["object", "null"], "x-ngsi-ld-kind": "LanguageProperty" },
                    "location": geo,
                    "observedAt": { "type": ["string", "null"], "format": "date-time", "x-ngsi-ld-kind": "Property", "description": "When it was observed." },
                    "dataProvider": { "type": ["string", "null"], "x-ngsi-ld-kind": "Property" }
                }
            },
            "WasteContainerIsle": {
                "properties": {
                    "id": { "type": "string" },
                    "type": { "type": "string" },
                    "location": geo,
                    "refDistrict": { "type": "string", "x-ngsi-ld-kind": "Relationship" },
                    "accessRestriction": { "type": ["string", "null"], "x-ngsi-ld-kind": "Property" }
                }
            }
        }
    })
}

fn field_ids(api: &InMemoryCkan, table: &str) -> Vec<String> {
    api.table_fields(table)
        .expect("the table")
        .iter()
        .map(|field| field["id"].as_str().unwrap_or_default().to_owned())
        .collect()
}

/// T-3012: an Endpoint serving two entity types fills two tables, each named by its type,
/// holding that type's rows only and a column for every attribute its model declares, the
/// ones no entity carries today included; the single table an earlier run wrote for every
/// type leaves with its resource; the row count is the answer's.
#[test]
fn every_entity_type_gets_its_own_table_with_every_model_attribute() {
    let dir = repo("per-type");
    let repo = Repository::load(&dir).expect("the repository loads");
    let found = targets(&repo, "ovzdusie").expect("the walk");
    let target = &found[1];
    let mut api = InMemoryCkan::new().with_organization("mesto-banska-bystrica");
    // The dataset as a run before T-3012 left it: one table of every type, named DataStore.
    ckan::publish(
        &mut api,
        &target.manifest,
        &target.instance,
        &record("air-rows"),
        &settings(),
    )
    .expect("the dataset");
    let package = api.package("air-rows").expect("the dataset")["id"].clone();
    api.action(
        "datastore_create",
        &json!({ "resource": { "package_id": package, "name": "DataStore" }, "fields": [{ "id": "entity_id", "type": "text" }] }),
    )
    .expect("the old table");
    let rows = Rows {
        csv: MIXED.to_owned(),
        schema: Some(mixed_schema()),
    };

    let line = publish_one(
        &mut api,
        target,
        &record("air-rows"),
        Some(&rows),
        &settings(),
    )
    .expect("published");

    assert_eq!(
        line.mirror.map(|m| m.rows),
        Some(3),
        "every row of the answer"
    );
    assert_eq!(api.rows(AIR).map(|rows| rows.len()), Some(2));
    assert_eq!(
        api.rows("WasteContainerIsle").map(|rows| rows.len()),
        Some(1)
    );
    assert!(api.rows("DataStore").is_none(), "the shared table is gone");
    let resources: Vec<&str> = api.package("air-rows").expect("the dataset")["resources"]
        .as_array()
        .expect("resources")
        .iter()
        .filter(|resource| resource["url_type"] == json!("datastore"))
        .filter_map(|resource| resource["name"].as_str())
        .collect();
    assert_eq!(resources, vec![AIR, "WasteContainerIsle"]);

    let schema = mixed_schema();
    for table in [AIR, "WasteContainerIsle"] {
        let definition = ckan_datastore::definition(&schema, table).expect("the class");
        let missing = ckan_datastore::missing_attributes(definition, &field_ids(&api, table));
        assert!(missing.is_empty(), "{table} has no column for {missing:?}");
    }
    let air = field_ids(&api, AIR);
    assert_eq!(
        air,
        vec![
            "entity_id",
            "type",
            "no2.value",
            "no2.unitCode",
            "no2.observedAt",
            "name.languageMap.cs",
            "location.value.type",
            "location.value.coordinates[0]",
            "location.value.coordinates[1]",
            "dataProvider.value",
            "observedAt.value",
            "location.geojson",
        ],
        "a column another type fills is not in this table"
    );
    let fields = api.table_fields(AIR).expect("the table");
    let observed_at = fields
        .iter()
        .find(|field| field["id"] == json!("observedAt.value"))
        .expect("the model's column");
    assert_eq!(
        observed_at["type"],
        json!("timestamp"),
        "an empty column takes the model's type"
    );
    assert_eq!(observed_at["info"]["notes"], json!("When it was observed."));
    assert_eq!(fields[2]["type"], json!("float"));

    let point = &api.rows(AIR).expect("rows")["urn:ngsi-ld:AirQualityObserved:praha.eu:ovzdusie:1"];
    let geometry: Value = serde_json::from_str(
        point["location.geojson"]
            .as_str()
            .expect("a GeoJSON string"),
    )
    .expect("GeoJSON");
    assert_eq!(
        geometry,
        json!({ "type": "Point", "coordinates": [14.45, 50.09] })
    );
    assert_eq!(point["observedAt.value"], Value::Null);
    let bare = &api.rows(AIR).expect("rows")["urn:ngsi-ld:AirQualityObserved:praha.eu:ovzdusie:2"];
    assert_eq!(
        bare["location.geojson"],
        Value::Null,
        "no geometry, no GeoJSON"
    );

    let isle = field_ids(&api, "WasteContainerIsle");
    assert!(isle.contains(&"refDistrict.object".to_owned()), "{isle:?}");
    assert!(
        isle.contains(&"accessRestriction.value".to_owned()),
        "{isle:?}"
    );
    assert!(
        !isle.iter().any(|field| field.starts_with("no2")),
        "{isle:?}"
    );
    let polygon = &api.rows("WasteContainerIsle").expect("rows")
        ["urn:ngsi-ld:WasteContainerIsle:praha.eu:ovzdusie:7"];
    let geometry: Value = serde_json::from_str(
        polygon["location.geojson"]
            .as_str()
            .expect("a GeoJSON string"),
    )
    .expect("GeoJSON");
    assert_eq!(geometry["type"], json!("Polygon"));
    assert_eq!(geometry["coordinates"][0][1], json!([14.2, 50.1]));

    let calls = api.actions().len();
    let again = publish_one(
        &mut api,
        target,
        &record("air-rows"),
        Some(&rows),
        &settings(),
    )
    .expect("the second run");
    assert_eq!(
        again.mirror.map(|m| m.table),
        Some(ckan_datastore::Outcome::Unchanged)
    );
    assert_eq!(
        api.actions()[calls..],
        ["datastore_upsert", "datastore_upsert"],
        "a reload writes rows and nothing else"
    );
}

/// T-3012: an answer without a row says nothing about which types are gone, so every table
/// stays; a row without a type, or an answer without the column, writes nothing at all.
#[test]
fn an_empty_answer_keeps_the_tables_and_a_row_without_a_type_writes_nothing() {
    let dir = repo("per-type-edges");
    let repo = Repository::load(&dir).expect("the repository loads");
    let found = targets(&repo, "ovzdusie").expect("the walk");
    let target = &found[1];
    let mut api = InMemoryCkan::new().with_organization("mesto-banska-bystrica");
    publish_one(
        &mut api,
        target,
        &record("air-rows"),
        Some(&answer(CSV)),
        &settings(),
    )
    .expect("published");

    let line = publish_one(
        &mut api,
        target,
        &record("air-rows"),
        Some(&answer("id,type,temperature.value\r\n")),
        &settings(),
    )
    .expect("an empty answer is not an error");
    assert_eq!(line.mirror.map(|m| m.rows), Some(0));
    assert_eq!(
        api.rows(AIR).map(|rows| rows.len()),
        Some(2),
        "the table stays"
    );

    let calls = api.actions().len();
    let untyped = "id,type,temperature.value\r\n\
urn:ngsi-ld:AirQualityObserved:bb.sk:ovzdusie:3,AirQualityObserved,1\r\n\
urn:ngsi-ld:AirQualityObserved:bb.sk:ovzdusie:4,,2\r\n";
    let error = publish_one(
        &mut api,
        target,
        &record("air-rows"),
        Some(&answer(untyped)),
        &settings(),
    )
    .expect_err("a row without a type");
    assert!(error.to_string().contains("row 1 has no type"), "{error}");
    let error = publish_one(
        &mut api,
        target,
        &record("air-rows"),
        Some(&answer("id,temperature.value\r\nurn:x,1\r\n")),
        &settings(),
    )
    .expect_err("no type column");
    assert!(error.to_string().contains("'type' column"), "{error}");
    assert!(
        !api.actions()[calls..]
            .iter()
            .any(|action| action.starts_with("datastore_") || *action == "resource_delete"),
        "{:?}",
        &api.actions()[calls..]
    );
}
