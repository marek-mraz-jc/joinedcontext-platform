//! The catalogue's own questions, answered with one NGSI-LD query on the catalogue Endpoint
//! (EP-140, T-4746): "which organizations publish data?" reads its `Organization` entities, and
//! "what data does Helsinki publish?" its `Dataset` entities by `refPublisher`, instead of the
//! dataset pages a search finds. The organizations' names come from the catalogue itself, read
//! while the connectors are warmed, never from a list in code.

use std::collections::{HashMap, HashSet};

use jc_core::ngsi_catalog::fold;
use serde_json::{json, Value};

use super::routed::{Read, Routing, Shown};

/// The name of the catalogue Endpoint, on its own or as `{project}/catalog`.
const CATALOG: &str = "catalog";
/// The most organizations one read lists.
pub const MOST: usize = 1000;

/// One organization of the catalogue: its entity id and the words it is asked about by.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Publisher {
    pub id: String,
    stems: Vec<String>,
}

/// Whether the connector is a catalogue Endpoint.
pub fn is_catalogue(endpoint: &str) -> bool {
    endpoint.rsplit('/').next() == Some(CATALOG)
}

/// The arguments of the read that lists the catalogue's organizations.
pub fn listing() -> Value {
    json!({ "type": ["Organization"], "limit": MOST })
}

/// The organizations of a `query_entities` answer; `None` when it is no list of entities.
pub fn publishers_of(text: &str) -> Option<Vec<Publisher>> {
    let value: Value = serde_json::from_str(text).ok()?;
    let entities = value.get("entities").unwrap_or(&value).as_array()?;
    let plain = |entity: &Value, key: &str| -> Option<String> {
        let field = entity.get(key)?;
        let text = field.get("value").unwrap_or(field).as_str()?;
        Some(text.to_owned())
    };
    Some(
        entities
            .iter()
            .filter_map(|entity| {
                let id = entity.get("id")?.as_str()?.to_owned();
                let named = [plain(entity, "name"), plain(entity, "identifier")];
                let stems = stems(&named.into_iter().flatten().collect::<Vec<_>>().join(" "));
                Some(Publisher { id, stems })
            })
            .collect(),
    )
}

// ponytail: a word's stem is the word less its last letter, which catches Slovak and Czech cases
// ("Žilina", "v Žiline"); a stemmer when a name is missed.
/// The folded words of `text` of five letters or more, each less its last letter.
fn stems(text: &str) -> Vec<String> {
    fold(text)
        .split(|c: char| !c.is_alphanumeric())
        .filter(|word| word.chars().count() >= 5)
        .map(|word| {
            let mut stem = word.to_owned();
            stem.pop();
            stem
        })
        .collect()
}

/// The one organization the question names by a stem no other organization shares; `None`
/// when it names none or several.
fn named<'a>(question: &str, publishers: &'a [Publisher]) -> Option<&'a Publisher> {
    let mut shared: HashMap<&str, HashSet<&str>> = HashMap::new();
    for publisher in publishers {
        for stem in &publisher.stems {
            shared.entry(stem).or_default().insert(&publisher.id);
        }
    }
    let asked = fold(question);
    let words: Vec<&str> = asked.split(|c: char| !c.is_alphanumeric()).collect();
    let hits: HashSet<&str> = publishers
        .iter()
        .filter(|publisher| {
            publisher.stems.iter().any(|stem| {
                shared[stem.as_str()].len() == 1 && words.iter().any(|w| w.starts_with(stem))
            })
        })
        .map(|publisher| publisher.id.as_str())
        .collect();
    match hits.into_iter().collect::<Vec<_>>()[..] {
        [one] => publishers.iter().find(|p| p.id == one),
        _ => None,
    }
}

/// How a question asking which organizations publish starts, folded, in the languages the
/// assistant answers.
const ASKS_FOR_ORGANIZATIONS: &[&str] = &[
    "ktore organizacie",
    "ake organizacie",
    "zoznam organizacii",
    "vsetky organizacie",
    "kto publikuje",
    "kto zverejnuje",
    "ktori vydavatelia",
    "ktere organizace",
    "jake organizace",
    "kdo publikuje",
    "which organizations",
    "what organizations",
    "all organizations",
    "list organizations",
    "list the organizations",
    "which publishers",
    "who publishes",
];

/// Words of a question about one publisher's datasets, folded.
const ASKS_FOR_PUBLISHED: &[&str] = &[
    "publikuje",
    "zverejnuje",
    "dataset",
    "datasety",
    "datasetov",
    "datasets",
    "publishes",
    "published",
];

fn asks_for_organizations(asked: &str) -> bool {
    ASKS_FOR_ORGANIZATIONS
        .iter()
        .any(|phrase| asked.contains(phrase))
}

/// The one read on the catalogue a question about its organizations or one publisher's
/// datasets needs; `None` when the deployment connects no catalogue that lists organizations,
/// or the question is neither.
pub fn catalogue_read(
    question: &str,
    asks_for_datasets: bool,
    live: &[Shown],
    publishers: &[Publisher],
) -> Option<Read> {
    let catalogue = live
        .iter()
        .find(|s| is_catalogue(&s.endpoint) && s.types.iter().any(|t| t == "Organization"))?;
    let asked = fold(question);
    if asks_for_organizations(&asked) {
        return Some(Read {
            endpoint: catalogue.endpoint.clone(),
            kind: "Organization".into(),
            q: None,
        });
    }
    let about = asks_for_datasets
        || asked
            .split(|c: char| !c.is_alphanumeric())
            .any(|word| ASKS_FOR_PUBLISHED.contains(&word));
    let publisher = named(question, publishers).filter(|_| about)?;
    catalogue
        .types
        .iter()
        .any(|t| t == "Dataset")
        .then(|| Read {
            endpoint: catalogue.endpoint.clone(),
            kind: "Dataset".into(),
            q: Some(format!("refPublisher==\"{}\"", publisher.id)),
        })
}

/// The route with the catalogue read in place of what the router chose, when the question is
/// one of the catalogue's own; a direct message stays direct.
pub fn routed(routing: Routing, read: Option<Read>) -> Routing {
    match read {
        Some(read) if !routing.direct => Routing {
            rag: false,
            data: true,
            reads: vec![read],
            catalogue: false,
            ..routing
        },
        _ => routing,
    }
}

/// The answer to a catalogue read that found nothing, in `lang` (English for a language without
/// its own): written without a model call, which could only guess at what the catalogue lacks.
pub fn none(lang: &str, kind: &str) -> &'static str {
    match (lang, kind == "Organization") {
        ("sk", true) => "Katalóg nezverejňuje žiadne organizácie.",
        ("sk", false) => "Katalóg neuvádza od tohto vydavateľa žiadne datasety.",
        ("cs", true) => "Katalog nezveřejňuje žádné organizace.",
        ("cs", false) => "Katalog neuvádí od tohoto vydavatele žádné datové sady.",
        (_, true) => "The catalogue lists no organizations.",
        (_, false) => "The catalogue lists no datasets of this publisher.",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn publishers() -> Vec<Publisher> {
        let answer = json!({ "entities": [
            { "id": "urn:ngsi-ld:Organization:hel-fi", "type": "Organization",
              "name": { "type": "Property", "value": "City of Helsinki" },
              "identifier": { "type": "Property", "value": "hel-fi" } },
            { "id": "urn:ngsi-ld:Organization:zilina", "type": "Organization",
              "name": { "type": "Property", "value": "Mesto Žilina" } },
            { "id": "urn:ngsi-ld:Organization:banskabystrica", "type": "Organization",
              "name": "Mesto Banská Bystrica", "identifier": "banskabystrica" },
            { "id": "urn:ngsi-ld:Organization:bbsk", "type": "Organization",
              "name": "Banskobystrický samosprávny kraj" },
        ]});
        publishers_of(&answer.to_string()).expect("a list")
    }

    fn live() -> Vec<Shown> {
        vec![
            Shown {
                endpoint: "helsinki/helsinki-weather".into(),
                title: None,
                types: vec!["WeatherObserved".into()],
                attributes: Vec::new(),
            },
            Shown {
                endpoint: "slovensko/catalog".into(),
                title: None,
                types: vec![
                    "DataService".into(),
                    "Dataset".into(),
                    "Organization".into(),
                ],
                attributes: Vec::new(),
            },
        ]
    }

    #[test]
    fn which_organizations_publish_reads_the_organizations_once() {
        for question in [
            "Ktoré organizácie publikujú dáta?",
            "List all organizations",
            "Kto publikuje otvorené dáta?",
        ] {
            assert_eq!(
                catalogue_read(question, false, &live(), &publishers()),
                Some(Read {
                    endpoint: "slovensko/catalog".into(),
                    kind: "Organization".into(),
                    q: None
                }),
                "{question}"
            );
        }
    }

    #[test]
    fn one_publishers_data_is_one_query_by_ref_publisher() {
        let read = |q: &str, datasets: bool| catalogue_read(q, datasets, &live(), &publishers());
        let by = |org: &str| {
            Some(Read {
                endpoint: "slovensko/catalog".into(),
                kind: "Dataset".into(),
                q: Some(format!("refPublisher==\"urn:ngsi-ld:Organization:{org}\"")),
            })
        };
        assert_eq!(read("Aké dáta publikuje Helsinki?", true), by("hel-fi"));
        assert_eq!(
            read("List the datasets of hel-fi Helsinki", false),
            by("hel-fi")
        );
        assert_eq!(read("Aké datasety má mesto v Žiline?", true), by("zilina"));
        // "Mesto" is every city's word, so it names none of them.
        assert_eq!(read("Aké datasety publikuje mesto?", true), None);
        // Two publishers named: the router decides.
        assert_eq!(read("Datasety Žiliny a Helsinki", true), None);
        // A question for values, not datasets, keeps the router's route.
        assert_eq!(read("Aká je teplota v Helsinki?", false), None);
    }

    #[test]
    fn no_catalogue_connector_no_catalogue_read() {
        let live = &live()[..1];
        assert_eq!(
            catalogue_read(
                "Ktoré organizácie publikujú dáta?",
                false,
                live,
                &publishers()
            ),
            None
        );
        assert!(is_catalogue("catalog") && is_catalogue("slovensko/catalog"));
        assert!(!is_catalogue("slovensko/catalogue"));
        assert_eq!(publishers_of("not json"), None);
    }

    #[test]
    fn the_catalogue_read_replaces_the_route_but_never_a_direct_one() {
        let routing = Routing {
            direct: false,
            rag: true,
            data: false,
            queries: vec!["q".into()],
            reads: Vec::new(),
            followups: vec!["f".into()],
            catalogue: true,
        };
        let read = Read {
            endpoint: "catalog".into(),
            kind: "Organization".into(),
            q: None,
        };
        let r = routed(routing.clone(), Some(read.clone()));
        assert!(r.data && !r.rag && !r.catalogue && r.reads == [read.clone()]);
        assert_eq!(r.followups, ["f"]);
        let direct = Routing {
            direct: true,
            ..routing
        };
        assert_eq!(routed(direct.clone(), Some(read)), direct);
    }
}
