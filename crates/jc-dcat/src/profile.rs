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
    /// DCAT-AP-SK 2.0.1 over DCAT-AP 2.1.1 (API/08 section 2).
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

/// DCAT-AP-SK 2.0.1: `leg:termsOfUse` on every distribution (API/08 section 2); no checksum,
/// because the DCAT-AP 2.1.1 base takes SHA-1 alone, and `dcat:byteSize` as `xsd:decimal`
/// (API/08 section 5).
#[cfg(feature = "dcat-ap-sk")]
fn slovak(mut record: Value, licence: Option<&str>, personal: Option<PersonalData>) -> Value {
    let personal = personal.map(|p| ("leg:personalDataContainmentType", slovak_personal(p)));
    for distribution in distributions(&mut record) {
        distribution.remove("spdx:checksum");
        if let Some(Value::String(bytes)) = distribution.remove("dcat:byteSize") {
            distribution.insert(
                "dcat:byteSize".to_owned(),
                json!({ "@value": bytes, "@type": "xsd:decimal" }),
            );
        }
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
    }
    if let Some(included) = record.get_mut("@included").and_then(Value::as_array_mut) {
        included.retain(|node| node["@id"] != "spdx:checksumAlgorithm_sha256");
    }
    record
}

/// DCAT-AP-CZ 2026-09-23: `pu:specifikace` on every distribution, its licences typed
/// `dct:LicenseDocument`, and the dataset's `r5r:applicableLegislation` repeated on each
/// distribution and on the data service (API/08 section 3).
#[cfg(feature = "dcat-ap-cz")]
fn czech(mut record: Value, licence: Option<&str>, personal: Option<PersonalData>) -> Value {
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
    }
    record
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

    #[cfg(feature = "dcat-ap-sk")]
    #[test]
    fn a_slovak_record_carries_terms_of_use_and_no_sha256() {
        let catalog = catalog(Some(Licence::CcBy4));
        let core = record(&catalog);
        let sk = render(core.clone(), Profile::DcatApSk, Some(&catalog));
        let licence = Licence::CcBy4.iri();

        let schema = schema_distribution(&sk);
        assert!(schema.get("spdx:checksum").is_none());
        assert_eq!(
            schema["dcat:byteSize"],
            json!({ "@value": "120", "@type": "xsd:decimal" })
        );
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
        assert!(!sk.to_string().contains("checksumAlgorithm_sha256"));
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
    fn a_slovak_record_without_a_licence_names_no_terms_and_its_turtle_types_the_size() {
        let catalog = catalog(None);
        let sk = render(record(&catalog), Profile::DcatApSk, Some(&catalog));
        assert!(!sk.to_string().contains("leg:termsOfUse"));
        let turtle = turtle(&sk);
        assert!(turtle.contains("\"120\"^^xsd:decimal"), "{turtle}");
        assert!(!turtle.contains("nonNegativeInteger"), "{turtle}");
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
