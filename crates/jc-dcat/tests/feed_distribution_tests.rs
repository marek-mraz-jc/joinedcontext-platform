//! EP-150, EP-151 (T-5203): each feed output of an Endpoint is one DCAT distribution per format
//! with its media type, and the record's page names each one in a `<link rel="alternate">`.

use jc_core::kinds::{Audience, FeedOutput, Representation};
use jc_dcat::{dataset, dataset_html, Subject};
use serde_json::{json, Value};
use std::collections::BTreeMap;

const BASE: &str = "https://data.example.sk";

fn with_subject<T>(feeds: &[FeedOutput], write: impl FnOnce(&Subject<'_>) -> T) -> T {
    let title = BTreeMap::from([("sk".to_owned(), "Podujatia".to_owned())]);
    let none = BTreeMap::new();
    let representations = if feeds.is_empty() {
        vec![Representation::NgsiLd]
    } else {
        vec![Representation::NgsiLd, Representation::Feed]
    };
    write(&Subject {
        slug: "podujatia",
        space: "podujatia",
        base_path: "/api/endpoint/podujatia",
        title: &title,
        description: &none,
        space_title: None,
        space_description: None,
        audience: Audience::Public,
        representations: &representations,
        catalog: None,
        classes: &["Event"],
        assigners: &[],
        feeds,
        calendars: &[],
    })
}

fn two() -> Vec<FeedOutput> {
    serde_json::from_value(json!([
        {"name": "kultura", "title": {"sk": "Kultúra", "en": "Culture"},
         "description": {"sk": "Podujatia kultúry"}, "sources": [{"type": "Event", "preset": "event"}]},
        {"name": "sport", "sources": [{"type": "Event", "preset": "event"}]}
    ]))
    .expect("the outputs parse")
}

fn feed_distributions(record: &Value) -> Vec<&Value> {
    record["dcat:distribution"]
        .as_array()
        .expect("distributions")
        .iter()
        .filter(|d| d["@id"].as_str().is_some_and(|id| id.contains("/feed/v1/")))
        .collect()
}

#[test]
fn each_output_is_one_distribution_per_format() {
    let record = with_subject(&two(), |s| dataset(s, &json!({}), BASE));
    let feeds = feed_distributions(&record);
    let urls: Vec<&str> = feeds
        .iter()
        .filter_map(|d| d["dcat:accessURL"].as_str())
        .collect();
    let root = format!("{BASE}/api/endpoint/podujatia/feed/v1");
    assert_eq!(
        urls,
        [
            format!("{root}/kultura/atom.xml"),
            format!("{root}/kultura/rss.xml"),
            format!("{root}/kultura/feed.json"),
            format!("{root}/sport/atom.xml"),
            format!("{root}/sport/rss.xml"),
            format!("{root}/sport/feed.json"),
        ]
    );
    let media: Vec<&str> = feeds
        .iter()
        .take(3)
        .filter_map(|d| d["dcat:mediaType"].as_str())
        .collect();
    assert_eq!(
        media,
        [
            "https://www.iana.org/assignments/media-types/application/atom+xml",
            "https://www.iana.org/assignments/media-types/application/rss+xml",
            "https://www.iana.org/assignments/media-types/application/feed+json",
        ]
    );
    assert_eq!(
        feeds[0]["dct:title"],
        json!([{"@value": "Culture (Atom)", "@language": "en"}, {"@value": "Kultúra (Atom)", "@language": "sk"}])
    );
    assert_eq!(feeds[3]["dct:title"], json!("sport (Atom)"));
    assert_eq!(
        feeds[1]["dct:format"],
        "http://publications.europa.eu/resource/authority/file-type/RSS"
    );
}

#[test]
fn an_endpoint_without_an_output_lists_no_feed() {
    let record = with_subject(&[], |s| dataset(s, &json!({}), BASE));
    assert!(feed_distributions(&record).is_empty());
    let page = with_subject(&[], |s| dataset_html(s, &json!({}), BASE));
    assert!(!page.contains("rel=\"alternate\""), "{page}");
}

#[test]
fn the_page_head_links_each_feed() {
    let page = with_subject(&two(), |s| dataset_html(s, &json!({}), BASE));
    let head = page.split("</head>").next().unwrap_or_default();
    assert_eq!(head.matches("<link rel=\"alternate\"").count(), 6, "{head}");
    assert!(head.contains(
        "<link rel=\"alternate\" type=\"application/rss+xml\" title=\"Culture (RSS 2.0)\" href=\"https://data.example.sk/api/endpoint/podujatia/feed/v1/kultura/rss.xml\">"
    ), "{head}");
    assert!(page.contains(">Culture (JSON Feed)</a>"), "{page}");
}

/// F-R2PLT3-014: the feed queues every node of the catalogue and writes each once, in linear
/// time: four times the nodes cost about four times as long. With `Vec::remove(0)` the shift
/// of the rest at each node made it grow with the square (measured 12x and more at this size);
/// a ratio, unlike a wall-clock bound, holds on a loaded host.
#[test]
fn a_large_feed_writes_every_node_once_in_linear_time() {
    let feed = |n: usize| {
        let included: Vec<Value> = (0..n)
            .map(
                |i| json!({ "@id": format!("https://example.org/d/{i}"), "@type": "dcat:Dataset" }),
            )
            .collect();
        json!({ "@id": "https://example.org/catalog", "@type": "dcat:Catalog", "@included": included })
    };
    let timed = |n: usize| {
        let feed = feed(n);
        let started = std::time::Instant::now();
        let turtle = jc_dcat::feed_turtle(&feed);
        let took = started.elapsed();
        assert_eq!(turtle.matches("a dcat:Dataset").count(), n);
        took
    };
    let small = timed(50_000);
    let large = timed(200_000);
    let ratio = large.as_secs_f64() / small.as_secs_f64();
    eprintln!("feed_turtle 50000 nodes {small:?}, 200000 nodes {large:?}, ratio {ratio:.1}");
    assert!(ratio < 8.0, "4x the nodes took {ratio:.1}x as long");
}
