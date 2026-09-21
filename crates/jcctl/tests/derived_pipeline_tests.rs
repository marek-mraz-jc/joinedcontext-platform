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

// ---------------------------------------------------------------------------------------------
// T-2535: the source query of a derived pipeline, value by value (PL-26, PL-27, PL-42, PL-14).
//
// A query source renders into the URL the runner fetches. Every value of it comes from a
// manifest a project member proposes, so each has to arrive at the gateway as exactly the one
// parameter it was written as: never a second parameter, never the end of the query string, and
// never text the runner expands on its own (`${VAR}` at config load, `${! … }` per message).
// ---------------------------------------------------------------------------------------------

/// A scheduled pipeline whose source query is `query`, indented under `source:`.
fn with_query(query: &str) -> PipelineSpec {
    pipeline(&format!(
        r#"  class: scheduled
  schedule: "10 0 * * *"
  source:
    endpointRef: {{ kind: Endpoint, name: air-quality-internal }}
    query:
{query}
  compute:
    kind: wasm
    module: ./compute
    function: process
  output:
    type: AirQualityIndexDaily
    mode: upsert
"#
    ))
}

/// The URL the fetch processor carries, as rendered.
fn fetch_url(query: &str) -> String {
    let derived = render(&with_query(query), &context()).expect("the pipeline renders");
    derived.processors[0]["http"]["url"]
        .as_str()
        .expect("the fetch carries a url")
        .to_owned()
}

/// The query parameters as the gateway will read them, in order.
fn parameters(url: &str) -> Vec<(String, String)> {
    reqwest::Url::parse(url)
        .expect("the fetch url parses")
        .query_pairs()
        .map(|(name, value)| (name.into_owned(), value.into_owned()))
        .collect()
}

fn values_of(url: &str, name: &str) -> Vec<String> {
    parameters(url)
        .into_iter()
        .filter(|(key, _)| key == name)
        .map(|(_, value)| value)
        .collect()
}

/// PL-42: a `q` holding `&id=…` stays the `q` it was written as and widens nothing.
#[test]
fn a_q_value_containing_an_ampersand_does_not_add_or_override_a_query_parameter() {
    let q = r#"pm10>50&id=urn:ngsi-ld:AirQualityObserved:banskabystrica.sk:air-quality:other"#;
    let url = fetch_url(&format!(
        "      type: AirQualityObserved\n      ids: [\"urn:ngsi-ld:AirQualityObserved:banskabystrica.sk:air-quality:mine\"]\n      q: '{q}'"
    ));
    assert_eq!(values_of(&url, "q"), vec![q.to_owned()], "{url}");
    assert_eq!(
        values_of(&url, "id"),
        vec!["urn:ngsi-ld:AirQualityObserved:banskabystrica.sk:air-quality:mine".to_owned()],
        "{url}"
    );
}

/// The type is a name, never a query fragment: jc-core refuses `&` and `=` in it
/// (`kinds/pipeline.rs` `validate_source` → `names::validate_entity_type`), so it cannot reach
/// the renderer at all.
#[test]
fn an_entity_type_containing_an_ampersand_or_equals_sign_is_encoded_or_refused() {
    for entity_type in ["Air&limit=1", "Air=x"] {
        let yaml = format!(
            r#"apiVersion: joinedcontext.com/v1alpha1
kind: Pipeline
metadata:
  name: district-air-index
  namespace: ovzdusie
spec:
  targetEndpoint: urn:ngsi-ld:Endpoint:banskabystrica.sk:ovzdusie:ep-derived
  class: scheduled
  schedule: "10 0 * * *"
  source:
    endpointRef: {{ kind: Endpoint, name: air-quality-internal }}
    query: {{ type: "{entity_type}" }}
  compute: {{ kind: wasm, module: ./compute, function: process }}
  output: {{ type: AirQualityIndexDaily, mode: upsert }}
"#
        );
        let refused = Pipeline::from_yaml(&yaml)
            .map_err(|e| e.to_string())
            .and_then(|manifest| manifest.validate().map_err(|e| e.to_string()));
        assert!(refused.is_err(), "{entity_type} was accepted");
    }
}

/// A `#` in `geoQ` is data, not the start of a fragment that silently drops what follows.
#[test]
fn a_geoq_value_containing_a_hash_does_not_truncate_the_query_string() {
    let url = fetch_url("      type: AirQualityObserved\n      geoQ: 'georel=near;maxDistance==100#x&coordinates=[1,2]'");
    assert_eq!(
        values_of(&url, "geoQ"),
        vec!["georel=near;maxDistance==100#x&coordinates=[1,2]".to_owned()],
        "{url}"
    );
    assert_eq!(values_of(&url, "limit"), vec!["1000".to_owned()], "{url}");
}

/// The page size is the platform's, whatever a value holds.
#[test]
fn the_fixed_limit_parameter_cannot_be_overridden_by_a_query_value() {
    for field in ["q", "scopeQ", "geoQ"] {
        let url = fetch_url(&format!(
            "      type: AirQualityObserved\n      {field}: 'a&limit=100000'"
        ));
        assert_eq!(
            values_of(&url, "limit"),
            vec!["1000".to_owned()],
            "{field}: {url}"
        );
        assert_eq!(
            values_of(&url, field),
            vec!["a&limit=100000".to_owned()],
            "{field}: {url}"
        );
    }
}

/// PL-42: the ids are PF-42 URNs, which jc-core parses before they reach the renderer, so none
/// can carry a separator; the list is exactly the ids, comma-joined.
#[test]
fn ids_are_joined_without_letting_one_id_break_out_of_the_list() {
    let url = fetch_url(
        "      type: AirQualityObserved\n      ids:\n        - urn:ngsi-ld:AirQualityObserved:banskabystrica.sk:air-quality:a\n        - urn:ngsi-ld:AirQualityObserved:banskabystrica.sk:air-quality:b",
    );
    assert_eq!(
        values_of(&url, "id"),
        vec!["urn:ngsi-ld:AirQualityObserved:banskabystrica.sk:air-quality:a,urn:ngsi-ld:AirQualityObserved:banskabystrica.sk:air-quality:b".to_owned()],
        "{url}"
    );
    for smuggled in [
        "urn:ngsi-ld:AirQualityObserved:banskabystrica.sk:air-quality:a&limit=1",
        "urn:ngsi-ld:AirQualityObserved:banskabystrica.sk:air-quality:a,b",
    ] {
        let yaml = format!(
            "apiVersion: joinedcontext.com/v1alpha1\nkind: Pipeline\nmetadata: {{ name: p, namespace: ovzdusie }}\nspec:\n  targetEndpoint: urn:ngsi-ld:Endpoint:banskabystrica.sk:ovzdusie:ep-derived\n  class: scheduled\n  schedule: \"10 0 * * *\"\n  source:\n    endpointRef: {{ kind: Endpoint, name: air-quality-internal }}\n    query: {{ type: AirQualityObserved, ids: [\"{smuggled}\"] }}\n  compute: {{ kind: wasm, module: ./compute, function: process }}\n  output: {{ type: AirQualityIndexDaily, mode: upsert }}\n"
        );
        let refused = Pipeline::from_yaml(&yaml)
            .map_err(|e| e.to_string())
            .and_then(|manifest| manifest.validate().map_err(|e| e.to_string()));
        assert!(refused.is_err(), "{smuggled} was accepted as one id");
    }
}

/// An attribute name holding a comma would read as two at the gateway, which splits the decoded
/// list on it, so it is refused by name rather than rendered.
#[test]
fn an_attrs_entry_containing_a_comma_is_not_confused_with_the_list_separator() {
    let refused = render(
        &with_query("      type: AirQualityObserved\n      attrs: ['pm10,pm25', no2]"),
        &context(),
    );
    assert_eq!(
        refused,
        Err(DerivedError::BadAttribute("pm10,pm25".to_owned()))
    );
    let url = fetch_url("      type: AirQualityObserved\n      attrs: [pm10, 'no2 (µg)']");
    assert_eq!(
        values_of(&url, "attrs"),
        vec!["pm10,no2 (µg)".to_owned()],
        "{url}"
    );
}

/// A value reaches the gateway exactly as written: non-ASCII text and a literal `%41` included.
#[test]
fn a_q_value_with_unicode_or_percent_sequences_round_trips_unchanged() {
    for q in ["name==\"Žilina\"", "code==\"%41\"", "a b"] {
        let url = fetch_url(&format!("      type: AirQualityObserved\n      q: '{q}'"));
        assert_eq!(values_of(&url, "q"), vec![q.to_owned()], "{url}");
    }
}

#[test]
fn an_empty_query_with_only_a_type_produces_just_type_and_limit() {
    let url = fetch_url("      type: AirQualityObserved");
    assert_eq!(
        parameters(&url),
        vec![
            ("type".to_owned(), "AirQualityObserved".to_owned()),
            ("limit".to_owned(), "1000".to_owned()),
        ]
    );
}

/// A window of nothing asks for nothing: `window_seconds` refuses a total of zero, and a
/// negative duration does not parse (ISO 8601 has no sign here).
#[test]
fn a_temporal_window_of_zero_or_negative_seconds_is_refused_or_clamped() {
    for window in ["P0D", "PT0S", "-P1D", "P", "PT"] {
        let refused = render(
            &with_query(&format!(
                "      type: AirQualityObserved\n      temporalQ: {{ window: '{window}' }}"
            )),
            &context(),
        );
        assert!(
            matches!(refused, Err(DerivedError::BadWindow(ref w)) if w == window),
            "{window}: {refused:?}"
        );
    }
}

/// PL-14: the token is a reference the runner fills in its own header, and nothing a manifest
/// writes can make the runner expand a variable or an interpolation into the URL it sends: a
/// `${VAR}` is substituted when Bento loads the config and a `${! … }` on every message, so either
/// would put the runner's credentials into a query string the gateway logs.
#[test]
fn the_service_account_token_placeholder_never_becomes_a_literal_value_in_rendered_config() {
    for field in ["q", "scopeQ", "geoQ"] {
        for value in [
            "${SERVICE_ACCOUNT_TOKEN}",
            "${! env(\"JC_CLIENT_SECRET\") }",
        ] {
            let url = fetch_url(&format!(
                "      type: AirQualityObserved\n      {field}: '{value}'"
            ));
            let written = url
                .split_once('?')
                .map(|(_, query)| query)
                .unwrap_or_default();
            // The one interpolation the renderer writes itself is the temporal instant, and this
            // query has none.
            assert!(!written.contains("${"), "{field}: {url}");
        }
    }
    let url =
        fetch_url("      type: AirQualityObserved\n      attrs: ['${SERVICE_ACCOUNT_TOKEN}']");
    assert!(!url.contains("${"), "{url}");
    let derived =
        render(&with_query("      type: AirQualityObserved"), &context()).expect("renders");
    assert_eq!(
        derived.processors[0]["http"]["headers"]["Authorization"],
        "Bearer ${SERVICE_ACCOUNT_TOKEN}"
    );
}
