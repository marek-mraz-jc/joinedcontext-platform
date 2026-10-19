//! The open-data catalogue as NGSI-LD entities (T-4500, T-4503, T-4746, ADR-N-025, EP-140): one
//! SDM DCAT-AP `Dataset` per public CKAN dataset, named as CKAN names it, one `Organization` per
//! CKAN organization that publishes one, and one `DataService` per Endpoint, read from CKAN's
//! `package_search` and from the Endpoints' DCAT-AP records (EP-27, EP-84).
//!
//! One function for both catalogues: the Portal writes the public one into the `catalog`
//! Endpoint from CKAN and the gateway's feed, and the MCP hub answers each caller's own from the
//! records of the Endpoints it may read, beside the public Datasets and Organizations the Portal
//! wrote, through [`link`]. Built from the same records by the same functions, the anonymous
//! caller's catalogue is the public one entity for entity.

use serde_json::{json, Map, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::OnceLock;
use unicode_normalization::UnicodeNormalization;

/// What a record does not say about its Endpoint, from whoever holds the Endpoint table.
#[derive(Debug, Clone, PartialEq)]
pub struct Place {
    /// The `{space}` segment of the Endpoint's context space, a word of its Dataset's
    /// `searchText`.
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

    fn is_public(&self) -> bool {
        self.access_level == "public"
    }
}

/// A CKAN organization, as a dataset of `package_search` carries it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Publisher {
    /// The organization's `name`, its slug.
    pub name: String,
    /// Its `title`, the name a person reads; the slug when CKAN has none.
    pub title: String,
}

/// One public CKAN dataset, the fields the catalogue reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Package {
    /// The dataset's `name`, the slug of its page.
    pub name: String,
    /// Its `title`; the name when CKAN has none.
    pub title: String,
    /// Its `notes`, empty when CKAN has none.
    pub notes: String,
    /// The organization that publishes it.
    pub organization: Option<Publisher>,
    /// The names of its tags.
    pub tags: Vec<String>,
    /// The `extras`, key to value.
    pub extras: BTreeMap<String, String>,
    /// Its `license_url`.
    pub license_url: Option<String>,
    /// CKAN's `metadata_created`.
    pub created: Option<String>,
    /// CKAN's `metadata_modified`.
    pub modified: Option<String>,
    /// The slug of the one Endpoint its resources name.
    pub slug: String,
    /// The `type` and `q` its NGSI-LD resource narrows the Endpoint to, when it is not the
    /// whole Endpoint (a harvested dataset over a shared Endpoint).
    pub rows: Option<(String, Option<String>)>,
}

impl Package {
    /// A dataset of `package_search`; `None` for a private or withdrawn one, and for one whose
    /// resources name no Endpoint or more than one, which the catalogue cannot point at.
    pub fn from_ckan(package: &Value) -> Option<Self> {
        if package.get("private").and_then(Value::as_bool) != Some(false)
            || package
                .get("state")
                .and_then(Value::as_str)
                .unwrap_or("active")
                != "active"
        {
            return None;
        }
        let name = package.get("name")?.as_str()?.to_owned();
        let string = |key: &str| {
            package
                .get(key)
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|text| !text.is_empty())
                .map(str::to_owned)
        };
        let urls: Vec<&str> = package
            .get("resources")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|resource| resource.get("url")?.as_str())
            .collect();
        let slugs: BTreeSet<&str> = urls.iter().filter_map(|url| endpoint_slug(url)).collect();
        let [slug] = slugs.into_iter().collect::<Vec<_>>()[..] else {
            return None;
        };
        let rows = urls
            .iter()
            .filter(|url| url.contains("/ngsi-ld/v1/entities?"))
            .find_map(|url| narrowed(url));
        let organization = package.get("organization").and_then(|org| {
            let name = org.get("name")?.as_str()?.to_owned();
            let title = org
                .get("title")
                .and_then(Value::as_str)
                .filter(|title| !title.trim().is_empty())
                .map_or_else(|| name.clone(), str::to_owned);
            Some(Publisher { name, title })
        });
        let tags = package
            .get("tags")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|tag| tag.get("name")?.as_str().map(str::to_owned))
            .collect();
        let extras = package
            .get("extras")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|extra| {
                Some((
                    extra.get("key")?.as_str()?.to_owned(),
                    extra.get("value")?.as_str()?.to_owned(),
                ))
            })
            .collect();
        Some(Self {
            title: string("title").unwrap_or_else(|| name.clone()),
            notes: string("notes").unwrap_or_default(),
            license_url: string("license_url"),
            created: string("metadata_created"),
            modified: string("metadata_modified"),
            slug: slug.to_owned(),
            name,
            organization,
            tags,
            extras,
            rows,
        })
    }
}

/// The Endpoint slug of a gateway URL, `…/api/endpoint/{slug}/…`.
fn endpoint_slug(url: &str) -> Option<&str> {
    static SLUG: OnceLock<regex::Regex> = OnceLock::new();
    let slug = SLUG.get_or_init(|| {
        regex::Regex::new(r"/api/endpoint/([a-z0-9][a-z0-9-]*)(?:[/?#]|$)").expect("a regex")
    });
    Some(slug.captures(url)?.get(1)?.as_str())
}

/// The `type` and `q` of an NGSI-LD query URL.
fn narrowed(url: &str) -> Option<(String, Option<String>)> {
    let (_, query) = url.split_once('?')?;
    let (mut kind, mut q) = (None, None);
    for (key, value) in form_urlencoded::parse(query.as_bytes()) {
        match key.as_ref() {
            "type" => kind = Some(value.into_owned()),
            "q" => q = Some(value.into_owned()),
            _ => {}
        }
    }
    Some((kind?, q))
}

/// The public CKAN datasets and the catalogue site that shows them.
#[derive(Debug, Clone, Copy)]
pub struct Ckan<'a> {
    /// `https://opendata.{domain}`, without a trailing slash.
    pub site: &'a str,
    /// The public datasets of `package_search`.
    pub packages: &'a [Package],
}

/// The catalogue's entities by id, from the JSON-LD nodes of a catalogue (a feed's
/// `@included`) and, for the public catalogue, CKAN's datasets: a `DataService` per
/// `dcat:Dataset` record whose `dct:identifier` is a slug in `places`; a `Dataset` per public CKAN
/// dataset whose Endpoint is among them, with its `Organization`; and a `Dataset` of its own for
/// every Endpoint no CKAN dataset names, a non-public one always. Linked by [`link`].
pub fn entities(
    nodes: &[Value],
    places: &BTreeMap<String, Place>,
    ckan: Option<Ckan<'_>>,
) -> BTreeMap<String, Value> {
    let services: BTreeMap<&str, &Value> = nodes
        .iter()
        .filter(|node| node.get("@type").and_then(Value::as_str) == Some("dcat:DataService"))
        .filter_map(|node| Some((node.get("@id")?.as_str()?, node)))
        .collect();
    let records: BTreeMap<&str, &Value> = nodes
        .iter()
        .filter(|node| node.get("@type").and_then(Value::as_str) == Some("dcat:Dataset"))
        .filter_map(|node| Some((node.get("dct:identifier")?.as_str()?, node)))
        .filter(|(slug, _)| places.contains_key(*slug))
        .collect();

    let mut out = BTreeMap::new();
    for (slug, record) in &records {
        let place = &places[*slug];
        let service = record
            .get("@id")
            .and_then(Value::as_str)
            .and_then(|id| services.get(format!("{id}#service").as_str()).copied());
        let (id, entity) = data_service(slug, record, service, place);
        out.insert(id, entity);
        // Until a CKAN dataset names the Endpoint, it is a dataset of its own; [`link`] drops
        // this one once one does.
        out.insert(
            format!("urn:ngsi-ld:Dataset:{slug}"),
            own_dataset(slug, record, place),
        );
    }
    if let Some(ckan) = ckan {
        for package in ckan.packages {
            let Some(place) = places.get(&package.slug).filter(|place| place.is_public()) else {
                continue;
            };
            if !records.contains_key(package.slug.as_str()) {
                continue;
            }
            let record = records[package.slug.as_str()];
            // A CKAN name equal to another Endpoint's slug replaces that Endpoint's own Dataset:
            // the published dataset wins, and [`link`] clears the DataService's `refDataset`.
            out.insert(
                format!("urn:ngsi-ld:Dataset:{}", package.name),
                ckan_dataset(package, record, place, ckan.site),
            );
            if let Some(org) = &package.organization {
                out.insert(
                    format!("urn:ngsi-ld:Organization:{}", org.name),
                    organization(org, ckan.site),
                );
            }
        }
    }
    link(&mut out);
    out
}

/// Ties a catalogue's entities together, after [`entities`] or after the hub merges the public
/// Datasets and Organizations into a caller's own: a `Dataset` whose `refDataService` is not
/// here goes, an Endpoint's own `Dataset` goes once a CKAN dataset names its `DataService`, an
/// `Organization` no `Dataset` names goes and every other one counts its Datasets in
/// `datasetCount`, and each `DataService` lists the Datasets that name it in `refDataset`. No
/// Relationship is left pointing at nothing.
pub fn link(catalog: &mut BTreeMap<String, Value>) {
    let target = |entity: &Value, key: &str| entity[key]["object"].as_str().map(str::to_owned);
    // An Endpoint's own Dataset is the one named by its slug.
    let own = |id: &str, service: &str| {
        service
            .strip_prefix("urn:ngsi-ld:DataService:")
            .is_some_and(|slug| id.strip_prefix("urn:ngsi-ld:Dataset:") == Some(slug))
    };
    let services: BTreeSet<String> = catalog
        .iter()
        .filter(|(_, entity)| entity["type"] == "DataService")
        .map(|(id, _)| id.clone())
        .collect();
    let named: BTreeSet<String> = catalog
        .iter()
        .filter(|(_, entity)| entity["type"] == "Dataset")
        .filter_map(|(id, entity)| {
            target(entity, "refDataService").filter(|service| !own(id, service))
        })
        .collect();
    catalog.retain(|id, entity| {
        if entity["type"] != "Dataset" {
            return true;
        }
        target(entity, "refDataService").is_some_and(|service| {
            services.contains(&service) && !(own(id, &service) && named.contains(&service))
        })
    });
    let mut published: BTreeMap<String, u64> = BTreeMap::new();
    let mut served: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (id, entity) in catalog.iter().filter(|(_, e)| e["type"] == "Dataset") {
        if let Some(publisher) = target(entity, "refPublisher") {
            *published.entry(publisher).or_default() += 1;
        }
        if let Some(service) = target(entity, "refDataService") {
            served.entry(service).or_default().push(id.clone());
        }
    }
    catalog.retain(|id, entity| entity["type"] != "Organization" || published.contains_key(id));
    for (id, entity) in catalog.iter_mut() {
        let Some(members) = entity.as_object_mut() else {
            continue;
        };
        if entity_type(members) == "Organization" {
            let count = published.get(id).copied().unwrap_or(0);
            members.insert("datasetCount".into(), property(json!(count)));
        } else if entity_type(members) == "DataService" {
            match served.get(id) {
                Some(datasets) => {
                    let list: Vec<Value> =
                        datasets.iter().map(|id| json!({ "object": id })).collect();
                    members.insert(
                        "refDataset".into(),
                        json!({ "type": "ListRelationship", "objectList": list }),
                    );
                }
                None => {
                    members.remove("refDataset");
                }
            }
        }
    }
}

/// The `area` rule of the Endpoint's space as the Dataset's polygon (DM-87); none written when
/// the space declares none.
fn located(dataset: &mut Map<String, Value>, place: &Place) {
    if let Some([w, s, e, n]) = place.bbox {
        dataset.insert(
            "location".into(),
            json!({ "type": "GeoProperty", "value": {
                "type": "Polygon",
                "coordinates": [[[w, s], [e, s], [e, n], [w, n], [w, s]]],
            }}),
        );
    }
}

fn entity_type(members: &Map<String, Value>) -> &str {
    members.get("type").and_then(Value::as_str).unwrap_or("")
}

fn relationship(object: &str) -> Value {
    json!({ "type": "Relationship", "object": object })
}

/// The catalogue's terms a record's Dataset and a CKAN-named Dataset share.
fn describe(dataset: &mut Map<String, Value>, record: &Value, place: &Place) -> Vec<String> {
    let keywords = union(std::iter::once(record.get("dcat:keyword")), text);
    let themes = union(std::iter::once(record.get("dcat:theme")), iri);
    let spatial = union(std::iter::once(record.get("dct:spatial")), iri);
    list(dataset, "keyword", &keywords);
    list(dataset, "theme", &themes);
    list(dataset, "spatial", &spatial);
    located(dataset, place);
    let mut words = Vec::new();
    for term in ["dct:title", "dct:description"] {
        if let Some(map) = language_map(record.get(term)) {
            words.extend(map.values().filter_map(Value::as_str).map(str::to_owned));
        }
    }
    words.extend(keywords);
    words.push(place.space.clone());
    words
}

/// The Dataset of an Endpoint no CKAN dataset names, from its record alone.
fn own_dataset(slug: &str, record: &Value, place: &Place) -> Value {
    let mut dataset = Map::new();
    dataset.insert("id".into(), json!(format!("urn:ngsi-ld:Dataset:{slug}")));
    dataset.insert("type".into(), json!("Dataset"));
    languages(&mut dataset, "title", record.get("dct:title"));
    languages(&mut dataset, "description", record.get("dct:description"));
    let words = describe(&mut dataset, record, place);
    if let Some(temporal) = record.get("dct:temporal") {
        let period: Map<String, Value> =
            [("startDate", "dcat:startDate"), ("endDate", "dcat:endDate")]
                .into_iter()
                .filter_map(|(key, term)| Some((key.to_owned(), json!(text(temporal.get(term)?)?))))
                .collect();
        if !period.is_empty() {
            dataset.insert("temporal".into(), property(Value::Object(period)));
        }
    }
    if let Some(frequency) = record.get("dct:accrualPeriodicity").and_then(iri) {
        dataset.insert("frequency".into(), property(json!(frequency)));
    }
    // The record's publisher, which no CKAN organization stands for here.
    if let Some(publisher) = record.get("dct:publisher") {
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
    dataset.insert(
        "refDataService".into(),
        relationship(&format!("urn:ngsi-ld:DataService:{slug}")),
    );
    dataset.insert("searchText".into(), property(json!(fold(&words.join(" ")))));
    Value::Object(dataset)
}

/// A public CKAN dataset as the catalogue's Dataset.
fn ckan_dataset(package: &Package, record: &Value, place: &Place, site: &str) -> Value {
    let mut dataset = Map::new();
    dataset.insert(
        "id".into(),
        json!(format!("urn:ngsi-ld:Dataset:{}", package.name)),
    );
    dataset.insert("type".into(), json!("Dataset"));
    dataset.insert("name".into(), property(json!(package.title)));
    if !package.notes.is_empty() {
        dataset.insert("description".into(), property(json!(package.notes)));
    }
    let mut words = vec![
        package.name.clone(),
        package.title.clone(),
        package.notes.clone(),
    ];
    // The Endpoint's own dataset carries the record's titles per language; a harvested one
    // over a shared Endpoint does not, since the record names the Endpoint, not it.
    let own = package.extras.get("identifier") == Some(&package.slug);
    if own {
        languages(&mut dataset, "title", record.get("dct:title"));
        if let Some(map) = language_map(record.get("dct:title")) {
            words.extend(map.values().filter_map(Value::as_str).map(str::to_owned));
        }
    }
    dataset.insert("identifier".into(), property(json!(package.name)));
    dataset.insert(
        "url".into(),
        property(json!(format!("{site}/dataset/{}", package.name))),
    );
    list(&mut dataset, "keyword", &package.tags);
    words.extend(package.tags.iter().cloned());
    let themes: Vec<String> = package
        .extras
        .get("theme")
        .and_then(|themes| serde_json::from_str::<Vec<String>>(themes).ok())
        .unwrap_or_default();
    list(&mut dataset, "theme", &themes);
    if let Some(spatial) = package.extras.get("spatial_uri") {
        list(&mut dataset, "spatial", std::slice::from_ref(spatial));
    }
    if let Some(license) = &package.license_url {
        dataset.insert("license".into(), property(json!(license)));
    }
    for (key, extra, fallback) in [
        ("issued", "issued", &package.created),
        ("modified", "modified", &package.modified),
    ] {
        if let Some(date) = package.extras.get(extra).or(fallback.as_ref()) {
            dataset.insert(key.into(), property(json!(date)));
        }
    }
    if let Some(source) = package.extras.get("source_url") {
        dataset.insert("source".into(), property(json!(source)));
    }
    located(&mut dataset, place);
    if let Some(org) = &package.organization {
        dataset.insert(
            "refPublisher".into(),
            relationship(&format!("urn:ngsi-ld:Organization:{}", org.name)),
        );
        words.push(org.title.clone());
        words.push(org.name.clone());
    }
    dataset.insert(
        "refDataService".into(),
        relationship(&format!("urn:ngsi-ld:DataService:{}", package.slug)),
    );
    if let Some((kind, q)) = &package.rows {
        let mut arguments = json!({ "endpoint": package.slug, "type": kind, "limit": 10 });
        if let Some(q) = q {
            arguments["q"] = json!(q);
        }
        dataset.insert(
            "nextQuery".into(),
            property(json!({ "tool": "query_entities", "arguments": arguments })),
        );
    }
    words.push(place.space.clone());
    dataset.insert("searchText".into(), property(json!(fold(&words.join(" ")))));
    Value::Object(dataset)
}

/// A CKAN organization as the catalogue's Organization; [`link`] counts its Datasets.
fn organization(org: &Publisher, site: &str) -> Value {
    json!({
        "id": format!("urn:ngsi-ld:Organization:{}", org.name),
        "type": "Organization",
        "name": property(json!(org.title)),
        "identifier": property(json!(org.name)),
        "url": property(json!(format!("{site}/organization/{}", org.name))),
        "searchText": property(json!(fold(&format!("{} {}", org.name, org.title)))),
    })
}

/// One Endpoint as a `DataService`, and its id.
fn data_service(
    slug: &str,
    record: &Value,
    service: Option<&Value>,
    place: &Place,
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
    const SITE: &str = "https://opendata.city.example";

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

    fn feed() -> Vec<Value> {
        [
            record(
                "air",
                (
                    "Air quality in Banská Bystrica",
                    "Ovzdušie v Banskej Bystrici",
                ),
            ),
            record("regions", ("Regional open data", "Otvorené dáta kraja")),
            record("inner", ("Internal registry", "Interný register")),
            record("gone", ("Gone", "Preč")),
        ]
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
            ("regions".to_owned(), Place::public("kraj-data", None)),
        ])
    }

    /// A dataset of `package_search`, published from the `air` Endpoint by the publisher.
    fn published() -> Value {
        json!({
            "name": "mesto-ovzdusie", "title": "Ovzdušie mesta", "notes": "Hodinové merania.",
            "private": false, "state": "active",
            "organization": { "name": "banskabystrica", "title": "Mesto Banská Bystrica" },
            "tags": [{ "name": "ovzdušie" }, { "name": "PM10" }],
            "license_url": "http://publications.europa.eu/resource/authority/licence/CC_BY_4_0",
            "metadata_created": "2026-09-28T00:00:00", "metadata_modified": "2026-10-19T10:00:00",
            "extras": [
                { "key": "identifier", "value": "air" },
                { "key": "issued", "value": "2026-09-28T00:00:00Z" },
                { "key": "theme", "value": "[\"http://publications.europa.eu/resource/authority/data-theme/ENVI\"]" },
                { "key": "spatial_uri", "value": "http://data.europa.eu/nuts/code/SK032" },
            ],
            "resources": [
                { "url": format!("{HOST}/api/endpoint/air/ngsi-ld/v1/") },
                { "url": format!("{HOST}/api/endpoint/air/file.csv") },
            ],
        })
    }

    /// A harvested dataset: the rows of one type of the shared `regions` Endpoint.
    fn harvested(name: &str, kind: &str) -> Value {
        let q = format!("isPartOf==\"{SITE}/dataset/{name}\"");
        let encoded: String = form_urlencoded::byte_serialize(q.as_bytes()).collect();
        json!({
            "name": name, "title": format!("{kind} v kraji"), "notes": "",
            "private": false, "state": "active",
            "organization": { "name": "bbsk", "title": "Banskobystrický samosprávny kraj" },
            "tags": [],
            "extras": [{ "key": "source_url", "value": format!("https://opendata.bbsk.sk/{name}") }],
            "resources": [
                { "url": format!("{HOST}/api/endpoint/regions/ngsi-ld/v1/entities?type={kind}&q={encoded}") },
                { "url": format!("{HOST}/api/endpoint/regions/mcp") },
            ],
        })
    }

    fn packages(values: &[Value]) -> Vec<Package> {
        values.iter().filter_map(Package::from_ckan).collect()
    }

    fn city() -> Vec<Package> {
        packages(&[
            published(),
            harvested("bbsk-skoly", "School"),
            harvested("bbsk-lekarne", "Pharmacy"),
        ])
    }

    fn public_catalogue(packages: &[Package]) -> BTreeMap<String, Value> {
        entities(
            &feed(),
            &places(),
            Some(Ckan {
                site: SITE,
                packages,
            }),
        )
    }

    /// Every Relationship of the catalogue points at an entity of it.
    fn assert_linked(catalog: &BTreeMap<String, Value>) {
        for (id, entity) in catalog {
            for (key, value) in entity.as_object().expect("an entity") {
                let targets: Vec<&str> = match value["type"].as_str() {
                    Some("Relationship") => value["object"].as_str().into_iter().collect(),
                    Some("ListRelationship") => value["objectList"]
                        .as_array()
                        .expect("a list")
                        .iter()
                        .filter_map(|item| item["object"].as_str())
                        .collect(),
                    _ => continue,
                };
                for target in targets {
                    assert!(
                        catalog.contains_key(target),
                        "{id}.{key} -> {target} dangles"
                    );
                }
            }
        }
    }

    #[test]
    fn a_dataset_per_ckan_dataset_and_an_organization_per_publisher() {
        let built = public_catalogue(&city());
        let ids: Vec<&str> = built.keys().map(String::as_str).collect();
        assert_eq!(
            ids,
            [
                "urn:ngsi-ld:DataService:air",
                "urn:ngsi-ld:DataService:regions",
                "urn:ngsi-ld:Dataset:bbsk-lekarne",
                "urn:ngsi-ld:Dataset:bbsk-skoly",
                "urn:ngsi-ld:Dataset:mesto-ovzdusie",
                "urn:ngsi-ld:Organization:banskabystrica",
                "urn:ngsi-ld:Organization:bbsk",
            ]
        );
        assert_linked(&built);
        // A record whose slug has no place is not in the catalogue, not even by a word.
        let text = serde_json::to_string(&built).expect("json");
        assert!(
            !text.contains("registry") && !text.contains("Preč"),
            "{text}"
        );

        let bbsk = &built["urn:ngsi-ld:Organization:bbsk"];
        assert_eq!(bbsk["datasetCount"]["value"], 2);
        assert_eq!(bbsk["name"]["value"], "Banskobystrický samosprávny kraj");
        assert_eq!(bbsk["identifier"]["value"], "bbsk");
        assert_eq!(bbsk["url"]["value"], format!("{SITE}/organization/bbsk"));
        assert!(bbsk["searchText"]["value"]
            .as_str()
            .expect("text")
            .contains("banskobystricky"));
        assert_eq!(
            built["urn:ngsi-ld:Organization:banskabystrica"]["datasetCount"]["value"],
            1
        );

        // Both harvested datasets read the shared Endpoint, each narrowed to its own rows.
        let regions = &built["urn:ngsi-ld:DataService:regions"];
        assert_eq!(
            regions["refDataset"],
            json!({ "type": "ListRelationship", "objectList": [
                { "object": "urn:ngsi-ld:Dataset:bbsk-lekarne" },
                { "object": "urn:ngsi-ld:Dataset:bbsk-skoly" },
            ]})
        );
        let schools = &built["urn:ngsi-ld:Dataset:bbsk-skoly"];
        assert_eq!(
            schools["refPublisher"]["object"],
            "urn:ngsi-ld:Organization:bbsk"
        );
        assert_eq!(
            schools["refDataService"]["object"],
            "urn:ngsi-ld:DataService:regions"
        );
        assert_eq!(
            schools["nextQuery"]["value"],
            json!({ "tool": "query_entities", "arguments": {
                "endpoint": "regions", "type": "School", "limit": 10,
                "q": format!("isPartOf==\"{SITE}/dataset/bbsk-skoly\""),
            }})
        );
        assert_eq!(
            schools["source"]["value"],
            "https://opendata.bbsk.sk/bbsk-skoly"
        );
        assert!(
            schools.get("title").is_none(),
            "the record names the Endpoint, not this dataset"
        );
        assert!(
            schools.get("description").is_none(),
            "no notes, no empty description"
        );
        assert!(schools.get("location").is_none());
    }

    #[test]
    fn a_published_dataset_carries_ckan_and_its_record() {
        let built = public_catalogue(&city());
        let air = &built["urn:ngsi-ld:Dataset:mesto-ovzdusie"];
        assert_eq!(air["name"]["value"], "Ovzdušie mesta");
        assert_eq!(air["description"]["value"], "Hodinové merania.");
        assert_eq!(
            air["title"]["languageMap"]["en"],
            "Air quality in Banská Bystrica"
        );
        assert_eq!(air["identifier"]["value"], "mesto-ovzdusie");
        assert_eq!(
            air["url"]["value"],
            format!("{SITE}/dataset/mesto-ovzdusie")
        );
        assert_eq!(air["keyword"]["value"], json!(["ovzdušie", "PM10"]));
        assert_eq!(
            air["theme"]["value"],
            json!(["http://publications.europa.eu/resource/authority/data-theme/ENVI"])
        );
        assert_eq!(
            air["spatial"]["value"],
            json!(["http://data.europa.eu/nuts/code/SK032"])
        );
        assert_eq!(
            air["license"]["value"],
            "http://publications.europa.eu/resource/authority/licence/CC_BY_4_0"
        );
        assert_eq!(
            air["issued"]["value"], "2026-09-28T00:00:00Z",
            "the extra first"
        );
        assert_eq!(
            air["modified"]["value"], "2026-10-19T10:00:00",
            "then CKAN's own"
        );
        assert_eq!(
            air["location"]["value"]["coordinates"][0][2],
            json!([19.25, 48.8])
        );
        assert!(
            air.get("nextQuery").is_none(),
            "the whole Endpoint: its DataService says how"
        );
        let search = air["searchText"]["value"].as_str().expect("text");
        for word in ["ovzdusie mesta", "banska bystrica", "air quality", "pm10"] {
            assert!(search.contains(word), "{word} in {search}");
        }
        assert!(!search.contains('š'), "{search}");

        let service = &built["urn:ngsi-ld:DataService:air"];
        assert_eq!(
            service["title"]["languageMap"]["sk"],
            "Ovzdušie v Banskej Bystrici"
        );
        assert_eq!(service["identifier"]["value"], "air");
        assert_eq!(
            service["endpointURL"]["value"],
            format!("{HOST}/api/endpoint/air/")
        );
        assert_eq!(
            service["accessURL"]["value"]
                .as_array()
                .expect("paths")
                .len(),
            1
        );
        assert_eq!(service["accessLevel"]["value"], "public");
        assert_eq!(service["visibleBecause"]["value"], "public");
        assert_eq!(
            service["refDataset"]["objectList"],
            json!([{ "object": "urn:ngsi-ld:Dataset:mesto-ovzdusie" }])
        );
        assert!(service.get("servesDataset").is_none());
    }

    #[test]
    fn a_withdrawn_dataset_and_a_publisher_left_without_one_leave_nothing() {
        // bbsk withdrew both of its datasets: neither they nor the organization remain.
        let built = public_catalogue(&packages(&[published()]));
        assert!(!built.contains_key("urn:ngsi-ld:Organization:bbsk"));
        assert!(!built.keys().any(|id| id.contains("bbsk-")));
        // The shared Endpoint is a dataset of its own again, as before bbsk named it.
        assert_eq!(
            built["urn:ngsi-ld:DataService:regions"]["refDataset"]["objectList"],
            json!([{ "object": "urn:ngsi-ld:Dataset:regions" }])
        );
        assert_linked(&built);
    }

    #[test]
    fn a_ckan_dataset_whose_endpoint_is_not_here_is_left_out() {
        let mut elsewhere = harvested("bbsk-mosty", "Bridge");
        elsewhere["resources"] = json!([{ "url": format!("{HOST}/api/endpoint/gone/mcp") }]);
        let built = public_catalogue(&packages(&[elsewhere]));
        assert!(!built.contains_key("urn:ngsi-ld:Dataset:bbsk-mosty"));
        assert!(
            !built.contains_key("urn:ngsi-ld:Organization:bbsk"),
            "no count of it either"
        );
        assert_linked(&built);
    }

    #[test]
    fn only_a_public_dataset_naming_one_endpoint_is_read() {
        let mut private = published();
        private["private"] = json!(true);
        let mut deleted = published();
        deleted["state"] = json!("deleted");
        let mut two = published();
        two["resources"] = json!([
            { "url": format!("{HOST}/api/endpoint/air/ngsi-ld/v1/") },
            { "url": format!("{HOST}/api/endpoint/regions/file.csv") },
        ]);
        let mut none = published();
        none["resources"] = json!([{ "url": "https://elsewhere.example/data.csv" }]);
        let mut unsaid = published();
        unsaid.as_object_mut().expect("object").remove("private");
        for package in [private, deleted, two, none, unsaid] {
            assert_eq!(Package::from_ckan(&package), None, "{package}");
        }
        let read = Package::from_ckan(&published()).expect("a public dataset");
        assert_eq!(read.slug, "air");
        assert_eq!(read.rows, None);
        let mut untitled = published();
        untitled["organization"]["title"] = json!(" ");
        untitled["title"] = json!("");
        let read = Package::from_ckan(&untitled).expect("read");
        assert_eq!(read.title, "mesto-ovzdusie");
        assert_eq!(read.organization.expect("org").title, "banskabystrica");
    }

    #[test]
    fn a_non_public_endpoint_is_its_own_dataset_and_counts_for_no_organization() {
        let mut places = places();
        places.insert(
            "inner".to_owned(),
            Place {
                space: "mesto-register".to_owned(),
                bbox: None,
                access_level: "organization".to_owned(),
                visible_because: "grant".to_owned(),
            },
        );
        // Even a CKAN dataset naming it (a mistake upstream) does not make it public here.
        let mut leaked = harvested("bbsk-interne", "Secret");
        leaked["resources"] = json!([{ "url": format!("{HOST}/api/endpoint/inner/mcp") }]);
        let mut with_leak = city();
        with_leak.extend(Package::from_ckan(&leaked));
        let built = entities(
            &feed(),
            &places,
            Some(Ckan {
                site: SITE,
                packages: &with_leak,
            }),
        );
        assert!(!built.contains_key("urn:ngsi-ld:Dataset:bbsk-interne"));
        let inner = &built["urn:ngsi-ld:Dataset:inner"];
        assert_eq!(inner["title"]["languageMap"]["en"], "Internal registry");
        assert!(inner.get("refPublisher").is_none());
        assert_eq!(
            inner["refDataService"]["object"],
            "urn:ngsi-ld:DataService:inner"
        );
        assert_eq!(
            inner["temporal"]["value"],
            json!({ "startDate": "2020-01-01" })
        );
        let service = &built["urn:ngsi-ld:DataService:inner"];
        assert_eq!(service["accessLevel"]["value"], "organization");
        assert_eq!(service["visibleBecause"]["value"], "grant");
        assert_eq!(
            built["urn:ngsi-ld:Organization:bbsk"]["datasetCount"]["value"],
            2
        );
        assert_linked(&built);
    }

    #[test]
    fn the_hub_merging_the_public_datasets_into_its_own_build_is_the_public_catalogue() {
        let public = public_catalogue(&city());
        // The hub builds the DataServices from the records it reaches, then takes the public
        // Datasets and Organizations the Portal wrote.
        let mut hub = entities(&feed(), &places(), None);
        let ids: Vec<&str> = hub.keys().map(String::as_str).collect();
        assert_eq!(
            ids,
            [
                "urn:ngsi-ld:DataService:air",
                "urn:ngsi-ld:DataService:regions",
                "urn:ngsi-ld:Dataset:air",
                "urn:ngsi-ld:Dataset:regions",
            ],
            "before the public catalogue is read, each Endpoint is a dataset of its own"
        );
        hub.extend(
            public
                .iter()
                .filter(|(_, entity)| {
                    entity["type"] == "Dataset" || entity["type"] == "Organization"
                })
                .map(|(id, entity)| (id.clone(), entity.clone())),
        );
        link(&mut hub);
        assert_eq!(hub, public);

        // A caller that reaches only `air` keeps only what reaches it, counted again.
        let mut narrow = entities(
            &feed(),
            &BTreeMap::from([("air".to_owned(), places()["air"].clone())]),
            None,
        );
        narrow.extend(
            public
                .iter()
                .filter(|(_, entity)| entity["type"] != "DataService")
                .map(|(id, entity)| (id.clone(), entity.clone())),
        );
        link(&mut narrow);
        let ids: Vec<&str> = narrow.keys().map(String::as_str).collect();
        assert_eq!(
            ids,
            [
                "urn:ngsi-ld:DataService:air",
                "urn:ngsi-ld:Dataset:mesto-ovzdusie",
                "urn:ngsi-ld:Organization:banskabystrica",
            ]
        );
        assert_linked(&narrow);
    }

    #[test]
    fn an_endpoint_no_ckan_dataset_names_stays_a_dataset_of_its_own() {
        // Only the harvested datasets are in CKAN: `air` is still found, by its record.
        let built = public_catalogue(&packages(&[harvested("bbsk-skoly", "School")]));
        let air = &built["urn:ngsi-ld:Dataset:air"];
        assert_eq!(
            air["title"]["languageMap"]["sk"],
            "Ovzdušie v Banskej Bystrici"
        );
        assert_eq!(
            air["refDataService"]["object"],
            "urn:ngsi-ld:DataService:air"
        );
        assert_eq!(air["keyword"]["value"], json!(["air", "ovzdušie"]));
        assert_eq!(
            air["location"]["value"]["coordinates"][0][0],
            json!([19.05, 48.68])
        );
        assert!(
            air.get("refPublisher").is_none(),
            "no CKAN organization stands for it"
        );
        assert_eq!(
            air["publisher"]["value"]["name"]["sk"],
            "Mesto Banská Bystrica"
        );
        assert!(air["searchText"]["value"]
            .as_str()
            .expect("text")
            .contains("mesto-ovzdusie"));
        assert!(
            !built.contains_key("urn:ngsi-ld:Dataset:regions"),
            "named by bbsk-skoly"
        );
        assert_eq!(
            built["urn:ngsi-ld:DataService:air"]["refDataset"]["objectList"],
            json!([{ "object": "urn:ngsi-ld:Dataset:air" }])
        );
        assert_linked(&built);
    }

    #[test]
    fn nothing_readable_is_an_empty_catalogue() {
        let packages = city();
        let ckan = Some(Ckan {
            site: SITE,
            packages: &packages,
        });
        assert!(entities(&feed(), &BTreeMap::new(), ckan).is_empty());
        assert!(entities(&[], &places(), ckan).is_empty());
        let mut empty = BTreeMap::new();
        link(&mut empty);
        assert!(empty.is_empty());
    }

    /// A CKAN dataset named like another Endpoint's slug takes that id: the published dataset
    /// wins over the Endpoint's own, whose DataService stays without a `refDataset`.
    #[test]
    fn a_ckan_name_equal_to_another_slug_wins_and_nothing_dangles() {
        let packages = packages(&[harvested("air", "School")]);
        let built = public_catalogue(&packages);
        let ids: Vec<&str> = built.keys().map(String::as_str).collect();
        assert_eq!(
            ids,
            [
                "urn:ngsi-ld:DataService:air",
                "urn:ngsi-ld:DataService:regions",
                "urn:ngsi-ld:Dataset:air",
                "urn:ngsi-ld:Organization:bbsk",
            ]
        );
        assert_linked(&built);
        let air = &built["urn:ngsi-ld:Dataset:air"];
        assert_eq!(
            air["refDataService"]["object"],
            "urn:ngsi-ld:DataService:regions"
        );
        assert_eq!(
            air["refPublisher"]["object"],
            "urn:ngsi-ld:Organization:bbsk"
        );
        assert!(built["urn:ngsi-ld:DataService:air"]
            .get("refDataset")
            .is_none());
        assert_eq!(
            built["urn:ngsi-ld:DataService:regions"]["refDataset"]["objectList"],
            json!([{ "object": "urn:ngsi-ld:Dataset:air" }])
        );
        assert_eq!(
            built["urn:ngsi-ld:Organization:bbsk"]["datasetCount"]["value"],
            1
        );

        // The hub's merge of the public catalogue over a caller's own keeps the same rule.
        let mut merged = entities(&feed(), &places(), None);
        merged.extend(built.clone());
        link(&mut merged);
        assert_eq!(merged, built);
    }

    #[test]
    fn folding_drops_diacritics_and_case() {
        assert_eq!(
            fold("Banská Bystrica, ŽILINA, Hämeenlinna"),
            "banska bystrica, zilina, hameenlinna"
        );
    }
}
