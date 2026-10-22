//! EP-150, EP-151 (T-5203): the feed outputs an Endpoint lists in `spec.representations.feed`,
//! checked when the Endpoint is saved: off by default, at most 20, one URL per name, the
//! required roles named, and against the space's model a preset only for items that appear over
//! time (owner 2026-10-22T09:25Z).

use jc_core::kinds::{feed_gaps, EndpointSpec, FeedOutput, Representation};

const MODEL: &str = r"
id: https://example.org/feeds
name: feeds
default_range: string
slots:
  name: {}
  headline: {}
  startDate: { range: datetime }
  datePublished: { range: date }
  url: { range: uri }
  dateObserved: { range: datetime }
  pm10: { range: float }
classes:
  Event:
    slots: [name, startDate, url]
  Article:
    slots: [headline, datePublished, url]
  AirQualityObserved:
    slots: [name, dateObserved, pm10]
";

fn endpoint(extra: &str) -> EndpointSpec {
    serde_norway::from_str(&format!(
        "contextSpaceRef: {{ kind: ContextSpace, name: podujatia }}\nslug: k4y7pq2mzt6vhx3nbwrs5cjdaf\naudience: public\n{extra}"
    ))
    .expect("the Endpoint parses")
}

fn outputs(yaml: &str) -> Vec<FeedOutput> {
    serde_norway::from_str(yaml).expect("the outputs parse")
}

fn gaps(yaml: &str) -> Vec<String> {
    feed_gaps(&outputs(yaml), Some(MODEL)).expect("the model parses")
}

#[test]
fn a_feed_is_served_exactly_where_an_output_is_listed() {
    let none = endpoint("");
    assert!(!none
        .served_representations()
        .contains(&Representation::Feed));
    let some = endpoint(
        "representations:\n  feed:\n    - name: vsetky\n      sources: [{ type: Event, preset: event }]\n",
    );
    assert!(some
        .served_representations()
        .contains(&Representation::Feed));
    assert!(some.validate().is_ok());
    // listed beside an enabled list that does not name it, the output still decides
    let listed = endpoint(
        "enabledRepresentations: [ngsi-ld]\nrepresentations:\n  feed:\n    - name: vsetky\n      sources: [{ type: Event, preset: event }]\n",
    );
    assert!(listed
        .served_representations()
        .contains(&Representation::Feed));
}

#[test]
fn feed_in_the_representation_lists_is_refused() {
    for list in [
        "enabledRepresentations: [ngsi-ld, feed]",
        "disabledRepresentations: [feed]",
    ] {
        let error = endpoint(list).validate().expect_err(list).to_string();
        assert!(
            error.contains("spec.representations.feed"),
            "{list}: {error}"
        );
    }
}

#[test]
fn three_outputs_over_two_classes_are_one_valid_endpoint() {
    let yaml = r#"
- name: vsetky
  title: { sk: Všetky podujatia, en: All events }
  sources: [{ type: Event, preset: event }]
- name: sport
  sources: [{ type: Event, preset: event }]
  filter: { q: 'category=="sport"' }
  lang: sk
  limit: 20
  sort: updated
- name: novinky
  sources:
    - { type: Article, preset: article }
    - { type: Event, preset: generic, mapping: { published: startDate } }
"#;
    assert_eq!(gaps(yaml), Vec::<String>::new());
}

#[test]
fn a_duplicate_name_and_a_21st_output_are_refused() {
    let twice = "- { name: a, sources: [{ type: Event, preset: event }] }\n- { name: a, sources: [{ type: Event, preset: event }] }\n";
    assert_eq!(
        gaps(twice),
        ["feed output `a` is listed twice; each name is one URL"]
    );
    let many: String = (0..21)
        .map(|i| format!("- {{ name: o{i}, sources: [{{ type: Event, preset: event }}] }}\n"))
        .collect();
    assert_eq!(
        gaps(&many),
        ["spec.representations.feed lists 21 outputs, and an Endpoint at most 20"]
    );
}

#[test]
fn a_bad_shape_is_named() {
    let found = gaps(
        r#"
- name: Kultura
  version: 2
  title: { slovak: x }
  lang: svk
  limit: 500
  filter: { q: 'a==1)', georel: near }
  sources: [{ type: Event }, { type: "Ev ent", preset: event }]
"#,
    );
    for expected in [
        "name is a slug",
        "is version 2",
        "title key `slovak`",
        "lang `svk`",
        "limit is 1 to 200",
        "filter.q is one NGSI-LD query",
        "georel, geometry and coordinates together",
        "Event: no title role",
        "Event: no updated role",
        "`Ev ent`, which is not an NGSI-LD type name",
    ] {
        assert!(
            found.iter().any(|gap| gap.contains(expected)),
            "{expected}: {found:?}"
        );
    }
}

#[test]
fn a_type_whose_items_do_not_appear_over_time_needs_explicit_roles() {
    assert_eq!(
        gaps("- { name: ovzdusie, sources: [{ type: AirQualityObserved, preset: generic }] }\n"),
        ["feed output `ovzdusie`, AirQualityObserved: the type has no start or publish time (startDate, datePublished, dateCreated, dateIssued, dateModified), so its items do not appear over time; map title and updated explicitly or leave it out"]
    );
    let explicit = "- { name: ovzdusie, sources: [{ type: AirQualityObserved, mapping: { title: name, updated: dateObserved } }] }\n";
    assert_eq!(gaps(explicit), Vec::<String>::new());
}

#[test]
fn a_role_is_checked_against_the_model() {
    let found = gaps(
        "- { name: x, sources: [{ type: Event, mapping: { title: nazov, updated: name, link: name } }] }\n- { name: y, sources: [{ type: Bridge, preset: generic }] }\n",
    );
    assert_eq!(
        found,
        [
            "feed output `x`, Event: title names `nazov`, which Event does not have",
            "feed output `x`, Event: link reads `name`, which holds no uri",
            "feed output `x`, Event: updated reads `name`, which holds no date or datetime",
            "feed output `y` lists the type Bridge, which the data model does not have",
        ]
    );
}

#[test]
fn an_output_round_trips_in_the_shape_the_broker_reads() {
    let yaml = "- name: kultura\n  title: { sk: Kultúra }\n  sources:\n    - type: Event\n      preset: event\n      mapping: { category: [eventType, category] }\n  filter: { scopeQ: /bbsk/# }\n";
    let parsed = outputs(yaml);
    let json = serde_json::to_value(&parsed).expect("serializes");
    assert_eq!(
        json,
        serde_json::json!([{
            "name": "kultura",
            "title": { "sk": "Kultúra" },
            "sources": [{ "type": "Event", "preset": "event", "mapping": { "category": ["eventType", "category"] } }],
            "filter": { "scopeQ": "/bbsk/#" }
        }])
    );
}
