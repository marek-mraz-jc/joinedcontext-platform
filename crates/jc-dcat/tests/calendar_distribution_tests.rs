//! API/11 EP-159 (T-5218): each iCalendar output of an Endpoint is one DCAT distribution titled by
//! the output, and the record's page names each one in a `<link rel="alternate">`; an Endpoint
//! that serves `ical` with no output list keeps its one `ical/v1/events.ics`.

use jc_core::kinds::{Audience, IcalOutput, Representation};
use jc_dcat::{dataset, dataset_html, Subject};
use serde_json::{json, Value};
use std::collections::BTreeMap;

const BASE: &str = "https://data.example.sk";

fn with_subject<T>(calendars: &[IcalOutput], write: impl FnOnce(&Subject<'_>) -> T) -> T {
    let title = BTreeMap::from([("sk".to_owned(), "Podujatia".to_owned())]);
    let none = BTreeMap::new();
    write(&Subject {
        slug: "podujatia",
        space: "podujatia",
        base_path: "/api/endpoint/podujatia",
        title: &title,
        description: &none,
        space_title: None,
        space_description: None,
        audience: Audience::Public,
        representations: &[Representation::NgsiLd, Representation::Ical],
        catalog: None,
        classes: &["Event"],
        assigners: &[],
        feeds: &[],
        calendars,
    })
}

fn two() -> Vec<IcalOutput> {
    serde_json::from_value(json!([
        {"name": "kalendar", "title": {"sk": "Kalendár", "en": "Calendar"},
         "description": {"sk": "Všetky podujatia"}, "sources": [{"type": "Event", "preset": "sdm-event"}]},
        {"name": "zasadnutia", "sources": [{"type": "Event", "preset": "sdm-event"}]}
    ]))
    .expect("two calendar outputs")
}

fn calendar_urls(record: &Value) -> Vec<String> {
    record["dcat:distribution"]
        .as_array()
        .expect("distributions")
        .iter()
        .filter(|d| {
            d["dct:format"]
                .as_str()
                .is_some_and(|f| f.ends_with("/ICS"))
        })
        .map(|d| d["dcat:accessURL"].as_str().unwrap_or_default().to_owned())
        .collect()
}

#[test]
fn each_calendar_output_is_one_distribution_titled_by_the_output() {
    let outputs = two();
    let record = with_subject(&outputs, |s| dataset(s, &json!({}), BASE));
    assert_eq!(
        calendar_urls(&record),
        vec![
            format!("{BASE}/api/endpoint/podujatia/ical/v1/kalendar.ics"),
            format!("{BASE}/api/endpoint/podujatia/ical/v1/zasadnutia.ics"),
        ]
    );
    let first = record["dcat:distribution"]
        .as_array()
        .expect("distributions")
        .iter()
        .find(|d| {
            d["dcat:accessURL"]
                .as_str()
                .is_some_and(|u| u.ends_with("kalendar.ics"))
        })
        .expect("the first calendar");
    assert_eq!(
        first["dct:title"],
        json!([{"@value": "Calendar", "@language": "en"}, {"@value": "Kalendár", "@language": "sk"}])
    );
    assert_eq!(
        first["dct:description"],
        json!([{"@value": "Všetky podujatia", "@language": "sk"}])
    );
}

#[test]
fn an_endpoint_without_an_output_list_keeps_events_ics() {
    let record = with_subject(&[], |s| dataset(s, &json!({}), BASE));
    assert_eq!(
        calendar_urls(&record),
        vec![format!("{BASE}/api/endpoint/podujatia/ical/v1/events.ics")]
    );
}

#[test]
fn the_record_page_names_each_calendar_as_an_alternate() {
    let outputs = two();
    let page = with_subject(&outputs, |s| dataset_html(s, &json!({}), BASE));
    for name in ["kalendar", "zasadnutia"] {
        let link = format!(
            "type=\"text/calendar\" title=\"{}\" href=\"{BASE}/api/endpoint/podujatia/ical/v1/{name}.ics\"",
            if name == "kalendar" { "Calendar" } else { "zasadnutia" }
        );
        assert!(page.contains(&link), "{link} missing in {page}");
    }
}
