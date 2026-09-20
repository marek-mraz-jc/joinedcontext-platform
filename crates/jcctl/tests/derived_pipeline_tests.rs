//! What a derived pipeline compiles into (T-0140, PL-31, PL-33, PL-34, PL-34a, PL-37).
//!
//! The golden values below are the contract with three parties at once: with Bento, whose field
//! names these are; with the gateway, which delivers to the address the subscription names; and
//! with the build lane, whose published digest is the only module the runner may load.
//!
//! The shapes were checked against the pinned Bento image rather than a manual: `generate` with
//! `count` terminates a CronJob pod where `http_client` would poll forever, and the `timeAt`
//! interpolation resolves per run inside an input URL.

use jc_core::kinds::{Pipeline, PipelineSpec};
use jcctl::pipelines_derived::{render, Derived, DerivedContext, DerivedError};
use serde_json::{json, Value};

const DIGEST: &str = "sha256:1111111111111111111111111111111111111111111111111111111111111111";

fn pipeline(spec: &str) -> PipelineSpec {
    let yaml = format!(
        r#"apiVersion: joinedcontext.com/v1alpha1
kind: Pipeline
metadata:
  name: district-air-index
  namespace: ovzdusie
spec:
  targetEndpoint: urn:ngsi-ld:Endpoint:banskabystrica.sk:ovzdusie:ep-derived
{spec}"#
    );
    let manifest = Pipeline::from_yaml(&yaml).expect("the pipeline manifest parses");
    manifest
        .validate()
        .expect("the pipeline manifest validates");
    manifest.spec
}

fn context() -> DerivedContext<'static> {
    DerivedContext {
        project: "ovzdusie",
        pipeline: "district-air-index",
        space: "air-quality",
        org_domain: "banskabystrica.sk",
        source_url: "https://bb.example.sk/api/endpoint/ep-air-quality/ngsi-ld/v1",
        module_digest: Some(DIGEST),
    }
}

fn rendered(spec: &str) -> Derived {
    render(&pipeline(spec), &context()).expect("the pipeline renders")
}

/// A resident derived pipeline, watching two attributes of one type.
const RESIDENT: &str = r#"  class: resident
  source:
    endpointRef: { kind: Endpoint, name: air-quality-internal }
    trigger:
      subscription:
        type: AirQualityObserved
        watchedAttributes: [pm10, pm25]
  compute:
    kind: wasm
    module: ./compute
    function: process
  output:
    type: AirQualityIndexDaily
    mode: upsert
"#;

/// A scheduled derived pipeline over yesterday's observations.
const SCHEDULED: &str = r#"  class: scheduled
  schedule: "10 0 * * *"
  source:
    endpointRef: { kind: Endpoint, name: air-quality-internal }
    query:
      type: AirQualityObserved
      attrs: [pm10, pm25, refDistrict]
      temporalQ: { window: P1D }
  compute:
    kind: wasm
    module: ./compute
    function: process
  output:
    type: AirQualityIndexDaily
    mode: upsert
"#;

/// PL-31: the runner listens on its own stream path and the gateway is told to deliver there.
#[test]
fn a_subscription_trigger_becomes_a_listener_and_the_subscription_that_feeds_it() {
    let derived = rendered(RESIDENT);

    assert_eq!(
        derived.input,
        json!({ "http_server": { "path": "/notify", "allowed_verbs": ["POST"] } })
    );

    let subscription = derived
        .subscription
        .expect("a trigger needs a subscription");
    assert_eq!(
        subscription["id"],
        json!("urn:ngsi-ld:Subscription:banskabystrica.sk:air-quality:district-air-index"),
        "the id is the four-segment URN every entity follows (PF-42)"
    );
    assert_eq!(subscription["type"], json!("Subscription"));
    assert_eq!(
        subscription["entities"],
        json!([{ "type": "AirQualityObserved" }])
    );
    assert_eq!(subscription["watchedAttributes"], json!(["pm10", "pm25"]));
    assert_eq!(
        subscription["notification"]["endpoint"]["uri"],
        json!("http://pipeline-runner.ovzdusie.svc.cluster.local:4195/district-air-index/notify"),
        "bento streams mode prefixes a stream's endpoints with its stream id"
    );
    assert_eq!(
        subscription["notification"]["format"],
        json!("normalized"),
        "the compute reads NGSI-LD, not key-values"
    );
}

/// An absent watch list is the widest one, so it is written as absence and not as an empty list,
/// which some brokers read as watching nothing at all.
#[test]
fn a_trigger_without_watched_attributes_names_none() {
    let derived = rendered(&RESIDENT.replace("        watchedAttributes: [pm10, pm25]\n", ""));
    let subscription = derived.subscription.expect("a subscription");
    assert!(
        subscription.get("watchedAttributes").is_none(),
        "{subscription}"
    );
}

/// PL-31, PL-26: a scheduled run is a pod that has to exit, so the clock is `generate` and the
/// fetch is a processor. An `http_client` input would poll until something killed the pod.
#[test]
fn a_query_source_becomes_a_clock_and_one_fetch() {
    let derived = rendered(SCHEDULED);

    assert_eq!(
        derived.input,
        json!({ "generate": { "count": 1, "interval": "", "mapping": "root = \"\"" } }),
        "one message, then the run ends"
    );
    assert!(
        derived.subscription.is_none(),
        "a poll needs no subscription"
    );

    let url = derived.processors[0]["http"]["url"]
        .as_str()
        .expect("the fetch carries a url")
        .to_owned();
    assert!(
        url.starts_with(
            "https://bb.example.sk/api/endpoint/ep-air-quality/ngsi-ld/v1/temporal/entities?"
        ),
        "{url}"
    );
    for expected in [
        "type=AirQualityObserved",
        "attrs=pm10,pm25,refDistrict",
        "timerel=after",
        // P1D in seconds, computed by the runner per run rather than baked in here.
        "timeAt=${! (now().ts_unix() - 86400).ts_format(\"2006-01-02T15:04:05Z\") }",
        "limit=1000",
    ] {
        assert!(url.contains(expected), "{expected} missing from {url}");
    }
    assert_eq!(
        derived.processors[0]["http"]["headers"]["Authorization"],
        json!("Bearer ${SERVICE_ACCOUNT_TOKEN}"),
        "PL-14: the config carries the interpolation, the environment the token"
    );
}

/// Without a temporal window the query is the current state, on the ordinary entities path.
#[test]
fn a_query_without_a_window_reads_the_current_state() {
    let derived = rendered(&SCHEDULED.replace("      temporalQ: { window: P1D }\n", ""));
    let url = derived.processors[0]["http"]["url"]
        .as_str()
        .expect("a url");
    assert!(url.contains("/ngsi-ld/v1/entities?"), "{url}");
    assert!(!url.contains("timerel"), "{url}");
}

/// Fixed-length parts only. A month is 28 to 31 days and a year 365 or 366, so a window that
/// silently changes length between runs is refused rather than approximated.
#[test]
fn a_window_is_days_hours_minutes_and_seconds_and_nothing_longer() {
    let seconds_of = |window: &str| {
        let spec = SCHEDULED.replace("window: P1D", &format!("window: {window}"));
        render(&pipeline(&spec), &context()).map(|derived| {
            derived.processors[0]["http"]["url"]
                .as_str()
                .expect("a url")
                .split("now().ts_unix() - ")
                .nth(1)
                .and_then(|tail| tail.split(')').next())
                .expect("the offset")
                .to_owned()
        })
    };

    assert_eq!(seconds_of("P1D").as_deref(), Ok("86400"));
    assert_eq!(seconds_of("PT1H").as_deref(), Ok("3600"));
    assert_eq!(seconds_of("PT30M").as_deref(), Ok("1800"));
    assert_eq!(seconds_of("P1DT2H30M").as_deref(), Ok("95400"));

    for refused in ["P1M", "P1Y", "1D", "P", "PT", "PD", "P1W", "P-1D"] {
        assert!(
            matches!(seconds_of(refused), Err(DerivedError::BadWindow(_))),
            "{refused} was accepted"
        );
    }
}

/// PL-34, PL-34a: the module that runs is the one the build lane published, named by its digest
/// so two pipelines that compiled to the same bytes mount one file.
#[test]
fn a_wasm_compute_runs_the_digest_the_build_lane_published() {
    assert_eq!(
        rendered(RESIDENT).processors,
        vec![json!({
            "wasm": {
                "module_path": "/modules/1111111111111111111111111111111111111111111111111111111111111111.wasm",
                "function": "process",
            }
        })]
    );
}

/// PL-34a: a pipeline whose module has not been built yet deploys nothing, rather than running
/// whatever the runner still has on disk. The same rule an App follows for its image (AP-13a).
#[test]
fn a_wasm_compute_without_a_published_module_renders_nothing() {
    let unbuilt = DerivedContext {
        module_digest: None,
        ..context()
    };
    assert_eq!(
        render(&pipeline(RESIDENT), &unbuilt),
        Err(DerivedError::NoModuleDigest)
    );

    for wrong in [
        "sha256:cafe",
        "1111111111111111111111111111111111111111111111111111111111111111",
        "sha256:zzzz111111111111111111111111111111111111111111111111111111111111",
    ] {
        let malformed = DerivedContext {
            module_digest: Some(wrong),
            ..context()
        };
        assert!(
            matches!(
                render(&pipeline(RESIDENT), &malformed),
                Err(DerivedError::BadDigest(_))
            ),
            "{wrong} was accepted as a digest"
        );
    }
}

/// The two kinds another renderer owns say so by name, rather than rendering an empty pipeline
/// that would look like it worked.
#[test]
fn a_compute_kind_this_renderer_does_not_own_is_refused_by_name() {
    for (kind, extra, owner) in [
        (
            "mapping",
            "    mappingRef: { kind: Mapping, name: aq-to-index }\n",
            "PL-29",
        ),
        ("container", "", "PL-35"),
    ] {
        let spec = SCHEDULED.replace(
            "    kind: wasm\n    module: ./compute\n    function: process\n",
            &format!("    kind: {kind}\n{extra}"),
        );
        match render(&pipeline(&spec), &context()) {
            Err(DerivedError::Elsewhere { reason, .. }) => {
                assert!(reason.contains(owner), "{kind}: {reason}")
            }
            other => panic!("{kind}: {other:?}"),
        }
    }
}

/// Bloblang compute is the author's own mapping in the pipeline's `bento.yaml`, so the
/// reconciler adds the fetch and nothing else.
#[test]
fn a_bloblang_compute_contributes_no_processor_of_its_own() {
    let spec = SCHEDULED.replace(
        "    kind: wasm\n    module: ./compute\n    function: process\n",
        "    kind: bloblang\n",
    );
    let derived = render(&pipeline(&spec), &context()).expect("renders");
    assert_eq!(derived.processors.len(), 1, "{:?}", derived.processors);
    assert!(derived.processors[0].get("http").is_some());
}

/// PL-42: ticked entities become the `id=` parameter of the fetch, beside the type.
#[test]
fn pinned_ids_narrow_the_fetch_to_those_entities() {
    let spec = SCHEDULED.replace(
        "      attrs: [pm10, pm25, refDistrict]\n",
        "      attrs: [pm10, pm25, refDistrict]\n      ids: [urn:ngsi-ld:AirQualityObserved:bb.sk:ovzdusie:a-1, urn:ngsi-ld:AirQualityObserved:bb.sk:ovzdusie:a-2]\n",
    );
    let derived = render(&pipeline(&spec), &context()).expect("renders");
    let url = derived.processors[0]["http"]["url"].as_str().expect("url");
    assert!(
        url.contains("id=urn:ngsi-ld:AirQualityObserved:bb.sk:ovzdusie:a-1,urn:ngsi-ld:AirQualityObserved:bb.sk:ovzdusie:a-2"),
        "{url}"
    );
    assert!(url.contains("type=AirQualityObserved"), "{url}");
}

/// PL-41: an inline `spec.compute.bloblang` is the last processor, after the fetch, so the
/// mapping sees the page the source returned.
#[test]
fn an_inline_bloblang_compute_is_the_last_mapping_processor() {
    let spec = SCHEDULED.replace(
        "    kind: wasm\n    module: ./compute\n    function: process\n",
        "    kind: bloblang\n    bloblang: |\n      root = this\n      root.index = this.pm10 * 2\n",
    );
    let derived = render(&pipeline(&spec), &context()).expect("renders");
    assert_eq!(derived.processors.len(), 2, "{:?}", derived.processors);
    assert!(derived.processors[0].get("http").is_some());
    assert_eq!(
        derived.processors[1]["mapping"].as_str(),
        Some("root = this\nroot.index = this.pm10 * 2\n")
    );
}

/// PL-37: a pipeline that writes what it watches feeds its own trigger.
#[test]
fn a_pipeline_that_writes_the_type_it_watches_is_refused() {
    let looping = RESIDENT.replace(
        "    type: AirQualityIndexDaily",
        "    type: AirQualityObserved",
    );
    assert_eq!(
        render(&pipeline(&looping), &context()),
        Err(DerivedError::Feedback {
            entity_type: "AirQualityObserved".to_owned()
        })
    );

    // The same manifest with the loop declared deliberate renders, because a person reviewed it.
    let declared = format!("{looping}  allowFeedback: true\n");
    render(&pipeline(&declared), &context()).expect("a declared loop renders");
}

/// `update-attrs` is refused on the same type too: nothing in the manifest says which attributes
/// the compute writes, and the reconciler will not guess that they miss the watched ones.
#[test]
fn the_loop_guard_reads_the_type_and_not_the_write_mode() {
    let looping = RESIDENT
        .replace(
            "    type: AirQualityIndexDaily",
            "    type: AirQualityObserved",
        )
        .replace("mode: upsert", "mode: update-attrs");
    assert!(matches!(
        render(&pipeline(&looping), &context()),
        Err(DerivedError::Feedback { .. })
    ));
}

/// A poll cannot feed itself through a subscription it does not have, so the guard stays out of
/// the way of every scheduled pipeline.
#[test]
fn a_scheduled_pipeline_writing_its_own_source_type_is_not_a_loop() {
    let same = SCHEDULED.replace(
        "    type: AirQualityIndexDaily",
        "    type: AirQualityObserved",
    );
    render(&pipeline(&same), &context()).expect("a poll is not a trigger");
}

/// PL-31: one input. Two is a merge nobody can review, none is nothing to render.
#[test]
fn a_source_declares_exactly_one_input_and_one_read_grant() {
    let both = RESIDENT.replace(
        "    trigger:\n",
        "    query: { type: AirQualityObserved }\n    trigger:\n",
    );
    assert_eq!(
        render(&pipeline(&both), &context()),
        Err(DerivedError::TwoInputs)
    );

    let neither = "  class: resident\n  source:\n    endpointRef: { kind: Endpoint, name: aq }\n";
    assert_eq!(
        render(&pipeline(neither), &context()),
        Err(DerivedError::NoInput)
    );

    let no_endpoint = RESIDENT.replace(
        "    endpointRef: { kind: Endpoint, name: air-quality-internal }\n",
        "",
    );
    assert_eq!(
        render(&pipeline(&no_endpoint), &context()),
        Err(DerivedError::NoEndpoint)
    );

    let not_derived = "  class: resident\n";
    assert_eq!(
        render(&pipeline(not_derived), &context()),
        Err(DerivedError::NotDerived)
    );
}

/// The rendered halves stay JSON the config writer can merge: no key is a Bento field this
/// version does not have, and nothing carries a credential.
#[test]
fn nothing_rendered_carries_a_credential_or_an_unknown_field() {
    for derived in [rendered(RESIDENT), rendered(SCHEDULED)] {
        let printed = serde_json::to_string(&json!({
            "input": derived.input,
            "processors": derived.processors,
            "subscription": derived.subscription,
        }))
        .expect("serializes");
        assert!(
            !printed.contains("Bearer ey") && !printed.contains("password"),
            "{printed}"
        );
        assert!(
            printed.contains("${SERVICE_ACCOUNT_TOKEN}") || derived.subscription.is_some(),
            "a fetch without an interpolated token: {printed}"
        );
        for object in std::iter::once(&derived.input).chain(derived.processors.iter()) {
            let (name, _) = object
                .as_object()
                .and_then(|map| map.iter().next())
                .expect("one component per object");
            assert!(
                ["generate", "http_server", "http", "wasm"].contains(&name.as_str()),
                "{name} is not a component this renderer emits"
            );
        }
    }
}

/// The subscription is a plain CIM 009 document: no vendor member, nothing the platform invented.
#[test]
fn the_subscription_is_standard_ngsi_ld_and_nothing_more() {
    let subscription = rendered(RESIDENT).subscription.expect("a subscription");
    let members: Vec<&String> = subscription
        .as_object()
        .expect("an object")
        .keys()
        .collect();
    // serde_json orders members alphabetically; what matters is the set, not the order.
    assert_eq!(
        members,
        vec![
            "entities",
            "id",
            "notification",
            "type",
            "watchedAttributes"
        ],
        "CC-16: only members CIM 009 defines"
    );
    let notification: &Value = &subscription["notification"];
    assert_eq!(
        notification
            .as_object()
            .expect("an object")
            .keys()
            .collect::<Vec<_>>(),
        vec!["endpoint", "format"]
    );
    assert_eq!(
        notification["endpoint"]
            .as_object()
            .expect("an object")
            .keys()
            .collect::<Vec<_>>(),
        vec!["accept", "uri"]
    );
}

/// PL-37 in the second shape (PL-54): any output that writes the watched type feeds the trigger.
#[test]
fn a_second_version_pipeline_whose_second_output_writes_the_watched_type_is_refused() {
    let yaml = r#"apiVersion: joinedcontext.com/v1alpha2
kind: Pipeline
metadata:
  name: district-air-index
  namespace: ovzdusie
spec:
  class: resident
  sources:
    - endpointRef: { kind: Endpoint, name: ovzdusie-internal }
      trigger: { subscription: { type: AirQualityObserved, watchedAttributes: [pm10] } }
  steps:
    - kind: bloblang
      bloblang: "root = this"
  outputs:
    - targetEndpoint: urn:ngsi-ld:Endpoint:banskabystrica.sk:ovzdusie:ep-derived
      type: AirQualityIndexDaily
    - targetEndpoint: urn:ngsi-ld:Endpoint:banskabystrica.sk:ovzdusie:ep-other
      type: AirQualityObserved
"#;
    let manifest = Pipeline::from_yaml(yaml).expect("parses");
    manifest.validate().expect("validates");
    assert_eq!(
        render(&manifest.spec, &context()),
        Err(DerivedError::Feedback {
            entity_type: "AirQualityObserved".to_owned()
        })
    );
}
