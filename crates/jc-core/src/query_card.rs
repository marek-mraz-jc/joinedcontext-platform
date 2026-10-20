//! The NGSI-LD query card of an Endpoint (T-4759, T-4760): how to query it, its types and their
//! attributes as compact lines with units and allowed values, generated from the LinkML the
//! Endpoint serves the caller. The one generator: the gateway serves it as `card://{slug}`, the
//! Portal's Endpoint page shows that text, and the assistant gives it to the model.

use std::collections::BTreeMap;

use crate::display_order::{query_attributes, QueryAttribute};

/// Above this many tokens a card lists its types with their attribute counts, and a type's lines
/// come with the first query on it (the BBSK catalogue: 24 types, 1,317 attributes, ~6.2k tokens).
pub const CARD_TOKENS: usize = 1_500;

/// Tokens of a text, about characters / 3.7 (the ratio of the board's measurements).
pub fn tokens(text: &str) -> usize {
    text.chars().count() * 10 / 37
}

/// The card: the fixed NGSI-LD lines naming only the read tools `offers` grants, then the
/// attributes every type shares once, then each type's lines, or with `brief` each type's count.
/// `endpoint` is the Endpoint's `{project}/{name}`, `title` its title.
pub fn card(
    endpoint: &str,
    title: Option<&str>,
    offers: impl Fn(&str) -> bool,
    classes: &BTreeMap<String, Vec<QueryAttribute>>,
    brief: bool,
) -> String {
    let title = title.map(|t| format!(" ({t})")).unwrap_or_default();
    let reads: Vec<&str> = [
        ("query_entities", "query_entities (type, q, georel, pick)"),
        ("query_temporal", "query_temporal (history)"),
        ("summarize", "summarize (aggregates)"),
    ]
    .into_iter()
    .filter(|(tool, _)| offers(tool))
    .map(|(_, said)| said)
    .collect();
    let mut card = format!("NGSI-LD Endpoint {endpoint}{title}, ETSI GS CIM 009 / TS 104 175.");
    if !reads.is_empty() {
        card.push_str(&format!(" Read it with {}.", reads.join(", ")));
    }
    if offers("query_entities") || offers("query_temporal") {
        card.push_str("\nq: attr==value; AND, | OR, == != > >= < <= ~= (regex), ranges a..b; a text value in quotes; a Relationship by its target id (refParentArea==\"urn:ngsi-ld:...\").");
    }
    if offers("query_temporal") {
        // the broker's default ceilings (ANTARES_TEMPORAL_INSTANCE_LIMIT, ANTARES_MAX_LIMIT; T-4800)
        card.push_str("\nHistory: at most 1,000 values per Attribute per call (1,000 Entities per page); a window over a day answers hourly averages unless format or lastN asks raw values; a cut answer carries more.next, the window to call next.");
    }
    if offers("query_entities") {
        card.push_str("\nGeo: georel near;maxDistance==<metres> or within on location. Units: compare in the unit shown (pm10 is ug/m3; a distance asked in km is sent in m).");
    }
    let shared = shared(classes);
    if !shared.is_empty() {
        card.push_str(&format!("\nEvery type also has: {}.", shared.join(", ")));
    }
    for (kind, attributes) in classes {
        if brief {
            card.push_str(&format!("\n{kind}: {} attributes", attributes.len()));
        } else {
            card.push_str(&type_lines(kind, attributes, &shared));
        }
    }
    if brief {
        card.push_str("\nA type's attributes come with your first query on it.");
    }
    card
}

/// The card an Endpoint serves for its LinkML `model`: whole, or brief past [`CARD_TOKENS`]. A
/// model that does not parse gives the query rules alone.
pub fn served(
    endpoint: &str,
    title: Option<&str>,
    offers: impl Fn(&str) -> bool,
    model: Option<&str>,
) -> String {
    let classes = model
        .and_then(|m| query_attributes(m).ok())
        .unwrap_or_default();
    let whole = card(endpoint, title, &offers, &classes, false);
    if tokens(&whole) <= CARD_TOKENS {
        whole
    } else {
        card(endpoint, title, &offers, &classes, true)
    }
}

/// The attribute lines every type writes alike, when there are two types or more.
fn shared(classes: &BTreeMap<String, Vec<QueryAttribute>>) -> Vec<String> {
    let mut all = classes.values();
    let (Some(first), true) = (all.next(), classes.len() > 1) else {
        return Vec::new();
    };
    first
        .iter()
        .map(|a| a.written.clone())
        .filter(|w| classes.values().all(|c| c.iter().any(|a| &a.written == w)))
        .collect()
}

/// One type's line, without the `shared` attributes, and its example `q` when it has one.
pub fn type_lines(kind: &str, attributes: &[QueryAttribute], shared: &[String]) -> String {
    let own: Vec<&str> = attributes
        .iter()
        .map(|a| a.written.as_str())
        .filter(|w| !shared.iter().any(|s| s == w))
        .collect();
    let mut lines = format!("\n{kind}: {}", own.join(", "));
    if let Some(example) = attributes.iter().find_map(|a| a.example.as_deref()) {
        lines.push_str(&format!("\n  e.g. q={example}"));
    }
    lines
}
