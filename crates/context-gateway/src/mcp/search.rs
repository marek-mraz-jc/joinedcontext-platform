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
use std::collections::{HashMap, HashSet};

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
    /// Per Endpoint, the types it answers for this caller, so a hit names one that serves them.
    served: Vec<(String, Vec<String>)>,
    domains: Vec<String>,
    themes: Vec<String>,
    /// The words of its titles, so a hit can say it matched on the title.
    titled: HashSet<String>,
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
    /// The words that name a place: a publisher's name, or a keyword written as a proper name,
    /// less the words of a topic and of an organisation (T-4568).
    places: HashSet<String>,
}

/// Words of a publisher's name that name what it is, not where (ponytail: a short list, grown
/// when a question is misread as a place).
const ORGANISATION: &[&str] = &[
    "of",
    "the",
    "and",
    "city",
    "mesto",
    "mestska",
    "obec",
    "region",
    "kraj",
    "self",
    "governing",
    "samospravny",
    "university",
    "project",
    "platform",
    "joinedcontext",
    "ministry",
    "ministerstvo",
    "institute",
    "agency",
    "office",
    "urad",
    "sr",
    "shmu",
    "su",
    "podnik",
    "statistics",
    "statisticky",
    "national",
    "narodny",
];

/// Words one language says for what the other says (ponytail: a short table, grown when a miss
/// is filed; embeddings only if T-4496 shows misses it cannot fix).
const SYNONYMS: &[(&str, &str)] = &[
    ("ovzdusie", "air quality"),
    ("ovzdusia", "air quality"),
    ("podujatia", "events"),
    ("udalosti", "events"),
    ("parkovanie", "parking"),
    ("doprava", "transport traffic"),
    // Public transport in Slovak and Czech, to the words and the Gtfs types an operator's
    // Endpoint serves (T-4734).
    ("mhd", "public transport gtfs"),
    ("hromadna doprava", "public transport gtfs"),
    ("verejna doprava", "public transport gtfs"),
    ("autobus", "bus public transport gtfs"),
    ("elektricka", "tram public transport gtfs"),
    ("tramvaj", "tram public transport gtfs"),
    ("cestovny poriadok", "timetable departures gtfs"),
    ("jizdni rad", "timetable departures gtfs"),
    ("zastavky", "stops"),
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
        let entities: Vec<&Value> = entities.into_iter().collect();
        // A Dataset's publisher is a place a question names: `Banská Bystrica`, `hel.fi`.
        let publishers: HashMap<&str, &str> = entities
            .iter()
            .filter(|e| e["type"] == "Organization")
            .filter_map(|e| Some((e["id"].as_str()?, e["name"]["value"].as_str()?)))
            .collect();
        for entity in entities.into_iter().filter(|e| e["type"] == "Dataset") {
            let slugs: Vec<String> = entity["refDataService"]["object"]
                .as_str()
                .and_then(|id| id.strip_prefix("urn:ngsi-ld:DataService:"))
                .map(str::to_owned)
                .into_iter()
                .collect();
            let mut text = entity["searchText"]["value"]
                .as_str()
                .unwrap_or("")
                .to_owned();
            // A CKAN dataset names its Organization; an Endpoint's own, its record's publisher.
            let names: Vec<&str> = match entity["refPublisher"]["object"].as_str() {
                Some(id) => publishers.get(id).copied().into_iter().collect(),
                None => entity["publisher"]["value"]["name"]
                    .as_object()
                    .into_iter()
                    .flat_map(|names| names.values().filter_map(Value::as_str))
                    .collect(),
            };
            for name in names {
                text.push(' ');
                text.push_str(name);
                index.places.extend(terms(name));
            }
            // A keyword whose every word is capitalised is a proper name: `Banská Bystrica`,
            // never `air quality`, nor `PM10`, which has no small letter.
            for keyword in strings(Some(&entity["keyword"]["value"])) {
                let proper = keyword.split_whitespace().all(|word| {
                    word.chars().next().is_some_and(char::is_uppercase)
                        && word.chars().any(char::is_lowercase)
                });
                if proper {
                    index.places.extend(terms(&keyword));
                }
            }
            // A harvested Dataset carries CKAN's title alone, in no language it states.
            let title: Map<String, Value> = match entity["title"]["languageMap"].as_object() {
                Some(titles) => titles.clone(),
                None => entity["name"]["value"]
                    .as_str()
                    .map(|name| Map::from_iter([("und".to_owned(), json!(name))]))
                    .unwrap_or_default(),
            };
            let titled = title
                .values()
                .filter_map(Value::as_str)
                .flat_map(terms)
                .collect();
            let mut doc = Doc {
                dataset: entity["id"].as_str().unwrap_or("").to_owned(),
                title,
                slugs,
                types: Vec::new(),
                served: Vec::new(),
                domains: Vec::new(),
                themes: strings(Some(&entity["theme"]["value"]))
                    .iter()
                    .filter_map(|iri| iri.rsplit('/').next().map(str::to_owned))
                    .collect(),
                titled,
                terms: HashMap::new(),
                len: 0,
            };
            for slug in &doc.slugs {
                if let Some(f) = facts.get(slug) {
                    doc.served.push((slug.clone(), f.types.clone()));
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
        let topics: HashSet<String> = facts
            .values()
            .flat_map(|f| f.types.iter().chain(&f.words).chain(&f.domains))
            .flat_map(|word| terms(word))
            .chain(
                SYNONYMS
                    .iter()
                    .flat_map(|(sk, en)| terms(sk).into_iter().chain(terms(en))),
            )
            .chain(ORGANISATION.iter().flat_map(|word| terms(word)))
            .collect();
        index.places.retain(|word| !topics.contains(word));
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
        // The place the question names, when the caller gives none apart (T-4568).
        let named: Vec<String> = if place.is_empty() {
            words
                .iter()
                .filter(|word| self.places.contains(*word))
                .cloned()
                .collect()
        } else {
            place.clone()
        };
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
        // A Dataset of the named place before any other, and another place's only when no
        // Dataset names this one, said so (T-4568).
        let local = |doc: &Doc| named.iter().all(|w| doc.terms.contains_key(w));
        let elsewhere =
            place.is_empty() && !named.is_empty() && !ranked.iter().any(|(_, doc)| local(doc));
        if !elsewhere {
            ranked.retain(|(_, doc)| local(doc));
        }
        let asked = Asked {
            words: &words,
            place: if elsewhere { &[] } else { &named },
            kind: kind.as_deref(),
            domain: domain.as_deref(),
            theme: theme.as_deref(),
        };
        let hits: Vec<Value> = ranked
            .iter()
            .take(limit)
            .map(|(score, doc)| hit(doc, *score, &lang, &asked))
            .collect();
        let mut text = String::from("endpoint | title | types | domain | dataset | why\n");
        for hit in &hits {
            text.push_str(&format!(
                "{} | {} | {} | {} | {} | {}\n",
                hit["endpoint"].as_str().unwrap_or(""),
                hit["title"].as_str().unwrap_or(""),
                strings(Some(&hit["types"])).join(","),
                strings(Some(&hit["domains"])).join(","),
                hit["dataset"].as_str().unwrap_or(""),
                hit["why"].as_str().unwrap_or(""),
            ));
        }
        if elsewhere {
            text.push_str("no Dataset names the place asked; these are from other places\n");
        }
        text.push_str(&format!(
            "{} of {} matched; nextQuery is the call that reads a hit",
            hits.len(),
            ranked.len()
        ));
        let mut answer = super::jsonrpc::answered(&json!(hits), "hits");
        answer["content"][0]["text"] = json!(text);
        answer["structuredContent"]["total"] = json!(ranked.len());
        if elsewhere {
            answer["structuredContent"]["elsewhere"] = json!(true);
        }
        Ok(answer)
    }
}

/// What a question asked, for a hit's reason.
struct Asked<'a> {
    words: &'a [String],
    place: &'a [String],
    kind: Option<&'a str>,
    domain: Option<&'a str>,
    theme: Option<&'a str>,
}

/// One hit: the Dataset, the Endpoint to read and the call that reads it.
///
/// The Endpoint is the first that serves the asked `type`, else the first that serves any, and
/// the hit names that Endpoint's types alone: each one answers its `nextQuery` (T-4567).
fn hit(doc: &Doc, score: f64, lang: &str, asked: &Asked<'_>) -> Value {
    let kind = asked.kind;
    let title = doc
        .title
        .get(lang)
        .or_else(|| doc.title.get("en"))
        .or_else(|| doc.title.values().next())
        .cloned()
        .unwrap_or(Value::Null);
    let named = |t: &String| kind.is_some_and(|k| t.to_lowercase() == k);
    // A type the question says in words: "events" is Event (T-4570).
    let spoken = |t: &String| terms(t).iter().any(|w| asked.words.contains(w));
    let (endpoint, types) = doc
        .served
        .iter()
        .find(|(_, types)| types.iter().any(named))
        .or_else(|| {
            // The Endpoint most about it: of those serving a spoken type, the one serving fewest.
            doc.served
                .iter()
                .enumerate()
                .filter(|(_, (_, types))| types.iter().any(spoken))
                .min_by_key(|(at, (_, types))| (types.len(), *at))
                .map(|(_, served)| served)
        })
        .or_else(|| doc.served.iter().find(|(_, types)| !types.is_empty()))
        .map(|(slug, types)| (slug.clone(), types.as_slice()))
        .unwrap_or_else(|| (doc.slugs.first().cloned().unwrap_or_default(), &[]));
    let mut arguments = json!({ "endpoint": endpoint, "limit": 10 });
    if let Some(first) = types
        .iter()
        .find(|t| named(t))
        .or_else(|| types.iter().find(|t| spoken(t)))
        .or(types.first())
    {
        arguments["type"] = json!([first]);
    }
    let why = reason(doc, types, asked);
    json!({
        "dataset": doc.dataset,
        "why": why,
        "endpoint": endpoint,
        "endpoints": doc.slugs,
        "title": title,
        "types": types,
        "domains": doc.domains,
        "themes": doc.themes,
        "score": (score * 100.0).round() / 100.0,
        "nextQuery": { "tool": "query_entities", "arguments": arguments },
    })
}

/// One line of why a Dataset matched: the place, the title, the types, the facets; else the
/// rest of its text (T-4568).
fn reason(doc: &Doc, types: &[String], asked: &Asked<'_>) -> String {
    let topic: Vec<&String> = asked
        .words
        .iter()
        .filter(|w| !asked.place.contains(w))
        .collect();
    let mut why = Vec::new();
    if !asked.place.is_empty() {
        why.push("place".to_owned());
    }
    if topic.iter().any(|w| doc.titled.contains(*w)) {
        why.push("title".to_owned());
    }
    let typed: Vec<&str> = types
        .iter()
        .filter(|t| {
            asked.kind.is_some_and(|k| t.to_lowercase() == k)
                || terms(t).iter().any(|w| topic.contains(&w))
        })
        .map(String::as_str)
        .collect();
    if !typed.is_empty() {
        why.push(format!("type {}", typed.join(", ")));
    }
    for (name, facet) in [("theme", asked.theme), ("domain", asked.domain)] {
        if let Some(facet) = facet {
            why.push(format!("{name} {facet}"));
        }
    }
    if why.is_empty() {
        why.push("description".to_owned());
    }
    why.join("; ")
}

/// The `search` tool as `tools/list` names it.
pub(crate) fn tool() -> Value {
    json!({
        "name": "search",
        "description": "The datasets you may read, ranked for a question by title, description \
                        (sk, en, diacritics optional), type, domain, place and publisher; each hit \
                        names its Endpoint and nextQuery.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "query": { "type": "string", "minLength": 1, "maxLength": 200, "description": "e.g. air quality Banská Bystrica" },
                "domain": { "type": "string", "minLength": 1, "maxLength": 200 },
                "theme": { "type": "string", "minLength": 1, "maxLength": 200 },
                "type": { "type": "string", "minLength": 1, "maxLength": 200 },
                "place": { "type": "string", "minLength": 1, "maxLength": 200 },
                "limit": { "type": "integer", "minimum": 1, "maximum": MAX_LIMIT },
                "lang": { "type": "string", "minLength": 2, "maxLength": 8 },
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
                    "refDataService": { "type": "Relationship", "object": format!("urn:ngsi-ld:DataService:{n:0>26}") },
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

    /// T-4734: on dev "mestská hromadná doprava" ranked "Compare districts" and Praha's city data
    /// above HSL. The Slovak and Czech public-transport words find the Endpoint serving Gtfs
    /// types first.
    #[test]
    fn public_transport_words_find_the_gtfs_endpoint_first() {
        let dataset = |n: &str, en: &str, sk: &str, text: &str| {
            json!({
                "id": format!("urn:ngsi-ld:Dataset:{n}"), "type": "Dataset",
                "title": { "languageMap": { "en": en, "sk": sk } },
                "searchText": { "value": text },
                "refDataService": { "type": "Relationship", "object": format!("urn:ngsi-ld:DataService:{n}") },
            })
        };
        let entities = [
            dataset(
                "districts",
                "Compare districts",
                "Porovnanie mestských častí",
                "compare districts porovnanie mestskych casti doprava obyvatelstvo",
            ),
            dataset(
                "praha",
                "Praha city data",
                "Údaje mesta Praha",
                "praha mestska data predajne cestovnych listkov doprava parkovanie",
            ),
            dataset(
                "roads",
                "Road transport statistics",
                "Cestná doprava",
                "cestna nakladna a osobna doprava statistika",
            ),
            dataset(
                "hsl",
                "HSL public transport",
                "Verejná doprava HSL",
                "hsl public transport vehicles stops departures alerts park and ride",
            ),
        ];
        let mut facts = HashMap::new();
        facts.insert(
            "hsl".to_owned(),
            Facts {
                types: ["GtfsStop", "GtfsRoute", "Vehicle", "Alert"]
                    .map(str::to_owned)
                    .to_vec(),
                ..Facts::default()
            },
        );
        let index = Index::build(&entities, &facts);
        for asked in [
            "mestská hromadná doprava",
            "Máte dáta o MHD?",
            "verejná doprava",
            "autobus a električka",
            "cestovný poriadok",
            "městská hromadná doprava",
            "jízdní řád",
        ] {
            let hits = index
                .search(
                    &json!({ "query": asked })
                        .as_object()
                        .cloned()
                        .unwrap_or_default(),
                )
                .expect("a search");
            assert_eq!(
                hits["structuredContent"]["hits"][0]["endpoint"],
                json!("hsl"),
                "{asked}: {hits}"
            );
        }
    }
}
