//! API/11 EP-157, EP-159 (T-5217): an Endpoint's named feed and iCal outputs are checked when it
//! is saved, each refusal naming the member. The filter goes through the broker's own parser, so
//! a filter the broker would refuse at serve is refused here instead.

use std::path::PathBuf;

use jc_core::kinds::EndpointSpec;

fn model() -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../testdata/ical-mapping/model.linkml.yaml");
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

fn endpoint(representations: &str) -> Result<EndpointSpec, String> {
    let yaml = format!(
        "contextSpaceRef: events\nslug: abcdefghijklmnopqrstuvwxyz234567\naudience: public\n\
         enabledRepresentations: [ngsi-ld, ical]\nrepresentations:\n{representations}"
    );
    let spec: EndpointSpec = serde_norway::from_str(&yaml).map_err(|e| e.to_string())?;
    spec.validate().map_err(|e| e.to_string())?;
    Ok(spec)
}

const FIVE: &str = r#"  feed:
    - name: vsetky
      title: { sk: Všetky podujatia }
      sources: [{ type: Event, preset: event }]
    - name: kultura
      filter: { q: 'category=="culture"' }
      sources: [{ type: Event, preset: event }]
    - name: v-okoli
      filter: { georel: "near;maxDistance==5000", geometry: Point, coordinates: "[19.15,48.73]" }
      sources: [{ type: Event, preset: event }]
  ical:
    - name: vsetky
      sources: [{ type: Event, preset: sdm-event }]
    - name: mesto
      filter: { scopeQ: /bbsk/banska-bystrica }
      sources: [{ type: Event, preset: sdm-event }]
"#;

#[test]
fn three_feeds_and_two_calendars_on_one_endpoint_pass() {
    let spec = endpoint(FIVE).expect("the Endpoint is valid");
    assert_eq!(spec.feed_outputs().len(), 3);
    assert_eq!(spec.ical_outputs().len(), 2);
    assert_eq!(
        spec.representation_gaps(Some(&model())).unwrap(),
        Vec::<String>::new()
    );
}

/// A filter that does not parse is refused at save, the format, the output and the member
/// named (EP-157), for a calendar and for a feed alike.
#[test]
fn a_filter_that_does_not_parse_is_refused_and_named() {
    for (filter, says) in [
        ("{ q: 'a==1)' }", "`filter.q` is no NGSI-LD query"),
        ("{ q: 'name~=' }", "`filter.q` is no NGSI-LD query"),
        (
            "{ scopeQ: 'bbsk' }",
            "`filter.scopeQ` is no NGSI-LD scope query",
        ),
        (
            "{ georel: wherever, geometry: Point, coordinates: '[19.1,48.7]' }",
            "`filter.georel` is no NGSI-LD geo-query",
        ),
        (
            "{ georel: near, geometry: Point, coordinates: '[19.1,48.7]' }",
            "`filter.georel` is no NGSI-LD geo-query",
        ),
        (
            "{ georel: within, geometry: Polygon, coordinates: 'not json' }",
            "`filter.georel` is no NGSI-LD geo-query",
        ),
        (
            "{ georel: within, geometry: Point, coordinates: '[1,2]', geoproperty: 'a b' }",
            "`filter.geoproperty: a b` is not an attribute name",
        ),
    ] {
        let ical = endpoint(&format!(
            "  ical:\n    - {{ name: a, filter: {filter}, sources: [{{ type: Event }}] }}\n"
        ))
        .unwrap_err();
        assert!(ical.contains("spec.representations.ical"), "{ical}");
        assert!(
            ical.contains(&format!("`ical` output `a`: {says}")),
            "{ical}"
        );

        let feed = endpoint(&format!(
            "  feed:\n    - {{ name: b, filter: {filter}, sources: [{{ type: Event, preset: event }}] }}\n"
        ))
        .unwrap_err();
        assert!(feed.contains("spec.representations.feed"), "{feed}");
        assert!(feed.contains(&format!("feed output `b`: {says}")), "{feed}");
    }
}

/// A role is checked against the model per format: a slot the class lacks, and a slot of the
/// wrong range, each named with its output.
#[test]
fn a_role_the_model_cannot_serve_is_refused_and_named() {
    let spec = endpoint(
        "  feed:\n    - { name: a, sources: [{ type: Event, preset: event, mapping: { updated: colour } }] }\n  \
         ical:\n    - { name: b, sources: [{ preset: sdm-event, mapping: { DTSTART: url } }] }\n",
    )
    .expect("the shape is valid");
    let gaps = spec.representation_gaps(Some(&model())).unwrap();
    assert!(
        gaps.iter()
            .any(|g| g.starts_with("feed output `a`") && g.contains("colour")),
        "{gaps:?}"
    );
    assert!(
        gaps.iter()
            .any(|g| g.starts_with("`ical` output `b` maps DTSTART to `url`")),
        "{gaps:?}"
    );
}

/// A member a reader does not know refuses the save (EP-156), naming it: a typo in a filter
/// would otherwise serve the output unfiltered.
#[test]
fn an_unknown_member_of_a_filter_is_refused() {
    for format in ["ical", "feed"] {
        let err = endpoint(&format!(
            "  {format}:\n    - {{ name: a, filter: {{ qq: 'a==1' }}, sources: [{{ type: Event, preset: event }}] }}\n"
        ))
        .unwrap_err();
        assert!(err.contains("unknown field `qq`"), "{format}: {err}");
    }
}
