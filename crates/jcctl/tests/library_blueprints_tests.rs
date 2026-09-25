//! T-1571: the platform's blueprint library ships the common flows (CC-28), every one renders
//! valid manifests from a valid parameter set (CC-24), and an instance takes the library as a
//! reviewable sync proposal (CC-26, MF-27).
//!
//! The library is `library/blueprints/` at the root of this repository: the folder each instance's
//! `SyncSource` follows. Every rendered manifest is checked the way the reconciler reads it — the
//! project's namespace set, `{orgDomain}` rendered (CC-74) — by the kind's own validation.

use jc_core::kinds::sync::SyncOrigin;
use jc_core::kinds::Blueprint;
use jc_core::registry;
use jcctl::blueprints::{expand, ExpandError};
use jcctl::loader::RawManifest;
use jcctl::sync::{self, RemoteError, State, SyncRemote};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

/// The four flows CC-28 names, by the library's folder names.
const FLOWS: [&str; 4] = [
    "threshold-alert",
    "data-source-onboarding",
    "dataset-publication",
    "cross-city-sharing",
];

/// A slug as the Portal mints one: 160 bits in lowercase base32 (EP-02).
const SLUG: &str = "k7x2m4q6w3z5n7p2r4t6v3y5b7d2f4h6";

fn library() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../library/blueprints")
}

fn source_of(flow: &str) -> String {
    std::fs::read_to_string(library().join(flow).join("blueprint.yaml"))
        .unwrap_or_else(|e| panic!("library/blueprints/{flow}/blueprint.yaml: {e}"))
}

fn blueprint(flow: &str) -> Blueprint {
    Blueprint::from_yaml(&source_of(flow))
        .unwrap_or_else(|e| panic!("{flow} does not parse as a Blueprint: {e}"))
}

/// Every parameter a person could set, for each flow.
fn full(flow: &str) -> Value {
    match flow {
        "threshold-alert" => json!({
            "name": "no2-high", "space": "air-quality", "entityType": "AirQualityObserved",
            "observedProperty": "no2", "comparison": ">=", "thresholdValue": 40.5,
            "webhookUrl": "https://alerts.example.fi/hooks/no2",
            "webhookSecret": { "name": "alerts-webhook", "key": "token" }, "throttling": 600,
        }),
        "data-source-onboarding" => json!({
            "name": "parking-garages", "space": "mobility",
            "url": "https://data.example.fi/parking.json", "entityType": "OffStreetParking",
            "period": "15m", "endpointSlug": SLUG,
        }),
        "dataset-publication" => json!({
            "name": "air-quality-open", "space": "air-quality", "publisher": "City of Helsinki",
            "contactName": "Open data team", "contactEmail": "opendata@hel.fi", "license": "CC0",
            "theme": "ENVI", "frequency": "HOURLY", "ckanInstance": "open-data",
            "ckanOrganization": "helsinki", "endpointSlug": SLUG,
        }),
        "cross-city-sharing" => json!({
            "name": "tallinn-transport", "hubSpace": "hub", "sharedSpace": "transport",
            "partnerProject": "tallinn", "partnerEndpoint": "https://tallinn.example.ee/ngsi-ld/v1",
            "entityTypes": ["Vehicle", "BusStop"], "serviceAccount": "hub-reader",
            "endpointSlug": SLUG,
        }),
        other => panic!("no parameters for {other}"),
    }
}

/// The required parameters alone, as an API caller that relies on the defaults sends them.
fn required_only(flow: &str) -> Value {
    let schema = &blueprint(flow).spec.parameter_schema;
    let required: Vec<&str> = schema["required"]
        .as_array()
        .expect("a required list")
        .iter()
        .filter_map(Value::as_str)
        .collect();
    let mut all = full(flow);
    let all = all.as_object_mut().expect("an object");
    all.retain(|name, _| required.contains(&name.as_str()));
    Value::Object(all.clone())
}

/// Checks one rendered manifest as the reconciler reads it from a project's folder.
fn assert_valid(flow: &str, template: &str, manifest: &str) -> RawManifest {
    let rendered = manifest.replace("{orgDomain}", "hel.fi");
    let mut raw: RawManifest = serde_norway::from_str(&rendered)
        .unwrap_or_else(|e| panic!("{flow}/{template} is not a manifest: {e}\n{rendered}"));
    raw.metadata.namespace = Some("helsinki".to_owned());
    let yaml = serde_norway::to_string(&raw).expect("a manifest serialises");
    match registry::validate_yaml(&raw.kind, &yaml) {
        Some(Ok(())) => raw,
        Some(Err(e)) => panic!(
            "{flow}/{template} renders an invalid {}: {e}\n{yaml}",
            raw.kind
        ),
        None => panic!("{flow}/{template} renders an unknown kind {}", raw.kind),
    }
}

/// CC-28: the library holds the four common flows, each a valid organization Blueprint.
#[test]
fn the_library_ships_the_four_common_flows() {
    let mut found: Vec<String> = std::fs::read_dir(library())
        .expect("library/blueprints exists")
        .map(|entry| {
            entry
                .expect("a directory entry")
                .file_name()
                .into_string()
                .expect("a UTF-8 name")
        })
        .collect();
    found.sort();
    let mut expected: Vec<String> = FLOWS.iter().map(|f| (*f).to_owned()).collect();
    expected.sort();
    assert_eq!(found, expected);

    for flow in FLOWS {
        let source = source_of(flow);
        assert!(
            matches!(registry::validate_yaml("Blueprint", &source), Some(Ok(()))),
            "{flow} is not a valid Blueprint"
        );
        let blueprint = blueprint(flow);
        assert_eq!(
            blueprint.metadata.name, flow,
            "the folder names the blueprint"
        );
        assert_eq!(blueprint.metadata.namespace.as_deref(), Some("org"));
    }
}

/// CC-24, CC-28: every flow renders valid manifests, with every parameter set and with the
/// required ones alone.
#[test]
fn every_flow_renders_valid_manifests_with_full_and_default_parameters() {
    for flow in FLOWS {
        for parameters in [full(flow), required_only(flow)] {
            let rendered = expand(&blueprint(flow), &parameters)
                .unwrap_or_else(|e| panic!("{flow} does not expand with {parameters}: {e}"));
            assert!(!rendered.is_empty(), "{flow} renders nothing");
            for expanded in &rendered {
                assert_valid(flow, &expanded.template, &expanded.manifest);
            }
        }
    }
}

/// CC-28: what each flow is made of, as the requirement names it.
#[test]
fn each_flow_renders_the_resources_the_requirement_names() {
    let kinds = |flow: &str| -> Vec<String> {
        expand(&blueprint(flow), &full(flow))
            .expect("expands")
            .iter()
            .map(|e| assert_valid(flow, &e.template, &e.manifest).kind)
            .collect()
    };
    assert_eq!(kinds("threshold-alert"), ["Subscription"]);
    assert_eq!(
        kinds("data-source-onboarding"),
        ["DataSource", "Pipeline", "Endpoint"]
    );
    assert_eq!(kinds("dataset-publication"), ["Endpoint"]);
    assert_eq!(
        kinds("cross-city-sharing"),
        ["ContextSourceRegistration", "Endpoint"]
    );

    // The alert: the watched property, the limit and the channel with its secret by reference.
    let alert = expand(&blueprint("threshold-alert"), &full("threshold-alert")).expect("expands");
    let alert = assert_valid("threshold-alert", "subscription", &alert[0].manifest);
    assert_eq!(alert.spec["q"], "no2>=40.5");
    assert_eq!(
        alert.spec["notification"]["endpoint"]["secretRef"],
        json!({ "name": "alerts-webhook", "key": "token" })
    );

    // Publication: the catalogue record and the open-data view are the one public Endpoint's.
    let published = expand(
        &blueprint("dataset-publication"),
        &full("dataset-publication"),
    )
    .expect("expands");
    let published = assert_valid("dataset-publication", "endpoint", &published[0].manifest);
    assert_eq!(published.spec["audience"], "public");
    assert_eq!(published.spec["catalog"]["license"], "CC0");
    assert_eq!(
        published.spec["publish"]["ckan"]["instanceRef"]["name"],
        "open-data"
    );

    // The pair: the partner in through a registration, this city out to the partner alone.
    let pair = expand(
        &blueprint("cross-city-sharing"),
        &full("cross-city-sharing"),
    )
    .expect("expands");
    let registration = assert_valid("cross-city-sharing", "registration", &pair[0].manifest);
    assert_eq!(
        registration.spec["information"][0]["entities"],
        json!([{ "type": "Vehicle" }, { "type": "BusStop" }])
    );
    let endpoint = assert_valid("cross-city-sharing", "endpoint", &pair[1].manifest);
    assert_eq!(endpoint.spec["audience"], "project-list");
    assert_eq!(endpoint.spec["allowedProjects"], json!(["tallinn"]));
}

/// CC-24: a parameter set the schema refuses renders nothing — a plain-http channel, a slug
/// short of EP-02's entropy, a missing required value, an unknown parameter.
#[test]
fn a_parameter_set_the_schema_refuses_renders_nothing() {
    let refused = |flow: &str, change: &dyn Fn(&mut serde_json::Map<String, Value>)| {
        let mut parameters = full(flow);
        change(parameters.as_object_mut().expect("an object"));
        match expand(&blueprint(flow), &parameters) {
            Err(ExpandError::Parameters(errors)) => assert!(!errors.is_empty()),
            other => panic!("{flow} accepted {parameters}: {other:?}"),
        }
    };
    refused("threshold-alert", &|p| {
        p.insert("webhookUrl".into(), json!("http://alerts.example.fi/hook"));
    });
    refused("data-source-onboarding", &|p| {
        p.insert("endpointSlug".into(), json!("short"));
    });
    refused("dataset-publication", &|p| {
        p.remove("contactEmail");
    });
    refused("cross-city-sharing", &|p| {
        p.insert(
            "partnerEndpoint".into(),
            json!("https://user:pw@tallinn.example.ee/ngsi-ld/v1"),
        );
    });
    refused("cross-city-sharing", &|p| {
        p.insert("namespace".into(), json!("another-project"));
    });
    // The format keywords are annotations to this validator, so every string a template quotes
    // carries a pattern of its own: a value that closes the quote cannot write a second member.
    refused("dataset-publication", &|p| {
        p.insert("publisher".into(), json!("City\", audience: public, x: \""));
    });
    refused("threshold-alert", &|p| {
        p.insert("space".into(), json!("air\" }\nspec: {"));
    });
    refused("dataset-publication", &|p| {
        p.insert("contactEmail".into(), json!("not-an-address"));
    });
}

/// A remote whose checkout is the library folder of this repository.
struct Library;

impl SyncRemote for Library {
    fn revision(&self, _origin: &SyncOrigin) -> Result<String, RemoteError> {
        Ok("4f1c2a9e0b7d".to_owned())
    }

    fn checkout(
        &self,
        _origin: &SyncOrigin,
        _revision: &str,
        into: &Path,
    ) -> Result<(), RemoteError> {
        for flow in FLOWS {
            let to = into.join(flow);
            std::fs::create_dir_all(&to).map_err(|e| RemoteError::Unavailable(e.to_string()))?;
            std::fs::write(to.join("blueprint.yaml"), source_of(flow))
                .map_err(|e| RemoteError::Unavailable(e.to_string()))?;
        }
        Ok(())
    }
}

/// CC-26, MF-27: an instance that follows the library gets it as one proposal, a merge request
/// somebody reviews, with each blueprint at its organization path.
#[test]
fn an_instance_following_the_library_gets_it_as_one_reviewable_proposal() {
    let base = std::env::temp_dir().join(format!("jc-library-sync-{}", std::process::id()));
    let (repo, workspace) = (base.join("repo"), base.join("workspace"));
    std::fs::create_dir_all(&repo).expect("a repository");
    std::fs::create_dir_all(&workspace).expect("a workspace");
    let source = RawManifest {
        api_version: jc_core::API_VERSION.to_owned(),
        kind: "SyncSource".to_owned(),
        metadata: serde_json::from_value(
            json!({ "name": "blueprint-library", "namespace": "helsinki" }),
        )
        .expect("metadata"),
        spec: json!({
            "source": { "git": {
                "url": "https://github.com/marek-mraz-jc/joinedcontext-platform.git",
                "ref": "main", "path": "library/blueprints",
            } },
            "schedule": { "interval": "1d" },
            "mode": "mirror",
            "conflictPolicy": "replace",
        }),
        status: None,
    };

    let run = sync::poll(&source, &State::default(), 0, &repo, &workspace, &Library);
    std::fs::remove_dir_all(&base).ok();
    let run = run.expect("the run");

    let proposal = run.proposal.expect("a proposal");
    assert!(proposal.rejected.is_empty(), "{:?}", proposal.rejected);
    assert!(
        !proposal.auto_merge,
        "a library release is reviewed, never merged by itself"
    );
    let mut paths: Vec<String> = proposal
        .files
        .keys()
        .map(|p| p.to_string_lossy().into_owned())
        .collect();
    paths.sort();
    let mut expected: Vec<String> = FLOWS
        .iter()
        .map(|f| format!("blueprints/{f}/blueprint.yaml"))
        .collect();
    expected.sort();
    assert_eq!(paths, expected);
}
