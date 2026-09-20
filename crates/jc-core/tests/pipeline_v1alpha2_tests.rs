//! A Pipeline at `v1alpha2`: sources, steps, outputs, and `v1alpha1` read as it (PL-52…PL-55).

use jc_core::kinds::{ComputeKind, OutputMode, Pipeline, Step};

const MERGED: &str = r#"apiVersion: joinedcontext.com/v1alpha2
kind: Pipeline
metadata:
  name: bikes-merged
  namespace: helsinki
spec:
  class: resident
  period: 30s
  sources:
    - dataSourceRef: { kind: DataSource, name: citybikes-gbfs }
    - dataSourceRef: { kind: DataSource, name: citybikes-legacy-api }
  steps:
    - kind: bloblang
      bloblang: "root = this"
    - processor:
        dedupe: { cache: pipeline_changes, key: '${! json("id") }' }
  outputs:
    - targetEndpoint: urn:ngsi-ld:Endpoint:hel.fi:helsinki:ep-bikes-ops
    - targetEndpoint: urn:ngsi-ld:Endpoint:hel.fi:helsinki-kpi:ep-kpi-write
      mode: update-attrs
"#;

const FIRST: &str = r#"apiVersion: joinedcontext.com/v1alpha1
kind: Pipeline
metadata:
  name: district-air-index
  namespace: helsinki
spec:
  class: resident
  source:
    endpointRef: { kind: Endpoint, name: air-internal }
    query: { type: AirQualityObserved }
  compute:
    kind: bloblang
    bloblang: "root = this"
  targetEndpoint: urn:ngsi-ld:Endpoint:hel.fi:air:ep-derived
  output: { type: AirQualityIndex, mode: upsert }
"#;

fn parsed(yaml: &str) -> Pipeline {
    Pipeline::from_yaml(yaml).expect("parses")
}

fn refusal(yaml: &str) -> String {
    match Pipeline::from_yaml(yaml) {
        Err(error) => error.to_string(),
        Ok(pipeline) => pipeline.validate().expect_err("refused").to_string(),
    }
}

#[test]
fn a_merged_pipeline_reads_two_sources_two_steps_and_two_outputs() {
    let pipeline = parsed(MERGED);
    pipeline.validate().expect("valid");
    assert_eq!(pipeline.spec.sources().len(), 2);
    let steps = pipeline.spec.steps();
    assert!(matches!(&steps[0], Step::Compute(c) if c.kind == ComputeKind::Bloblang));
    assert!(matches!(&steps[1], Step::Processor(p) if p.processor.contains_key("dedupe")));
    let outputs = pipeline.spec.outputs();
    assert_eq!(outputs.len(), 2);
    assert_eq!(outputs[1].mode, Some(OutputMode::UpdateAttrs));
    // It writes back as it was read, in the second shape only.
    let again = parsed(&pipeline.to_yaml().expect("serializes"));
    assert_eq!(again, pipeline);
    assert!(again.spec.source.is_none() && again.spec.target_endpoint.is_none());
}

#[test]
fn a_first_version_pipeline_reads_as_the_second_it_means() {
    let pipeline = parsed(FIRST);
    pipeline.validate().expect("valid");
    let sources = pipeline.spec.sources();
    assert_eq!(sources.len(), 1);
    assert_eq!(
        sources[0].endpoint_ref.as_ref().map(|r| r.name()),
        Some("air-internal")
    );
    assert!(
        matches!(&pipeline.spec.steps()[..], [Step::Compute(c)] if c.bloblang.as_deref() == Some("root = this"))
    );
    let outputs = pipeline.spec.outputs();
    assert_eq!(outputs.len(), 1);
    assert_eq!(
        outputs[0].target_endpoint.to_string(),
        "urn:ngsi-ld:Endpoint:hel.fi:air:ep-derived"
    );
    assert_eq!(outputs[0].entity_type.as_deref(), Some("AirQualityIndex"));
    assert_eq!(outputs[0].mode, Some(OutputMode::Upsert));
    // Read, never rewritten: the file stays in the first shape.
    assert!(pipeline.spec.sources.is_empty() && pipeline.spec.outputs.is_empty());
}

#[test]
fn a_first_version_pipeline_without_compute_has_no_steps_and_one_without_source_no_sources() {
    let bare = FIRST
        .replace("  source:\n    endpointRef: { kind: Endpoint, name: air-internal }\n    query: { type: AirQualityObserved }\n", "")
        .replace("  compute:\n    kind: bloblang\n    bloblang: \"root = this\"\n", "");
    let pipeline = parsed(&bare);
    pipeline.validate().expect("valid");
    assert!(pipeline.spec.sources().is_empty());
    assert!(pipeline.spec.steps().is_empty());
    assert_eq!(pipeline.spec.outputs().len(), 1);
}

#[test]
fn the_second_version_refuses_the_first_versions_fields() {
    let mixed = MERGED.replace(
        "  outputs:\n",
        "  targetEndpoint: urn:ngsi-ld:Endpoint:hel.fi:helsinki:ep-x\n  outputs:\n",
    );
    // The envelope says which fields its version carries before the spec says anything.
    assert!(
        refusal(&mixed).contains("never `source`"),
        "{}",
        refusal(&mixed)
    );
    let only_old = FIRST.replace("v1alpha1", "v1alpha2");
    let error = Pipeline::from_yaml(&only_old)
        .expect("parses")
        .validate()
        .expect_err("refused");
    assert!(error.to_string().contains("v1alpha2"), "{error}");
}

#[test]
fn the_first_version_refuses_the_second_versions_fields() {
    let early = MERGED.replace("v1alpha2", "v1alpha1");
    let error = Pipeline::from_yaml(&early)
        .expect("parses")
        .validate()
        .expect_err("refused");
    assert!(error.to_string().contains("v1alpha2"), "{error}");
}

#[test]
fn only_a_pipeline_is_served_at_the_second_version() {
    assert!(jc_core::serves("Pipeline", "joinedcontext.com/v1alpha2"));
    assert!(jc_core::serves("Pipeline", "joinedcontext.com/v1alpha1"));
    assert!(!jc_core::serves("Endpoint", "joinedcontext.com/v1alpha2"));
    assert!(!jc_core::serves("Pipeline", "joinedcontext.com/v1beta1"));
    let endpoint = "apiVersion: joinedcontext.com/v1alpha2\nkind: Endpoint\nmetadata: { name: e, namespace: p }\nspec: {}\n";
    assert!(
        jc_core::ResourceEnvelope::<jc_core::kinds::EndpointSpec>::from_yaml(endpoint).is_err()
    );
}

#[test]
fn no_sources_or_no_outputs_is_refused() {
    let no_outputs = MERGED.split("  outputs:").next().expect("head").to_owned();
    assert!(
        refusal(&no_outputs).contains("at least one output"),
        "{}",
        refusal(&no_outputs)
    );
    let no_sources = MERGED.replace(
        "  sources:\n    - dataSourceRef: { kind: DataSource, name: citybikes-gbfs }\n    - dataSourceRef: { kind: DataSource, name: citybikes-legacy-api }\n",
        "",
    );
    assert!(
        refusal(&no_sources).contains("at least one source"),
        "{}",
        refusal(&no_sources)
    );
}

#[test]
fn a_processor_the_runner_does_not_ship_is_refused_with_the_list() {
    for name in ["no_such_processor", "command", "subprocess", "wasm"] {
        let yaml = MERGED.replace("dedupe:", &format!("{name}:"));
        let message = refusal(&yaml);
        assert!(
            message.contains(&format!("unknown processor `{name}`")),
            "{name}: {message}"
        );
        assert!(
            message.contains("mapping, "),
            "the list is named: {message}"
        );
    }
}

#[test]
fn a_processor_step_names_exactly_one_processor() {
    let two = MERGED.replace(
        "        dedupe: { cache: pipeline_changes, key: '${! json(\"id\") }' }\n",
        "        dedupe: { cache: pipeline_changes }\n        log: { message: x }\n",
    );
    assert!(
        refusal(&two).contains("exactly one processor"),
        "{}",
        refusal(&two)
    );
    let none = MERGED.replace(
        "        dedupe: { cache: pipeline_changes, key: '${! json(\"id\") }' }\n",
        "        {}\n",
    );
    assert!(Pipeline::from_yaml(&none).is_err() || refusal(&none).contains("exactly one"));
}

#[test]
fn a_container_step_is_alone_in_a_scheduled_pipeline() {
    let container = "    - kind: container\n";
    let beside = MERGED.replace(
        "    - kind: bloblang\n      bloblang: \"root = this\"\n",
        container,
    );
    assert!(
        refusal(&beside).contains("container step"),
        "{}",
        refusal(&beside)
    );
    let alone_resident = MERGED.replace(
        "    - kind: bloblang\n      bloblang: \"root = this\"\n    - processor:\n        dedupe: { cache: pipeline_changes, key: '${! json(\"id\") }' }\n",
        container,
    );
    assert!(
        refusal(&alone_resident).contains("container step"),
        "{}",
        refusal(&alone_resident)
    );
    let alone_scheduled = alone_resident.replace(
        "  class: resident\n  period: 30s\n",
        "  class: scheduled\n  schedule: \"0 * * * *\"\n",
    );
    parsed(&alone_scheduled)
        .validate()
        .expect("a lone container step in a scheduled pipeline");
}

#[test]
fn every_source_and_output_is_checked_like_the_first_versions_one() {
    let bad_target = MERGED.replace(
        "urn:ngsi-ld:Endpoint:hel.fi:helsinki-kpi:ep-kpi-write",
        "urn:ngsi-ld:Policy:hel.fi:helsinki-kpi:not-an-endpoint",
    );
    assert!(
        refusal(&bad_target).contains("Endpoint"),
        "{}",
        refusal(&bad_target)
    );
    let both = MERGED.replace(
        "    - dataSourceRef: { kind: DataSource, name: citybikes-legacy-api }\n",
        "    - dataSourceRef: { kind: DataSource, name: citybikes-legacy-api }\n      endpointRef: { kind: Endpoint, name: e }\n",
    );
    assert!(refusal(&both).contains("not both"), "{}", refusal(&both));
}
