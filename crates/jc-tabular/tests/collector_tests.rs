//! T-5507: a download's page walk, its window and its ceilings.

use jc_tabular::{page_entities, BadWindow, Collect, Collector, Limits, Window};
use serde_json::{json, Value};

fn entities(from: u32, count: u32) -> Vec<u8> {
    let page: Vec<Value> = (from..from + count)
        .map(|n| json!({"id": format!("urn:ngsi-ld:Event:e{n}"), "type": "Event"}))
        .collect();
    serde_json::to_vec(&page).expect("a JSON array")
}

fn params(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
    pairs
        .iter()
        .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
        .collect()
}

fn limits(max_rows: u32) -> Limits {
    Limits {
        max_rows,
        ..Limits::DEFAULT
    }
}

fn ids(collector: Collector) -> Vec<String> {
    let table = collector.finish();
    let column = table
        .columns
        .iter()
        .position(|name| name == "id")
        .expect("an id column");
    table
        .rows
        .iter()
        .map(|row| row[column].as_str().expect("a text id").to_owned())
        .collect()
}

#[test]
fn a_window_cuts_rows_but_counts_them_against_the_ceiling() {
    let window = Window::parse(&params(&[("offset", "2"), ("limit", "3")])).expect("a window");
    let mut collector = Collector::new(limits(4), window, u64::MAX);
    assert_eq!(collector.push_page(&entities(0, 4)), Ok(4));
    assert_eq!(collector.push_page(&entities(4, 1)), Err(Collect::TooLarge));

    let mut kept = Collector::new(limits(10), window, u64::MAX);
    assert_eq!(kept.push_page(&entities(0, 4)), Ok(4));
    assert_eq!(kept.push_page(&entities(4, 4)), Ok(4));
    assert_eq!(
        ids(kept),
        [
            "urn:ngsi-ld:Event:e2",
            "urn:ngsi-ld:Event:e3",
            "urn:ngsi-ld:Event:e4"
        ]
    );
}

#[test]
fn one_row_over_the_ceiling_across_three_pages_is_too_large() {
    let mut collector = Collector::new(limits(5), Window::parse(&[]).expect("no window"), u64::MAX);
    assert_eq!(collector.push_page(&entities(0, 2)), Ok(2));
    assert_eq!(collector.push_page(&entities(2, 2)), Ok(2));
    assert_eq!(collector.push_page(&entities(4, 2)), Err(Collect::TooLarge));
}

#[test]
fn the_bytes_read_are_a_ceiling_of_their_own() {
    let page = entities(0, 2);
    let held = page.len() as u64;
    let mut collector = Collector::new(limits(100), Window::parse(&[]).expect("no window"), held);
    assert_eq!(collector.push_page(&page), Ok(2));
    assert_eq!(collector.push_page(b"[]"), Err(Collect::TooLarge));
}

#[test]
fn an_empty_page_and_a_single_object_page_read_as_the_broker_sent_them() {
    assert_eq!(page_entities(b""), Ok(Vec::new()));
    assert_eq!(page_entities(b"[]"), Ok(Vec::new()));
    assert_eq!(page_entities(b"42"), Ok(Vec::new()));
    let one = json!({"id": "urn:ngsi-ld:Event:e1", "type": "Event"});
    assert_eq!(page_entities(one.to_string().as_bytes()), Ok(vec![one]));
    assert_eq!(page_entities(b"{not json"), Err(Collect::Unreadable));

    let mut collector =
        Collector::new(limits(10), Window::parse(&[]).expect("no window"), u64::MAX);
    assert_eq!(collector.push_page(b""), Ok(0));
    assert_eq!(
        collector.push_page(br#"{"id":"urn:ngsi-ld:Event:e1","type":"Event"}"#),
        Ok(1)
    );
    assert_eq!(ids(collector), ["urn:ngsi-ld:Event:e1"]);
}

#[test]
fn a_window_that_is_no_whole_number_is_the_typed_error() {
    assert_eq!(
        Window::parse(&params(&[("offset", "x")])),
        Err(BadWindow("offset"))
    );
    assert_eq!(
        Window::parse(&params(&[("limit", "-1")])),
        Err(BadWindow("limit"))
    );
    assert_eq!(
        BadWindow("offset").to_string(),
        "offset must be a whole number"
    );
    assert_eq!(
        Window::parse(&params(&[("offset", ""), ("limit", "")])),
        Ok(Window {
            offset: 0,
            limit: usize::MAX
        })
    );
}
