//! The national renderings of the record (docs API/08 section 5, T-4499).
//!
//! A profile never writes a record of its own: it takes the DCAT-AP 3.0 record of
//! [`crate::dataset`] and adds or retypes what its national text asks for, so the core values
//! (title, publisher, themes, licence, distributions) are the same under every profile. What a
//! profile requires and the Endpoint does not declare is left out, never invented.

#[cfg(any(feature = "dcat-ap-sk", feature = "dcat-ap-cz"))]
use jc_core::kinds::PersonalData;
use jc_core::kinds::{Catalog, DcatProfile};
use serde_json::{json, Map, Value};

/// A rendering of the record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Profile {
    /// DCAT-AP 3.0.0, the record as written: the default.
    DcatAp,
    /// DCAT-AP-SK 3.0.0 over DCAT-AP 3.0.0 (API/08 section 2).
    #[cfg(feature = "dcat-ap-sk")]
    DcatApSk,
    /// DCAT-AP-CZ 2026-09-23 over DCAT-AP 3.0.0 (API/08 section 3).
    #[cfg(feature = "dcat-ap-cz")]
    DcatApCz,
}

impl Profile {
    /// The profile `id` names, as `?profile=` and `Organization.spec.dcatProfiles` spell it;
    /// `None` for an id this build does not carry.
    pub fn parse(id: &str) -> Option<Self> {
        match id {
            "dcat-ap" => Some(Self::DcatAp),
            #[cfg(feature = "dcat-ap-sk")]
            "dcat-ap-sk" => Some(Self::DcatApSk),
            #[cfg(feature = "dcat-ap-cz")]
            "dcat-ap-cz" => Some(Self::DcatApCz),
            _ => None,
        }
    }
}

/// The profiles an Organization serves its records in: DCAT-AP always, and those of its
/// `spec.dcatProfiles` this build carries.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Offered(u8);

impl Offered {
    const SK: u8 = 1;
    const CZ: u8 = 2;

    /// What `listed`, an Organization's `spec.dcatProfiles`, offers.
    pub fn of(listed: &[DcatProfile]) -> Self {
        Self(listed.iter().fold(0, |bits, profile| {
            bits | match profile {
                DcatProfile::DcatApSk => Self::SK,
                DcatProfile::DcatApCz => Self::CZ,
            }
        }))
    }

    /// The set as one byte, to keep in an atomic.
    pub const fn bits(self) -> u8 {
        self.0
    }

    /// The set [`Self::bits`] wrote.
    pub const fn from_bits(bits: u8) -> Self {
        Self(bits)
    }

    /// The profile `id` names, when this build carries it and the Organization serves it.
    pub fn pick(self, id: &str) -> Option<Profile> {
        let profile = Profile::parse(id)?;
        let served = match profile {
            Profile::DcatAp => true,
            #[cfg(feature = "dcat-ap-sk")]
            Profile::DcatApSk => self.0 & Self::SK != 0,
            #[cfg(feature = "dcat-ap-cz")]
            Profile::DcatApCz => self.0 & Self::CZ != 0,
        };
        served.then_some(profile)
    }
}

/// `record`, a [`crate::dataset`] of the Endpoint whose catalogue block is `catalog`, in
/// `profile`.
pub fn render(record: Value, profile: Profile, catalog: Option<&Catalog>) -> Value {
    #[cfg_attr(
        not(any(feature = "dcat-ap-sk", feature = "dcat-ap-cz")),
        allow(unused_variables)
    )]
    let licence = catalog.and_then(|c| c.license).map(|l| l.iri());
    #[cfg_attr(
        not(any(feature = "dcat-ap-sk", feature = "dcat-ap-cz")),
        allow(unused_variables)
    )]
    let personal = catalog.and_then(|c| c.personal_data);
    match profile {
        Profile::DcatAp => record,
        #[cfg(feature = "dcat-ap-sk")]
        Profile::DcatApSk => slovak(record, licence.as_deref(), personal),
        #[cfg(feature = "dcat-ap-cz")]
        Profile::DcatApCz => czech(record, licence.as_deref(), personal),
    }
}

/// Each distribution of the record, to change in place.
#[cfg_attr(
    not(any(feature = "dcat-ap-sk", feature = "dcat-ap-cz")),
    allow(dead_code)
)]
fn distributions(record: &mut Value) -> impl Iterator<Item = &mut Map<String, Value>> {
    record
        .get_mut("dcat:distribution")
        .and_then(Value::as_array_mut)
        .into_iter()
        .flatten()
        .filter_map(Value::as_object_mut)
}

/// A terms-of-use node: the three rights, each the Endpoint's EU licence, because the licence
/// the owner chose covers the work, the database as a work and the database maker's right
/// (API/08 section 6); and the personal-data statement, `(key, code)`, which no licence implies
/// and the Endpoint states itself (API/08 section 5). `None` when neither is known.
#[cfg_attr(
    not(any(feature = "dcat-ap-sk", feature = "dcat-ap-cz")),
    allow(dead_code)
)]
fn terms(
    class: &str,
    rights: [&str; 3],
    licence: Option<&str>,
    personal: Option<(&str, &str)>,
) -> Option<Value> {
    let mut node = Map::new();
    if let Some(licence) = licence {
        for key in rights {
            node.insert(key.to_owned(), json!({ "@id": licence }));
        }
    }
    if let Some((key, code)) = personal {
        node.insert(key.to_owned(), json!({ "@id": code }));
    }
    if node.is_empty() {
        return None;
    }
    node.insert("@type".to_owned(), json!(class));
    Some(Value::Object(node))
}

/// The Slovak code for a personal-data statement, from the code list
/// `https://data.gov.sk/set/codelist/personal-data-occurence-type` (API/08 section 5).
#[cfg(feature = "dcat-ap-sk")]
fn slovak_personal(statement: PersonalData) -> &'static str {
    match statement {
        PersonalData::Contains => "https://data.gov.sk/def/personal-data-occurence-type/1",
        PersonalData::DoesNotContain => "https://data.gov.sk/def/personal-data-occurence-type/2",
    }
}

/// The Czech statement IRI of the terms-of-use vocabulary (API/08 section 5).
#[cfg(feature = "dcat-ap-cz")]
fn czech_personal(statement: PersonalData) -> &'static str {
    match statement {
        PersonalData::Contains => "https://data.gov.cz/podmínky-užití/obsahuje-osobní-údaje/",
        PersonalData::DoesNotContain => {
            "https://data.gov.cz/podmínky-užití/neobsahuje-osobní-údaje/"
        }
    }
}

/// A plain string under `key` as a text in `language`: what the gateway names itself, a
/// representation's or an artifact's title or a desk's name, is one string, and the Slovak shapes
/// ask for a language-tagged one, the Czech shapes for a `rdf:langString` (API/08 section 5).
#[cfg(any(feature = "dcat-ap-sk", feature = "dcat-ap-cz"))]
fn tag(node: &mut Map<String, Value>, key: &str, language: &str) {
    if let Some(Value::String(text)) = node.get(key) {
        let tagged = json!([{ "@value": text, "@language": language }]);
        node.insert(key.to_owned(), tagged);
    }
}

/// The EU file-type code of a file whose record names only its media type: the schema artifacts.
#[cfg(feature = "dcat-ap-sk")]
fn file_type_of(media_type: &str) -> Option<&'static str> {
    let bare = media_type
        .rsplit("/media-types/")
        .next()
        .unwrap_or(media_type);
    Some(match bare {
        "text/turtle" => "RDF_TURTLE",
        "application/ld+json" => "JSON_LD",
        "application/json" | "application/schema+json" => "JSON",
        "application/geo+json" => "GEOJSON",
        "text/yaml" | "application/yaml" => "YAML",
        "text/markdown" => "MARKDOWN",
        "text/csv" => "CSV",
        _ => return None,
    })
}

/// DCAT-AP-SK 3.0.0 (API/08 sections 2 and 5): `leg:termsOfUse` on every distribution; a file is
/// a download whose access URL is its download URL, an interface has a data service of its own
/// whose endpoint is its access URL; the names the gateway writes as one string are `@sk`; the
/// contact is a `vcard:Organization`; another publisher's source dataset is no `dcat:Dataset`.
#[cfg(feature = "dcat-ap-sk")]
fn slovak(mut record: Value, licence: Option<&str>, personal: Option<PersonalData>) -> Value {
    let personal = personal.map(|p| ("leg:personalDataContainmentType", slovak_personal(p)));
    let dataset = record.get("@id").cloned().unwrap_or(Value::Null);
    let rights = record.get("dct:accessRights").cloned();
    let mut services = Vec::new();
    let mut formats = Vec::new();
    for distribution in distributions(&mut record) {
        if let Some(node) = terms(
            "leg:TermsOfUse",
            [
                "leg:authorsWorkType",
                "leg:originalDatabaseType",
                "leg:databaseProtectedBySpecialRightsType",
            ],
            licence,
            personal,
        ) {
            distribution.insert("leg:termsOfUse".to_owned(), node);
        }
        tag(distribution, "dct:title", "sk");
        let Some(url) = distribution
            .get("dcat:accessURL")
            .and_then(Value::as_str)
            .map(str::to_owned)
        else {
            continue;
        };
        let file = !distribution.contains_key("dcat:accessService")
            || url
                .rsplit('/')
                .next()
                .is_some_and(|name| name.starts_with("file."));
        if file {
            distribution.remove("dcat:accessService");
            distribution.insert("dcat:downloadURL".to_owned(), json!(url));
            if !distribution.contains_key("dct:format") {
                let code = distribution
                    .get("dcat:mediaType")
                    .and_then(Value::as_str)
                    .and_then(file_type_of);
                if let Some(code) = code {
                    let format = format!(
                        "http://publications.europa.eu/resource/authority/file-type/{code}"
                    );
                    distribution.insert("dct:format".to_owned(), json!(format));
                    formats.push(format);
                }
            }
            continue;
        }
        let iri = format!("{url}#service");
        let mut service = json!({
            "@id": iri,
            "@type": "dcat:DataService",
            "dct:title": distribution.get("dct:title").cloned().unwrap_or(Value::Null),
            "dcat:endpointURL": url,
            "dcat:servesDataset": dataset,
        });
        for key in ["dct:conformsTo", "dct:license"] {
            if let Some(value) = distribution.get(key) {
                service[key] = value.clone();
            }
        }
        if let Some(rights) = &rights {
            service["dct:accessRights"] = rights.clone();
        }
        distribution.insert("dcat:accessService".to_owned(), json!(iri));
        services.push(service);
    }
    if let Some(contact) = record
        .get_mut("dcat:contactPoint")
        .and_then(Value::as_object_mut)
    {
        tag(contact, "vcard:fn", "sk");
    }
    untype_sources(&mut record);
    if let Some(included) = record.get_mut("@included").and_then(Value::as_array_mut) {
        included.extend(services);
        for format in formats {
            let node = json!({ "@id": format, "@type": "dct:MediaTypeOrExtent" });
            if !included.contains(&node) {
                included.push(node);
            }
        }
    }
    record
}

/// Another publisher's original dataset, named in `dct:source`, stays an IRI without the
/// `dcat:Dataset` type, so a national catalogue never harvests it as this publisher's and its
/// shapes ask nothing of it (API/08 section 5).
#[cfg(any(feature = "dcat-ap-sk", feature = "dcat-ap-cz"))]
fn untype_sources(record: &mut Value) {
    let sources: Vec<Value> = record
        .get("dct:source")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if let Some(included) = record.get_mut("@included").and_then(Value::as_array_mut) {
        for node in included.iter_mut() {
            if node["@type"] == "dcat:Dataset" && sources.contains(&node["@id"]) {
                if let Some(node) = node.as_object_mut() {
                    node.remove("@type");
                }
            }
        }
    }
}

/// DCAT-AP-CZ 2026-09-23: `pu:specifikace` on every distribution, its licences typed
/// `dct:LicenseDocument`, the dataset's `r5r:applicableLegislation` repeated on each
/// distribution and on the data service, and another publisher's source no `dcat:Dataset`
/// (API/08 sections 3 and 5).
#[cfg(feature = "dcat-ap-cz")]
fn czech(mut record: Value, licence: Option<&str>, personal: Option<PersonalData>) -> Value {
    untype_sources(&mut record);
    let personal = personal.map(czech_personal);
    let legislation = record.get("dcatap:applicableLegislation").cloned();
    for distribution in distributions(&mut record) {
        if let Some(node) = terms(
            "pu:Specifikace",
            [
                "pu:autorské-dílo",
                "pu:databáze-jako-autorské-dílo",
                "pu:databáze-chráněná-zvláštními-právy",
            ],
            licence,
            personal.map(|code| ("pu:osobní-údaje", code)),
        ) {
            distribution.insert("pu:specifikace".to_owned(), node);
        }
        if let Some(legislation) = &legislation {
            distribution.insert(
                "dcatap:applicableLegislation".to_owned(),
                legislation.clone(),
            );
        }
    }
    if let Some(included) = record.get_mut("@included").and_then(Value::as_array_mut) {
        for node in included.iter_mut() {
            if let (Some(legislation), Some("dcat:DataService")) =
                (&legislation, node["@type"].as_str())
            {
                node["dcatap:applicableLegislation"] = legislation.clone();
            }
        }
        // The shapes take every right and the statement as a `dct:LicenseDocument`.
        for document in licence.into_iter().chain(personal) {
            included.push(json!({ "@id": document, "@type": "dct:LicenseDocument" }));
        }
        for node in included.iter_mut() {
            if node["@type"] == "dcat:DataService" {
                one_language(node, "dct:title");
            }
        }
    }
    for key in ["dct:title", "dct:description"] {
        one_language(&mut record, key);
    }
    for distribution in distributions(&mut record) {
        if let Some(Value::Array(texts)) = distribution.get("dct:description") {
            let kept = pick_language(texts);
            distribution.insert("dct:description".to_owned(), kept);
        }
        tag(distribution, "dct:title", "cs");
    }
    if let Some(contact) = record
        .get_mut("dcat:contactPoint")
        .and_then(Value::as_object_mut)
    {
        tag(contact, "vcard:fn", "cs");
    }
    record
}

/// The one text DCAT-AP-CZ takes where its shapes give a `rdf:langString` `sh:maxCount 1`
/// (API/08 section 3): the Czech one, else the English one, else the first.
#[cfg(feature = "dcat-ap-cz")]
fn pick_language(texts: &[Value]) -> Value {
    ["cs", "en"]
        .iter()
        .find_map(|language| texts.iter().find(|t| t["@language"] == *language))
        .or_else(|| texts.first())
        .cloned()
        .unwrap_or(Value::Null)
}

#[cfg(feature = "dcat-ap-cz")]
fn one_language(node: &mut Value, key: &str) {
    if let Some(Value::Array(texts)) = node.get(key) {
        node[key] = pick_language(texts);
    }
}

/// A publisher's catalogue of [`crate::publisher_feed`], its records already rendered in
/// `profile`, with what the profile asks of the catalogue node itself (API/08 sections 2, 3).
pub fn render_catalog(feed: Value, profile: Profile) -> Value {
    match profile {
        Profile::DcatAp => feed,
        #[cfg(feature = "dcat-ap-sk")]
        Profile::DcatApSk => {
            let mut feed = feed;
            if let Some(contact) = feed
                .get_mut("dcat:contactPoint")
                .and_then(Value::as_object_mut)
            {
                tag(contact, "vcard:fn", "sk");
            }
            feed
        }
        #[cfg(feature = "dcat-ap-cz")]
        Profile::DcatApCz => {
            let mut feed = feed;
            for key in ["dct:title", "dct:description"] {
                one_language(&mut feed, key);
            }
            if let Some(contact) = feed
                .get_mut("dcat:contactPoint")
                .and_then(Value::as_object_mut)
            {
                tag(contact, "vcard:fn", "cs");
            }
            feed
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{dataset, turtle, Subject};
    use jc_core::kinds::{Audience, Licence, PersonalData, Representation};
    use std::collections::BTreeMap;

    const BASE: &str = "https://data.example.sk";

    fn catalog(licence: Option<Licence>) -> Catalog {
        Catalog {
            license: licence,
            applicable_legislation: vec!["http://data.europa.eu/eli/reg_impl/2023/138/oj".into()],
            ..Catalog::default()
        }
    }

    fn index() -> Value {
        json!({ "models": [{ "name": "air", "version": 1, "artifacts": {
            "schema.json": { "type": "application/schema+json", "bytes": 120, "sha256": "ab12" }
        } }] })
    }

    fn record(catalog: &Catalog) -> Value {
        let title = BTreeMap::from([("sk".to_owned(), "Ovzdušie".to_owned())]);
        let none = BTreeMap::new();
        let subject = Subject {
            slug: "ovzdusie",
            space: "ovzdusie",
            base_path: "/api/endpoint/ovzdusie",
            title: &title,
            description: &none,
            space_title: None,
            space_description: None,
            audience: Audience::Public,
            representations: &[Representation::NgsiLd],
            catalog: Some(catalog),
            classes: &["AirQualityObserved"],
            assigners: &[],
        };
        dataset(&subject, &index(), BASE)
    }

    #[cfg(any(feature = "dcat-ap-sk", feature = "dcat-ap-cz"))]
    fn schema_distribution(record: &Value) -> &Value {
        record["dcat:distribution"]
            .as_array()
            .and_then(|all| all.iter().find(|d| d["dcat:byteSize"] != Value::Null))
            .expect("the schema artifact is a distribution")
    }

    #[test]
    fn an_id_this_build_does_not_carry_is_none() {
        assert_eq!(Profile::parse("dcat-ap"), Some(Profile::DcatAp));
        assert_eq!(Profile::parse("dcat-ap-xx"), None);
        assert_eq!(Profile::parse(""), None);
    }

    #[test]
    fn only_a_listed_profile_is_picked_and_the_core_always() {
        let none = Offered::default();
        assert_eq!(none.pick("dcat-ap"), Some(Profile::DcatAp));
        assert_eq!(none.pick("dcat-ap-sk"), None);
        assert_eq!(none.pick("nonsense"), None);
        let sk = Offered::of(&[DcatProfile::DcatApSk]);
        assert_eq!(Offered::from_bits(sk.bits()), sk);
        assert_eq!(sk.pick("dcat-ap-cz"), None);
        #[cfg(feature = "dcat-ap-sk")]
        assert_eq!(sk.pick("dcat-ap-sk"), Some(Profile::DcatApSk));
        #[cfg(not(feature = "dcat-ap-sk"))]
        assert_eq!(sk.pick("dcat-ap-sk"), None);
        let both = Offered::of(&[DcatProfile::DcatApCz, DcatProfile::DcatApSk]);
        #[cfg(feature = "dcat-ap-cz")]
        assert_eq!(both.pick("dcat-ap-cz"), Some(Profile::DcatApCz));
        assert_eq!(both.pick("dcat-ap"), Some(Profile::DcatAp));
    }

    #[test]
    fn the_core_rendering_is_the_record_itself() {
        let catalog = catalog(Some(Licence::CcBy4));
        let core = record(&catalog);
        assert_eq!(render(core.clone(), Profile::DcatAp, Some(&catalog)), core);
    }

    /// A record with a file, an interface, a schema artifact, a source and a desk: every shape
    /// of distribution the Slovak rendering splits.
    #[cfg(feature = "dcat-ap-sk")]
    fn full_record(catalog: &Catalog) -> Value {
        let title = BTreeMap::from([
            ("en".to_owned(), "Air".to_owned()),
            ("sk".to_owned(), "Ovzdušie".to_owned()),
        ]);
        let none = BTreeMap::new();
        let subject = Subject {
            slug: "ovzdusie",
            space: "ovzdusie",
            base_path: "/api/endpoint/ovzdusie",
            title: &title,
            description: &none,
            space_title: None,
            space_description: None,
            audience: Audience::Public,
            representations: &[Representation::NgsiLd, Representation::Csv],
            catalog: Some(catalog),
            classes: &["AirQualityObserved"],
            assigners: &[],
        };
        dataset(&subject, &index(), BASE)
    }

    #[cfg(feature = "dcat-ap-sk")]
    fn slovak_catalog() -> Catalog {
        let mut catalog = catalog(Some(Licence::CcBy4));
        catalog.contact_point = Some(jc_core::kinds::ContactPoint {
            name: "Oddelenie informatiky".into(),
            email: "opendata@example.org".into(),
        });
        catalog.source = vec![jc_core::kinds::Source {
            url: "https://data.example.sk/dataset/ovzdusie".into(),
            title: BTreeMap::from([("sk".to_owned(), "Ovzdušie".to_owned())]),
            description: BTreeMap::new(),
        }];
        catalog
    }

    #[cfg(feature = "dcat-ap-sk")]
    fn by_url<'a>(record: &'a Value, suffix: &str) -> &'a Value {
        record["dcat:distribution"]
            .as_array()
            .and_then(|all| {
                all.iter().find(|d| {
                    d["dcat:accessURL"]
                        .as_str()
                        .is_some_and(|u| u.ends_with(suffix))
                })
            })
            .unwrap_or_else(|| panic!("no distribution at {suffix}"))
    }

    #[cfg(feature = "dcat-ap-sk")]
    #[test]
    fn a_slovak_record_carries_terms_of_use_and_keeps_its_sha256() {
        let catalog = catalog(Some(Licence::CcBy4));
        let core = record(&catalog);
        let sk = render(core.clone(), Profile::DcatApSk, Some(&catalog));
        let licence = Licence::CcBy4.iri();

        // DCAT-AP-SK 3 stands on DCAT-AP 3.0: the checksum and the size stay as the core writes them.
        assert_eq!(
            schema_distribution(&sk)["spdx:checksum"],
            schema_distribution(&core)["spdx:checksum"]
        );
        assert_eq!(schema_distribution(&sk)["dcat:byteSize"], json!("120"));
        for distribution in sk["dcat:distribution"].as_array().into_iter().flatten() {
            let terms = &distribution["leg:termsOfUse"];
            assert_eq!(terms["@type"], "leg:TermsOfUse");
            for key in [
                "leg:authorsWorkType",
                "leg:originalDatabaseType",
                "leg:databaseProtectedBySpecialRightsType",
            ] {
                assert_eq!(terms[key], json!({ "@id": licence }), "{key}");
            }
            assert!(terms.get("leg:personalDataContainmentType").is_none());
        }
        // the core values do not move
        for key in [
            "@id",
            "dct:title",
            "dct:identifier",
            "dct:license",
            "dcat:theme",
        ] {
            assert_eq!(sk.get(key), core.get(key), "{key}");
        }
        assert_eq!(
            sk["dcat:distribution"].as_array().map(Vec::len),
            core["dcat:distribution"].as_array().map(Vec::len)
        );
    }

    #[cfg(feature = "dcat-ap-sk")]
    #[test]
    fn a_slovak_file_is_a_download_and_an_interface_has_its_own_service() {
        let catalog = slovak_catalog();
        let sk = render(full_record(&catalog), Profile::DcatApSk, Some(&catalog));

        for suffix in ["/file.csv", "/schema/v1/schema.json"] {
            let file = by_url(&sk, suffix);
            assert_eq!(file["dcat:downloadURL"], file["dcat:accessURL"], "{suffix}");
            assert!(file.get("dcat:accessService").is_none(), "{suffix}");
            assert!(file["dct:format"].as_str().is_some(), "{suffix}");
        }
        assert_eq!(
            by_url(&sk, "/schema/v1/schema.json")["dct:format"],
            "http://publications.europa.eu/resource/authority/file-type/JSON"
        );

        let api = by_url(&sk, "/ngsi-ld/v1/");
        assert!(api.get("dcat:downloadURL").is_none());
        let service_iri = api["dcat:accessService"].as_str().expect("its own service");
        assert_eq!(
            service_iri,
            format!(
                "{}#service",
                api["dcat:accessURL"].as_str().unwrap_or_default()
            )
        );
        let included = sk["@included"].as_array().expect("@included");
        let service = included
            .iter()
            .find(|n| n["@id"] == service_iri)
            .expect("the service node");
        assert_eq!(service["@type"], "dcat:DataService");
        assert_eq!(service["dcat:endpointURL"], api["dcat:accessURL"]);
        assert_eq!(service["dcat:servesDataset"], sk["@id"]);
        assert_eq!(
            service["dct:title"],
            json!([{ "@value": "NGSI-LD API", "@language": "sk" }])
        );
        assert_eq!(api["dct:title"], service["dct:title"]);

        // the desk is an Organization named in Slovak, the source no dataset of this publisher
        let contact = &sk["dcat:contactPoint"];
        assert_eq!(
            contact["@type"],
            json!(["vcard:Organization", "vcard:Kind"])
        );
        assert_eq!(
            contact["vcard:fn"],
            json!([{ "@value": "Oddelenie informatiky", "@language": "sk" }])
        );
        let source = included
            .iter()
            .find(|n| n["@id"] == "https://data.example.sk/dataset/ovzdusie")
            .expect("the source node");
        assert!(source.get("@type").is_none(), "{source}");

        let turtle = turtle(&sk);
        assert!(
            turtle.contains(
                "dcat:downloadURL <https://data.example.sk/api/endpoint/ovzdusie/file.csv>"
            ),
            "{turtle}"
        );
        assert!(turtle.contains("\"NGSI-LD API\"@sk"), "{turtle}");
    }

    #[cfg(feature = "dcat-ap-sk")]
    #[test]
    fn a_slovak_record_without_a_licence_names_no_terms() {
        let catalog = catalog(None);
        let sk = render(record(&catalog), Profile::DcatApSk, Some(&catalog));
        assert!(!sk.to_string().contains("leg:termsOfUse"));
        let turtle = turtle(&sk);
        assert!(
            turtle.contains("\"120\"^^xsd:nonNegativeInteger"),
            "{turtle}"
        );
    }

    /// T-4596: the Czech shapes asked a title, a publisher and a theme of every Golemio and ČHMÚ
    /// original the Prague records name, because the CZ rendering kept them `dcat:Dataset`s.
    #[cfg(feature = "dcat-ap-cz")]
    #[test]
    fn a_czech_record_names_its_source_as_no_dataset_of_this_publisher() {
        let mut catalog = catalog(Some(Licence::Cc0));
        catalog.source = vec![jc_core::kinds::Source {
            url: "https://data.example.sk/dataset/ovzdusie".into(),
            title: BTreeMap::from([("cs".to_owned(), "Ovzduší".to_owned())]),
            description: BTreeMap::from([("cs".to_owned(), "Hodinové průměry".to_owned())]),
        }];
        let core = record(&catalog);
        let typed = |record: &Value| {
            record["@included"]
                .as_array()
                .expect("@included")
                .iter()
                .find(|n| n["@id"] == "https://data.example.sk/dataset/ovzdusie")
                .expect("the source node")
                .get("@type")
                .cloned()
        };
        assert_eq!(typed(&core), Some(json!("dcat:Dataset")));
        let cz = render(core, Profile::DcatApCz, Some(&catalog));
        assert_eq!(typed(&cz), None, "{cz}");
        assert_eq!(
            cz["dct:source"],
            json!(["https://data.example.sk/dataset/ovzdusie"])
        );
    }

    #[cfg(feature = "dcat-ap-cz")]
    #[test]
    fn a_czech_record_carries_the_specification_and_the_legislation_everywhere() {
        let catalog = catalog(Some(Licence::Cc0));
        let core = record(&catalog);
        let cz = render(core.clone(), Profile::DcatApCz, Some(&catalog));
        let licence = Licence::Cc0.iri();
        let legislation = core["dcatap:applicableLegislation"].clone();
        assert_ne!(legislation, Value::Null);

        for distribution in cz["dcat:distribution"].as_array().into_iter().flatten() {
            let terms = &distribution["pu:specifikace"];
            assert_eq!(terms["@type"], "pu:Specifikace");
            assert_eq!(terms["pu:autorské-dílo"], json!({ "@id": licence }));
            assert!(terms.get("pu:osobní-údaje").is_none());
            assert_eq!(distribution["dcatap:applicableLegislation"], legislation);
        }
        let included = cz["@included"].as_array().expect("@included");
        let service = included
            .iter()
            .find(|n| n["@type"] == "dcat:DataService")
            .expect("the data service");
        assert_eq!(service["dcatap:applicableLegislation"], legislation);
        assert!(included
            .iter()
            .any(|n| n["@id"] == json!(licence) && n["@type"] == "dct:LicenseDocument"));
        // the SHA-256 checksum stays: the CZ base is DCAT-AP 3.0
        assert!(schema_distribution(&cz).get("spdx:checksum").is_some());

        let turtle = turtle(&cz);
        assert!(
            turtle.contains("<https://data.gov.cz/slovník/podmínky-užití/autorské-dílo>"),
            "{turtle}"
        );
        assert!(turtle.contains("a pu:Specifikace"), "{turtle}");
    }

    #[cfg(feature = "dcat-ap-sk")]
    #[test]
    fn a_slovak_statement_is_its_code_and_stands_without_a_licence() {
        let mut licensed = catalog(Some(Licence::CcBy4));
        licensed.personal_data = Some(PersonalData::Contains);
        let sk = render(record(&licensed), Profile::DcatApSk, Some(&licensed));
        let terms = &schema_distribution(&sk)["leg:termsOfUse"];
        assert_eq!(
            terms["leg:personalDataContainmentType"],
            json!({ "@id": "https://data.gov.sk/def/personal-data-occurence-type/1" })
        );
        assert_eq!(
            terms["leg:authorsWorkType"],
            json!({ "@id": Licence::CcBy4.iri() })
        );

        let mut unlicensed = catalog(None);
        unlicensed.personal_data = Some(PersonalData::DoesNotContain);
        let sk = render(record(&unlicensed), Profile::DcatApSk, Some(&unlicensed));
        let terms = &schema_distribution(&sk)["leg:termsOfUse"];
        assert_eq!(
            terms,
            &json!({
                "@type": "leg:TermsOfUse",
                "leg:personalDataContainmentType":
                    { "@id": "https://data.gov.sk/def/personal-data-occurence-type/2" }
            })
        );
        let turtle = turtle(&sk);
        assert!(
            turtle.contains("<https://data.gov.sk/def/personal-data-occurence-type/2>"),
            "{turtle}"
        );
    }

    #[cfg(feature = "dcat-ap-cz")]
    #[test]
    fn a_czech_statement_is_a_licence_document_of_the_vocabulary() {
        for (statement, code) in [
            (
                PersonalData::Contains,
                "https://data.gov.cz/podmínky-užití/obsahuje-osobní-údaje/",
            ),
            (
                PersonalData::DoesNotContain,
                "https://data.gov.cz/podmínky-užití/neobsahuje-osobní-údaje/",
            ),
        ] {
            let mut stated = catalog(Some(Licence::Cc0));
            stated.personal_data = Some(statement);
            let cz = render(record(&stated), Profile::DcatApCz, Some(&stated));
            for distribution in cz["dcat:distribution"].as_array().into_iter().flatten() {
                assert_eq!(
                    distribution["pu:specifikace"]["pu:osobní-údaje"],
                    json!({ "@id": code })
                );
            }
            let included = cz["@included"].as_array().expect("@included");
            assert!(included
                .iter()
                .any(|n| n["@id"] == code && n["@type"] == "dct:LicenseDocument"));
            let turtle = turtle(&cz);
            assert!(turtle.contains(&format!("<{code}>")), "{turtle}");
        }
    }

    #[cfg(feature = "dcat-ap-cz")]
    #[test]
    fn a_czech_record_keeps_one_text_per_property_czech_first() {
        let mut catalog = catalog(Some(Licence::Cc0));
        catalog.contact_point = Some(jc_core::kinds::ContactPoint {
            name: "Odbor informatiky".into(),
            email: "opendata@example.org".into(),
        });
        let titled = |pairs: &[(&str, &str)]| {
            let title: BTreeMap<String, String> = pairs
                .iter()
                .map(|(l, t)| ((*l).to_owned(), (*t).to_owned()))
                .collect();
            let subject = Subject {
                slug: "kola",
                space: "kola",
                base_path: "/api/endpoint/kola",
                title: &title,
                description: &title,
                space_title: None,
                space_description: None,
                audience: Audience::Public,
                representations: &[Representation::NgsiLd],
                catalog: Some(&catalog),
                classes: &[],
                assigners: &[],
            };
            render(
                dataset(&subject, &index(), BASE),
                Profile::DcatApCz,
                Some(&catalog),
            )
        };
        let cz = titled(&[("cs", "Kola"), ("en", "Bikes"), ("sk", "Bicykle")]);
        assert_eq!(
            cz["dct:title"],
            json!({ "@value": "Kola", "@language": "cs" })
        );
        assert_eq!(
            cz["dct:description"],
            json!({ "@value": "Kola", "@language": "cs" })
        );
        let service = cz["@included"]
            .as_array()
            .and_then(|all| all.iter().find(|n| n["@type"] == "dcat:DataService"))
            .expect("the service");
        assert_eq!(service["dct:title"], cz["dct:title"]);
        // The names the gateway writes as one string are `rdf:langString`s too.
        assert_eq!(
            cz["dcat:contactPoint"]["vcard:fn"],
            json!([{ "@value": "Odbor informatiky", "@language": "cs" }])
        );
        for distribution in cz["dcat:distribution"].as_array().expect("distributions") {
            assert!(
                distribution["dct:title"][0]["@language"] == "cs",
                "{distribution}"
            );
        }
        let en = titled(&[("en", "Bikes"), ("sk", "Bicykle")]);
        assert_eq!(
            en["dct:title"],
            json!({ "@value": "Bikes", "@language": "en" })
        );
        let sk = titled(&[("sk", "Bicykle")]);
        assert_eq!(
            sk["dct:title"],
            json!({ "@value": "Bicykle", "@language": "sk" })
        );
    }

    fn owner_catalog() -> jc_core::kinds::ProjectCatalog {
        jc_core::kinds::ProjectCatalog {
            publisher: jc_core::kinds::Publisher {
                name: BTreeMap::from([("sk".to_owned(), "Mesto Banská Bystrica".to_owned())]),
                uri: Some("https://data.gov.sk/id/legal-subject/00313271".into()),
            },
            contact_point: Some(jc_core::kinds::ContactPoint {
                name: "Oddelenie informatiky".into(),
                email: "opendata@example.org".into(),
            }),
        }
    }

    #[test]
    fn a_publishers_catalogue_names_the_publisher_desk_and_home_and_lists_the_records() {
        let catalog = catalog(Some(Licence::CcBy4));
        let owned = owner_catalog();
        let title = BTreeMap::from([
            ("en".to_owned(), "City of Banská Bystrica".to_owned()),
            ("sk".to_owned(), "Mesto Banská Bystrica".to_owned()),
        ]);
        let none = BTreeMap::new();
        let owner = crate::CatalogueOwner {
            project: "banskabystrica",
            title: &title,
            description: &none,
            domain: Some("banskabystrica.sk"),
            catalog: &owned,
        };
        let feed = crate::publisher_feed(vec![record(&catalog)], BASE, &owner);
        assert_eq!(feed["@id"], format!("{BASE}/catalog/banskabystrica"));
        assert_eq!(feed["@type"], "dcat:Catalog");
        assert_eq!(
            feed["dct:publisher"]["@id"],
            "https://data.gov.sk/id/legal-subject/00313271"
        );
        assert_eq!(
            feed["dcat:contactPoint"]["vcard:hasEmail"],
            "mailto:opendata@example.org"
        );
        assert_eq!(feed["foaf:homepage"], "https://banskabystrica.sk/");
        // no description of its own: the title stands in, in every language
        assert_eq!(feed["dct:description"], feed["dct:title"]);
        assert_eq!(feed["dct:title"].as_array().map(Vec::len), Some(2));
        assert_eq!(
            feed["dcat:dataset"],
            json!([{ "@id": format!("{BASE}/api/endpoint/ovzdusie") }])
        );
        let turtle = turtle(&feed);
        assert!(
            turtle.contains("foaf:homepage <https://banskabystrica.sk/>"),
            "{turtle}"
        );
        assert!(
            turtle.contains("dct:publisher <https://data.gov.sk/id/legal-subject/00313271>"),
            "{turtle}"
        );

        // a project with no title of its own is titled by its publisher; an empty one lists nothing
        let untitled = crate::CatalogueOwner {
            title: &none,
            domain: None,
            ..owner
        };
        let empty = crate::publisher_feed(Vec::new(), BASE, &untitled);
        assert_eq!(
            empty["dct:title"],
            json!([{ "@value": "Mesto Banská Bystrica", "@language": "sk" }])
        );
        assert_eq!(empty["dcat:dataset"], json!([]));
        assert!(empty.get("foaf:homepage").is_none());
        assert_eq!(render_catalog(empty.clone(), Profile::DcatAp), empty);
    }

    #[cfg(all(feature = "dcat-ap-sk", feature = "dcat-ap-cz"))]
    #[test]
    fn a_catalogue_takes_what_each_profile_asks_of_it() {
        let owned = owner_catalog();
        let title = BTreeMap::from([
            ("cs".to_owned(), "Banská Bystrica".to_owned()),
            ("sk".to_owned(), "Mesto Banská Bystrica".to_owned()),
        ]);
        let owner = crate::CatalogueOwner {
            project: "banskabystrica",
            title: &title,
            description: &title,
            domain: None,
            catalog: &owned,
        };
        let feed = crate::publisher_feed(Vec::new(), BASE, &owner);
        let sk = render_catalog(feed.clone(), Profile::DcatApSk);
        assert_eq!(
            sk["dcat:contactPoint"]["vcard:fn"],
            json!([{ "@value": "Oddelenie informatiky", "@language": "sk" }])
        );
        assert_eq!(sk["dct:title"], feed["dct:title"]);
        let cz = render_catalog(feed, Profile::DcatApCz);
        assert_eq!(
            cz["dct:title"],
            json!({ "@value": "Banská Bystrica", "@language": "cs" })
        );
        assert_eq!(cz["dct:description"], cz["dct:title"]);
    }

    /// Without a national profile the statement is never written: DCAT-AP 3.0 has no term for
    /// it, and this runs in every build, the featureless one included.
    #[test]
    fn the_core_record_never_carries_the_statement() {
        for statement in [PersonalData::Contains, PersonalData::DoesNotContain] {
            let mut stated = catalog(Some(Licence::CcBy4));
            stated.personal_data = Some(statement);
            let core = render(record(&stated), Profile::DcatAp, Some(&stated));
            let written = core.to_string();
            assert!(!written.contains("personal"), "{written}");
            assert!(!written.contains("osobn"), "{written}");
        }
    }

    #[test]
    fn the_core_turtle_is_unchanged_by_the_profiles_existing() {
        let catalog = catalog(Some(Licence::CcBy4));
        let title = BTreeMap::new();
        let subject = Subject {
            slug: "s",
            space: "s",
            base_path: "/api/endpoint/s",
            title: &title,
            description: &title,
            space_title: None,
            space_description: None,
            audience: Audience::Public,
            representations: &[],
            catalog: Some(&catalog),
            classes: &[],
            assigners: &[],
        };
        let turtle = turtle(&dataset(&subject, &index(), BASE));
        assert!(
            turtle.contains("\"120\"^^xsd:nonNegativeInteger"),
            "{turtle}"
        );
        assert!(!turtle.contains("leg:") || turtle.contains("@prefix leg:"));
    }
}
