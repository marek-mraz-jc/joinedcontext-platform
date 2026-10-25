//! Stale-entity expiry on a Pipeline: off unless written, a bounded window, the types named once,
//! one output (PL-64).

use jc_core::kinds::Pipeline;

fn with(spec_tail: &str) -> String {
    format!(
        r#"apiVersion: joinedcontext.com/v1alpha2
kind: Pipeline
metadata:
  name: vehicles
  namespace: helsinki
spec:
  class: resident
  period: 30s
  sources:
    - dataSourceRef: {{ kind: DataSource, name: vehicle-feed }}
  outputs:
    - targetEndpoint: urn:ngsi-ld:Endpoint:hel.fi:helsinki:ep-vehicles-write
{spec_tail}"#
    )
}

fn refusal(yaml: &str) -> String {
    match Pipeline::from_yaml(yaml) {
        Err(error) => error.to_string(),
        Ok(pipeline) => pipeline.validate().expect_err("refused").to_string(),
    }
}

#[test]
fn a_pipeline_without_expiry_deletes_nothing_and_writes_none_back() {
    let pipeline = Pipeline::from_yaml(&with("")).expect("parses");
    pipeline.validate().expect("valid");
    assert_eq!(pipeline.spec.expiry, None);
    let written = serde_json::to_value(&pipeline.spec).expect("serialises");
    assert!(written.get("expiry").is_none(), "{written}");
}

#[test]
fn a_window_in_hours_or_days_round_trips() {
    for (after, hours) in [
        ("1h", 1),
        ("12h", 12),
        ("14d", 336),
        ("365d", 8760),
        ("8760h", 8760),
    ] {
        let yaml = with(&format!(
            "  expiry:\n    after: {after}\n    types: [Vehicle]\n"
        ));
        let pipeline = Pipeline::from_yaml(&yaml).expect("parses");
        pipeline
            .validate()
            .unwrap_or_else(|e| panic!("{after}: {e}"));
        let expiry = pipeline.spec.expiry.as_ref().expect("expiry");
        assert_eq!(expiry.hours(), Some(hours), "{after}");
        let written = serde_json::to_value(&pipeline.spec).expect("serialises");
        assert_eq!(written["expiry"]["after"], after);
        assert_eq!(written["expiry"]["types"][0], "Vehicle");
    }
}

#[test]
fn a_window_outside_one_hour_to_a_year_or_in_other_units_is_refused() {
    for after in [
        "0h",
        "0d",
        "366d",
        "8761h",
        "30m",
        "90s",
        "1w",
        "d",
        "h",
        "",
        "01d",
        "1.5d",
        "-1d",
        "14 d",
        "99999999999999999999d",
    ] {
        let yaml = with(&format!(
            "  expiry:\n    after: \"{after}\"\n    types: [Vehicle]\n"
        ));
        let reason = refusal(&yaml);
        assert!(reason.contains("spec.expiry.after"), "{after}: {reason}");
        assert!(
            reason.contains("1h") && reason.contains("365d"),
            "{after}: {reason}"
        );
    }
}

#[test]
fn the_types_are_one_to_twenty_distinct_entity_types() {
    let reason = refusal(&with("  expiry:\n    after: 14d\n    types: []\n"));
    assert!(reason.contains("spec.expiry.types"), "{reason}");

    let many: Vec<String> = (0..21).map(|i| format!("T{i}")).collect();
    let reason = refusal(&with(&format!(
        "  expiry:\n    after: 14d\n    types: [{}]\n",
        many.join(", ")
    )));
    assert!(reason.contains("one to 20"), "{reason}");

    let reason = refusal(&with(
        "  expiry:\n    after: 14d\n    types: [Vehicle, Vehicle]\n",
    ));
    assert!(reason.contains("`Vehicle` is named twice"), "{reason}");

    // A type that could widen the sweep's query is not an entity type at all.
    for bad in ["\"Vehicle,Bus\"", "\"urn:x\"", "\"\"", "\"Veh icle\""] {
        let reason = refusal(&with(&format!(
            "  expiry:\n    after: 14d\n    types: [{bad}]\n"
        )));
        assert!(!reason.is_empty(), "{bad} accepted");
    }
}

#[test]
fn expiry_needs_exactly_one_output_and_no_unknown_field() {
    let two = with("    - targetEndpoint: urn:ngsi-ld:Endpoint:hel.fi:other:ep-other\n  expiry:\n    after: 14d\n    types: [Vehicle]\n");
    let reason = refusal(&two);
    assert!(reason.contains("more than one output"), "{reason}");

    let reason = refusal(&with(
        "  expiry:\n    after: 14d\n    types: [Vehicle]\n    everything: true\n",
    ));
    assert!(reason.contains("everything"), "{reason}");
}
