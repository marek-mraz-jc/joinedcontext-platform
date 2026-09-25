//! The Endpoint's catalogue block in its DCAT-AP record and its ODRL offer (T-2789, EP-78,
//! EP-79, EP-80, DS-01).
//!
//! **The contract.** An Endpoint that declares `spec.catalog` answers a record that carries
//! every declared member as the DCAT-AP 3.0 term, typed as the SEMIC shapes require; its
//! licence becomes ODRL duties in the record's offer and on every permission of the access
//! surface; a non-public Endpoint's offer names its audience class and never a project.
//!
//! **SEMIC shapes.** Every variant below is also written as JSON-LD and Turtle to
//! `DCAT_RECORDS_DIR` (else `target/tmp/dcat-records`), and `scripts/ci/validate-dcat-ap.py` validates each against the pinned
//! SEMIC DCAT-AP 3.0 shapes and checks that both serialisations are one graph. The fast CI
//! lane runs the two together; the assertions here run everywhere.

use context_gateway::handlers::access_odrl;
use context_gateway::handlers::endpoint_surface::{dataset, dataset_turtle};
use context_gateway::pdp::evaluator::Subject;
use context_gateway::resolver::Endpoint;
use jc_core::kinds::{Audience, Catalog, Licence, PolicySpec, Representation};
use serde_json::{json, Value};
use std::sync::Arc;

const SLUG: &str = "k4y7pq2mzt6vhx3nbwrs5cjd8f";
const BASE: &str = "https://data.example.org";
const SHARE_ALIKE: &str = "http://creativecommons.org/ns#ShareAlike";

fn policy(assignee: &str) -> PolicySpec {
    serde_norway::from_str(&format!(
        "contextSpaceRef: events\n\
         assigner: did:web:city.example.org\n\
         assignee: {{ kind: role, id: {assignee} }}\n\
         operations: [queryEntity, retrieveEntity]\n\
         information:\n  - entities:\n      - type: Event\n"
    ))
    .expect("the policy spec parses")
}

fn catalog(yaml: &str) -> Catalog {
    let catalog: Catalog = serde_norway::from_str(yaml).expect("the catalog parses");
    catalog.validate().expect("the catalog validates");
    catalog
}

const FULL: &str = "publisher: { name: { en: City of Example, sk: Mesto Príklad }, uri: 'https://city.example.org/' }\n\
contactPoint: { name: Open data desk, email: opendata@city.example.org }\n\
license: CC_BY_4_0\n\
attribution: { en: 'Source: City of Example' }\n\
themes: [TRAN, SOCI]\n\
keywords: { en: [events, culture], sk: [podujatia] }\n\
spatial: [SK032, 'https://sws.geonames.org/3061186/']\n\
temporal: { start: 2019-01-01, end: 2026-12-31 }\n\
frequency: DAILY\n\
source: [{ url: 'https://opendata.example.org/dataset/events', title: { en: Events }, description: { en: The city's events feed } }]\n\
pipelineRef: { kind: Pipeline, name: city-events }\n\
applicableLegislation: ['http://data.europa.eu/eli/reg_impl/2023/138/oj']\n";

fn endpoint(audience: Audience, catalog: Option<Catalog>) -> Endpoint {
    Endpoint {
        declared_types: None,
        roles: Default::default(),
        slug: SLUG.to_owned(),
        title: [("en".to_owned(), "City events".to_owned())].into(),
        description: [("en".to_owned(), "Every event the city lists".to_owned())].into(),
        space: "events".to_owned(),
        project: "city".to_owned(),
        audience,
        allowed_projects: if audience == Audience::ProjectList {
            vec!["secret-partner".to_owned()]
        } else {
            Vec::new()
        },
        representations: vec![
            Representation::NgsiLd,
            Representation::GeoJson,
            Representation::Csv,
            Representation::Xlsx,
            Representation::Json,
            Representation::Zip,
            Representation::OgcFeatures,
            Representation::Sta,
            Representation::Mcp,
        ],
        rate_limit: None,
        file_limits: None,
        hidden_attributes: Default::default(),
        projection: None,
        base_path: format!("/api/endpoint/{SLUG}"),
        models: Vec::new(),
        view_mapping: None,
        catalog: catalog.map(Arc::new),
        policies: vec![policy("public"), policy("steward")],
    }
}

/// A schema index with one artifact that has a digest and a size, as the schema surface builds it.
fn index() -> Value {
    json!({ "models": [{ "name": "Event", "version": 1, "artifacts": {
        "model.schema.json": { "type": "application/schema+json", "bytes": 812,
            "sha256": "4a1f0c3e9b8d7a6f5e4d3c2b1a0f9e8d7c6b5a4f3e2d1c0b9a8f7e6d5c4b3a2f" },
        "model.shacl.ttl": { "type": "text/turtle; charset=utf-8", "bytes": 300,
            "sha256": "0f1e2d3c4b5a69788796a5b4c3d2e1f00f1e2d3c4b5a69788796a5b4c3d2e1f0" },
    } }] })
}

/// Every record variant the SEMIC check sees: no catalogue, each licence class, each audience.
fn variants() -> Vec<(&'static str, Endpoint)> {
    let with = |licence: &str| catalog(&FULL.replace("CC_BY_4_0", licence));
    vec![
        ("public-bare", endpoint(Audience::Public, None)),
        (
            "public-cc-by",
            endpoint(Audience::Public, Some(catalog(FULL))),
        ),
        ("public-cc0", endpoint(Audience::Public, Some(with("CC0")))),
        (
            "public-odbl",
            endpoint(Audience::Public, Some(with("ODC_ODBL"))),
        ),
        (
            "organization-cc-by-sa",
            endpoint(Audience::Organization, Some(with("CC_BYSA_4_0"))),
        ),
        (
            "project-list-odc-by",
            endpoint(Audience::ProjectList, Some(with("ODC_BY"))),
        ),
        ("restricted-bare", endpoint(Audience::Organization, None)),
        (
            "public-minimal",
            endpoint(Audience::Public, Some(catalog("license: ODC_PDDL\n"))),
        ),
    ]
}

fn record(endpoint: &Endpoint) -> Value {
    dataset(endpoint, None, &index(), BASE)
}

fn distributions(record: &Value) -> Vec<&Value> {
    record["dcat:distribution"]
        .as_array()
        .map(|items| items.iter().collect())
        .unwrap_or_default()
}

/// The offer in `odrl:hasPolicy`, beside the access pointer when there is one.
fn offer(record: &Value) -> Option<&Value> {
    match &record["odrl:hasPolicy"] {
        Value::Object(_) => Some(&record["odrl:hasPolicy"]),
        Value::Array(items) => items.iter().find(|item| item.is_object()),
        _ => None,
    }
}

fn duty_actions(permission: &Value) -> Vec<String> {
    permission["odrl:duty"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|duty| duty["odrl:action"].as_str().map(str::to_owned))
        .collect()
}

#[test]
fn every_variant_is_written_for_the_semic_check() {
    let directory = std::env::var("DCAT_RECORDS_DIR")
        .unwrap_or_else(|_| format!("{}/dcat-records", env!("CARGO_TARGET_TMPDIR")));
    std::fs::create_dir_all(&directory).expect("the records directory");
    for (name, endpoint) in variants() {
        let json = serde_json::to_string_pretty(&record(&endpoint)).expect("json");
        std::fs::write(format!("{directory}/{name}.jsonld"), json).expect("jsonld written");
        let turtle = dataset_turtle(&endpoint, None, &index(), BASE);
        std::fs::write(format!("{directory}/{name}.ttl"), turtle).expect("turtle written");
    }
}

#[test]
fn a_full_catalogue_block_is_rendered_as_dcat_ap_terms() {
    let record = record(&endpoint(Audience::Public, Some(catalog(FULL))));
    assert_eq!(record["dct:publisher"]["@id"], "https://city.example.org/");
    assert_eq!(record["dct:publisher"]["@type"], "foaf:Agent");
    assert_eq!(
        record["dcat:contactPoint"]["vcard:hasEmail"],
        "mailto:opendata@city.example.org"
    );
    assert_eq!(
        record["dcat:theme"],
        json!([
            "http://publications.europa.eu/resource/authority/data-theme/TRAN",
            "http://publications.europa.eu/resource/authority/data-theme/SOCI"
        ])
    );
    assert_eq!(
        record["dct:spatial"],
        json!([
            "http://data.europa.eu/nuts/code/SK032",
            "https://sws.geonames.org/3061186/"
        ])
    );
    assert_eq!(record["dct:temporal"]["dcat:startDate"], "2019-01-01");
    assert_eq!(
        record["dct:accrualPeriodicity"],
        "http://publications.europa.eu/resource/authority/frequency/DAILY"
    );
    assert_eq!(
        record["dct:source"],
        json!(["https://opendata.example.org/dataset/events"])
    );
    assert_eq!(record["prov:wasGeneratedBy"]["@type"], "prov:Activity");
    assert_eq!(
        record["dcatap:applicableLegislation"],
        json!(["http://data.europa.eu/eli/reg_impl/2023/138/oj"])
    );
    let keywords = record["dcat:keyword"].as_array().expect("keywords");
    assert!(keywords.contains(&json!({ "@value": "podujatia", "@language": "sk" })));
}

#[test]
fn every_distribution_carries_the_licence_the_attribution_and_the_data_service() {
    let record = record(&endpoint(Audience::Public, Some(catalog(FULL))));
    let licence = "http://publications.europa.eu/resource/authority/licence/CC_BY_4_0";
    assert_eq!(distributions(&record).len(), 11, "{record:#}");
    for distribution in distributions(&record) {
        assert_eq!(distribution["dct:license"], licence, "{distribution:#}");
        assert_eq!(distribution["dct:rights"]["@type"], "dct:RightsStatement");
        assert!(distribution["dcat:mediaType"]
            .as_str()
            .is_some_and(|m| m.starts_with("https://www.iana.org/assignments/media-types/")));
    }
    let service = record["@included"]
        .as_array()
        .and_then(|items| items.iter().find(|i| i["@type"] == "dcat:DataService"))
        .expect("the endpoint is a data service");
    assert_eq!(
        service["dcat:endpointURL"],
        format!("{BASE}/api/endpoint/{SLUG}/")
    );
    for distribution in distributions(&record)
        .iter()
        .filter(|d| d["dct:format"].is_string())
    {
        assert_eq!(distribution["dcat:accessService"], service["@id"]);
    }
}

#[test]
fn a_media_type_with_parameters_is_named_by_its_bare_iana_iri() {
    let record = record(&endpoint(Audience::Public, None));
    let shacl = distributions(&record)
        .into_iter()
        .find(|d| {
            d["dcat:accessURL"]
                .as_str()
                .is_some_and(|u| u.ends_with("model.shacl.ttl"))
        })
        .expect("the SHACL artifact");
    assert_eq!(
        shacl["dcat:mediaType"],
        "https://www.iana.org/assignments/media-types/text/turtle"
    );
    assert_eq!(shacl["dcat:byteSize"], "300");
}

#[test]
fn a_record_without_a_description_says_which_space_it_serves() {
    let mut bare = endpoint(Audience::Public, None);
    bare.description.clear();
    let record = record(&bare);
    assert!(
        record["dct:description"]
            .as_str()
            .is_some_and(|d| d.contains("events")),
        "{record:#}"
    );
}

/// EP-79: one ODRL test per licence class.
#[test]
fn the_offer_carries_the_duties_of_each_licence_class() {
    for (licence, expected) in [
        ("CC0", vec![]),
        ("ODC_PDDL", vec![]),
        ("CC_BY_4_0", vec!["odrl:attribute"]),
        ("ODC_BY", vec!["odrl:attribute"]),
        ("CC_BYSA_4_0", vec!["odrl:attribute", SHARE_ALIKE]),
        ("ODC_ODBL", vec!["odrl:attribute", SHARE_ALIKE]),
    ] {
        let record = record(&endpoint(
            Audience::Public,
            Some(catalog(&format!("license: {licence}\n"))),
        ));
        let offer = offer(&record).unwrap_or_else(|| panic!("{licence}: no offer"));
        assert_eq!(offer["@type"], json!(["odrl:Offer", "odrl:Policy"]));
        assert_eq!(offer["odrl:permission"]["odrl:action"], "odrl:use");
        assert_eq!(
            duty_actions(&offer["odrl:permission"]),
            expected,
            "{licence}"
        );
        // Nobody named the publisher, so the organization every policy names offers it.
        assert_eq!(offer["odrl:assigner"], "did:web:city.example.org");
    }
}

#[test]
fn an_endpoint_without_a_licence_makes_no_offer() {
    let public = record(&endpoint(Audience::Public, None));
    assert!(public.get("odrl:hasPolicy").is_none(), "{public:#}");
    let restricted = record(&endpoint(Audience::Organization, None));
    assert_eq!(
        restricted["odrl:hasPolicy"],
        format!("{BASE}/api/endpoint/{SLUG}/access")
    );
}

/// EP-79, EP-80: the audience class is named, a project never is.
#[test]
fn a_restricted_offer_names_the_audience_class_and_no_project() {
    let catalog = catalog(FULL);
    for audience in [Audience::Organization, Audience::ProjectList] {
        let record = record(&endpoint(audience, Some(catalog.clone())));
        let policies = record["odrl:hasPolicy"]
            .as_array()
            .expect("pointer and offer");
        assert_eq!(policies[0], format!("{BASE}/api/endpoint/{SLUG}/access"));
        let constraints = &offer(&record).expect("offer")["odrl:permission"]["odrl:constraint"];
        assert_eq!(constraints[0]["odrl:leftOperand"], "odrl:recipient");
        assert_eq!(
            constraints[0]["odrl:rightOperand"]["@id"],
            "https://city.example.org/"
        );
        assert_eq!(constraints[1]["odrl:rightOperand"], audience.as_str());
        let text = record.to_string();
        assert!(!text.contains("secret-partner"), "{audience}: {text}");
        assert!(!text.contains("steward"), "{audience}: {text}");
    }
}

/// EP-79: the access surface's ODRL `Set` carries the same duties on every permission.
#[test]
fn the_access_policy_carries_the_licence_duties_on_every_permission() {
    for (licence, expected) in [
        ("CC0", vec![]),
        ("CC_BY_4_0", vec!["attribute"]),
        ("CC_BYSA_4_0", vec!["attribute", SHARE_ALIKE]),
    ] {
        let endpoint = endpoint(
            Audience::Public,
            Some(catalog(&format!("license: {licence}\n"))),
        );
        let document = access_odrl::policy(
            &Subject::anonymous(),
            &endpoint,
            chrono::Utc::now(),
            BASE,
            |_| "digest".to_owned(),
        );
        let permissions = document["permission"].as_array().expect("permissions");
        assert!(!permissions.is_empty());
        for permission in permissions {
            let actions: Vec<&str> = permission["duty"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|duty| duty["action"].as_str())
                .collect();
            assert_eq!(actions, expected, "{licence}: {permission:#}");
        }
        let turtle = access_odrl::turtle(&document);
        if expected.contains(&SHARE_ALIKE) {
            assert!(
                turtle.contains(&format!("odrl:duty [ odrl:action <{SHARE_ALIKE}> ]")),
                "{turtle}"
            );
        }
        if expected.contains(&"attribute") {
            assert!(
                turtle.contains("odrl:duty [ odrl:action odrl:attribute ]"),
                "{turtle}"
            );
        } else {
            assert!(!turtle.contains("odrl:duty"), "{turtle}");
        }
    }
}

#[test]
fn every_licence_of_the_table_parses_by_its_code() {
    for licence in Licence::ALL {
        let parsed: Licence = serde_norway::from_str(licence.code()).expect("parses");
        assert_eq!(parsed, licence);
    }
}
