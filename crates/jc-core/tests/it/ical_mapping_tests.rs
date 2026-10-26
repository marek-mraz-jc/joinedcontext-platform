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
    let [output] = ok.ical_outputs() else {
        panic!("the single object is one output: {:?}", ok.ical_outputs());
    };
    assert_eq!(
        output.name, "events",
        "the deprecated object is the output `events`"
    );
    let ical = &output.sources[0];
    assert_eq!(
        ical.base(),
        None,
        "a mapping without a preset is the whole mapping"
    );
    assert_eq!(ical.effective().len(), 2);

    let listed = endpoint("enabledRepresentations: [ngsi-ld, ical]\n").unwrap();
    assert!(
        listed.ical_outputs().is_empty(),
        "no output listed serves no calendar (owner 2026-10-22T09:25Z)"
    );
    assert!(
        endpoint("").unwrap().ical_outputs().is_empty(),
        "ical is opt-in"
    );

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
        ["`ical` output `events` maps DTSTART to `name`, a LanguageProperty; DTSTART takes date or datetime"]
    );
    assert!(endpoint("")
        .unwrap()
        .representation_gaps(None)
        .unwrap()
        .is_empty());
}

const OUTPUTS: &str = "enabledRepresentations: [ngsi-ld, ical]\nrepresentations:\n  ical:\n";

fn output(yaml: &str) -> String {
    format!(
        "{OUTPUTS}{}",
        yaml.lines()
            .map(|l| format!("    {l}\n"))
            .collect::<String>()
    )
}

/// EP-159 (owner 2026-10-22T09:30Z): named outputs, unique names, at most 20, presets only over
/// the event types, BCP 47 tags, a DURATION constant.
#[test]
fn the_endpoint_lists_named_outputs() {
    let spec = endpoint(&output(
        "- name: vsetky\n  title: { sk: Všetky podujatia, en-GB: All events }\n  lang: sk-SK\n  timezone: Europe/Bratislava\n  sources: [{ type: Event, preset: sdm-event, mapping: { DURATION: { constant: PT2H }, LAST-MODIFIED: dateModified } }]\n- name: sport\n  sort: start\n  limit: 500\n  filter: { q: 'eventCategory==\"sport\"' }\n  sources: [{ type: Event }, { type: CouncilMeeting }]\n",
    ))
    .unwrap();
    let names: Vec<&str> = spec
        .ical_outputs()
        .iter()
        .map(|o| o.name.as_str())
        .collect();
    assert_eq!(names, ["vsetky", "sport"]);
    let wire = serde_json::to_value(&spec.representations).unwrap();
    assert_eq!(
        wire["ical"][1],
        serde_json::json!({"name": "sport", "sources": [{"type": "Event"}, {"type": "CouncilMeeting"}],
            "filter": {"q": "eventCategory==\"sport\""}, "sort": "start", "limit": 500}),
        "the broker reads these member names (antares-ical Output)"
    );

    let refused = |yaml: &str, says: &str| {
        let err = endpoint(&output(yaml)).unwrap_err();
        assert!(err.contains(says), "{yaml}: {err}");
    };
    refused(
        "- { name: a, sources: [{}] }\n- { name: a, sources: [{}] }\n",
        "the output name `a` twice",
    );
    let many: String = (0..=20)
        .map(|i| format!("- {{ name: o{i}, sources: [{{}}] }}\n"))
        .collect();
    refused(&many, "lists 21 outputs, at most 20");
    refused(
        "- { name: Sport, sources: [{}] }\n",
        "`ical` output `Sport`: its name is not",
    );
    refused("- { name: preview, sources: [{}] }\n", "its name is not");
    refused("- { name: a, sources: [] }\n", "it needs 1 to 10 sources");
    refused(
        "- { name: a, sources: [{ type: Article }] }\n",
        "`ical` output `a` uses a preset for Article",
    );
    refused(
        "- { name: a, lang: sk_SK, sources: [{}] }\n",
        "`lang: sk_SK` is not a language tag",
    );
    refused("- { name: a, sort: name, sources: [{}] }\n", "`sort: name`");
    refused(
        "- { name: a, limit: 0, sources: [{}] }\n",
        "`limit` must be 1 to 10000",
    );
    refused(
        "- { name: a, filter: {}, sources: [{}] }\n",
        "`filter` is empty",
    );
    refused(
        "- { name: a, filter: { georel: near }, sources: [{}] }\n",
        "needs georel, geometry and coordinates",
    );
    refused(
        "- { name: a, filter: { q: \"a==1\\r\\nX:y\" }, sources: [{}] }\n",
        "`filter.q` must be",
    );
    refused(
        "- { name: a, title: { sk: \"a\\r\\nX-INJECT:1\" }, sources: [{}] }\n",
        "`title` takes a language tag",
    );
    refused(
        "- { name: a, sources: [{ mapping: { DURATION: { constant: \"PT2H\\r\\nX:y\" } } }] }\n",
        "an RFC 5545 duration",
    );
    refused(
        "- { name: a, version: 2, sources: [{}] }\n",
        "it is version 2",
    );
    refused(
        "- { name: a, sources: [{}], colour: red }\n",
        "unknown field `colour`",
    );
    // a type other than the event types maps its own DTSTART and SUMMARY
    endpoint(&output("- { name: a, sources: [{ type: Article, mapping: { DTSTART: datePublished, SUMMARY: headline } }] }\n")).unwrap();
}

#[test]
fn an_output_names_itself_in_the_model_gaps() {
    let model = testdata("model.linkml.yaml");
    let spec = endpoint(&output(
        "- { name: kultura, sources: [{ preset: sdm-event, mapping: { DTSTART: name } }] }\n",
    ))
    .unwrap();
    assert_eq!(
        spec.representation_gaps(Some(&model)).unwrap(),
        ["`ical` output `kultura` maps DTSTART to `name`, a LanguageProperty; DTSTART takes date or datetime"]
    );
}

/// A model of sources that mix whole-day and timed events keeps `startDate` a `string` with
/// an ISO 8601 pattern (bbsk-podujatia, slovensko-podujatia): DTSTART takes it, and a string
/// without such a pattern stays refused (T-5208: the Portal refused both Endpoints whole).
#[test]
fn a_string_with_an_iso_8601_date_pattern_is_a_date() {
    let model = |pattern: &str| {
        format!(
            "classes:\n  Event:\n    slots: [name, startDate]\nslots:\n  name:\n    required: true\n  \
             startDate:\n    range: string\n    required: true\n{pattern}"
        )
    };
    let gaps = |pattern: &str| {
        let ical: IcalMapping =
            serde_norway::from_str("type: Event\nmapping: { DTSTART: startDate, SUMMARY: name }\n")
                .unwrap();
        ical.model_gaps(Some(&model(pattern))).unwrap()
    };
    let refused =
        ["`ical` maps DTSTART to `startDate`, range string; DTSTART takes date or datetime"];
    let dated = "^[0-9]{4}-[0-9]{2}-[0-9]{2}(T[0-9]{2}:[0-9]{2}(:[0-9]{2}(\\.[0-9]+)?)?(Z|[+-][0-9]{2}:[0-9]{2}))?$";
    assert_eq!(
        gaps(&format!("    pattern: '{dated}'\n")),
        Vec::<String>::new()
    );
    assert_eq!(
        gaps("    pattern: '^[0-9]{4}-[0-9]{2}-[0-9]{2}$'\n"),
        Vec::<String>::new()
    );
    assert_eq!(gaps(""), refused);
    // free text, an unanchored date and a pattern that does not compile are no dates
    assert_eq!(gaps("    pattern: '^.*$'\n"), refused);
    assert_eq!(gaps("    pattern: '[0-9]{4}-[0-9]{2}-[0-9]{2}'\n"), refused);
    assert_eq!(gaps("    pattern: '^[0-9]{4}-(['\n"), refused);
}
