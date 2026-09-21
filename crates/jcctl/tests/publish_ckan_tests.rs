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
