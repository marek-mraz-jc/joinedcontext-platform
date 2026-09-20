//! T-0314/T-0316: `kind: CkanInstance` and `Endpoint.spec.publish.ckan` (EP-62…EP-67).

use jc_core::error::Error;
use jc_core::kinds::ckan::DataStoreRefresh;
use jc_core::kinds::endpoint::Representation;
use jc_core::kinds::{CkanInstance, EndpointSpec};

fn instance(yaml: &str) -> Result<CkanInstance, Error> {
    let parsed: CkanInstance =
        serde_norway::from_str(yaml).map_err(|e| Error::Parse(e.to_string()))?;
    parsed.validate()?;
    Ok(parsed)
}

fn endpoint(publish: &str) -> Result<EndpointSpec, Error> {
    let spec: EndpointSpec = serde_norway::from_str(&format!(
        r#"contextSpaceRef: ovzdusie
slug: zt4qm7ge2xdv6ksb3ncf5arw2y
audience: public
enabledRepresentations: [ngsi-ld, geojson, csv]
{publish}"#
    ))
    .map_err(|e| Error::Parse(e.to_string()))?;
    spec.validate()?;
    Ok(spec)
}

const INSTANCE: &str = r#"apiVersion: joinedcontext.com/v1alpha1
kind: CkanInstance
metadata:
  name: open-data
  namespace: ovzdusie
spec:
  url: https://data.banskabystrica.sk/
  organizationDefault: mesto-banska-bystrica
  apiTokenRef: { name: ckan-open-data, key: apiToken }
"#;

#[test]
fn an_instance_names_a_catalogue_and_a_secret_reference() {
    let parsed = instance(INSTANCE).expect("the instance validates");

    assert_eq!(parsed.spec.base_url(), "https://data.banskabystrica.sk");
    assert_eq!(parsed.spec.api_token_ref.name, "ckan-open-data");
    assert_eq!(parsed.spec.api_token_ref.key.as_deref(), Some("apiToken"));
}

/// EP-67: plain HTTP would carry the API token in the clear on every publication.
#[test]
fn a_catalogue_over_plain_http_is_refused() {
    let error = instance(&INSTANCE.replace("https://", "http://")).expect_err("http is refused");

    assert!(matches!(error, Error::Name { field: "url", .. }), "{error}");
}

#[test]
fn a_url_without_a_host_is_refused() {
    let error = instance(&INSTANCE.replace("https://data.banskabystrica.sk/", "https://"))
        .expect_err("no host");

    assert!(matches!(error, Error::Name { field: "url", .. }), "{error}");
}

/// EP-67, CC-06: the kind has no field a token could be written into.
#[test]
fn an_inline_token_does_not_parse() {
    let with_token = INSTANCE.replace(
        "  apiTokenRef: { name: ckan-open-data, key: apiToken }\n",
        "  apiTokenRef: { name: ckan-open-data, key: apiToken }\n  apiToken: \"pasted-by-hand\"\n",
    );

    let error = instance(&with_token).expect_err("an inline token is refused");

    assert!(matches!(error, Error::Parse(_)), "{error}");
}

#[test]
fn a_ckan_slug_is_what_ckan_accepts() {
    for bad in [
        "A",
        "Mesto",
        "mesto banska",
        "-mesto",
        "mesto-",
        "m",
        "mesto.bb",
    ] {
        let yaml = INSTANCE.replace("mesto-banska-bystrica", bad);
        assert!(instance(&yaml).is_err(), "`{bad}` must not validate");
    }
    for good in ["bb", "mesto_banska_bystrica", "open-data-2026"] {
        let yaml = INSTANCE.replace("mesto-banska-bystrica", good);
        assert!(instance(&yaml).is_ok(), "`{good}` must validate");
    }
}

/// EP-62: the publication names where the dataset goes, and nothing more.
#[test]
fn an_endpoint_publishes_to_a_named_instance() {
    let spec = endpoint(
        r#"publish:
  ckan:
    instanceRef: { kind: CkanInstance, name: open-data }
    organization: mesto-banska-bystrica
    name: kvalita-ovzdusia
    datastore: { representation: csv, refresh: onChange }
"#,
    )
    .expect("the endpoint validates");

    let publication = spec
        .publish
        .as_ref()
        .and_then(|p| p.ckan.as_ref())
        .expect("a CKAN publication");
    assert_eq!(
        publication.dataset_name("ovzdusie-public"),
        "kvalita-ovzdusia"
    );
    let datastore = publication.datastore.as_ref().expect("a datastore");
    assert_eq!(datastore.representation, Representation::Csv);
    assert_eq!(datastore.refresh, DataStoreRefresh::OnChange);
    assert_eq!(datastore.refresh.as_str(), "onChange");
}

#[test]
fn a_publication_without_a_name_is_the_endpoints_own_name() {
    let spec = endpoint(
        r#"publish:
  ckan:
    instanceRef: { kind: CkanInstance, name: open-data }
"#,
    )
    .expect("the endpoint validates");

    let publication = spec
        .publish
        .and_then(|p| p.ckan)
        .expect("a CKAN publication");
    assert_eq!(
        publication.dataset_name("ovzdusie-public"),
        "ovzdusie-public"
    );
    assert!(
        publication.datastore.is_none(),
        "no mirror unless asked for"
    );
}

/// EP-65: a mirror reads through the endpoint, so it can only read what the endpoint serves.
#[test]
fn a_mirror_of_a_representation_the_endpoint_does_not_serve_is_refused() {
    let error = endpoint(
        r#"publish:
  ckan:
    instanceRef: { kind: CkanInstance, name: open-data }
    datastore: { representation: xlsx }
"#,
    )
    .expect_err("xlsx is not enabled");

    assert!(
        matches!(
            error,
            Error::Name {
                field: "publish.ckan.datastore.representation",
                ..
            }
        ),
        "{error}"
    );
}

/// EP-65, EP-44: a DataStore table is rows, so it is filled from a tabular representation.
#[test]
fn a_mirror_of_a_non_tabular_representation_is_refused() {
    let error = endpoint(
        r#"publish:
  ckan:
    instanceRef: { kind: CkanInstance, name: open-data }
    datastore: { representation: geojson }
"#,
    )
    .expect_err("geojson is not tabular");

    assert!(
        matches!(
            error,
            Error::Name {
                field: "publish.ckan.datastore.representation",
                ..
            }
        ),
        "{error}"
    );
}

#[test]
fn the_publication_must_reference_a_ckan_instance() {
    let error = endpoint(
        r#"publish:
  ckan:
    instanceRef: { kind: ContextSpace, name: open-data }
"#,
    )
    .expect_err("the wrong kind is refused");

    assert!(
        matches!(
            error,
            Error::Kind {
                expected: "CkanInstance",
                ..
            }
        ),
        "{error}"
    );
}

#[test]
fn an_endpoint_without_a_publish_block_publishes_nowhere() {
    let spec = endpoint("").expect("the endpoint validates");

    assert!(spec.publish.is_none());
}
