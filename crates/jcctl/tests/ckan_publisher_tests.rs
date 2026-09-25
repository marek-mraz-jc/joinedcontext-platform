//! The CKAN publisher: one Endpoint, one dataset, one resource per representation
//! (T-0316, EP-62…EP-67).

use jc_core::kinds::ckan::CkanInstanceSpec;
use jcctl::loader::RawManifest;
use jcctl::publish::ckan::{
    package, publish, withdraw, CkanApi, InMemoryCkan, Outcome, PublishError, Settings, GENERATOR,
};
use serde_json::{json, Value};

const TOKEN: &str = "ckan-api-token-that-must-never-be-published";

fn manifest(yaml: &str) -> RawManifest {
    serde_norway::from_str(yaml).expect("the manifest parses")
}

fn endpoint(representations: &str, publish_block: &str) -> RawManifest {
    manifest(&format!(
        r#"apiVersion: joinedcontext.com/v1alpha1
kind: Endpoint
metadata:
  name: ovzdusie-public
  namespace: ovzdusie
spec:
  contextSpaceRef: ovzdusie
  slug: zt4qm7ge2xdv6ksb3ncf5arw2y
  audience: public
  enabledRepresentations: {representations}
{publish_block}"#
    ))
}

const PUBLISH: &str = r#"  publish:
    ckan:
      instanceRef: { kind: CkanInstance, name: open-data }
      organization: mesto-banska-bystrica
      name: kvalita-ovzdusia
"#;

fn instance() -> CkanInstanceSpec {
    serde_norway::from_str(
        r#"url: https://data.banskabystrica.sk
organizationDefault: mesto-banska-bystrica
apiTokenRef: { name: ckan-open-data, key: apiToken }
"#,
    )
    .expect("the instance spec parses")
}

/// The DCAT-AP record the endpoint itself answers with (EP-27).
fn record() -> Value {
    json!({
        "@context": "https://www.w3.org/ns/dcat.jsonld",
        "@id": "https://data.example.org/api/endpoint/zt4qm7ge2xdv6ksb3ncf5arw2y/",
        "@type": "dcat:Dataset",
        "dct:identifier": "ovzdusie-public",
        "dct:title": [
            { "@value": "Kvalita ovzdušia", "@language": "sk" },
            { "@value": "Air quality", "@language": "en" }
        ],
        "dct:description": [{ "@value": "Hourly air quality observations.", "@language": "en" }],
        "dcat:keyword": ["ovzdusie", "air-quality", "air-quality"],
        "dct:license": "https://creativecommons.org/licenses/by/4.0/",
        "dct:language": "sk",
        "dct:accrualPeriodicity": "http://purl.org/cld/freq/hourly",
        "dct:spatial": { "type": "Polygon", "coordinates": [[[19.1, 48.7], [19.2, 48.7], [19.2, 48.8], [19.1, 48.7]]] },
        "dct:publisher": "Mesto Banská Bystrica"
    })
}

fn settings() -> Settings {
    Settings::new("data.example.org").titled("Mesto Banská Bystrica")
}

fn ckan() -> InMemoryCkan {
    InMemoryCkan::new()
        .with_organization("mesto-banska-bystrica")
        .with_token(TOKEN)
}

fn resource_urls(package: &Value) -> Vec<&str> {
    package["resources"]
        .as_array()
        .expect("resources")
        .iter()
        .map(|resource| resource["url"].as_str().expect("a resource URL"))
        .collect()
}

fn extra<'a>(package: &'a Value, key: &str) -> Option<&'a str> {
    package["extras"]
        .as_array()?
        .iter()
        .find(|extra| extra["key"] == json!(key))?["value"]
        .as_str()
}

/// EP-63: the metadata is the record's, not a second authoring of the same thing.
#[test]
fn the_dataset_metadata_is_the_endpoints_own_dcat_record() {
    let dataset = package(
        &endpoint("[ngsi-ld, csv]", PUBLISH),
        &instance(),
        &record(),
        &settings(),
    )
    .expect("the endpoint publishes")
    .expect("a dataset");

    assert_eq!(dataset["name"], json!("kvalita-ovzdusia"));
    assert_eq!(dataset["owner_org"], json!("mesto-banska-bystrica"));
    assert_eq!(dataset["title"], json!("Air quality"));
    assert_eq!(dataset["notes"], json!("Hourly air quality observations."));
    assert_eq!(extra(&dataset, "identifier"), Some("ovzdusie-public"));
    assert_eq!(extra(&dataset, "language"), Some("sk"));
    assert_eq!(
        extra(&dataset, "frequency"),
        Some("http://purl.org/cld/freq/hourly")
    );
    assert_eq!(extra(&dataset, "publisher"), Some("Mesto Banská Bystrica"));
    assert_eq!(extra(&dataset, "generated_by"), Some(GENERATOR));
    // A licence IRI is not a CKAN licence id, so it stays an extra rather than being
    // written into a register field that would not resolve.
    assert_eq!(
        extra(&dataset, "license_url"),
        Some("https://creativecommons.org/licenses/by/4.0/")
    );
    assert!(dataset.get("license_id").is_none(), "{dataset}");
    // The spatial extension reads GeoJSON out of this extra.
    let spatial: Value =
        serde_json::from_str(extra(&dataset, "spatial").expect("spatial")).expect("GeoJSON");
    assert_eq!(spatial["type"], json!("Polygon"));
    // One concept per locale in the record is one tag in CKAN.
    let tags: Vec<&str> = dataset["tags"]
        .as_array()
        .expect("tags")
        .iter()
        .map(|tag| tag["name"].as_str().expect("a tag name"))
        .collect();
    assert_eq!(tags, vec!["air-quality", "ovzdusie"]);
}

/// EP-64: one resource per enabled representation, each at its own URL under the endpoint.
#[test]
fn every_enabled_representation_becomes_a_resource_under_the_endpoint() {
    let dataset = package(
        &endpoint(
            "[ngsi-ld, sta, geojson, csv, mcp, zip, ogc-features, xlsx, json]",
            PUBLISH,
        ),
        &instance(),
        &record(),
        &settings(),
    )
    .expect("the endpoint publishes")
    .expect("a dataset");

    let base = "https://data.example.org/api/endpoint/zt4qm7ge2xdv6ksb3ncf5arw2y";
    assert_eq!(
        resource_urls(&dataset),
        vec![
            format!("{base}/ngsi-ld/v1/"),
            format!("{base}/sta/v1.1/"),
            format!("{base}/file.geojson"),
            format!("{base}/file.csv"),
            format!("{base}/mcp"),
            format!("{base}/file.zip"),
            format!("{base}/ogc/features/"),
            format!("{base}/file.xlsx"),
            format!("{base}/file.json"),
            format!("{base}/schema/index.json"),
        ]
    );
    let formats: Vec<&str> = dataset["resources"]
        .as_array()
        .expect("resources")
        .iter()
        .map(|resource| resource["format"].as_str().expect("a format"))
        .collect();
    assert_eq!(
        formats,
        vec!["NGSI-LD", "STA", "GeoJSON", "CSV", "MCP", "ZIP", "OGCFEAT", "XLSX", "JSON", "JSON"]
    );
}

/// EP-66: nothing a citizen clicks bypasses the gateway and its policy set.
#[test]
fn no_resource_url_leaves_the_endpoint() {
    let dataset = package(
        &endpoint("[ngsi-ld, csv, geojson]", PUBLISH),
        &instance(),
        &record(),
        &settings(),
    )
    .expect("the endpoint publishes")
    .expect("a dataset");

    let base = "https://data.example.org/api/endpoint/zt4qm7ge2xdv6ksb3ncf5arw2y/";
    for url in resource_urls(&dataset) {
        assert!(url.starts_with(base), "{url} escapes the endpoint");
    }
    assert_eq!(dataset["url"], json!(base));
}

/// EP-62: a dataset is created once and updated afterwards, never duplicated.
#[test]
fn the_first_run_creates_the_dataset_and_the_second_changes_nothing() {
    let mut api = ckan();
    let endpoint = endpoint("[ngsi-ld, csv]", PUBLISH);

    let created = publish(&mut api, &endpoint, &instance(), &record(), &settings())
        .expect("the first run publishes");
    assert_eq!(created, Outcome::Created);
    assert_eq!(api.actions(), vec!["package_create"]);

    let again = publish(&mut api, &endpoint, &instance(), &record(), &settings())
        .expect("the second run publishes");
    assert_eq!(again, Outcome::Unchanged);
    assert_eq!(
        api.actions(),
        vec!["package_create"],
        "a converged run writes nothing"
    );
}

/// EP-24, EP-64: MCP is published for every Endpoint without being listed, because the
/// gateway serves it; `mcp: false` takes the resource off the catalogue with the instance.
#[test]
fn the_mcp_resource_follows_the_opt_out() {
    let mut api = ckan();
    let mcp = "https://data.example.org/api/endpoint/zt4qm7ge2xdv6ksb3ncf5arw2y/mcp";
    publish(
        &mut api,
        &endpoint("[ngsi-ld]", PUBLISH),
        &instance(),
        &record(),
        &settings(),
    )
    .expect("the first run publishes");
    let dataset = api.package("kvalita-ovzdusia").expect("the dataset");
    assert!(resource_urls(dataset).contains(&mcp));

    let off = format!("  mcp: false\n{PUBLISH}");
    let outcome = publish(
        &mut api,
        &endpoint("[ngsi-ld]", &off),
        &instance(),
        &record(),
        &settings(),
    )
    .expect("the second run publishes");
    assert_eq!(outcome, Outcome::Updated);
    let dataset = api.package("kvalita-ovzdusia").expect("the dataset");
    assert!(!resource_urls(dataset).contains(&mcp));
}

/// EP-64: a representation that was switched off loses its resource.
#[test]
fn a_disabled_representation_loses_its_resource() {
    let mut api = ckan();
    publish(
        &mut api,
        &endpoint("[ngsi-ld, csv, geojson]", PUBLISH),
        &instance(),
        &record(),
        &settings(),
    )
    .expect("the first run publishes");

    let outcome = publish(
        &mut api,
        &endpoint("[ngsi-ld]", PUBLISH),
        &instance(),
        &record(),
        &settings(),
    )
    .expect("the second run publishes");

    assert_eq!(outcome, Outcome::Updated);
    assert_eq!(api.actions(), vec!["package_create", "package_update"]);
    let dataset = api.package("kvalita-ovzdusia").expect("the dataset");
    assert_eq!(
        resource_urls(dataset),
        vec![
            "https://data.example.org/api/endpoint/zt4qm7ge2xdv6ksb3ncf5arw2y/ngsi-ld/v1/",
            "https://data.example.org/api/endpoint/zt4qm7ge2xdv6ksb3ncf5arw2y/mcp",
            "https://data.example.org/api/endpoint/zt4qm7ge2xdv6ksb3ncf5arw2y/schema/index.json",
        ]
    );
    // The update addresses the dataset by the id CKAN minted, not by a name that may move.
    let (_, update) = api.calls().nth(1).expect("the update call");
    assert_eq!(update["id"], json!("pkg-kvalita-ovzdusia"));
}

/// The organization is created when the catalogue does not have it yet.
#[test]
fn a_missing_organization_is_created_before_the_dataset() {
    let mut api = InMemoryCkan::new().with_token(TOKEN);

    let outcome = publish(
        &mut api,
        &endpoint("[ngsi-ld]", PUBLISH),
        &instance(),
        &record(),
        &settings(),
    )
    .expect("the run publishes");

    assert_eq!(outcome, Outcome::Created);
    assert_eq!(api.actions(), vec!["organization_create", "package_create"]);
    let (_, organization) = api.calls().next().expect("the organization call");
    assert_eq!(organization["name"], json!("mesto-banska-bystrica"));
    assert_eq!(organization["title"], json!("Mesto Banská Bystrica"));
}

/// The dataset name and the organization default come from the manifests, not from here.
#[test]
fn the_dataset_falls_back_to_the_endpoint_name_and_the_instance_organization() {
    let minimal = r#"  publish:
    ckan:
      instanceRef: { kind: CkanInstance, name: open-data }
"#;
    let dataset = package(
        &endpoint("[csv]", minimal),
        &instance(),
        &record(),
        &settings(),
    )
    .expect("the endpoint publishes")
    .expect("a dataset");

    assert_eq!(dataset["name"], json!("ovzdusie-public"));
    assert_eq!(dataset["owner_org"], json!("mesto-banska-bystrica"));
}

/// An instance without a default organization and a publication without one is an error,
/// not a dataset that lands wherever CKAN feels like.
#[test]
fn a_publication_without_an_organization_is_refused() {
    let mut instance = instance();
    instance.organization_default = None;
    let minimal = r#"  publish:
    ckan:
      instanceRef: { kind: CkanInstance, name: open-data }
"#;

    let error = package(
        &endpoint("[csv]", minimal),
        &instance,
        &record(),
        &settings(),
    )
    .expect_err("no organization");

    assert_eq!(error, PublishError::NoOrganization);
}

/// An endpoint that declares no publication is left alone.
#[test]
fn an_endpoint_without_a_publication_block_is_not_published() {
    let mut api = ckan();

    let outcome = publish(
        &mut api,
        &endpoint("[ngsi-ld]", ""),
        &instance(),
        &record(),
        &settings(),
    )
    .expect("the run completes");

    assert_eq!(outcome, Outcome::NotPublished);
    assert!(api.actions().is_empty(), "{:?}", api.actions());
}

/// CC-19: withdrawing the publication removes the dataset, and doing it twice is fine.
#[test]
fn withdrawing_the_publication_deletes_the_dataset_once() {
    let mut api = ckan();
    publish(
        &mut api,
        &endpoint("[ngsi-ld]", PUBLISH),
        &instance(),
        &record(),
        &settings(),
    )
    .expect("the run publishes");

    assert_eq!(
        withdraw(&mut api, "kvalita-ovzdusia").expect("the dataset is withdrawn"),
        Outcome::Withdrawn
    );
    assert!(api.package("kvalita-ovzdusia").is_none());
    assert_eq!(
        withdraw(&mut api, "kvalita-ovzdusia").expect("a second withdrawal is a no-op"),
        Outcome::Unchanged
    );
    assert_eq!(
        api.actions(),
        vec!["package_create", "package_delete"],
        "a dataset that is already gone is not deleted twice"
    );
}

/// EP-67: the API token belongs to the transport. No payload this module builds carries
/// it, and the manifest that names it cannot hold it inline.
#[test]
fn the_api_token_never_reaches_a_payload() {
    let mut api = ckan();
    publish(
        &mut api,
        &endpoint("[ngsi-ld, csv]", PUBLISH),
        &instance(),
        &record(),
        &settings(),
    )
    .expect("the run publishes");

    assert_eq!(api.token(), TOKEN, "the transport holds the token");
    for (action, payload) in api.calls() {
        let sent = serde_json::to_string(payload).expect("the payload serializes");
        assert!(!sent.contains(TOKEN), "{action} carried the API token");
        assert!(
            !sent.to_lowercase().contains("apitoken"),
            "{action} carried a token field"
        );
    }
    let stored = serde_json::to_string(api.package("kvalita-ovzdusia").expect("the dataset"))
        .expect("the dataset serializes");
    assert!(
        !stored.contains(TOKEN),
        "the published dataset carried the token"
    );

    // The kind has no field a token could be written into, so a manifest cannot carry one.
    let inline = serde_norway::from_str::<CkanInstanceSpec>(
        r#"url: https://data.banskabystrica.sk
apiTokenRef: { name: ckan-open-data, key: apiToken }
apiToken: "a-token-somebody-pasted"
"#,
    );
    assert!(inline.is_err(), "an inline token must not parse");
}

/// A manifest that is not an Endpoint is a programming error, not a silent no-op.
#[test]
fn only_an_endpoint_publishes() {
    let project = manifest(
        r#"apiVersion: joinedcontext.com/v1alpha1
kind: Project
metadata:
  name: ovzdusie
  namespace: org
spec:
  organizationRef: banskabystrica
"#,
    );

    let error =
        package(&project, &instance(), &record(), &settings()).expect_err("not an endpoint");

    assert_eq!(error, PublishError::NotAnEndpoint);
}

/// The publisher reports what CKAN said, and says nothing about the credential it used.
#[test]
fn a_refusal_from_ckan_is_reported_without_a_credential() {
    struct Refusing;
    impl CkanApi for Refusing {
        fn show(
            &self,
            _action: &str,
            _name: &str,
        ) -> Result<Option<Value>, jcctl::publish::ckan::CkanError> {
            Ok(None)
        }
        fn action(
            &mut self,
            action: &str,
            _payload: &Value,
        ) -> Result<Value, jcctl::publish::ckan::CkanError> {
            Err(jcctl::publish::ckan::CkanError::Rejected {
                action: action.to_owned(),
                message: "Authorization Error".to_owned(),
            })
        }
    }

    let error = publish(
        &mut Refusing,
        &endpoint("[ngsi-ld]", PUBLISH),
        &instance(),
        &record(),
        &settings(),
    )
    .expect_err("CKAN refused");

    let message = error.to_string();
    assert!(message.contains("Authorization Error"), "{message}");
    assert!(!message.contains(TOKEN), "{message}");
}

/// The record of an endpoint that also serves its model, as EP-68 describes it.
fn record_with_schema() -> Value {
    let base = "https://data.example.org/api/endpoint/zt4qm7ge2xdv6ksb3ncf5arw2y";
    let mut record = record();
    record["dcat:distribution"] = json!([
        {
            "@type": "dcat:Distribution",
            "dct:title": "NGSI-LD API",
            "dcat:accessURL": format!("{base}/ngsi-ld/v1/"),
            "dcat:mediaType": "application/ld+json"
        },
        {
            "@type": "dcat:Distribution",
            "dct:title": "bb-air-quality model.shacl.ttl",
            "dcat:accessURL": format!("{base}/schema/v2/model.shacl.ttl"),
            "dcat:mediaType": "text/turtle",
            "dcat:byteSize": 4096,
            "dct:conformsTo": "https://www.w3.org/TR/shacl/",
            "spdx:checksum": {
                "@type": "spdx:Checksum",
                "spdx:algorithm": "spdx:checksumAlgorithm_sha256",
                "spdx:checksumValue": "aa11"
            }
        },
        {
            "@type": "dcat:Distribution",
            "dct:title": "bb-air-quality context.jsonld",
            "dcat:accessURL": format!("{base}/schema/v2/context.jsonld"),
            "dcat:mediaType": "application/ld+json",
            "dct:conformsTo": "https://www.w3.org/TR/json-ld11/",
            "spdx:checksum": {
                "@type": "spdx:Checksum",
                "spdx:algorithm": "spdx:checksumAlgorithm_sha256",
                "spdx:checksumValue": "bb22"
            }
        }
    ]);
    record
}

fn resource<'a>(package: &'a Value, url_suffix: &str) -> &'a Value {
    package["resources"]
        .as_array()
        .expect("resources")
        .iter()
        .find(|resource| {
            resource["url"]
                .as_str()
                .is_some_and(|url| url.ends_with(url_suffix))
        })
        .unwrap_or_else(|| panic!("no resource ending in {url_suffix}"))
}

/// EP-68: the model is discoverable from the catalogue in every formalism, not only the data.
#[test]
fn every_schema_artifact_the_record_lists_becomes_its_own_resource() {
    let dataset = package(
        &endpoint("[ngsi-ld]", PUBLISH),
        &instance(),
        &record_with_schema(),
        &settings(),
    )
    .expect("the endpoint publishes")
    .expect("a dataset");

    let base = "https://data.example.org/api/endpoint/zt4qm7ge2xdv6ksb3ncf5arw2y";
    assert_eq!(
        resource_urls(&dataset),
        vec![
            format!("{base}/ngsi-ld/v1/"),
            format!("{base}/mcp"),
            format!("{base}/schema/index.json"),
            format!("{base}/schema/v2/model.shacl.ttl"),
            format!("{base}/schema/v2/context.jsonld"),
        ],
        "a representation distribution is not a schema artifact and must not be repeated"
    );

    let shapes = resource(&dataset, "model.shacl.ttl");
    assert_eq!(shapes["name"], json!("bb-air-quality model.shacl.ttl"));
    assert_eq!(shapes["format"], json!("SHACL"));
    assert_eq!(shapes["mimetype"], json!("text/turtle"));
    assert_eq!(shapes["size"], json!(4096));
    // The digest the record declares is the one a downloader checks against (EP-68).
    assert_eq!(shapes["hash"], json!("aa11"));
    assert_eq!(shapes["hash_algorithm"], json!("sha256"));
    assert_eq!(
        resource(&dataset, "context.jsonld")["format"],
        json!("JSON-LD")
    );
}

/// EP-68: a model regenerated under the same file names is drift, not a converged run.
#[test]
fn a_new_digest_under_the_same_file_name_updates_the_dataset() {
    let mut api = ckan();
    let endpoint = endpoint("[ngsi-ld]", PUBLISH);
    publish(
        &mut api,
        &endpoint,
        &instance(),
        &record_with_schema(),
        &settings(),
    )
    .expect("the first run publishes");

    let mut regenerated = record_with_schema();
    regenerated["dcat:distribution"][1]["spdx:checksum"]["spdx:checksumValue"] = json!("cc33");

    let outcome = publish(&mut api, &endpoint, &instance(), &regenerated, &settings())
        .expect("the second run publishes");

    assert_eq!(outcome, Outcome::Updated);
    let dataset = api.package("kvalita-ovzdusia").expect("the dataset");
    assert_eq!(resource(dataset, "model.shacl.ttl")["hash"], json!("cc33"));
}

/// An endpoint of a given audience, for the visibility mapping of EP-69.
fn endpoint_for(audience: &str) -> RawManifest {
    manifest(&format!(
        r#"apiVersion: joinedcontext.com/v1alpha1
kind: Endpoint
metadata:
  name: ovzdusie-public
  namespace: ovzdusie
spec:
  contextSpaceRef: ovzdusie
  slug: zt4qm7ge2xdv6ksb3ncf5arw2y
  audience: {audience}
  allowedProjects: {projects}
  enabledRepresentations: [ngsi-ld]
{PUBLISH}"#,
        projects = if audience == "project-list" {
            "[doprava]"
        } else {
            "[]"
        }
    ))
}

fn private_of(audience: &str) -> Value {
    package(&endpoint_for(audience), &instance(), &record(), &settings())
        .expect("the endpoint publishes")
        .expect("a dataset")["private"]
        .clone()
}

/// EP-69, PF-45: CKAN shows the same thing the endpoint does, and anything short of
/// `public` is private.
#[test]
fn only_a_public_endpoint_becomes_a_public_dataset() {
    assert_eq!(private_of("public"), json!(false));
    assert_eq!(private_of("organization"), json!(true));
    assert_eq!(private_of("project-list"), json!(true));
}

/// EP-69: closing an endpoint closes its dataset on the next reconcile.
#[test]
fn a_narrowed_audience_flips_the_dataset_to_private() {
    let mut api = ckan();
    publish(
        &mut api,
        &endpoint_for("public"),
        &instance(),
        &record(),
        &settings(),
    )
    .expect("the first run publishes");
    assert_eq!(
        api.package("kvalita-ovzdusia").expect("the dataset")["private"],
        json!(false)
    );

    let outcome = publish(
        &mut api,
        &endpoint_for("organization"),
        &instance(),
        &record(),
        &settings(),
    )
    .expect("the second run publishes");

    assert_eq!(outcome, Outcome::Updated);
    assert_eq!(
        api.package("kvalita-ovzdusia").expect("the dataset")["private"],
        json!(true)
    );
}

// --- licence and language (T-2465) -----------------------------------------------------------

/// A record the way the gateway writes one today: titles in two languages, no licence.
fn bilingual_record() -> Value {
    json!({
        "@type": "dcat:Dataset",
        "dct:title": [
            { "@value": "Air quality in Banská Bystrica", "@language": "en" },
            { "@value": "Kvalita ovzdušia v Banskej Bystrici", "@language": "sk" }
        ],
        "dct:description": [
            { "@value": "Readings of the city's stations.", "@language": "en" },
            { "@value": "Merania staníc mesta.", "@language": "sk" }
        ]
    })
}

const LICENSED: &str = r#"  publish:
    ckan:
      instanceRef: { kind: CkanInstance, name: open-data }
      license: cc-by
"#;

/// EP-62: the licence named in the publication block becomes the dataset's `license_id`
/// when the record names none, which is what the gateway's record does today.
#[test]
fn a_publication_with_a_licence_sets_the_license_id() {
    let dataset = package(
        &endpoint("[csv]", LICENSED),
        &instance(),
        &bilingual_record(),
        &settings(),
    )
    .expect("the endpoint publishes")
    .expect("a dataset");
    assert_eq!(dataset["license_id"], json!("cc-by"));

    // And nothing when neither says one: no licence is invented (Architecture/21 §2).
    let unlicensed = package(
        &endpoint(
            "[csv]",
            "  publish:\n    ckan:\n      instanceRef: open-data\n",
        ),
        &instance(),
        &bilingual_record(),
        &settings(),
    )
    .expect("the endpoint publishes")
    .expect("a dataset");
    assert!(unlicensed.get("license_id").is_none(), "{unlicensed}");
}

/// EP-63: a licence the record carries is the Endpoint's own statement and wins over the
/// block, so the dataset and the record never disagree.
#[test]
fn the_records_own_licence_wins_over_the_publication_block() {
    let mut with_licence = bilingual_record();
    with_licence["dct:license"] = json!("odc-by");
    let dataset = package(
        &endpoint("[csv]", LICENSED),
        &instance(),
        &with_licence,
        &settings(),
    )
    .expect("the endpoint publishes")
    .expect("a dataset");
    assert_eq!(dataset["license_id"], json!("odc-by"));
}

/// EP-63: a Slovak space's dataset takes the Slovak title and notes, not the English ones
/// the record lists first.
#[test]
fn the_space_language_picks_the_title_and_the_notes() {
    let dataset = package(
        &endpoint("[csv]", LICENSED),
        &instance(),
        &bilingual_record(),
        &settings().in_language("sk"),
    )
    .expect("the endpoint publishes")
    .expect("a dataset");
    assert_eq!(
        dataset["title"],
        json!("Kvalita ovzdušia v Banskej Bystrici")
    );
    assert_eq!(dataset["notes"], json!("Merania staníc mesta."));

    // Without a language, and with one the record does not carry, English is the answer.
    for settings in [settings(), settings().in_language("de")] {
        let dataset = package(
            &endpoint("[csv]", LICENSED),
            &instance(),
            &bilingual_record(),
            &settings,
        )
        .expect("the endpoint publishes")
        .expect("a dataset");
        assert_eq!(dataset["title"], json!("Air quality in Banská Bystrica"));
    }
}

/// CC-18: a licence and a language change nothing on a second run.
#[test]
fn a_licensed_slovak_dataset_is_unchanged_on_the_second_run() {
    let mut api = ckan();
    let settings = settings().in_language("sk");
    let target = endpoint("[csv]", LICENSED);
    let first = publish(
        &mut api,
        &target,
        &instance(),
        &bilingual_record(),
        &settings,
    )
    .expect("the first run");
    assert_eq!(first, Outcome::Created);
    let second = publish(
        &mut api,
        &target,
        &instance(),
        &bilingual_record(),
        &settings,
    )
    .expect("the second run");
    assert_eq!(second, Outcome::Unchanged);
}

/// A record with the catalogue block, in the shape the gateway answers it (EP-78).
fn catalogued_record() -> Value {
    json!({
        "@id": "https://data.example.org/api/endpoint/zt4qm7ge2xdv6ksb3ncf5arw2y",
        "@type": "dcat:Dataset",
        "dct:identifier": "zt4qm7ge2xdv6ksb3ncf5arw2y",
        "dct:title": [{ "@value": "Kvalita ovzdušia", "@language": "sk" }],
        "dct:description": [{ "@value": "Hodinové merania.", "@language": "sk" }],
        "dct:publisher": {
            "@id": "https://www.banskabystrica.sk/",
            "@type": "foaf:Agent",
            "foaf:name": [
                { "@value": "City of Banská Bystrica", "@language": "en" },
                { "@value": "Mesto Banská Bystrica", "@language": "sk" }
            ]
        },
        "dcat:contactPoint": {
            "@type": "vcard:Kind",
            "vcard:fn": "Otvorené dáta",
            "vcard:hasEmail": "mailto:opendata@example.org"
        },
        "dct:license": "http://publications.europa.eu/resource/authority/licence/CC_BY_4_0",
        "dcat:theme": ["http://publications.europa.eu/resource/authority/data-theme/ENVI"],
        "dcat:keyword": [{ "@value": "ovzdušie", "@language": "sk" }, { "@value": "air", "@language": "en" }],
        "dct:spatial": ["http://data.europa.eu/nuts/code/SK032"],
        "dct:temporal": { "@type": "dct:PeriodOfTime", "dcat:startDate": "2020-01-01" },
        "dct:accrualPeriodicity": "http://publications.europa.eu/resource/authority/frequency/HOURLY",
        "dcat:distribution": [{
            "@id": "https://data.example.org/api/endpoint/zt4qm7ge2xdv6ksb3ncf5arw2y/schema/v1/model.schema.json",
            "@type": "dcat:Distribution",
            "dct:title": "AirQuality model.schema.json",
            "dcat:accessURL": "https://data.example.org/api/endpoint/zt4qm7ge2xdv6ksb3ncf5arw2y/schema/v1/model.schema.json",
            "dcat:mediaType": "https://www.iana.org/assignments/media-types/application/schema+json",
            "dcat:byteSize": "812",
            "spdx:checksum": { "@type": "spdx:Checksum", "spdx:checksumValue": "4a1f" }
        }]
    })
}

/// EP-63, EP-78: CKAN harvests the catalogue block, field for field, in the space's language.
#[test]
fn the_catalogue_block_becomes_the_ckan_dataset_fields() {
    let dataset = package(
        &endpoint("[ngsi-ld, csv]", PUBLISH),
        &instance(),
        &catalogued_record(),
        &Settings::new("data.example.org").in_language("sk"),
    )
    .expect("the endpoint publishes")
    .expect("a dataset");

    assert_eq!(dataset["license_id"], json!("cc-by"));
    assert_eq!(
        extra(&dataset, "license_url"),
        Some("http://publications.europa.eu/resource/authority/licence/CC_BY_4_0")
    );
    assert_eq!(
        extra(&dataset, "publisher_name"),
        Some("Mesto Banská Bystrica")
    );
    assert_eq!(
        extra(&dataset, "publisher_uri"),
        Some("https://www.banskabystrica.sk/")
    );
    assert_eq!(extra(&dataset, "contact_name"), Some("Otvorené dáta"));
    assert_eq!(
        extra(&dataset, "contact_email"),
        Some("opendata@example.org")
    );
    assert_eq!(extra(&dataset, "temporal_start"), Some("2020-01-01"));
    assert_eq!(
        extra(&dataset, "spatial_uri"),
        Some("http://data.europa.eu/nuts/code/SK032")
    );
    assert_eq!(
        extra(&dataset, "spatial"),
        None,
        "GeoJSON only under `spatial`"
    );
    assert_eq!(
        extra(&dataset, "frequency"),
        Some("http://publications.europa.eu/resource/authority/frequency/HOURLY")
    );
    assert!(extra(&dataset, "theme").is_some_and(|t| t.contains("data-theme/ENVI")));
    // Typed nodes are written field by field, never as a JSON dump.
    assert_eq!(extra(&dataset, "publisher"), None);
    assert_eq!(extra(&dataset, "contact_point"), None);
    assert_eq!(extra(&dataset, "temporal"), None);
    let tags: Vec<&str> = dataset["tags"]
        .as_array()
        .expect("tags")
        .iter()
        .filter_map(|tag| tag["name"].as_str())
        .collect();
    assert!(
        tags.contains(&"ovzdušie") && tags.contains(&"air"),
        "{tags:?}"
    );

    let schema = dataset["resources"]
        .as_array()
        .expect("resources")
        .iter()
        .find(|r| {
            r["url"]
                .as_str()
                .is_some_and(|u| u.ends_with("model.schema.json"))
        })
        .expect("the schema artifact resource");
    assert_eq!(schema["mimetype"], json!("application/schema+json"));
    assert_eq!(schema["size"], json!(812));
}
