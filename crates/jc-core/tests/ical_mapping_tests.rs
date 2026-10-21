//! EP-125 (T-4900): the `ical` mapping an Endpoint writes, checked against the space's model.
//! The cases live in `testdata/ical-mapping/`, which the broker's render tests share (T-4901).

use std::collections::BTreeMap;
use std::path::PathBuf;

use jc_core::kinds::{EndpointSpec, IcalMapping};
use serde::Deserialize;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Case {
    ical: IcalMapping,
    gaps: Vec<String>,
}

fn testdata(file: &str) -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../testdata/ical-mapping")
        .join(file);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

#[test]
fn every_case_reports_its_gaps() {
    let model = testdata("model.linkml.yaml");
    let cases: BTreeMap<String, Case> =
        serde_norway::from_str(&testdata("cases.yaml")).expect("cases parse");
    assert!(cases.len() >= 20, "the fixture set is complete");
    for (name, case) in cases {
        let gaps = case.ical.model_gaps(Some(&model)).expect("model parses");
        assert_eq!(gaps, case.gaps, "case {name}");
    }
}

#[test]
fn a_space_without_a_model_has_no_type() {
    assert_eq!(
        IcalMapping::default().model_gaps(None).unwrap(),
        ["`ical` maps the type Event, which the data model does not have"]
    );
    assert!(
        IcalMapping::default()
            .model_gaps(Some("classes: ["))
            .is_err(),
        "an unreadable model is an error, not a pass"
    );
}

/// The default preset reads the SDM table the broker served before mappings existed: an Endpoint
/// that lists `ical` with no mapping keeps working on a model with `startDate` and `name` required,
/// and an optional field whose attribute the type lacks is left out, not refused.
#[test]
fn the_default_is_the_sdm_event_table() {
    let minimal = "classes:\n  Event:\n    attributes:\n      name: { required: true }\n      startDate: { required: true, range: datetime }\n";
    assert_eq!(
        IcalMapping::default().model_gaps(Some(minimal)).unwrap(),
        Vec::<String>::new()
    );
    let not_required = "classes:\n  Event:\n    attributes:\n      name: {}\n      startDate: { required: true, range: datetime }\n";
    assert_eq!(
        IcalMapping::default().model_gaps(Some(not_required)).unwrap(),
        ["`ical` maps SUMMARY to `name`, which is not a required attribute of Event: mark it required or end a firstOf with a required one"]
    );
}

fn endpoint(extra: &str) -> Result<EndpointSpec, String> {
    let yaml =
        format!("contextSpaceRef: events\nslug: abcdefghijklmnopqrstuvwxyz234567\naudience: public\n{extra}");
    let spec: EndpointSpec = serde_norway::from_str(&yaml).map_err(|e| e.to_string())?;
    spec.validate().map_err(|e| e.to_string())?;
    Ok(spec)
}

#[test]
fn the_endpoint_parses_and_shape_checks_its_mapping() {
    let ok = endpoint("enabledRepresentations: [ngsi-ld, ical]\nrepresentations:\n  ical:\n    mapping:\n      DTSTART: startDate\n      SUMMARY: { attribute: name, language: sk }\n").unwrap();
    let ical = ok.ical_mapping().expect("serves ical");
    assert_eq!(
        ical.base(),
        None,
        "a mapping without a preset is the whole mapping"
    );
    assert_eq!(ical.effective().len(), 2);

    let listed = endpoint("enabledRepresentations: [ngsi-ld, ical]\n").unwrap();
    assert_eq!(listed.ical_mapping(), Some(IcalMapping::default()));
    assert_eq!(endpoint("").unwrap().ical_mapping(), None, "ical is opt-in");

    let unknown_field = endpoint("enabledRepresentations: [ical]\nrepresentations:\n  ical:\n    mapping: { X-WR-CALNAME: name }\n").unwrap_err();
    assert!(
        unknown_field.contains("unknown variant `X-WR-CALNAME`"),
        "{unknown_field}"
    );
    let unknown_remap = endpoint("enabledRepresentations: [ical]\nrepresentations:\n  ical:\n    mapping: { SUMMARY: { attribute: name, template: x } }\n").unwrap_err();
    assert!(
        unknown_remap.contains("unknown field `template`"),
        "{unknown_remap}"
    );
    let unknown_representation = endpoint("representations:\n  gtfs: {}\n").unwrap_err();
    assert!(
        unknown_representation.contains("unknown field `gtfs`"),
        "{unknown_representation}"
    );

    let not_served = endpoint("representations:\n  ical: { preset: schema-org }\n").unwrap_err();
    assert!(not_served.contains("does not serve `ical`"), "{not_served}");
    let shape = endpoint("enabledRepresentations: [ical]\nrepresentations:\n  ical:\n    preset: sdm-event\n    mapping: { DTSTAMP: startDate }\n").unwrap_err();
    assert!(shape.contains("DTSTAMP is fixed"), "{shape}");
}

#[test]
fn representation_gaps_reads_the_mapping_in_force() {
    let model = testdata("model.linkml.yaml");
    let spec = endpoint("enabledRepresentations: [ngsi-ld, ical]\nrepresentations:\n  ical:\n    preset: sdm-event\n    mapping: { DTSTART: name }\n").unwrap();
    assert_eq!(
        spec.representation_gaps(Some(&model)).unwrap(),
        ["`ical` maps DTSTART to `name`, a LanguageProperty; DTSTART takes date or datetime"]
    );
    assert!(endpoint("")
        .unwrap()
        .representation_gaps(None)
        .unwrap()
        .is_empty());
}
