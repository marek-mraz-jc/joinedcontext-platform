//! The open-data catalogue as NGSI-LD entities (T-4500, T-4503, ADR-N-025): one SDM DCAT-AP
//! `Dataset` per context space and one `DataService` per Endpoint of it, read from the Endpoints'
//! DCAT-AP records (EP-27, EP-84).
//!
//! One function for both catalogues: the Portal writes the public one into the `catalog`
//! Endpoint from the gateway's feed, and the MCP hub answers each caller's own from the records
//! of the Endpoints it may read. Built from the same records by the same function, the
//! anonymous caller's catalogue is the public one entity for entity.

use serde_json::{json, Map, Value};
use std::collections::{BTreeMap, BTreeSet};
use unicode_normalization::UnicodeNormalization;

/// What a record does not say about its Endpoint, from whoever holds the Endpoint table.
#[derive(Debug, Clone, PartialEq)]
pub struct Place {
    /// The `{space}` segment of the Endpoint's context space: its `Dataset` is
    /// `urn:ngsi-ld:Dataset:{space}`.
    pub space: String,
    /// The space's `area` quality rule, `[minLon, minLat, maxLon, maxLat]` (DM-87).
    pub bbox: Option<[f64; 4]>,
    /// The Endpoint's audience: `public`, `organization` or `project-list`.
    pub access_level: String,
    /// Why the caller sees it: `public`, `organization`, `project` or `grant`.
    pub visible_because: String,
}

impl Place {
    /// A public Endpoint as the anonymous caller sees it.
    pub fn public(space: impl Into<String>, bbox: Option<[f64; 4]>) -> Self {
        Self {
            space: space.into(),
            bbox,
            access_level: "public".to_owned(),
            visible_because: "public".to_owned(),
        }
    }
}

/// The catalogue's entities by id, from the JSON-LD nodes of a catalogue (a feed's
/// `@included`): every `dcat:Dataset` record whose `dct:identifier` is a slug in `places`, with
/// the `dcat:DataService` beside it.
pub fn entities(nodes: &[Value], places: &BTreeMap<String, Place>) -> BTreeMap<String, Value> {
    let services: BTreeMap<&str, &Value> = nodes
        .iter()
        .filter(|node| node.get("@type").and_then(Value::as_str) == Some("dcat:DataService"))
        .filter_map(|node| Some((node.get("@id")?.as_str()?, node)))
        .collect();

    // The records by space, in slug order: the first one names the space's Dataset.
    let mut spaces: BTreeMap<&str, Vec<(&str, &Value)>> = BTreeMap::new();
    for record in nodes {
        if record.get("@type").and_then(Value::as_str) != Some("dcat:Dataset") {
            continue;
        }
        let Some(slug) = record.get("dct:identifier").and_then(Value::as_str) else {
            continue;
        };
        if let Some(place) = places.get(slug) {
            spaces
                .entry(place.space.as_str())
                .or_default()
                .push((slug, record));
        }
    }

    let mut out = BTreeMap::new();
    for (space, mut records) in spaces {
        records.sort_by_key(|(slug, _)| *slug);
        let dataset_id = format!("urn:ngsi-ld:Dataset:{space}");
        let mut links = Vec::new();
        let mut bbox = None;
        for (slug, record) in &records {
            let place = &places[*slug];
            bbox = bbox.or(place.bbox);
            let service = record
                .get("@id")
                .and_then(Value::as_str)
                .and_then(|id| services.get(format!("{id}#service").as_str()).copied());
            let (id, entity) = data_service(slug, record, service, place, &dataset_id);
            links.push(json!({
                "id": id,
                "identifier": slug,
                "endpointURL": entity.get("endpointURL").map(|url| url["value"].clone()),
            }));
            out.insert(id, entity);
        }
        let first = records[0].1;
        let mut dataset = Map::new();
        dataset.insert("id".into(), json!(dataset_id));
        dataset.insert("type".into(), json!("Dataset"));
        languages(&mut dataset, "title", first.get("dct:title"));
        languages(&mut dataset, "description", first.get("dct:description"));
        let keywords = union(records.iter().map(|(_, r)| r.get("dcat:keyword")), text);
        let themes = union(records.iter().map(|(_, r)| r.get("dcat:theme")), iri);
        let spatial = union(records.iter().map(|(_, r)| r.get("dct:spatial")), iri);
        list(&mut dataset, "keyword", &keywords);
        list(&mut dataset, "theme", &themes);
        list(&mut dataset, "spatial", &spatial);
        if let Some([w, s, e, n]) = bbox {
            dataset.insert(
                "location".into(),
                json!({ "type": "GeoProperty", "value": {
                    "type": "Polygon",
                    "coordinates": [[[w, s], [e, s], [e, n], [w, n], [w, s]]],
                }}),
            );
        }
        if let Some(temporal) = first.get("dct:temporal") {
            let period: Map<String, Value> =
                [("startDate", "dcat:startDate"), ("endDate", "dcat:endDate")]
                    .into_iter()
                    .filter_map(|(key, term)| {
                        Some((key.to_owned(), json!(text(temporal.get(term)?)?)))
                    })
                    .collect();
            if !period.is_empty() {
                dataset.insert("temporal".into(), property(Value::Object(period)));
            }
        }
        if let Some(frequency) = first.get("dct:accrualPeriodicity").and_then(iri) {
            dataset.insert("frequency".into(), property(json!(frequency)));
        }
        if let Some(publisher) = first.get("dct:publisher") {
            let mut value = Map::new();
            if let Some(id) = publisher.get("@id").and_then(Value::as_str) {
                value.insert("id".into(), json!(id));
            }
            if let Some(name) = language_map(publisher.get("foaf:name")) {
                value.insert("name".into(), Value::Object(name));
            }
            if !value.is_empty() {
                dataset.insert("publisher".into(), property(Value::Object(value)));
            }
        }
        dataset.insert("dataService".into(), property(Value::Array(links)));
        let mut words: Vec<String> = Vec::new();
        for (_, record) in &records {
            for term in ["dct:title", "dct:description"] {
                if let Some(map) = language_map(record.get(term)) {
                    words.extend(map.values().filter_map(Value::as_str).map(str::to_owned));
                }
            }
        }
        words.extend(keywords);
        words.push(space.to_owned());
        dataset.insert("searchText".into(), property(json!(fold(&words.join(" ")))));
        out.insert(dataset_id, Value::Object(dataset));
    }
    out
}

/// One Endpoint as a `DataService`, and its id.
fn data_service(
    slug: &str,
    record: &Value,
    service: Option<&Value>,
    place: &Place,
    dataset: &str,
) -> (String, Value) {
    let id = format!("urn:ngsi-ld:DataService:{slug}");
    let mut entity = Map::new();
    entity.insert("id".into(), json!(id));
    entity.insert("type".into(), json!("DataService"));
    languages(&mut entity, "title", record.get("dct:title"));
    languages(&mut entity, "description", record.get("dct:description"));
    entity.insert("identifier".into(), property(json!(slug)));
    if let Some(url) = service
        .and_then(|s| s.get("dcat:endpointURL"))
        .and_then(iri)
    {
        entity.insert("endpointURL".into(), property(json!(url)));
    }
    // The access paths, not the schema artifacts: those are the Endpoint's own to list, and
    // reading them costs a broker hop the hub does not make per caller.
    let distributions: Vec<Value> = record
        .get("dcat:distribution")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|distribution| {
            let url = iri(distribution.get("dcat:accessURL")?)?;
            if url.contains("/schema/") {
                return None;
            }
            let mut access = Map::new();
            access.insert("accessURL".into(), json!(url));
            for (key, term) in [
                ("conformsTo", "dct:conformsTo"),
                ("mediaType", "dcat:mediaType"),
            ] {
                if let Some(value) = distribution.get(term).and_then(iri) {
                    access.insert(key.into(), json!(value));
                }
            }
            Some(Value::Object(access))
        })
        .collect();
    if !distributions.is_empty() {
        entity.insert("accessURL".into(), property(Value::Array(distributions)));
    }
    for (key, term) in [
        ("accessRights", "dct:accessRights"),
        ("license", "dct:license"),
    ] {
        if let Some(value) = record.get(term).and_then(iri) {
            entity.insert(key.into(), property(json!(value)));
        }
    }
    entity.insert("accessLevel".into(), property(json!(place.access_level)));
    entity.insert(
        "visibleBecause".into(),
        property(json!(place.visible_because)),
    );
    entity.insert(
        "servesDataset".into(),
        json!({ "type": "Relationship", "object": dataset }),
    );
    (id, Value::Object(entity))
}

fn property(value: Value) -> Value {
    json!({ "type": "Property", "value": value })
}

/// A JSON-LD language-tagged value or list of them as a language map; a plain string is English.
fn language_map(value: Option<&Value>) -> Option<Map<String, Value>> {
    let mut map = Map::new();
    let values: Vec<&Value> = match value? {
        Value::Array(items) => items.iter().collect(),
        other => vec![other],
    };
    for item in values {
        match item {
            Value::String(text) => {
                map.entry("en").or_insert_with(|| json!(text));
            }
            Value::Object(tagged) => {
                if let (Some(language), Some(text)) = (
                    tagged.get("@language").and_then(Value::as_str),
                    tagged.get("@value").and_then(Value::as_str),
                ) {
                    map.entry(language).or_insert_with(|| json!(text));
                }
            }
            _ => {}
        }
    }
    (!map.is_empty()).then_some(map)
}

fn languages(entity: &mut Map<String, Value>, key: &str, value: Option<&Value>) {
    if let Some(map) = language_map(value) {
        entity.insert(
            key.into(),
            json!({ "type": "LanguageProperty", "languageMap": map }),
        );
    }
}

fn list(entity: &mut Map<String, Value>, key: &str, values: &[String]) {
    if !values.is_empty() {
        entity.insert(key.into(), property(json!(values)));
    }
}

/// The text of a literal: a string or a JSON-LD `@value`.
fn text(value: &Value) -> Option<String> {
    match value {
        Value::String(text) => Some(text.clone()),
        Value::Object(literal) => literal.get("@value")?.as_str().map(str::to_owned),
        _ => None,
    }
}

/// An IRI: a string or a node with an `@id`.
fn iri(value: &Value) -> Option<String> {
    match value {
        Value::String(iri) => Some(iri.clone()),
        Value::Object(node) => node.get("@id")?.as_str().map(str::to_owned),
        _ => None,
    }
}

/// Every value of every term, once each, in the order first seen.
fn union<'a>(
    terms: impl Iterator<Item = Option<&'a Value>>,
    read: fn(&Value) -> Option<String>,
) -> Vec<String> {
    let mut seen = BTreeSet::new();
    let mut out = Vec::new();
    for term in terms.flatten() {
        let items: Vec<&Value> = match term {
            Value::Array(items) => items.iter().collect(),
            other => vec![other],
        };
        for value in items.into_iter().filter_map(read) {
            if seen.insert(value.clone()) {
                out.push(value);
            }
        }
    }
    out
}

/// Lowercase with the diacritics removed, so `q=searchText~=(?i).*bystrica.*` finds
/// `Bystrici` and `zilina` finds `Žilina` whatever the caller typed.
pub fn fold(text: &str) -> String {
    text.nfd()
        .filter(|c| !unicode_normalization::char::is_combining_mark(*c))
        .collect::<String>()
        .to_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOST: &str = "https://city.example";

    fn tagged(en: &str, sk: &str) -> Value {
        json!([{ "@language": "en", "@value": en }, { "@language": "sk", "@value": sk }])
    }

    /// One Endpoint's record and service, the way the gateway's feed carries them.
    fn record(slug: &str, title: (&str, &str)) -> [Value; 2] {
        let id = format!("{HOST}/api/endpoint/{slug}");
        [
            json!({
                "@id": id, "@type": "dcat:Dataset", "dct:identifier": slug,
                "dct:title": tagged(title.0, title.1),
                "dct:description": tagged("Hourly readings.", "Hodinové merania."),
                "dcat:keyword": tagged("air", "ovzdušie"),
                "dcat:theme": ["http://publications.europa.eu/resource/authority/data-theme/ENVI"],
                "dct:spatial": ["https://data.gov.sk/id/lau2/SK0321508438"],
                "dct:temporal": { "@type": "dct:PeriodOfTime", "dcat:startDate": "2020-01-01" },
                "dct:accrualPeriodicity": "http://publications.europa.eu/resource/authority/frequency/HOURLY",
                "dct:publisher": { "@id": "https://banskabystrica.sk/", "@type": "foaf:Agent",
                    "foaf:name": tagged("City of Banská Bystrica", "Mesto Banská Bystrica") },
                "dct:accessRights": "http://publications.europa.eu/resource/authority/access-right/PUBLIC",
                "dct:license": "http://publications.europa.eu/resource/authority/licence/CC_BY_4_0",
                "dcat:distribution": [
                    {
                        "@id": format!("{id}/ngsi-ld/v1/"), "@type": "dcat:Distribution",
                        "dcat:accessURL": format!("{id}/ngsi-ld/v1/"),
                        "dct:conformsTo": "https://www.etsi.org/deliver/etsi_gs/CIM/001_099/009/",
                        "dcat:mediaType": "https://www.iana.org/assignments/media-types/application/json",
                    },
                    {
                        "@type": "dcat:Distribution",
                        "dcat:accessURL": format!("{id}/schema/v1/model.schema.json"),
                    },
                ],
            }),
            json!({
                "@id": format!("{id}#service"), "@type": "dcat:DataService",
                "dcat:endpointURL": format!("{id}/"), "dcat:servesDataset": id,
            }),
        ]
    }

    fn nodes(records: &[[Value; 2]]) -> Vec<Value> {
        records
            .iter()
            .flat_map(|pair| pair.iter().cloned())
            .collect()
    }

    fn places() -> BTreeMap<String, Place> {
        BTreeMap::from([
            (
                "air".to_owned(),
                Place::public("mesto-ovzdusie", Some([19.05, 48.68, 19.25, 48.8])),
            ),
            (
                "air-history".to_owned(),
                Place::public("mesto-ovzdusie", None),
            ),
            ("bikes".to_owned(), Place::public("mesto-doprava", None)),
        ])
    }

    fn city() -> Vec<Value> {
        nodes(&[
            record(
                "air",
                (
                    "Air quality in Banská Bystrica",
                    "Ovzdušie v Banskej Bystrici",
                ),
            ),
            record("air-history", ("Air quality history", "História ovzdušia")),
            record("bikes", ("Bike stations", "Stanice bicyklov")),
            record("inner", ("Internal registry", "Interný register")),
        ])
    }

    #[test]
    fn a_dataset_per_space_and_a_service_per_endpoint_agree_with_the_record() {
        let built = entities(&city(), &places());
        let ids: Vec<&str> = built.keys().map(String::as_str).collect();
        assert_eq!(
            ids,
            [
                "urn:ngsi-ld:DataService:air",
                "urn:ngsi-ld:DataService:air-history",
                "urn:ngsi-ld:DataService:bikes",
                "urn:ngsi-ld:Dataset:mesto-doprava",
                "urn:ngsi-ld:Dataset:mesto-ovzdusie",
            ]
        );
        // A record whose slug has no place is not in the catalogue, not even by a word.
        assert!(!serde_json::to_string(&built)
            .expect("json")
            .contains("registry"));

        let [record, _] = record(
            "air",
            (
                "Air quality in Banská Bystrica",
                "Ovzdušie v Banskej Bystrici",
            ),
        );
        let service = &built["urn:ngsi-ld:DataService:air"];
        assert_eq!(
            service["title"]["languageMap"]["en"],
            "Air quality in Banská Bystrica"
        );
        assert_eq!(
            service["title"]["languageMap"]["sk"],
            "Ovzdušie v Banskej Bystrici"
        );
        assert_eq!(
            service["description"]["languageMap"]["sk"],
            "Hodinové merania."
        );
        assert_eq!(service["identifier"]["value"], "air");
        assert_eq!(
            service["endpointURL"]["value"],
            format!("{HOST}/api/endpoint/air/")
        );
        let distribution = &record["dcat:distribution"][0];
        assert_eq!(
            service["accessURL"]["value"],
            json!([{
                "accessURL": distribution["dcat:accessURL"],
                "conformsTo": distribution["dct:conformsTo"],
                "mediaType": distribution["dcat:mediaType"],
            }]),
            "the access paths only, not the schema artifacts"
        );
        assert_eq!(service["accessRights"]["value"], record["dct:accessRights"]);
        assert_eq!(service["license"]["value"], record["dct:license"]);
        assert_eq!(service["accessLevel"]["value"], "public");
        assert_eq!(service["visibleBecause"]["value"], "public");
        assert_eq!(
            service["servesDataset"]["object"],
            "urn:ngsi-ld:Dataset:mesto-ovzdusie"
        );

        let dataset = &built["urn:ngsi-ld:Dataset:mesto-ovzdusie"];
        // The space's first Endpoint by slug names it.
        assert_eq!(
            dataset["title"]["languageMap"]["en"],
            "Air quality in Banská Bystrica"
        );
        assert_eq!(dataset["theme"]["value"], record["dcat:theme"]);
        assert_eq!(dataset["spatial"]["value"], record["dct:spatial"]);
        assert_eq!(dataset["keyword"]["value"], json!(["air", "ovzdušie"]));
        assert_eq!(
            dataset["temporal"]["value"],
            json!({ "startDate": "2020-01-01" })
        );
        assert_eq!(
            dataset["frequency"]["value"],
            record["dct:accrualPeriodicity"]
        );
        assert_eq!(
            dataset["publisher"]["value"]["id"],
            "https://banskabystrica.sk/"
        );
        assert_eq!(
            dataset["publisher"]["value"]["name"]["sk"],
            "Mesto Banská Bystrica"
        );
        assert_eq!(
            dataset["location"]["value"]["coordinates"][0][2],
            json!([19.25, 48.8])
        );
        let services: Vec<&Value> = dataset["dataService"]["value"]
            .as_array()
            .expect("a list")
            .iter()
            .map(|link| &link["identifier"])
            .collect();
        assert_eq!(services, ["air", "air-history"]);
        let search = dataset["searchText"]["value"].as_str().expect("text");
        assert!(
            search.contains("banska bystrica") && search.contains("bystrici"),
            "{search}"
        );
        assert!(
            search.contains("ovzdusie") && !search.contains('š'),
            "{search}"
        );
        // No area declared: no location written, not an empty one.
        assert!(built["urn:ngsi-ld:Dataset:mesto-doprava"]
            .get("location")
            .is_none());
    }

    #[test]
    fn a_caller_with_a_grant_sees_why_and_a_dataset_only_of_what_it_reads() {
        let mut places = places();
        places.remove("air");
        places.insert(
            "inner".to_owned(),
            Place {
                space: "mesto-ovzdusie".to_owned(),
                bbox: None,
                access_level: "organization".to_owned(),
                visible_because: "grant".to_owned(),
            },
        );
        let built = entities(&city(), &places);
        let inner = &built["urn:ngsi-ld:DataService:inner"];
        assert_eq!(inner["accessLevel"]["value"], "organization");
        assert_eq!(inner["visibleBecause"]["value"], "grant");
        let dataset = &built["urn:ngsi-ld:Dataset:mesto-ovzdusie"];
        assert_eq!(dataset["title"]["languageMap"]["en"], "Air quality history");
        assert!(!dataset["searchText"]["value"]
            .as_str()
            .expect("text")
            .contains("banska"));
        assert!(!built.contains_key("urn:ngsi-ld:DataService:air"));
    }

    #[test]
    fn nothing_readable_is_an_empty_catalogue() {
        assert!(entities(&city(), &BTreeMap::new()).is_empty());
        assert!(entities(&[], &places()).is_empty());
    }

    #[test]
    fn folding_drops_diacritics_and_case() {
        assert_eq!(
            fold("Banská Bystrica, ŽILINA, Hämeenlinna"),
            "banska bystrica, zilina, hameenlinna"
        );
    }
}
