use jc_core::kinds::{ComputeKind, OutputMode, Pipeline, PipelineClass};
use jc_core::urn::Urn;

const GOLDEN_MQTT: &str = r#"apiVersion: joinedcontext.com/v1alpha1
kind: Pipeline
metadata:
  name: smart-meter-mqtt
  namespace: bb-energie
spec:
  class: resident             # resident | scheduled | auto
  targetEndpoint: urn:ngsi-ld:Endpoint:banskabystrica.sk:energie:ep-smart-meters
  secretRefs:
    - name: mqtt-credentials
      key: password
      envVar: MQTT_PASSWORD
  quotas:
    maxMemoryMb: 128
    cpuMillicores: 250
"#;

const GOLDEN_DERIVED: &str = r#"apiVersion: joinedcontext.com/v1alpha1
kind: Pipeline
metadata:
  name: district-air-index-daily
  namespace: bb-ovzdusie
spec:
  class: scheduled
  schedule: "10 0 * * *"                   # once a day, after midnight
  source:
    endpointRef: { kind: Endpoint, name: ovzdusie-internal }   # read grant, same or another space
    query: { type: AirQualityObserved, attrs: [pm10, pm25, refDistrict], temporalQ: { window: P1D } }
    # resident alternative: trigger: { subscription: { type: AirQualityObserved, watchedAttributes: [pm10, pm25] } }
  compute:
    kind: wasm                             # bloblang | mapping | wasm | container
    module: ./compute                      # Rust crate beside the pipeline, built in CI to wasm32-wasip1
    function: process
  targetEndpoint: urn:ngsi-ld:Endpoint:banskabystrica.sk:ovzdusie:ep-derived   # write grant; may point into another space
  output:
    type: AirQualityIndexDaily
    mode: upsert                           # upsert | update-attrs (same entity, new attributes)
"#;

#[test]
fn test_pipeline_smart_meter_mqtt_golden() {
    let p1 = Pipeline::from_yaml(GOLDEN_MQTT).expect("parses mqtt pipeline");
    p1.validate().expect("mqtt pipeline is valid");

    assert_eq!(p1.spec.class, PipelineClass::Resident);
    assert_eq!(
        p1.spec.target().expect("a target").to_string(),
        "urn:ngsi-ld:Endpoint:banskabystrica.sk:energie:ep-smart-meters"
    );
    assert_eq!(p1.spec.secret_refs.len(), 1);
    assert_eq!(p1.spec.secret_refs[0].name, "mqtt-credentials");
    assert_eq!(p1.spec.secret_refs[0].key.as_deref(), Some("password"));
    assert_eq!(
        p1.spec.secret_refs[0].env_var.as_deref(),
        Some("MQTT_PASSWORD")
    );

    let quotas = p1.spec.quotas.as_ref().expect("quotas present");
    assert_eq!(quotas.max_memory_mb, Some(128));
    assert_eq!(quotas.cpu_millicores, Some(250));

    let yaml = p1.to_yaml().expect("serializes to yaml");
    let roundtripped = Pipeline::from_yaml(&yaml).expect("deserializes back");
    assert_eq!(p1, roundtripped);

    let path = p1.resource_path().expect("resource path");
    assert_eq!(
        path,
        "projects/bb-energie/pipelines/smart-meter-mqtt/pipeline.yaml"
    );
}

#[test]
fn test_pipeline_district_air_index_daily_golden() {
    let p2 = Pipeline::from_yaml(GOLDEN_DERIVED).expect("parses derived pipeline");
    p2.validate().expect("derived pipeline is valid");

    assert_eq!(p2.spec.class, PipelineClass::Scheduled);
    assert_eq!(p2.spec.schedule.as_deref(), Some("10 0 * * *"));

    let source = p2.spec.source.as_ref().expect("source present");
    assert_eq!(
        source.endpoint_ref.as_ref().map(|r| r.name()),
        Some("ovzdusie-internal")
    );

    let query = source.query.as_ref().expect("source query present");
    assert_eq!(query.entity_type.as_deref(), Some("AirQualityObserved"));
    assert_eq!(query.attrs, vec!["pm10", "pm25", "refDistrict"]);
    assert_eq!(
        query.temporal_q.as_ref().map(|t| t.window.as_str()),
        Some("P1D")
    );

    let compute = p2.spec.compute.as_ref().expect("compute present");
    assert_eq!(compute.kind, ComputeKind::Wasm);
    assert_eq!(compute.module.as_deref(), Some("./compute"));
    assert_eq!(compute.function.as_deref(), Some("process"));

    assert_eq!(
        p2.spec.target().expect("a target").to_string(),
        "urn:ngsi-ld:Endpoint:banskabystrica.sk:ovzdusie:ep-derived"
    );

    let output = p2.spec.output.as_ref().expect("output present");
    assert_eq!(output.entity_type, "AirQualityIndexDaily");
    assert_eq!(output.mode, OutputMode::Upsert);

    let yaml = p2.to_yaml().expect("serializes to yaml");
    let roundtripped = Pipeline::from_yaml(&yaml).expect("deserializes back");
    assert_eq!(p2, roundtripped);
}

#[test]
fn test_pipeline_class_and_schedule_rules() {
    let mut p_sched = Pipeline::from_yaml(GOLDEN_DERIVED).expect("golden");
    p_sched.spec.schedule = None;
    assert!(
        p_sched.validate().is_err(),
        "scheduled class without schedule must fail"
    );

    let mut p_res = Pipeline::from_yaml(GOLDEN_MQTT).expect("golden");
    p_res.spec.schedule = Some("*/5 * * * *".to_string());
    assert!(
        p_res.validate().is_err(),
        "resident class with schedule must fail"
    );

    let mut p_auto = Pipeline::from_yaml(GOLDEN_MQTT).expect("golden");
    p_auto.spec.class = PipelineClass::Auto;
    p_auto.spec.schedule = None;
    assert!(
        p_auto.validate().is_ok(),
        "auto class without schedule must pass"
    );

    p_auto.spec.schedule = Some("0 * * * *".to_string());
    assert!(
        p_auto.validate().is_ok(),
        "auto class with schedule must pass"
    );
}

#[test]
fn test_cron_expression_validation() {
    let mut p = Pipeline::from_yaml(GOLDEN_DERIVED).expect("golden");

    // 4 fields
    p.spec.schedule = Some("0 0 * *".to_string());
    assert!(p.validate().is_err(), "4-field cron must fail");

    // 6 fields
    p.spec.schedule = Some("0 0 * * * *".to_string());
    assert!(p.validate().is_err(), "6-field cron must fail");
}

/// PL-40: `enabled` is absent-means-true and `true` is never written back, so a manifest
/// that never mentioned the field round-trips unchanged; a Pause is one explicit `false`.
#[test]
fn enabled_defaults_to_true_and_round_trips_only_when_false() {
    let running = Pipeline::from_yaml(GOLDEN_MQTT).expect("golden");
    assert!(running.spec.enabled);
    let yaml = serde_norway::to_string(&running).expect("serializes");
    assert!(
        !yaml.contains("enabled"),
        "true is the default and stays implicit:\n{yaml}"
    );

    let paused_yaml =
        GOLDEN_MQTT.replace("  class: resident", "  class: resident\n  enabled: false");
    let paused = Pipeline::from_yaml(&paused_yaml).expect("a paused manifest parses");
    paused.validate().expect("a paused manifest validates");
    assert!(!paused.spec.enabled);
    let yaml = serde_norway::to_string(&paused).expect("serializes");
    assert!(yaml.contains("enabled: false"), "{yaml}");
    let again = Pipeline::from_yaml(&yaml).expect("round trip");
    assert_eq!(again.spec, paused.spec);
}

/// PL-41: the inline mapping belongs to a `bloblang` step only, and an empty one is a mistake
/// rather than a way to say "see bento.yaml".
#[test]
fn inline_bloblang_is_for_bloblang_steps_only_and_never_empty() {
    let inline = GOLDEN_DERIVED.replace(
        "    kind: wasm                             # bloblang | mapping | wasm | container\n    module: ./compute                      # Rust crate beside the pipeline, built in CI to wasm32-wasip1\n    function: process\n",
        "    kind: bloblang\n    bloblang: |\n      root = this\n      root.computedBy = \"pipeline\"\n",
    );
    let p = Pipeline::from_yaml(&inline).expect("inline bloblang parses");
    p.validate().expect("inline bloblang validates");
    let mapping = p
        .spec
        .compute
        .as_ref()
        .unwrap()
        .bloblang
        .as_deref()
        .unwrap();
    assert!(mapping.starts_with("root = this\n"), "{mapping:?}");
    let yaml = serde_norway::to_string(&p).expect("serializes");
    assert_eq!(Pipeline::from_yaml(&yaml).expect("round trip").spec, p.spec);

    let mut wrong_kind = Pipeline::from_yaml(GOLDEN_DERIVED).expect("golden");
    wrong_kind.spec.compute.as_mut().unwrap().bloblang = Some("root = this".to_string());
    let err = wrong_kind
        .validate()
        .expect_err("bloblang on a wasm step must fail");
    assert!(err.to_string().contains("bloblang"), "{err}");

    let mut empty = p.clone();
    empty.spec.compute.as_mut().unwrap().bloblang = Some("  \n".to_string());
    assert!(empty.validate().is_err(), "an empty mapping must fail");
}

/// PL-42: `query.ids` pins the read to named entities; an id of another type than the query's
/// is a mistake the manifest refuses rather than a fetch that returns nothing.
#[test]
fn query_ids_round_trip_and_must_match_the_query_type() {
    let pinned = GOLDEN_DERIVED.replace(
        "    query: { type: AirQualityObserved, attrs: [pm10, pm25, refDistrict], temporalQ: { window: P1D } }\n",
        "    query:\n      type: AirQualityObserved\n      ids:\n        - urn:ngsi-ld:AirQualityObserved:banskabystrica.sk:ovzdusie:radvan-01\n        - urn:ngsi-ld:AirQualityObserved:banskabystrica.sk:ovzdusie:radvan-02\n",
    );
    let p = Pipeline::from_yaml(&pinned).expect("ids parse");
    p.validate().expect("ids of the query's type validate");
    let ids = &p.spec.source.as_ref().unwrap().query.as_ref().unwrap().ids;
    assert_eq!(ids.len(), 2);
    assert_eq!(ids[0].entity_type(), "AirQualityObserved");
    let yaml = serde_norway::to_string(&p).expect("serializes");
    assert_eq!(Pipeline::from_yaml(&yaml).expect("round trip").spec, p.spec);

    let mixed = pinned.replace(
        "urn:ngsi-ld:AirQualityObserved:banskabystrica.sk:ovzdusie:radvan-02",
        "urn:ngsi-ld:Device:banskabystrica.sk:ovzdusie:radvan-02",
    );
    let err = Pipeline::from_yaml(&mixed)
        .expect("parses")
        .validate()
        .expect_err("a Device id under an AirQualityObserved query must fail");
    assert!(err.to_string().contains("Device"), "{err}");
}

#[test]
fn test_compute_wasm_and_mapping_rules() {
    let mut p = Pipeline::from_yaml(GOLDEN_DERIVED).expect("golden");

    // wasm without function
    p.spec.compute.as_mut().unwrap().function = None;
    assert!(p.validate().is_err(), "wasm without function must fail");

    // mapping with module
    p.spec.compute.as_mut().unwrap().kind = ComputeKind::Mapping;
    p.spec.compute.as_mut().unwrap().mapping_ref =
        Some(jc_core::envelope::Ref::Name("mapping-name".to_string()));
    p.spec.compute.as_mut().unwrap().module = Some("./compute".to_string());
    p.spec.compute.as_mut().unwrap().function = None;
    assert!(p.validate().is_err(), "mapping with module must fail");
}

#[test]
fn test_target_endpoint_must_have_endpoint_type() {
    let mut p = Pipeline::from_yaml(GOLDEN_MQTT).expect("golden");
    p.spec.target_endpoint = Some(
        "urn:ngsi-ld:Policy:banskabystrica.sk:energie:public-air-quality"
            .parse::<Urn>()
            .expect("valid urn"),
    );
    assert!(
        p.validate().is_err(),
        "targetEndpoint with non-Endpoint entity type must fail"
    );
}

#[test]
fn test_pl14_secret_ref_rejects_inline_secret() {
    let bad_yaml = r#"
apiVersion: joinedcontext.com/v1alpha1
kind: Pipeline
metadata:
  name: smart-meter-mqtt
  namespace: bb-energie
spec:
  class: resident
  targetEndpoint: urn:ngsi-ld:Endpoint:banskabystrica.sk:energie:ep-smart-meters
  secretRefs:
    - name: mqtt-credentials
      key: password
      password: inline-plain-secret
"#;
    let res = Pipeline::from_yaml(bad_yaml);
    assert!(
        res.is_err(),
        "expected deserialization failure when secretRefs has inline password field"
    );
}

#[test]
fn test_quotas_zero_bounds_fail() {
    let mut p = Pipeline::from_yaml(GOLDEN_MQTT).expect("golden");
    p.spec.quotas.as_mut().unwrap().max_memory_mb = Some(0);
    assert!(p.validate().is_err(), "maxMemoryMb: 0 must fail");
}

/// PL-26 and PL-27 read `spec.period`; it is a Bento duration because the reconciler
/// copies it into `input.generate.interval`.
#[test]
fn period_parses_every_bento_unit_and_refuses_anything_else() {
    let mut spec = Pipeline::from_yaml(GOLDEN_MQTT)
        .expect("the golden manifest parses")
        .spec;

    for (period, seconds) in [
        ("250ms", 0u64),
        ("1500ms", 1),
        ("15s", 15),
        ("45s", 45),
        ("5m", 300),
        ("1h", 3600),
    ] {
        spec.period = Some(period.to_owned());
        spec.validate().unwrap_or_else(|e| panic!("{period}: {e}"));
        assert_eq!(spec.period_seconds(), Some(seconds), "{period}");
    }

    for bad in ["", "0s", "15", "15 s", "15sec", "-15s", "s", "1d", "1.5s"] {
        spec.period = Some(bad.to_owned());
        assert!(spec.validate().is_err(), "`{bad}` must be refused");
    }

    spec.period = None;
    assert_eq!(spec.period_seconds(), None, "no period means push-based");
    spec.validate()
        .expect("a push-based pipeline needs no period");
}
