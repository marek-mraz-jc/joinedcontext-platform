//! The `spec.catalog` block of an Endpoint: what a DCAT-AP 3.0 catalogue needs to know about
//! the dataset beyond its distributions (T-2789, EP-78, EP-79, EP-80).
//!
//! Every table-coded member is a closed enum over the EU authority table it comes from, so a
//! manifest cannot name a licence whose duties nobody has written down, and the record renders
//! the authority IRI rather than a string a harvester has to guess the meaning of.

use crate::envelope::TypedRef;
use crate::error::{Error, Result};
use crate::names;
use chrono::NaiveDate;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Text per ISO 639-1 language code.
pub type Texts = BTreeMap<String, String>;

/// The catalogue description of one Endpoint (EP-78).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Catalog {
    /// Who publishes the dataset: `dct:publisher`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub publisher: Option<Publisher>,
    /// Where questions about it go: `dcat:contactPoint`, a role address (EP-80).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub contact_point: Option<ContactPoint>,
    /// The licence, from the EU licence table: `dct:license`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub license: Option<Licence>,
    /// The words the attribution duty asks for, per language: `dct:rights`.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub attribution: Texts,
    /// EU data themes: `dcat:theme`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub themes: Vec<DataTheme>,
    /// Keywords per language: `dcat:keyword`.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub keywords: BTreeMap<String, Vec<String>>,
    /// NUTS codes or location IRIs: `dct:spatial`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub spatial: Vec<String>,
    /// The period the data covers: `dct:temporal`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temporal: Option<Temporal>,
    /// How often the data changes, from the EU frequency table: `dct:accrualPeriodicity`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frequency: Option<Frequency>,
    /// The original open datasets the data comes from: `dct:source`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub source: Vec<Source>,
    /// The Pipeline of this project that fills the space: `prov:wasGeneratedBy`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pipeline_ref: Option<TypedRef>,
    /// ELI IRIs of the laws that apply: `dcatap:applicableLegislation`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub applicable_legislation: Vec<String>,
}

/// The publisher of a dataset, a `foaf:Agent`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Publisher {
    /// The publisher's name per language: `foaf:name`.
    pub name: Texts,
    /// The publisher's own IRI, its web site or registry entry.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uri: Option<String>,
}

/// The contact point of a dataset, a `vcard:Kind`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct ContactPoint {
    /// The desk's name: `vcard:fn`.
    pub name: String,
    /// A role address, never a person's own (EP-80): `vcard:hasEmail`.
    pub email: String,
}

/// One original dataset the data comes from, itself a `dcat:Dataset`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Source {
    /// Its IRI, the page or API of the original.
    pub url: String,
    /// Its title per language; DCAT-AP requires one of every dataset it names.
    pub title: Texts,
    /// Its description per language; DCAT-AP requires one of every dataset it names.
    pub description: Texts,
}

/// The period a dataset covers; either end may be open.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Temporal {
    /// The first day covered: `dcat:startDate`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start: Option<NaiveDate>,
    /// The last day covered: `dcat:endDate`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end: Option<NaiveDate>,
}

/// What a licence asks of whoever uses the data, as ODRL duties (EP-79).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Duty {
    /// Nothing: a public-domain dedication.
    None,
    /// Credit the publisher: `odrl:attribute`.
    Attribute,
    /// Credit the publisher and share derivatives alike: `odrl:attribute` and `cc:ShareAlike`.
    AttributeShareAlike,
}

/// A licence of the EU licence table whose duties are known (EP-79).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub enum Licence {
    /// Creative Commons Zero 1.0.
    #[serde(rename = "CC0")]
    Cc0,
    /// Creative Commons Attribution 4.0.
    #[serde(rename = "CC_BY_4_0")]
    CcBy4,
    /// Creative Commons Attribution-ShareAlike 4.0.
    #[serde(rename = "CC_BYSA_4_0")]
    CcBySa4,
    /// Open Data Commons Attribution.
    #[serde(rename = "ODC_BY")]
    OdcBy,
    /// Open Data Commons Open Database Licence.
    #[serde(rename = "ODC_ODBL")]
    OdcOdbl,
    /// Open Data Commons Public Domain Dedication and Licence.
    #[serde(rename = "ODC_PDDL")]
    OdcPddl,
}

impl Licence {
    /// Every licence, in the order a picker lists them.
    pub const ALL: [Licence; 6] = [
        Self::CcBy4,
        Self::CcBySa4,
        Self::Cc0,
        Self::OdcBy,
        Self::OdcOdbl,
        Self::OdcPddl,
    ];

    /// The EU licence table code.
    pub const fn code(self) -> &'static str {
        match self {
            Self::Cc0 => "CC0",
            Self::CcBy4 => "CC_BY_4_0",
            Self::CcBySa4 => "CC_BYSA_4_0",
            Self::OdcBy => "ODC_BY",
            Self::OdcOdbl => "ODC_ODBL",
            Self::OdcPddl => "ODC_PDDL",
        }
    }

    /// The EU licence table IRI, which is what `dct:license` names.
    pub fn iri(self) -> String {
        format!(
            "http://publications.europa.eu/resource/authority/licence/{}",
            self.code()
        )
    }

    /// The licence's name as a person reads it.
    pub const fn title(self) -> &'static str {
        match self {
            Self::Cc0 => "Creative Commons Zero 1.0",
            Self::CcBy4 => "Creative Commons Attribution 4.0",
            Self::CcBySa4 => "Creative Commons Attribution-ShareAlike 4.0",
            Self::OdcBy => "Open Data Commons Attribution License",
            Self::OdcOdbl => "Open Data Commons Open Database License",
            Self::OdcPddl => "Open Data Commons Public Domain Dedication and License",
        }
    }

    /// The CKAN licence register id, which is what a CKAN dataset names (EP-63).
    pub const fn ckan_id(self) -> &'static str {
        match self {
            Self::Cc0 => "cc-zero",
            Self::CcBy4 => "cc-by",
            Self::CcBySa4 => "cc-by-sa",
            Self::OdcBy => "odc-by",
            Self::OdcOdbl => "odc-odbl",
            Self::OdcPddl => "odc-pddl",
        }
    }

    /// What the licence asks of a user (EP-79).
    pub const fn duty(self) -> Duty {
        match self {
            Self::Cc0 | Self::OdcPddl => Duty::None,
            Self::CcBy4 | Self::OdcBy => Duty::Attribute,
            Self::CcBySa4 | Self::OdcOdbl => Duty::AttributeShareAlike,
        }
    }
}

/// A theme of the EU data-theme table.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "UPPERCASE")]
pub enum DataTheme {
    /// Agriculture, fisheries, forestry and food.
    Agri,
    /// Economy and finance.
    Econ,
    /// Education, culture and sport.
    Educ,
    /// Energy.
    Ener,
    /// Environment.
    Envi,
    /// Government and public sector.
    Gove,
    /// Health.
    Heal,
    /// International issues.
    Intr,
    /// Justice, legal system and public safety.
    Just,
    /// Regions and cities.
    Regi,
    /// Population and society.
    Soci,
    /// Science and technology.
    Tech,
    /// Transport.
    Tran,
}

impl DataTheme {
    /// Every theme, in the table's order.
    pub const ALL: [DataTheme; 13] = [
        Self::Agri,
        Self::Econ,
        Self::Educ,
        Self::Ener,
        Self::Envi,
        Self::Gove,
        Self::Heal,
        Self::Intr,
        Self::Just,
        Self::Regi,
        Self::Soci,
        Self::Tech,
        Self::Tran,
    ];

    /// The table code.
    pub const fn code(self) -> &'static str {
        match self {
            Self::Agri => "AGRI",
            Self::Econ => "ECON",
            Self::Educ => "EDUC",
            Self::Ener => "ENER",
            Self::Envi => "ENVI",
            Self::Gove => "GOVE",
            Self::Heal => "HEAL",
            Self::Intr => "INTR",
            Self::Just => "JUST",
            Self::Regi => "REGI",
            Self::Soci => "SOCI",
            Self::Tech => "TECH",
            Self::Tran => "TRAN",
        }
    }

    /// The table IRI, which is what `dcat:theme` names.
    pub fn iri(self) -> String {
        format!(
            "http://publications.europa.eu/resource/authority/data-theme/{}",
            self.code()
        )
    }

    /// The table's English label, the `skos:prefLabel` DCAT-AP requires of a theme.
    pub const fn label(self) -> &'static str {
        match self {
            Self::Agri => "Agriculture, fisheries, forestry and food",
            Self::Econ => "Economy and finance",
            Self::Educ => "Education, culture and sport",
            Self::Ener => "Energy",
            Self::Envi => "Environment",
            Self::Gove => "Government and public sector",
            Self::Heal => "Health",
            Self::Intr => "International issues",
            Self::Just => "Justice, legal system and public safety",
            Self::Regi => "Regions and cities",
            Self::Soci => "Population and society",
            Self::Tech => "Science and technology",
            Self::Tran => "Transport",
        }
    }
}

/// An update frequency of the EU frequency table.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "UPPERCASE")]
pub enum Frequency {
    /// Continuous.
    Cont,
    /// Hourly.
    Hourly,
    /// Daily.
    Daily,
    /// Weekly.
    Weekly,
    /// Monthly.
    Monthly,
    /// Quarterly.
    Quarterly,
    /// Annual.
    Annual,
    /// Irregular.
    Irreg,
    /// Never updated.
    Never,
    /// Unknown.
    Unknown,
}

impl Frequency {
    /// Every frequency, fastest first.
    pub const ALL: [Frequency; 10] = [
        Self::Cont,
        Self::Hourly,
        Self::Daily,
        Self::Weekly,
        Self::Monthly,
        Self::Quarterly,
        Self::Annual,
        Self::Irreg,
        Self::Never,
        Self::Unknown,
    ];

    /// The table code.
    pub const fn code(self) -> &'static str {
        match self {
            Self::Cont => "CONT",
            Self::Hourly => "HOURLY",
            Self::Daily => "DAILY",
            Self::Weekly => "WEEKLY",
            Self::Monthly => "MONTHLY",
            Self::Quarterly => "QUARTERLY",
            Self::Annual => "ANNUAL",
            Self::Irreg => "IRREG",
            Self::Never => "NEVER",
            Self::Unknown => "UNKNOWN",
        }
    }

    /// The table IRI, which is what `dct:accrualPeriodicity` names.
    pub fn iri(self) -> String {
        format!(
            "http://publications.europa.eu/resource/authority/frequency/{}",
            self.code()
        )
    }
}

/// The IRI a `spatial` entry stands for: a NUTS code becomes the EU NUTS IRI, an IRI stays.
pub fn spatial_iri(entry: &str) -> String {
    if is_nuts_code(entry) {
        format!("http://data.europa.eu/nuts/code/{entry}")
    } else {
        entry.to_owned()
    }
}

/// `SK032`, `FI1B1`, `CZ010`: two capital letters and up to three more characters.
fn is_nuts_code(entry: &str) -> bool {
    let bytes = entry.as_bytes();
    (2..=5).contains(&bytes.len())
        && bytes[..2].iter().all(u8::is_ascii_uppercase)
        && bytes[2..]
            .iter()
            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit())
}

impl Catalog {
    /// Validates every member, naming the field that is wrong (EP-78, EP-80).
    pub fn validate(&self) -> Result<()> {
        if let Some(publisher) = &self.publisher {
            texts("spec.catalog.publisher.name", &publisher.name, true)?;
            if let Some(uri) = &publisher.uri {
                iri("spec.catalog.publisher.uri", uri)?;
            }
        }
        if let Some(contact) = &self.contact_point {
            if contact.name.trim().is_empty() || contact.name.chars().any(char::is_control) {
                return Err(Error::Name {
                    field: "spec.catalog.contactPoint.name",
                    value: contact.name.clone(),
                    reason: "name the desk a person writes to, in one line",
                });
            }
            email(&contact.email)?;
        }
        texts("spec.catalog.attribution", &self.attribution, false)?;
        let mut themes = std::collections::BTreeSet::new();
        for theme in &self.themes {
            if !themes.insert(theme) {
                return Err(Error::Name {
                    field: "spec.catalog.themes",
                    value: theme.code().to_owned(),
                    reason: "a theme is listed twice",
                });
            }
        }
        for (language, words) in &self.keywords {
            names::validate_locale(language)?;
            for word in words {
                if word.trim().is_empty() || word.len() > 100 || word.chars().any(char::is_control)
                {
                    return Err(Error::Name {
                        field: "spec.catalog.keywords",
                        value: word.clone(),
                        reason: "a keyword is one line of at most 100 bytes",
                    });
                }
            }
        }
        for entry in &self.spatial {
            if !is_nuts_code(entry) {
                iri("spec.catalog.spatial", entry).map_err(|_| Error::Name {
                    field: "spec.catalog.spatial",
                    value: entry.clone(),
                    reason:
                        "name a NUTS code (SK032) or a location IRI (https://sws.geonames.org/…/)",
                })?;
            }
        }
        if let Some(period) = &self.temporal {
            match (period.start, period.end) {
                (None, None) => {
                    return Err(Error::Name {
                        field: "spec.catalog.temporal",
                        value: String::new(),
                        reason: "name a start, an end or both",
                    })
                }
                (Some(start), Some(end)) if end < start => {
                    return Err(Error::Name {
                        field: "spec.catalog.temporal.end",
                        value: end.to_string(),
                        reason: "the end is before the start",
                    })
                }
                _ => {}
            }
        }
        for source in &self.source {
            iri("spec.catalog.source[].url", &source.url)?;
            texts("spec.catalog.source[].title", &source.title, true)?;
            texts(
                "spec.catalog.source[].description",
                &source.description,
                true,
            )?;
        }
        if let Some(pipeline) = &self.pipeline_ref {
            if pipeline.kind != "Pipeline" {
                return Err(Error::Kind {
                    expected: "Pipeline",
                    got: pipeline.kind.clone(),
                });
            }
            names::validate_dns1123_label(&pipeline.name).map_err(|e| match e {
                Error::Name { reason, .. } => Error::Name {
                    field: "spec.catalog.pipelineRef.name",
                    value: pipeline.name.clone(),
                    reason,
                },
                other => other,
            })?;
        }
        for law in &self.applicable_legislation {
            iri("spec.catalog.applicableLegislation", law)?;
        }
        Ok(())
    }
}

/// A language map whose keys are locales and whose texts are not blank.
fn texts(field: &'static str, map: &Texts, required: bool) -> Result<()> {
    if required && map.values().all(|text| text.trim().is_empty()) {
        return Err(Error::Name {
            field,
            value: String::new(),
            reason: "give the text in at least one language",
        });
    }
    for (language, text) in map {
        names::validate_locale(language)?;
        if text.trim().is_empty() {
            return Err(Error::Name {
                field,
                value: language.clone(),
                reason: "a language is listed with no text",
            });
        }
    }
    Ok(())
}

/// An absolute `http(s)` IRI with a host and nothing that would close an IRI in Turtle.
fn iri(field: &'static str, value: &str) -> Result<()> {
    let rest = value
        .strip_prefix("https://")
        .or_else(|| value.strip_prefix("http://"));
    let host = rest.and_then(|rest| rest.split(['/', '?', '#']).next());
    let clean = !value.chars().any(|c| c <= ' ' || "<>\"{}|^`\\".contains(c));
    if host.is_some_and(|host| !host.is_empty()) && clean {
        Ok(())
    } else {
        Err(Error::Name {
            field,
            value: value.to_owned(),
            reason: "an absolute http:// or https:// IRI is expected",
        })
    }
}

/// One address, `local@domain.tld`, with nothing a `mailto:` IRI cannot carry.
fn email(value: &str) -> Result<()> {
    let refuse = || Error::Name {
        field: "spec.catalog.contactPoint.email",
        value: value.to_owned(),
        reason: "a role address like opendata@city.example is expected",
    };
    let (local, domain) = value.split_once('@').ok_or_else(refuse)?;
    let clean = |part: &str| {
        !part.is_empty()
            && part
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || "._+-".contains(c))
    };
    let labels_ok = domain
        .split('.')
        .all(|label| !label.is_empty() && !label.starts_with('-') && !label.ends_with('-'));
    if clean(local) && local.len() <= 64 && clean(domain) && domain.contains('.') && labels_ok {
        Ok(())
    } else {
        Err(refuse())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(yaml: &str) -> Result<Catalog> {
        let catalog: Catalog = serde_norway::from_str(yaml).map_err(|e| Error::Name {
            field: "spec.catalog",
            value: e.to_string(),
            reason: "does not parse",
        })?;
        catalog.validate().map(|()| catalog)
    }

    #[test]
    fn a_full_block_parses_and_renders_the_authority_iris() {
        let catalog = parse(
            "publisher: { name: { en: City of Helsinki }, uri: https://www.hel.fi/ }\n\
             contactPoint: { name: Open data, email: avoindata@hel.fi }\n\
             license: CC_BY_4_0\n\
             attribution: { en: 'Source: City of Helsinki' }\n\
             themes: [TRAN, REGI]\n\
             keywords: { fi: [tapahtumat], en: [events] }\n\
             spatial: [FI1B1, 'https://sws.geonames.org/658225/']\n\
             temporal: { start: 2019-01-01 }\n\
             frequency: DAILY\n\
             source: [{ url: 'https://hri.fi/data/x', title: { en: X }, description: { en: Y } }]\n\
             pipelineRef: { kind: Pipeline, name: helsinki-events }\n\
             applicableLegislation: ['http://data.europa.eu/eli/reg_impl/2023/138/oj']\n",
        )
        .expect("valid");
        assert_eq!(
            catalog.license.map(Licence::iri).as_deref(),
            Some("http://publications.europa.eu/resource/authority/licence/CC_BY_4_0")
        );
        assert_eq!(
            catalog.themes[0].iri(),
            "http://publications.europa.eu/resource/authority/data-theme/TRAN"
        );
        assert_eq!(
            catalog.frequency.map(Frequency::iri).as_deref(),
            Some("http://publications.europa.eu/resource/authority/frequency/DAILY")
        );
        assert_eq!(
            spatial_iri(&catalog.spatial[0]),
            "http://data.europa.eu/nuts/code/FI1B1"
        );
        assert_eq!(
            spatial_iri(&catalog.spatial[1]),
            "https://sws.geonames.org/658225/"
        );
    }

    #[test]
    fn an_empty_block_is_valid() {
        assert_eq!(parse("{}").expect("valid"), Catalog::default());
    }

    #[test]
    fn a_licence_outside_the_table_is_refused_with_the_list() {
        let error = serde_norway::from_str::<Catalog>("license: CC_BY_NC_4_0\n")
            .expect_err("unknown licence");
        let message = error.to_string();
        assert!(
            message.contains("CC_BY_4_0") && message.contains("CC0"),
            "{message}"
        );
    }

    #[test]
    fn each_licence_has_the_duty_its_terms_ask_for() {
        assert_eq!(Licence::Cc0.duty(), Duty::None);
        assert_eq!(Licence::OdcPddl.duty(), Duty::None);
        assert_eq!(Licence::CcBy4.duty(), Duty::Attribute);
        assert_eq!(Licence::OdcBy.duty(), Duty::Attribute);
        assert_eq!(Licence::CcBySa4.duty(), Duty::AttributeShareAlike);
        assert_eq!(Licence::OdcOdbl.duty(), Duty::AttributeShareAlike);
    }

    #[test]
    fn an_unknown_member_is_refused() {
        assert!(serde_norway::from_str::<Catalog>("licence: CC0\n").is_err());
    }

    #[test]
    fn a_contact_that_is_not_an_address_is_refused() {
        for email in [
            "nobody",
            "@hel.fi",
            "open data@hel.fi",
            "a@localhost",
            "a@hel..fi",
            "a@-hel.fi",
            "a@hel.fi>",
        ] {
            let yaml = format!("contactPoint: {{ name: Desk, email: '{email}' }}\n");
            assert!(parse(&yaml).is_err(), "{email} was accepted");
        }
        assert!(parse("contactPoint: { name: '  ', email: a@b.fi }\n").is_err());
    }

    #[test]
    fn spatial_takes_a_nuts_code_or_an_iri_and_nothing_else() {
        assert!(parse("spatial: [SK032, CZ010, FI1B1, SK]\n").is_ok());
        for entry in [
            "sk032",
            "Banska Bystrica",
            "ftp://x/y",
            "https://",
            "SK0321X",
        ] {
            let yaml = format!("spatial: ['{entry}']\n");
            assert!(parse(&yaml).is_err(), "{entry} was accepted");
        }
    }

    #[test]
    fn an_iri_that_would_close_the_turtle_iri_is_refused() {
        assert!(parse(
            "source: [{ url: 'https://x/a>b', title: { en: X }, description: { en: Y } }]\n"
        )
        .is_err());
        assert!(parse("applicableLegislation: ['https://x/a b']\n").is_err());
    }

    #[test]
    fn a_period_needs_an_end_after_its_start() {
        assert!(parse("temporal: {}\n").is_err());
        assert!(parse("temporal: { start: 2024-02-01, end: 2024-01-01 }\n").is_err());
        assert!(parse("temporal: { end: 2024-01-01 }\n").is_ok());
    }

    #[test]
    fn texts_need_a_locale_and_words() {
        assert!(parse("publisher: { name: {} }\n").is_err());
        assert!(parse("publisher: { name: { english: City } }\n").is_err());
        assert!(parse("attribution: { en: '' }\n").is_err());
        assert!(
            parse("source: [{ url: 'https://x/', title: { en: X }, description: {} }]\n").is_err()
        );
    }

    #[test]
    fn a_theme_listed_twice_and_a_blank_keyword_are_refused() {
        assert!(parse("themes: [TRAN, TRAN]\n").is_err());
        assert!(parse("keywords: { en: ['  '] }\n").is_err());
        assert!(parse("keywords: { xx1: [a] }\n").is_err());
    }

    #[test]
    fn the_pipeline_reference_names_a_pipeline() {
        assert!(parse("pipelineRef: { kind: Mapping, name: x }\n").is_err());
        assert!(parse("pipelineRef: { kind: Pipeline, name: Not_A_Name }\n").is_err());
    }
}
