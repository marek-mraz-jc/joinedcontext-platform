//! The shipped ingestion examples, checked against the code that has to run them
//! (T-0294…T-0297, PL-01, PL-03, PL-22, PL-26, PL-27, PL-39, MF-35, CC-06).
//!
//! `bento test` proves the transformations; these tests prove the half Bento cannot see. An
//! example is documentation people copy, so a manifest that no longer parses, a reference that
//! points at nothing, a cadence that would land in the wrong runtime or a credential written
//! as a value is a defect in the same sense as a broken function.

use std::fs;
use std::path::{Path, PathBuf};

use jc_core::envelope::ResourceEnvelope;
use jc_core::kinds::{
    Audience, CkanInstanceSpec, DataSourceSpec, DataSourceType, EndpointSpec, Pipeline,
};
use jcctl::bento::{render, InputContext, ORG_DOMAIN_VAR};
use jcctl::pipelines::{runtime_of, Runtime};

/// Every example folder, and the connection type it is there to demonstrate.
const EXAMPLES: [(&str, DataSourceType); 15] = [
    ("hsl-hfp-mqtt", DataSourceType::Mqtt),
    ("http-json-poll", DataSourceType::Http),
    ("csv-fetch", DataSourceType::Http),
    ("gtfs-rt", DataSourceType::GtfsRt),
    ("helsinki-city-bikes", DataSourceType::Http),
    ("helsinki-hsy-air", DataSourceType::Http),
    ("helsinki-digitraffic-tms", DataSourceType::Http),
    ("helsinki-fmi-weather", DataSourceType::Http),
    ("helsinki-marine-vessels", DataSourceType::Http),
    ("helsinki-parking-zones", DataSourceType::Http),
    ("helsinki-linked-events", DataSourceType::Http),
    ("helsinki-palvelukartta", DataSourceType::Http),
    ("helsinki-hri-population", DataSourceType::Http),
    ("helsinki-ev-charging", DataSourceType::Http),
    // The one example that deletes rather than upserts (T-1158): it reads a page of the
    // endpoint it writes to, which is an `http` source like any other.
    ("vehicle-reaper", DataSourceType::Http),
];

/// The examples that also publish: the folder carries the Endpoint the pipeline writes into
/// and the catalogue entry it becomes (T-0321…T-0323, EP-01, EP-62).
const PUBLISHED: [&str; 10] = [
    "helsinki-city-bikes",
    "helsinki-hsy-air",
    "helsinki-digitraffic-tms",
    "helsinki-fmi-weather",
    "helsinki-marine-vessels",
    "helsinki-parking-zones",
    "helsinki-linked-events",
    "helsinki-palvelukartta",
    "helsinki-hri-population",
    "helsinki-ev-charging",
];

fn examples_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/ingestion")
}

fn read(example: &str, file: &str) -> String {
    let path = examples_dir().join(example).join(file);
    fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

fn data_source(example: &str) -> ResourceEnvelope<DataSourceSpec> {
    let parsed = ResourceEnvelope::<DataSourceSpec>::from_yaml(&read(example, "datasource.yaml"))
        .unwrap_or_else(|e| panic!("{example}/datasource.yaml: {e}"));
    parsed
        .validate()
        .unwrap_or_else(|e| panic!("{example}/datasource.yaml: {e}"));
    parsed
}

fn pipeline(example: &str) -> Pipeline {
    let parsed = Pipeline::from_yaml(&read(example, "pipeline.yaml"))
        .unwrap_or_else(|e| panic!("{example}/pipeline.yaml: {e}"));
    parsed
        .validate()
        .unwrap_or_else(|e| panic!("{example}/pipeline.yaml: {e}"));
    parsed
}

/// MF-35, PL-39: both manifests parse, validate and agree with each other.
#[test]
fn every_example_is_a_valid_pair_of_manifests() {
    for (example, expected_type) in EXAMPLES {
        let source = data_source(example);
        assert_eq!(
            source.spec.source_type, expected_type,
            "{example} demonstrates a different connection than it claims"
        );

        let pipeline = pipeline(example);
        let reference = pipeline
            .spec
            .source
            .as_ref()
            .and_then(|source| source.data_source_ref.as_ref())
            .unwrap_or_else(|| panic!("{example}: the pipeline references no DataSource"));
        assert_eq!(
            reference.name(),
            source.metadata.name,
            "{example}: the reference names a source that is not in this folder"
        );
        assert_eq!(reference.kind(), Some("DataSource"), "{example}");
        assert_eq!(
            pipeline.metadata.namespace, source.metadata.namespace,
            "{example}: a pipeline resolves its source inside its own project"
        );
    }
}

/// PL-26, PL-27: the cadence each example declares is the one the reader is told it has.
#[test]
fn each_example_lands_in_the_runtime_its_comment_promises() {
    // Push-based: no period at all, so the thirty-second rule leaves it resident.
    assert_eq!(
        runtime_of(&pipeline("hsl-hfp-mqtt").spec),
        Runtime::Resident
    );
    // Fifteen seconds is inside the rule.
    assert_eq!(runtime_of(&pipeline("gtfs-rt").spec), Runtime::Resident);

    let Runtime::Scheduled(five_minutes) = runtime_of(&pipeline("http-json-poll").spec) else {
        panic!("five minutes is past the thirty-second rule");
    };
    assert_eq!(five_minutes.concurrency_policy, "Forbid");

    // PL-27, the one-minute cron floor: forty-five seconds runs every minute and fetches once.
    let Runtime::Scheduled(forty_five) = runtime_of(&pipeline("csv-fetch").spec) else {
        panic!("forty-five seconds is past the thirty-second rule");
    };
    assert_eq!(forty_five.schedule, "* * * * *");
    assert_eq!(forty_five.interval.as_deref(), Some("45s"));
    assert_eq!(forty_five.count, 1);

    // The seven Helsinki dataset pipelines each declare the cron expression their period means,
    // so the job starts, fetches once and exits. Without one the reconciler falls back to a job
    // every minute whose runner waits out the period before its single fetch, which for a daily
    // dataset would hold a pod for a day to do a second of work (PL-27).
    for (example, schedule) in [
        ("helsinki-marine-vessels", "* * * * *"),
        ("helsinki-parking-zones", "*/5 * * * *"),
        ("helsinki-fmi-weather", "*/10 * * * *"),
        ("helsinki-linked-events", "*/30 * * * *"),
        ("helsinki-ev-charging", "*/30 * * * *"),
        ("helsinki-palvelukartta", "7 * * * *"),
        ("helsinki-hri-population", "20 3 * * *"),
    ] {
        let Runtime::Scheduled(job) = runtime_of(&pipeline(example).spec) else {
            panic!("{example} is past the thirty-second rule and belongs in a CronJob");
        };
        assert_eq!(job.schedule, schedule, "{example}");
        assert_eq!(
            job.interval, None,
            "{example}: the cron expression is the cadence, so the runner fetches once and exits"
        );
        assert_eq!(job.count, 1, "{example}");
        assert_eq!(job.concurrency_policy, "Forbid", "{example}");
    }
}

/// PL-39: the rendered config is the author's file with the connection's input in front of it.
#[test]
fn every_example_renders_the_input_the_reader_never_wrote() {
    for (example, _) in EXAMPLES {
        let source = data_source(example);
        let authored = read(example, "bento.yaml");
        assert!(
            !authored.contains("\ninput:"),
            "{example}: the input is the reconciler's, not the author's"
        );

        let pipeline = pipeline(example);
        let context = InputContext {
            source: &source.metadata.name,
            project: pipeline.metadata.namespace.as_deref().unwrap_or_default(),
            pipeline: &pipeline.metadata.name,
            pipeline_spec: Some(&pipeline.spec),
        };
        let rendered =
            render(&authored, &source.spec, &context).unwrap_or_else(|e| panic!("{example}: {e}"));
        assert!(rendered.starts_with("input:"), "{example}:\n{rendered}");

        let expected_input = match &source.spec.source_type {
            DataSourceType::Mqtt => "  mqtt:".to_owned(),
            DataSourceType::WebSocket => "  websocket:".to_owned(),
            DataSourceType::Http | DataSourceType::GtfsRt => "  http_client:".to_owned(),
            DataSourceType::Runner(name) => format!("  {name}:"),
        };
        assert!(
            rendered.contains(expected_input.as_str()),
            "{example} did not render {expected_input}:\n{rendered}"
        );
    }
}

/// The GTFS-realtime decoder the example's `tests/decoder.yaml` mirrors is the one the
/// reconciler actually prepends; the copy in the folder exists only so the test can run
/// without the runner image.
#[test]
fn the_gtfs_example_mirrors_the_decoder_the_reconciler_prepends() {
    let source = data_source("gtfs-rt");
    let pipeline = pipeline("gtfs-rt");
    let rendered = render(
        &read("gtfs-rt", "bento.yaml"),
        &source.spec,
        &InputContext {
            source: &source.metadata.name,
            project: pipeline.metadata.namespace.as_deref().unwrap_or_default(),
            pipeline: &pipeline.metadata.name,
            pipeline_spec: Some(&pipeline.spec),
        },
    )
    .expect("renders");

    let config: serde_norway::Value = serde_norway::from_str(&rendered).expect("parses");
    let first = config
        .get("pipeline")
        .and_then(|p| p.get("processors"))
        .and_then(|p| p.as_sequence())
        .and_then(|p| p.first())
        .and_then(|p| p.get("protobuf"))
        .expect("the decoder runs first");
    assert_eq!(
        first.get("message").and_then(|m| m.as_str()),
        Some("transit_realtime.FeedMessage")
    );

    let mirror = read("gtfs-rt", "tests/decoder.yaml");
    assert!(
        mirror.contains("transit_realtime.FeedMessage") && mirror.contains("to_json"),
        "tests/decoder.yaml no longer mirrors the rendered decoder"
    );
    assert!(
        read("gtfs-rt", "proto/gtfs-realtime.proto").contains("package transit_realtime;"),
        "the vendored descriptors are what the decoder names"
    );
}

/// CC-06, PL-16: a credential appears in an example as a reference and in the rendered config
/// as an interpolation. Never as a value, not even a placeholder one somebody might copy.
#[test]
fn no_example_carries_a_credential() {
    for (example, _) in EXAMPLES {
        let source = data_source(example);
        let pipeline = pipeline(example);
        let rendered = render(
            &read(example, "bento.yaml"),
            &source.spec,
            &InputContext {
                source: &source.metadata.name,
                project: pipeline.metadata.namespace.as_deref().unwrap_or_default(),
                pipeline: &pipeline.metadata.name,
                pipeline_spec: Some(&pipeline.spec),
            },
        )
        .expect("renders");

        for (file, text) in [
            ("datasource.yaml", read(example, "datasource.yaml")),
            ("pipeline.yaml", read(example, "pipeline.yaml")),
            ("bento.yaml", read(example, "bento.yaml")),
            ("<rendered>", rendered),
        ] {
            for line in text.lines() {
                let lower = line.to_ascii_lowercase();
                let names_a_credential = ["password", "token", "secret", "authorization"]
                    .iter()
                    .any(|needle| lower.contains(needle));
                if !names_a_credential {
                    continue;
                }
                let value = line.split_once(':').map(|(_, v)| v.trim()).unwrap_or("");
                let is_reference = value.is_empty()
                    || value.starts_with('{')
                    || value.starts_with("Bearer ${")
                    || value.starts_with("${")
                    || lower.contains("ref:")
                    || line.trim_start().starts_with('#');
                assert!(
                    is_reference,
                    "{example}/{file} writes a credential as a value: {line}"
                );
            }
        }
    }
}

/// The demonstration instance every example writes into (`examples/ingestion/README.md`).
const DEMO_ORG_DOMAIN: &str = "hel.fi";

/// PF-44, PF-42: the `{orgDomain}` of a minted id reaches the mapping through the runner's
/// environment, which the reconciler fills from the project's Organization.
///
/// Bloblang cannot be executed from here, so what is checked is the chain that makes the claim
/// true: no `bento.yaml` writes a domain, every id it mints is the four-segment template with
/// the domain slot read from the environment, and the golden tests that assert a whole URN put
/// the domain there themselves. A pipeline moved to another Organization therefore mints under
/// the new domain without an edit, and one that tried to write a foreign domain would be
/// refused by the gateway at admission (Architecture/03 §3).
#[test]
fn the_domain_of_a_minted_id_comes_from_the_environment_and_never_from_the_pipeline_file() {
    let injection = format!("env(\"{ORG_DOMAIN_VAR}\")");
    // The two shapes a written-in domain takes: a string of its own, `"hel.fi"`, or a segment of
    // a spelled-out id, `:hel.fi:`. The bare substring is not the test, because three of the
    // Helsinki feeds are served from the city's own domain and `https://api.hel.fi/...` in a
    // `source` attribute is provenance rather than a minted id.
    let written_as_a_value = format!("\"{DEMO_ORG_DOMAIN}\"");
    let written_as_a_segment = format!(":{DEMO_ORG_DOMAIN}:");
    for (example, _) in EXAMPLES {
        let bento = read(example, "bento.yaml");
        for line in bento.lines() {
            assert!(
                !line.contains(&written_as_a_value) && !line.contains(&written_as_a_segment),
                "{example}/bento.yaml writes the organization domain into the pipeline \
                 (PF-44): {line}"
            );
        }
        // An example that mints an id takes the domain from the environment. One that mints
        // none — the reaper reads ids the broker already holds and names them back — has no
        // domain to inject, and demanding one would be demanding a line with nothing to do.
        if bento.contains("urn:ngsi-ld:%v") {
            assert!(
                bento.contains(&injection),
                "{example}/bento.yaml mints an id without {injection} (PF-44)"
            );
        }
        for line in bento.lines().filter(|line| line.contains("urn:ngsi-ld:")) {
            assert!(
                line.contains("urn:ngsi-ld:%v:%v:%v:%v"),
                "{example}/bento.yaml builds an id from a prefix instead of the four-segment \
                 template, so a segment could be missing or swapped: {line}"
            );
        }

        let golden = read(example, "bento_bento_test.yaml");
        assert!(
            golden.contains(&format!("{ORG_DOMAIN_VAR}: {DEMO_ORG_DOMAIN}")),
            "{example}/bento_bento_test.yaml expects ids it never supplies a domain for"
        );
        let domain_segment = format!(":{DEMO_ORG_DOMAIN}:");
        assert!(
            golden
                .lines()
                .any(|line| { line.contains("urn:ngsi-ld:") && line.contains(&domain_segment) }),
            "{example}/bento_bento_test.yaml asserts no minted id at all"
        );
    }
}

/// T-0321…T-0323, EP-01, EP-62: an example that publishes carries the Endpoint its pipeline
/// writes into and the catalogue entry it becomes, so a reader sees the whole path from the
/// feed to the open-data portal in one folder rather than three repositories.
#[test]
fn every_published_example_carries_the_endpoint_its_pipeline_writes_into() {
    let path = examples_dir().join("ckan-instance.yaml");
    let text = fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    let catalogue = ResourceEnvelope::<CkanInstanceSpec>::from_yaml(&text).expect("a CkanInstance");
    catalogue.validate().expect("the catalogue validates");

    let mut slugs = Vec::new();
    for example in PUBLISHED {
        let endpoint = ResourceEnvelope::<EndpointSpec>::from_yaml(&read(example, "endpoint.yaml"))
            .unwrap_or_else(|e| panic!("{example}/endpoint.yaml: {e}"));
        endpoint
            .validate()
            .unwrap_or_else(|e| panic!("{example}/endpoint.yaml: {e}"));

        let pipeline = pipeline(example);
        let target = &pipeline
            .spec
            .target()
            .unwrap_or_else(|| panic!("{example}: the pipeline names no target"));
        assert_eq!(
            target.local_id(),
            endpoint.metadata.name,
            "{example}: the pipeline writes into an Endpoint that is not in this folder"
        );
        assert_eq!(
            target.space(),
            endpoint.spec.context_space_ref.name(),
            "{example}: the target URN and the Endpoint disagree about the space"
        );
        assert_eq!(
            endpoint.metadata.namespace, pipeline.metadata.namespace,
            "{example}: an Endpoint and its pipeline live in one project"
        );

        // An open dataset is the point of these three: a catalogue entry that resolves to a
        // surface anyone may read (EP-14, EP-62).
        assert_eq!(endpoint.spec.audience, Audience::Public, "{example}");
        let ckan = endpoint
            .spec
            .publish
            .as_ref()
            .and_then(|publication| publication.ckan.as_ref())
            .unwrap_or_else(|| panic!("{example}: the Endpoint publishes nowhere"));
        assert_eq!(
            ckan.instance_ref.name(),
            catalogue.metadata.name,
            "{example}: the publication names a catalogue that is not in this tree"
        );
        assert_eq!(ckan.instance_ref.kind(), Some("CkanInstance"), "{example}");

        // EP-02: the slug is the address and carries no meaning, so it is neither the name nor
        // shared with another endpoint.
        let slug = endpoint.spec.slug.as_str().to_owned();
        assert!(
            !slug.contains(&endpoint.metadata.name),
            "{example}: the slug spells out the endpoint it addresses"
        );
        slugs.push(slug);
    }

    slugs.sort();
    let distinct = slugs.len();
    slugs.dedup();
    assert_eq!(
        slugs.len(),
        distinct,
        "two endpoints share a slug, so one of them is reachable under the other's address"
    );
}
