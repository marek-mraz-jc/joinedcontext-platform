//! `spec.sourceDeletes` on a Pipeline: the four modes, their defaults and every refusal
//! (PL-96…PL-99).

use jc_core::kinds::data_source::check_class;
use jc_core::kinds::{DataSourceSpec, DataSourceType, Pipeline, SourceDeleteMode};

fn with(clock: &str, spec_tail: &str) -> String {
    format!(
        r#"apiVersion: joinedcontext.com/v1alpha2
kind: Pipeline
metadata:
  name: datasets
  namespace: catalogue
spec:
  class: auto
{clock}  sources:
    - dataSourceRef: {{ kind: DataSource, name: ckan-feed }}
  outputs:
    - targetEndpoint: urn:ngsi-ld:Endpoint:bbsk.sk:catalogue:ep-datasets-write
{spec_tail}"#
    )
}

const HOURLY: &str = "  schedule: \"0 * * * *\"\n";

fn refusal(yaml: &str) -> String {
    match Pipeline::from_yaml(yaml) {
        Err(error) => error.to_string(),
        Ok(pipeline) => pipeline.validate().expect_err("refused").to_string(),
    }
}

fn accepted(yaml: &str) -> Pipeline {
    let pipeline = Pipeline::from_yaml(yaml).expect("parses");
    pipeline.validate().expect("valid");
    pipeline
}

#[test]
fn absent_means_nothing_deleted_and_nothing_written_back() {
    let pipeline = accepted(&with(HOURLY, ""));
    assert_eq!(pipeline.spec.source_deletes, None);
    let written = serde_json::to_value(&pipeline.spec).expect("serialises");
    assert!(written.get("sourceDeletes").is_none(), "{written}");
}

#[test]
fn never_is_explicit_and_round_trips() {
    let pipeline = accepted(&with(HOURLY, "  sourceDeletes: { mode: never }\n"));
    let rule = pipeline.spec.source_deletes.as_ref().expect("rule");
    assert_eq!(rule.mode, SourceDeleteMode::Never);
    let written = serde_json::to_value(&pipeline.spec).expect("serialises");
    assert_eq!(
        written["sourceDeletes"],
        serde_json::json!({"mode": "never"})
    );
}

#[test]
fn a_snapshot_defaults_to_two_misses_and_ten_percent() {
    let pipeline = accepted(&with(HOURLY, "  sourceDeletes: { mode: snapshot }\n"));
    let rule = pipeline.spec.source_deletes.as_ref().expect("rule");
    assert_eq!((rule.misses(), rule.max_share()), (2, 10));
}

#[test]
fn a_snapshot_takes_misses_and_share_at_their_bounds() {
    for (misses, share) in [(1, 1), (10, 100), (3, 25)] {
        let tail =
            format!("  sourceDeletes: {{ mode: snapshot, misses: {misses}, maxShare: {share} }}\n");
        let pipeline = accepted(&with("  period: 1h\n", &tail));
        let rule = pipeline.spec.source_deletes.as_ref().expect("rule");
        assert_eq!((rule.misses(), rule.max_share()), (misses, share));
    }
}

#[test]
fn a_snapshot_refuses_misses_or_share_out_of_range() {
    for (field, value) in [
        ("misses", 0),
        ("misses", 11),
        ("maxShare", 0),
        ("maxShare", 101),
    ] {
        let tail = format!("  sourceDeletes: {{ mode: snapshot, {field}: {value} }}\n");
        let error = refusal(&with(HOURLY, &tail));
        assert!(
            error.contains(&format!("spec.sourceDeletes.{field}")),
            "{field}={value}: {error}"
        );
    }
    let error = refusal(&with(
        HOURLY,
        "  sourceDeletes: { mode: snapshot, maxShare: 300 }\n",
    ));
    assert!(
        error.contains("maxShare") || error.contains("300"),
        "{error}"
    );
}

#[test]
fn a_snapshot_on_a_pipeline_that_waits_for_pushes_is_refused() {
    let error = refusal(&with("", "  sourceDeletes: { mode: snapshot }\n"));
    assert!(error.contains("spec.sourceDeletes.mode"), "{error}");
    assert!(error.contains("waits for pushes"), "{error}");
}

#[test]
fn a_snapshot_of_a_source_that_never_ends_is_refused() {
    let pipeline = accepted(&with(HOURLY, "  sourceDeletes: { mode: snapshot }\n"));
    let mut source: DataSourceSpec = serde_norway::from_str(
        "type: mqtt\nmqtt:\n  urls: [\"tls://broker.example:8883\"]\n  topics: [\"bus/#\"]\n",
    )
    .expect("mqtt source");
    let mut resident = pipeline.spec.clone();
    resident.class = jc_core::kinds::PipelineClass::Resident;
    let error = check_class(&resident, &source)
        .expect_err("mqtt never ends")
        .to_string();
    assert!(error.contains("spec.sourceDeletes.mode"), "{error}");
    source.source_type = DataSourceType::Http;
    check_class(&resident, &source).expect("http ends");
}

#[test]
fn events_need_a_delete_marker() {
    for tail in [
        "  sourceDeletes: { mode: events }\n",
        "  sourceDeletes: { mode: events, deletedWhen: \"  \" }\n",
    ] {
        let error = refusal(&with("", tail));
        assert!(error.contains("spec.sourceDeletes.deletedWhen"), "{error}");
    }
    let pipeline = accepted(&with(
        "",
        "  sourceDeletes: { mode: events, deletedWhen: 'this.status.value == \"withdrawn\"' }\n",
    ));
    let rule = pipeline.spec.source_deletes.as_ref().expect("rule");
    assert_eq!(rule.mode, SourceDeleteMode::Events);
}

#[test]
fn an_events_marker_keeps_the_author_bloblang_rules() {
    let error = refusal(&with(
        "",
        "  sourceDeletes: { mode: events, deletedWhen: 'env(\"GITHUB_TOKEN\") != \"\"' }\n",
    ));
    assert!(error.contains("spec.sourceDeletes.deletedWhen"), "{error}");
}

#[test]
fn a_lease_needs_a_rolling_entity_lifetime() {
    let error = refusal(&with("", "  sourceDeletes: { mode: lease }\n"));
    assert!(error.contains("rolling"), "{error}");
    let error = refusal(&with(
        "",
        "  sourceDeletes: { mode: lease }\n  lifetime:\n    - type: Vehicle\n      entity: { at: \"2099-01-01T00:00:00Z\" }\n",
    ));
    assert!(error.contains("rolling"), "{error}");
    accepted(&with(
        "",
        "  sourceDeletes: { mode: lease }\n  lifetime:\n    - type: Vehicle\n      entity: { rolling: PT10M }\n",
    ));
}

#[test]
fn fields_of_another_mode_are_refused() {
    for (tail, field) in [
        ("  sourceDeletes: { mode: never, misses: 2 }\n", "misses"),
        (
            "  sourceDeletes: { mode: events, deletedWhen: 'true', maxShare: 5 }\n",
            "maxShare",
        ),
        (
            "  sourceDeletes: { mode: snapshot, deletedWhen: 'true' }\n",
            "deletedWhen",
        ),
    ] {
        let error = refusal(&with(HOURLY, tail));
        assert!(
            error.contains(&format!("spec.sourceDeletes.{field}")),
            "{tail}: {error}"
        );
    }
}

#[test]
fn an_unknown_field_or_mode_is_refused() {
    let error = refusal(&with(
        HOURLY,
        "  sourceDeletes: { mode: snapshot, owner: last-writer }\n",
    ));
    assert!(error.contains("owner"), "{error}");
    let error = refusal(&with(HOURLY, "  sourceDeletes: { mode: sometimes }\n"));
    assert!(error.contains("sometimes"), "{error}");
}

#[test]
fn source_deletes_and_the_age_sweep_are_refused_together() {
    let error = refusal(&with(
        HOURLY,
        "  sourceDeletes: { mode: snapshot }\n  expiry:\n    after: 14d\n    types: [Dataset]\n",
    ));
    assert!(error.contains("spec.expiry"), "{error}");
}
