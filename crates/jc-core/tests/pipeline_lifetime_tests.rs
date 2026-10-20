//! `spec.lifetime` of a Pipeline (PL-80…PL-83): every mode validates and is written back the
//! same, and each admission refusal that needs no model names its path and its rule.

use jc_core::kinds::pipeline_lifetime::seconds;
use jc_core::kinds::Pipeline;

fn with(steps: &str, lifetime: &str) -> String {
    format!(
        r#"apiVersion: joinedcontext.com/v1alpha2
kind: Pipeline
metadata:
  name: vehicles
  namespace: helsinki
spec:
  class: resident
  sources:
    - dataSourceRef: {{ kind: DataSource, name: vehicle-feed }}
{steps}  outputs:
    - targetEndpoint: urn:ngsi-ld:Endpoint:hel.fi:helsinki:ep-vehicles-write
      type: Bus
    - targetEndpoint: urn:ngsi-ld:Endpoint:hel.fi:helsinki:ep-alerts-write
      type: Alert
  lifetime:
{lifetime}"#
    )
}

fn accepted(yaml: &str) -> Pipeline {
    let pipeline = Pipeline::from_yaml(yaml).expect("parses");
    pipeline.validate().expect("valid");
    pipeline
}

fn refusal(yaml: &str) -> String {
    match Pipeline::from_yaml(yaml) {
        Err(error) => error.to_string(),
        Ok(pipeline) => pipeline.validate().expect_err("refused").to_string(),
    }
}

const MAPPING: &str =
    "  steps:\n    - kind: bloblang\n      bloblang: |\n        root = this\n        {line}\n";

fn mapping(line: &str) -> String {
    MAPPING.replace("{line}", line)
}

#[test]
fn every_mode_validates_and_is_written_back_unchanged() {
    for lifetime in [
        "    - type: Bus\n      entity: { from: observedAt, plus: PT10M }\n",
        "    - type: Alert\n      entity: { from: validTo, notAfter: P30D }\n",
        "    - type: Bus\n      entity: { rolling: PT5M }\n",
        "    - type: Alert\n      entity: { at: \"2030-01-01T00:00:00+01:00\" }\n",
        "    - type: Bus\n      attributes:\n        - { names: [location], rolling: PT5M }\n        - { names: [speed, bearing], from: observedAt, plus: PT1M }\n",
        "    - type: Bus\n      entity: { rolling: P1D }\n      attributes:\n        - { names: [location], at: \"2030-01-01T00:00:00Z\" }\n",
    ] {
        let pipeline = accepted(&with("", lifetime));
        let written = serde_json::to_value(&pipeline.spec).expect("serializes");
        let back: jc_core::kinds::PipelineSpec =
            serde_json::from_value(written.clone()).expect("reads back");
        assert_eq!(back, pipeline.spec, "{lifetime}");
        assert!(written["lifetime"][0]["type"].is_string(), "{written}");
    }
}

#[test]
fn a_pipeline_without_lifetime_writes_none_back() {
    let yaml = with("", "").replace("  lifetime:\n", "");
    let written = serde_json::to_value(&accepted(&yaml).spec).expect("serializes");
    assert!(written.get("lifetime").is_none(), "{written}");
}

#[test]
fn durations_are_iso_8601_from_a_minute_to_ten_years() {
    for (text, expected) in [
        ("PT1M", Some(60)),
        ("PT5M", Some(300)),
        ("PT1H30M", Some(5400)),
        ("P30D", Some(2_592_000)),
        ("P1DT12H", Some(129_600)),
        ("P3650D", Some(315_360_000)),
        ("PT59S", None),
        ("P3651D", None),
        ("PT", None),
        ("P", None),
        ("5m", None),
        ("P1W", None),
        ("PT1.5M", None),
        ("-PT5M", None),
        ("P99999999999999999999D", None),
    ] {
        assert_eq!(seconds(text), expected, "{text}");
    }
    let reason = refusal(&with(
        "",
        "    - type: Bus\n      entity: { rolling: PT30S }\n",
    ));
    assert!(
        reason.contains("spec.lifetime[0].entity.rolling") && reason.contains("PT1M"),
        "{reason}"
    );
    let reason = refusal(&with(
        "",
        "    - type: Bus\n      entity: { from: observedAt, plus: 10m }\n",
    ));
    assert!(reason.contains("spec.lifetime[0].entity.plus"), "{reason}");
}

#[test]
fn one_mode_per_target_and_plus_belongs_to_from() {
    let reason = refusal(&with(
        "",
        "    - type: Bus\n      entity: { rolling: PT5M, at: \"2030-01-01T00:00:00Z\" }\n",
    ));
    assert!(
        reason.contains("spec.lifetime[0].entity") && reason.contains("exactly one"),
        "{reason}"
    );
    let reason = refusal(&with("", "    - type: Bus\n      entity: {}\n"));
    assert!(reason.contains("exactly one"), "{reason}");
    let reason = refusal(&with(
        "",
        "    - type: Bus\n      entity: { rolling: PT5M, notAfter: P1D }\n",
    ));
    assert!(
        reason.contains("`plus` and `notAfter` belong to `from`"),
        "{reason}"
    );
    let reason = refusal(&with(
        "",
        "    - type: Bus\n      attributes:\n        - { names: [location], from: observedAt, rolling: PT5M }\n",
    ));
    assert!(
        reason.contains("spec.lifetime[0].attributes[0]") && reason.contains("exactly one"),
        "{reason}"
    );
}

#[test]
fn an_at_without_an_offset_is_refused() {
    for at in ["2030-01-01T00:00:00", "2030-01-01", "tomorrow"] {
        let reason = refusal(&with(
            "",
            &format!("    - type: Alert\n      entity: {{ at: \"{at}\" }}\n"),
        ));
        assert!(
            reason.contains("spec.lifetime[0].entity.at") && reason.contains("RFC 3339"),
            "{at}: {reason}"
        );
    }
}

#[test]
fn names_are_slot_names_and_never_a_quote_or_a_newline() {
    for name in [
        r#""a\"b""#,
        r#""a\nb""#,
        "\"a.b\"",
        "\"a b\"",
        "\"1st\"",
        "\"\"",
    ] {
        let reason = refusal(&with(
            "",
            &format!("    - type: Bus\n      attributes:\n        - {{ names: [{name}], rolling: PT5M }}\n"),
        ));
        assert!(reason.contains("not a slot name"), "{name}: {reason}");
    }
    let reason = refusal(&with(
        "",
        "    - type: Bus\n      entity: { from: \"valid\\\"To\" }\n",
    ));
    assert!(
        reason.contains("spec.lifetime[0].entity.from") && reason.contains("not a slot name"),
        "{reason}"
    );
}

#[test]
fn a_system_member_is_no_attribute_name() {
    for name in ["expiresAt", "createdAt", "id", "type", "scope", "deletedAt"] {
        let reason = refusal(&with(
            "",
            &format!("    - type: Bus\n      attributes:\n        - {{ names: [{name}], rolling: PT5M }}\n"),
        ));
        assert!(
            reason.contains("system member") && reason.contains("DM-130"),
            "{name}: {reason}"
        );
    }
}

#[test]
fn an_attribute_has_one_lifetime_and_a_type_one_entry() {
    let reason = refusal(&with(
        "",
        "    - type: Bus\n      attributes:\n        - { names: [location], rolling: PT5M }\n        - { names: [speed, location], rolling: PT1H }\n",
    ));
    assert!(
        reason.contains("spec.lifetime[0].attributes[1].names") && reason.contains("named twice"),
        "{reason}"
    );
    let reason = refusal(&with(
        "",
        "    - type: Bus\n      entity: { rolling: PT5M }\n    - type: Bus\n      entity: { rolling: PT1H }\n",
    ));
    assert!(
        reason.contains("spec.lifetime[1].type") && reason.contains("one entry per type"),
        "{reason}"
    );
    let reason = refusal(&with(
        "",
        "    - type: Bus\n      attributes:\n        - { names: [], rolling: PT5M }\n",
    ));
    assert!(reason.contains("one to 64"), "{reason}");
    let reason = refusal(&with("", "    - type: Bus\n"));
    assert!(
        reason.contains("spec.lifetime[0]") && reason.contains("`entity`, `attributes` or both"),
        "{reason}"
    );
}

#[test]
fn a_type_no_output_writes_is_refused() {
    let reason = refusal(&with(
        "",
        "    - type: Tram\n      entity: { rolling: PT5M }\n",
    ));
    assert!(
        reason.contains("spec.lifetime[0].type") && reason.contains("Alert, Bus"),
        "{reason}"
    );
    let reason = refusal(&with(
        "",
        "    - type: bus\n      entity: { rolling: PT5M }\n",
    ));
    assert!(!reason.is_empty(), "{reason}");
    let many: String = (0..21)
        .map(|n| format!("    - type: Bus{n}\n      entity: {{ rolling: PT5M }}\n"))
        .collect();
    assert!(refusal(&with("", &many)).contains("one to 20"));
}

#[test]
fn an_entity_lifetime_and_the_stale_sweep_for_one_type_are_refused_together() {
    let one_output = |lifetime: &str| {
        with("", lifetime).replace(
            "    - targetEndpoint: urn:ngsi-ld:Endpoint:hel.fi:helsinki:ep-alerts-write\n      type: Alert\n",
            "",
        ) + "  expiry: { after: 1h, types: [Bus] }\n"
    };
    let reason = refusal(&one_output(
        "    - type: Bus\n      entity: { rolling: PT5M }\n",
    ));
    assert!(
        reason.contains("spec.lifetime[0].entity") && reason.contains("spec.expiry"),
        "{reason}"
    );
    accepted(&one_output(
        "    - type: Bus\n      attributes:\n        - { names: [location], rolling: PT5M }\n",
    ));
}

#[test]
fn a_mapping_that_writes_expires_at_and_a_lifetime_for_the_same_target_are_refused() {
    let entity = "    - type: Bus\n      entity: { rolling: PT5M }\n";
    let location =
        "    - type: Bus\n      attributes:\n        - { names: [location], rolling: PT5M }\n";
    for line in [
        "root.expiresAt = now()",
        "root.\"expiresAt\" = now()",
        "expiresAt = now()",
        "root = this.assign({\"expiresAt\": now()})",
    ] {
        let reason = refusal(&with(&mapping(line), entity));
        assert!(
            reason.contains("spec.lifetime[0].entity") && reason.contains("PL-83"),
            "{line}: {reason}"
        );
    }
    for line in [
        "root.location.expiresAt = now()",
        "location.expiresAt = now()",
        "root.\"location\".\"expiresAt\" = now()",
        "root.location = this.location.assign({\"expiresAt\": now()})",
    ] {
        let reason = refusal(&with(&mapping(line), location));
        assert!(
            reason.contains("spec.lifetime[0].attributes[0].names") && reason.contains("PL-83"),
            "{line}: {reason}"
        );
    }
    // Another attribute's expiresAt, a comparison or a read leave the lifetime alone.
    accepted(&with(&mapping("root.speed.expiresAt = now()"), location));
    accepted(&with(
        &mapping("root.flag = this.expiresAt == null"),
        entity,
    ));
    accepted(&with(&mapping("root.speed.expiresAt = now()"), entity));
    let processor = "  steps:\n    - processor:\n        mapping: 'root.expiresAt = now()'\n";
    assert!(refusal(&with(processor, entity)).contains("PL-83"));
}
