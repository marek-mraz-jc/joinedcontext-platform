//! The hub's `search` tool (T-4493): one ranked call from a question to the Dataset and the
//! Endpoint to read next.
//!
//! The index is built from the caller's own catalogue ([`super::catalog`]), so it holds only the
//! Datasets the request reaches: an Endpoint outside it gives no word and no hit. Each Dataset is
//! one document of its `searchText` (titles, descriptions, keywords, space), the publisher, and
//! per Endpoint it lists the entity types with their Smart Data Model domain and subject words
//! (T-4498). Words are folded (no diacritics, lowercase), CamelCase type names split, and cut to
//! a five-letter stem, so Bystrica, Bystrici and BYSTRICA are one word; a small sk/en table adds
//! the other language's words to a question. Ranking is BM25.

use serde_json::{json, Map, Value};
use std::collections::HashMap;

/// What the catalogue does not hold about an Endpoint and the search reads: its entity types,
/// their SDM domains, its EU themes, and the words the classification gives them in sk and en.
#[derive(Default)]
pub(crate) struct Facts {
    pub(crate) types: Vec<String>,
    pub(crate) domains: Vec<String>,
    pub(crate) themes: Vec<String>,
    pub(crate) words: Vec<String>,
}

struct Doc {
    dataset: String,
    slugs: Vec<String>,
    title: Map<String, Value>,
    types: Vec<String>,
    domains: Vec<String>,
    themes: Vec<String>,
    terms: HashMap<String, u32>,
    len: usize,
}

/// One catalogue's search index.
#[derive(Default)]
pub(crate) struct Index {
    docs: Vec<Doc>,
    /// Per term, the documents holding it.
    df: HashMap<String, usize>,
    avg_len: f64,
}

/// Words one language says for what the other says (ponytail: a short table, grown when a miss
/// is filed; embeddings only if T-4496 shows misses it cannot fix).
const SYNONYMS: &[(&str, &str)] = &[
    ("ovzdusie", "air quality"),
    ("ovzdusia", "air quality"),
    ("podujatia", "events"),
    ("udalosti", "events"),
    ("parkovanie", "parking"),
    ("doprava", "transport traffic"),
    ("pocasie", "weather"),
    ("skoly", "schools"),
    ("zmluvy", "contracts"),
    ("odpad", "waste"),
    ("hluk", "noise"),
    ("voda", "water"),
    ("energia", "energy"),
    ("obyvatelstvo", "population"),
    ("no2", "nitrogen dioxide air quality"),
    ("pm10", "particulate matter air quality"),
];

/// BM25's term saturation and length normalisation, the usual values.
const K1: f64 = 1.2;
const B: f64 = 0.75;

/// A hit list's length when the caller names none, and its most.
const DEFAULT_LIMIT: usize = 10;
const MAX_LIMIT: usize = 50;

/// The search words of a text: CamelCase split, folded, cut to five letters.
pub(crate) fn terms(text: &str) -> Vec<String> {
    let mut spaced = String::with_capacity(text.len() + 8);
    let mut previous_lower = false;
    for c in text.chars() {
        if c.is_uppercase() && previous_lower {
            spaced.push(' ');
        }
        previous_lower = c.is_lowercase() || c.is_ascii_digit();
        spaced.push(c);
    }
    jc_core::ngsi_catalog::fold(&spaced)
        .split(|c: char| !c.is_alphanumeric())
        .filter(|word| word.chars().count() >= 2)
        .map(|word| word.chars().take(5).collect())
        .collect()
}

/// A question's words with the other language's beside them.
fn question(text: &str) -> Vec<String> {
    let mut words = terms(text);
    for (sk, en) in SYNONYMS {
        let (sk, en) = (terms(sk), terms(en));
        if sk.iter().all(|w| words.contains(w)) {
            words.extend(en);
        } else if en.iter().all(|w| words.contains(w)) {
            words.extend(sk);
        }
    }
    words.sort();
    words.dedup();
    words
}

fn strings(value: Option<&Value>) -> Vec<String> {
    value
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

impl Index {
    /// The index of a catalogue's Datasets; `facts` is per Endpoint slug.
    pub(crate) fn build<'a>(
        entities: impl IntoIterator<Item = &'a Value>,
        facts: &HashMap<String, Facts>,
    ) -> Self {
        let mut index = Index::default();
        for entity in entities.into_iter().filter(|e| e["type"] == "Dataset") {
            let slugs: Vec<String> = entity["dataService"]["value"]
                .as_array()
                .map(|links| {
                    links
                        .iter()
                        .filter_map(|link| link["identifier"].as_str().map(str::to_owned))
                        .collect()
                })
                .unwrap_or_default();
            let mut text = entity["searchText"]["value"]
                .as_str()
                .unwrap_or("")
                .to_owned();
            if let Some(names) = entity["publisher"]["value"]["name"].as_object() {
                for name in names.values().filter_map(Value::as_str) {
                    text.push(' ');
                    text.push_str(name);
                }
            }
            let mut doc = Doc {
                dataset: entity["id"].as_str().unwrap_or("").to_owned(),
                title: entity["title"]["languageMap"]
                    .as_object()
                    .cloned()
                    .unwrap_or_default(),
                slugs,
                types: Vec::new(),
                domains: Vec::new(),
                themes: strings(Some(&entity["theme"]["value"]))
                    .iter()
                    .filter_map(|iri| iri.rsplit('/').next().map(str::to_owned))
                    .collect(),
                terms: HashMap::new(),
                len: 0,
            };
            for slug in &doc.slugs {
                if let Some(f) = facts.get(slug) {
                    for (into, from) in [(&mut doc.types, &f.types), (&mut doc.domains, &f.domains)]
                    {
                        into.extend(
                            from.iter()
                                .filter(|x| !into.contains(x))
                                .cloned()
                                .collect::<Vec<_>>(),
                        );
                    }
                    doc.themes.extend(
                        f.themes
                            .iter()
                            .filter(|t| !doc.themes.contains(t))
                            .cloned()
                            .collect::<Vec<_>>(),
                    );
                    for word in f.types.iter().chain(&f.words) {
                        text.push(' ');
                        text.push_str(word);
                    }
                }
            }
            let words = terms(&text);
            doc.len = words.len();
            for word in words {
                *doc.terms.entry(word).or_default() += 1;
            }
            for word in doc.terms.keys() {
                *index.df.entry(word.clone()).or_default() += 1;
            }
            index.docs.push(doc);
        }
        let total: usize = index.docs.iter().map(|doc| doc.len).sum();
        index.avg_len = total as f64 / index.docs.len().max(1) as f64;
        index
    }

    /// The `search` tool's answer, or why its arguments are refused.
    pub(crate) fn search(&self, arguments: &Map<String, Value>) -> Result<Value, String> {
        const ARGUMENTS: &[&str] = &["query", "domain", "theme", "type", "place", "limit", "lang"];
        if let Some(unknown) = arguments
            .keys()
            .find(|key| !ARGUMENTS.contains(&key.as_str()))
        {
            return Err(format!("`{unknown}` is not an argument of search"));
        }
        let text = |name: &str| -> Result<Option<String>, String> {
            match arguments.get(name) {
                None => Ok(None),
                Some(Value::String(s)) if !s.trim().is_empty() && s.len() <= 200 => {
                    Ok(Some(s.trim().to_owned()))
                }
                Some(_) => Err(format!("`{name}` is a text of 1 to 200 characters")),
            }
        };
        let query =
            text("query")?.ok_or("`query` is the question, e.g. air quality Banská Bystrica")?;
        let limit = match arguments.get("limit") {
            None => DEFAULT_LIMIT,
            Some(value) => value
                .as_u64()
                .filter(|n| (1..=MAX_LIMIT as u64).contains(n))
                .ok_or(format!("`limit` is a whole number from 1 to {MAX_LIMIT}"))?
                as usize,
        };
        let lang = text("lang")?
            .unwrap_or_else(|| "en".to_owned())
            .to_lowercase();
        let place = text("place")?.map(|p| terms(&p)).unwrap_or_default();
        let facet = |name: &str| text(name).map(|v| v.map(|v| v.to_lowercase()));
        let (domain, theme, kind) = (facet("domain")?, facet("theme")?, facet("type")?);
        let has = |list: &[String], wanted: &Option<String>| {
            wanted
                .as_ref()
                .is_none_or(|w| list.iter().any(|x| x.to_lowercase() == *w))
        };

        let mut words = question(&query);
        words.extend(place.iter().cloned());
        let n = self.docs.len() as f64;
        let mut ranked: Vec<(f64, &Doc)> = self
            .docs
            .iter()
            .filter(|doc| {
                has(&doc.domains, &domain) && has(&doc.themes, &theme) && has(&doc.types, &kind)
            })
            .filter(|doc| place.iter().all(|w| doc.terms.contains_key(w)))
            .filter_map(|doc| {
                let score: f64 = words
                    .iter()
                    .filter_map(|w| {
                        let tf = f64::from(*doc.terms.get(w)?);
                        let df = *self.df.get(w).unwrap_or(&0) as f64;
                        let idf = ((n - df + 0.5) / (df + 0.5) + 1.0).ln();
                        Some(
                            idf * tf * (K1 + 1.0)
                                / (tf + K1 * (1.0 - B + B * doc.len as f64 / self.avg_len)),
                        )
                    })
                    .sum();
                (score > 0.0).then_some((score, doc))
            })
            .collect();
        ranked.sort_by(|a, b| {
            b.0.total_cmp(&a.0)
                .then_with(|| a.1.dataset.cmp(&b.1.dataset))
        });
        let hits: Vec<Value> = ranked
            .iter()
            .take(limit)
            .map(|(score, doc)| hit(doc, *score, &lang))
            .collect();
        let mut text = String::from("endpoint | title | types | domain | dataset\n");
        for hit in &hits {
            text.push_str(&format!(
                "{} | {} | {} | {} | {}\n",
                hit["endpoint"].as_str().unwrap_or(""),
                hit["title"].as_str().unwrap_or(""),
                strings(Some(&hit["types"])).join(","),
                strings(Some(&hit["domains"])).join(","),
                hit["dataset"].as_str().unwrap_or(""),
            ));
        }
        text.push_str(&format!(
            "{} of {} matched; nextQuery is the call that reads a hit",
            hits.len(),
            ranked.len()
        ));
        let mut answer = super::jsonrpc::answered(&json!(hits), "hits");
        answer["content"][0]["text"] = json!(text);
        answer["structuredContent"]["total"] = json!(ranked.len());
        Ok(answer)
    }
}

/// One hit: the Dataset, the Endpoint to read and the call that reads it.
fn hit(doc: &Doc, score: f64, lang: &str) -> Value {
    let title = doc
        .title
        .get(lang)
        .or_else(|| doc.title.get("en"))
        .or_else(|| doc.title.values().next())
        .cloned()
        .unwrap_or(Value::Null);
    let endpoint = doc.slugs.first().cloned().unwrap_or_default();
    let mut arguments = json!({ "endpoint": endpoint, "limit": 10 });
    if let Some(first) = doc.types.first() {
        arguments["type"] = json!(first);
    }
    json!({
        "dataset": doc.dataset,
        "endpoint": endpoint,
        "endpoints": doc.slugs,
        "title": title,
        "types": doc.types,
        "domains": doc.domains,
        "themes": doc.themes,
        "score": (score * 100.0).round() / 100.0,
        "nextQuery": { "tool": "query_entities", "arguments": arguments },
    })
}

/// The `search` tool as `tools/list` names it.
pub(crate) fn tool() -> Value {
    json!({
        "name": "search",
        "description": "Find the dataset for a question in one call: ranked words of titles and \
                        descriptions (sk and en, diacritics optional), entity types and their \
                        Smart Data Model domain, places and publishers, over the Endpoints you \
                        may read. Each hit names the Endpoint and its nextQuery, the call that \
                        reads it.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "query": { "type": "string", "minLength": 1, "maxLength": 200, "description": "The question's words, e.g. air quality Banská Bystrica" },
                "domain": { "type": "string", "minLength": 1, "maxLength": 200, "description": "A Smart Data Model domain, e.g. Environment" },
                "theme": { "type": "string", "minLength": 1, "maxLength": 200, "description": "An EU data theme code, e.g. ENVI" },
                "type": { "type": "string", "minLength": 1, "maxLength": 200, "description": "An entity type the dataset holds" },
                "place": { "type": "string", "minLength": 1, "maxLength": 200, "description": "A place every hit names" },
                "limit": { "type": "integer", "minimum": 1, "maximum": MAX_LIMIT },
                "lang": { "type": "string", "minLength": 2, "maxLength": 8, "description": "The language of the titles, e.g. sk; English otherwise" },
            },
            "required": ["query"],
            "additionalProperties": false,
        },
        "annotations": { "readOnlyHint": true, "destructiveHint": false },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn words_fold_split_and_stem() {
        assert_eq!(terms("AirQualityObserved"), ["air", "quali", "obser"]);
        assert_eq!(terms("Banská Bystrica"), terms("banska BYSTRICI"));
        assert_eq!(terms("Žilina"), terms("Ziline"));
        assert!(terms("a NO2").contains(&"no2".to_owned()));
    }

    /// T-4493: the index of 10,000 Datasets builds in under a second (release).
    #[test]
    #[ignore = "a 10,000-Dataset measurement; run in release"]
    fn ten_thousand_datasets_index_in_under_a_second() {
        let entities: Vec<Value> = (0..10_000)
            .map(|n| {
                json!({
                    "id": format!("urn:ngsi-ld:Dataset:mesto-s{n}"), "type": "Dataset",
                    "title": { "languageMap": { "en": format!("Air quality station {n}"), "sk": format!("Kvalita ovzdušia, stanica {n}") } },
                    "searchText": { "value": format!("air quality station {n} kvalita ovzdusia stanica {n} banska bystrica mesto-s{n}") },
                    "dataService": { "value": [{ "identifier": format!("{n:0>26}") }] },
                })
            })
            .collect();
        let started = std::time::Instant::now();
        let index = Index::build(&entities, &HashMap::new());
        let took = started.elapsed();
        eprintln!("10,000 Datasets indexed in {took:?}");
        assert!(took.as_secs_f64() < 1.0, "{took:?}");
        let started = std::time::Instant::now();
        let hits = index.search(
            &json!({ "query": "stanica 4242" })
                .as_object()
                .cloned()
                .unwrap_or_default(),
        );
        eprintln!("one search over 10,000 in {:?}", started.elapsed());
        assert!(hits
            .is_ok_and(|h| h["structuredContent"]["hits"][0]["endpoint"]
                == json!(format!("{:0>26}", 4242))));
    }

    #[test]
    fn a_question_gains_the_other_languages_words() {
        assert!(question("ovzdušie").contains(&"quali".to_owned()));
        assert!(question("air quality").contains(&"ovzdu".to_owned()));
        assert!(question("events Zilina").contains(&"poduj".to_owned()));
    }
}
