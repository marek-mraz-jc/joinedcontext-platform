//! T-4672 (DM-98…DM-102, EP-135): the display order, key attributes and labels from the LinkML
//! model, an override's reorder and subset, and the name heuristic as the fallback.

use std::collections::BTreeSet;

use jc_core::display_order::{display_order, Display, Field};

const MODEL: &str = r#"
id: https://example.org/models/city
name: city
in_language: sk
slots:
  name:
    title: Name
  description: {}
  startDate:
    annotations:
      label: { sk: Začiatok, en: Start }
  endDate: {}
  address:
    title: Postal address
  source:
    annotations:
      hidden: true
  located: {}
  secret: {}
classes:
  Thing:
    slots: [located]
  Event:
    is_a: Thing
    slots: [name, description, startDate, endDate, address, source, secret]
    slot_usage:
      endDate:
        rank: 2
      name:
        rank: 1
        annotations:
          summary: true
          label: { tag: label, value: { sk: Názov, cs: Název } }
      startDate:
        annotations:
          summary: { tag: summary, value: true }
  Plain:
    slots: [name, description, startDate, endDate, address]
"#;

fn order(class: &str, served: &[&str], view: Option<&Display>) -> Vec<Field> {
    let attributes: Vec<(&str, bool)> = served.iter().map(|a| (*a, false)).collect();
    display_order(Some(MODEL), class, &attributes, view).expect("the model parses")
}

fn slots(fields: &[Field]) -> Vec<&str> {
    fields.iter().map(|f| f.slot.as_str()).collect()
}

const EVENT: &[&str] = &[
    "id",
    "type",
    "location",
    "dateModified",
    "located",
    "address",
    "startDate",
    "description",
    "endDate",
    "name",
    "source",
    "remark",
    "url",
];

#[test]
fn rank_first_then_own_slots_then_inherited_extras_by_the_heuristic_system_last() {
    let fields = order("Event", EVENT, None);
    assert_eq!(
        slots(&fields),
        [
            "name",
            "endDate", // ranked
            "description",
            "startDate",
            "address", // own, as written
            "located", // inherited through is_a
            "url",
            "remark", // not in the model: link before other
            "location",
            "dateModified",
            "type",
            "id", // system, in their own order
        ]
    );
}

#[test]
fn hidden_and_ungranted_slots_never_appear() {
    let fields = order("Event", EVENT, None);
    assert!(!slots(&fields).contains(&"source"), "hidden");
    // `secret` is a slot of Event, but the caller is not served it.
    assert!(fields
        .iter()
        .all(|f| f.slot != "secret" && !f.labels.contains_key("secret")));
    let narrow = order("Event", &["startDate", "description"], None);
    assert_eq!(slots(&narrow), ["description", "startDate"]);
}

#[test]
fn summary_is_the_annotated_slots_else_the_first_four() {
    let fields = order("Event", EVENT, None);
    let summary: Vec<&str> = fields
        .iter()
        .filter(|f| f.summary)
        .map(|f| f.slot.as_str())
        .collect();
    assert_eq!(summary, ["name", "startDate"]);
    let plain = order(
        "Plain",
        &["address", "endDate", "startDate", "description", "name"],
        None,
    );
    let summary: Vec<&str> = plain
        .iter()
        .filter(|f| f.summary)
        .map(|f| f.slot.as_str())
        .collect();
    assert_eq!(summary, ["name", "description", "startDate", "endDate"]);
}

#[test]
fn labels_fall_back_from_language_to_default_to_english_to_title_to_heuristic_to_name() {
    let fields = order("Event", EVENT, None);
    let label = |slot: &str, lang: &str| {
        fields
            .iter()
            .find(|f| f.slot == slot)
            .map(|f| f.label(lang))
            .unwrap_or_default()
    };
    assert_eq!(label("name", "cs"), "Název", "the language asked");
    assert_eq!(label("name", "fi"), "Názov", "the model's default language");
    assert_eq!(
        label("startDate", "fi"),
        "Začiatok",
        "the default before English"
    );
    assert_eq!(label("address", "sk"), "Postal address", "the slot's title");
    assert_eq!(label("url", "sk"), "Odkaz", "the heuristic's label");
    assert_eq!(label("remark", "sk"), "Remark", "the name spelt");
}

#[test]
fn an_override_reorders_and_narrows_and_its_unknown_names_are_refused() {
    let view = Display {
        fields: vec!["address".into(), "name".into(), "secret".into()],
        summary: vec!["address".into()],
    };
    let fields = order("Event", EVENT, Some(&view));
    // `secret` is not served to this caller: skipped, never shown.
    assert_eq!(slots(&fields), ["address", "name"]);
    assert_eq!(fields.iter().filter(|f| f.summary).count(), 1);
    assert!(fields[0].summary);

    let projection: BTreeSet<String> = ["name", "address", "startDate"].map(String::from).into();
    let ok = Display {
        fields: vec!["address".into()],
        summary: vec!["name".into()],
    };
    assert_eq!(ok.check("Event", &projection), Ok(()));
    let err = view
        .check("Event", &projection)
        .expect_err("secret is not projected");
    assert!(err.contains("secret") && !err.contains("address"), "{err}");
}

#[test]
fn a_type_without_a_model_class_is_ordered_by_the_heuristic() {
    let served = [
        ("benzene", true),
        ("remark", false),
        ("name", false),
        ("startDate", false),
    ];
    for model in [None, Some(MODEL)] {
        let fields = display_order(model, "Unmodelled", &served, None).expect("parses");
        assert_eq!(slots(&fields), ["name", "startDate", "benzene", "remark"]);
        assert_eq!(fields[0].label("sk"), "Názov");
        assert!(
            fields.iter().all(|f| !f.summary),
            "no class, no key attributes"
        );
    }
    assert!(display_order(Some("classes: ["), "Event", &served, None).is_err());
}

/// The broker's schema surface (`schema/v1/model.linkml.yaml`) serves annotations as text, as
/// Model Tools compiled them: `"true"`, and a label's language map as JSON text.
#[test]
fn the_brokers_form_of_the_annotations_reads_the_same() {
    let served = r#"
classes:
  Event:
    slots: [name, startDate, source]
    slot_usage:
      startDate:
        annotations:
          summary: { tag: summary, value: "true" }
          label: { tag: label, value: '{"sk": "Za\u010diatok", "en": "Start"}' }
      source:
        annotations:
          hidden: { tag: hidden, value: "true" }
"#;
    let fields = display_order(
        Some(served),
        "Event",
        &[("name", false), ("startDate", false), ("source", false)],
        None,
    )
    .expect("parses");
    assert_eq!(slots(&fields), ["name", "startDate"]);
    assert!(fields[1].summary && !fields[0].summary);
    assert_eq!(fields[1].label("sk"), "Začiatok");
}
