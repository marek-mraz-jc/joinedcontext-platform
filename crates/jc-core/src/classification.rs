//! What a dataset is, from the Smart Data Models of the entities it holds (T-4498, EP-78, DM-06):
//! the SDM domain and subject of each entity type, the EU data theme that follows, and keywords
//! in Slovak and English. An Endpoint whose `catalog.themes` is empty is themed by this; a theme
//! set by hand wins.
//!
//! Sources:
//! - domain and subject: the `dataModel.{Domain}/{Subject}` repository and folder each schema of
//!   joinedcontext-deployment `scripts/open-data/sdm/` was fetched from (`pins.json`); an
//!   extension under `sdm/extensions/` takes the domain of the model its schema's `derivedFrom`
//!   names;
//! - theme: the EU Vocabularies data-theme authority table
//!   (<http://publications.europa.eu/resource/authority/data-theme>), one theme per subject,
//!   chosen for what the subject records. A subject inherits no theme from its domain: a council
//!   meeting extends `Event` of TourismDestinations and is government data.
//!
//! A type the table does not name is unclassified, never guessed.

use crate::kinds::catalog::DataTheme;
use std::collections::{BTreeMap, BTreeSet};

/// One row of the table: the entity type, its SDM domain and its EU data theme.
type Row = (&'static str, &'static str, DataTheme);

/// Every entity type the table classifies.
const SUBJECTS: &[Row] = {
    use DataTheme::*;
    &[
        // dataModel.CPSV-AP
        ("Address", "CPSV-AP", Gove),
        ("Cost", "CPSV-AP", Econ),
        ("Evidence", "CPSV-AP", Gove),
        ("PublicOrganization", "CPSV-AP", Gove),
        ("Rule", "CPSV-AP", Just),
        // dataModel.Environment
        ("AirQualityObserved", "Environment", Envi),
        // dataModel.KeyPerformanceIndicator: a city's indicators of itself.
        ("KeyPerformanceIndicator", "KeyPerformanceIndicator", Regi),
        // dataModel.Organization
        ("Organization", "Organization", Gove),
        ("Person", "Organization", Soci),
        // dataModel.Parking
        ("OffStreetParking", "Parking", Tran),
        ("OnStreetParking", "Parking", Tran),
        // dataModel.ParksAndGardens
        ("Garden", "ParksAndGardens", Envi),
        // dataModel.PointOfInterest
        ("PointOfInterest", "PointOfInterest", Regi),
        // dataModel.TourismDestinations: events of a city's cultural life.
        ("Event", "TourismDestinations", Educ),
        // dataModel.Transportation
        ("BikeHireDockingStation", "Transportation", Tran),
        ("BikeLane", "Transportation", Tran),
        ("CityWork", "Transportation", Tran),
        ("RoadAccident", "Transportation", Tran),
        // dataModel.UrbanMobility
        ("PublicTransportRoute", "UrbanMobility", Tran),
        // dataModel.WasteManagement
        ("WasteContainer", "WasteManagement", Envi),
        ("WasteContainerIsle", "WasteManagement", Envi),
        // dataModel.Weather
        ("WeatherObserved", "Weather", Envi),
        // sdm/extensions, each in the domain of the model it is derived from.
        ("AdministrativeArea", "PointOfInterest", Regi),
        ("Article", "CPSV-AP", Gove),
        ("Bridge", "PointOfInterest", Tran),
        ("BudgetLine", "CPSV-AP", Econ),
        ("CommitteeSeat", "Organization", Gove),
        ("Contract", "CPSV-AP", Econ),
        ("CouncilMeeting", "TourismDestinations", Gove),
        ("CouncilVote", "TourismDestinations", Gove),
        ("CycleRoute", "Transportation", Tran),
        ("Grant", "CPSV-AP", Econ),
        ("Invoice", "CPSV-AP", Econ),
        ("OfficeHours", "Organization", Gove),
        ("Petition", "CPSV-AP", Gove),
        ("PublicInvestment", "CPSV-AP", Econ),
        ("PublicProcurement", "CPSV-AP", Econ),
        ("PurchaseOrder", "CPSV-AP", Econ),
        ("RiverBasin", "PointOfInterest", Envi),
        ("RoadStructure", "PointOfInterest", Tran),
        ("WaterBodyStatus", "PointOfInterest", Envi),
    ]
};

/// Each SDM domain's keyword: `(domain, sk, en)`.
const DOMAINS: &[(&str, &str, &str)] = &[
    ("CPSV-AP", "verejné služby", "public services"),
    ("Environment", "životné prostredie", "environment"),
    (
        "KeyPerformanceIndicator",
        "kľúčové ukazovatele výkonnosti",
        "key performance indicators",
    ),
    ("Organization", "organizácie", "organisations"),
    ("Parking", "parkovanie", "parking"),
    ("ParksAndGardens", "parky a záhrady", "parks and gardens"),
    ("PointOfInterest", "body záujmu", "points of interest"),
    ("TourismDestinations", "cestovný ruch", "tourism"),
    ("Transportation", "doprava", "transport"),
    ("UrbanMobility", "mestská mobilita", "urban mobility"),
    (
        "WasteManagement",
        "odpadové hospodárstvo",
        "waste management",
    ),
    ("Weather", "počasie", "weather"),
];

/// Whether `name` is an SDM domain of the table, e.g. `Environment` (MF-55).
pub fn is_domain(name: &str) -> bool {
    DOMAINS.iter().any(|(domain, ..)| *domain == name)
}

/// What a set of entity types says the dataset is.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Classification {
    /// The SDM domains, e.g. `Environment`.
    pub domains: BTreeSet<&'static str>,
    /// The SDM subjects, the classified types themselves, e.g. `AirQualityObserved`.
    pub subjects: BTreeSet<&'static str>,
    /// The EU data themes.
    pub themes: BTreeSet<DataTheme>,
    /// Keywords per language (`sk`, `en`): each domain's name, then each subject.
    pub keywords: BTreeMap<String, Vec<String>>,
    /// The types the table does not name.
    pub unclassified: BTreeSet<String>,
}

/// The row of one entity type, by its short name or its expanded IRI (the part after the last
/// `/` or `#`); `None` when the table does not name it.
fn row(entity_type: &str) -> Option<&'static Row> {
    let short = entity_type.rsplit(['/', '#']).next().unwrap_or(entity_type);
    SUBJECTS.iter().find(|(name, ..)| *name == short)
}

/// The classification of a dataset holding `types`.
pub fn classify<'a>(types: impl IntoIterator<Item = &'a str>) -> Classification {
    let mut out = Classification::default();
    for entity_type in types {
        match row(entity_type) {
            Some(&(name, domain, theme)) => {
                out.subjects.insert(name);
                out.domains.insert(domain);
                out.themes.insert(theme);
            }
            None => {
                out.unclassified.insert(entity_type.to_owned());
            }
        }
    }
    for (domain, sk, en) in DOMAINS {
        if out.domains.contains(domain) {
            out.keywords
                .entry("sk".into())
                .or_default()
                .push((*sk).into());
            out.keywords
                .entry("en".into())
                .or_default()
                .push((*en).into());
        }
    }
    for name in &out.subjects {
        for words in out.keywords.values_mut() {
            words.push((*name).to_owned());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn air_quality_is_environment() {
        let c = classify(["AirQualityObserved"]);
        assert_eq!(c.domains, BTreeSet::from(["Environment"]));
        assert_eq!(c.themes, BTreeSet::from([DataTheme::Envi]));
        assert_eq!(
            c.keywords["sk"],
            ["životné prostredie", "AirQualityObserved"]
        );
        assert_eq!(c.keywords["en"], ["environment", "AirQualityObserved"]);
    }

    #[test]
    fn event_and_indicator_take_their_rows() {
        let row = |t: &str| {
            let c = classify([t]);
            (
                c.domains.into_iter().collect::<Vec<_>>(),
                c.themes.into_iter().collect::<Vec<_>>(),
            )
        };
        assert_eq!(
            row("Event"),
            (vec!["TourismDestinations"], vec![DataTheme::Educ])
        );
        assert_eq!(
            row("KeyPerformanceIndicator"),
            (vec!["KeyPerformanceIndicator"], vec![DataTheme::Regi])
        );
        // An extension takes its parent's domain and its own theme.
        assert_eq!(
            row("CouncilMeeting"),
            (vec!["TourismDestinations"], vec![DataTheme::Gove])
        );
    }

    #[test]
    fn two_domains_give_both_and_an_iri_is_its_short_name() {
        let c = classify([
            "https://smartdatamodels.org/dataModel.Parking/OffStreetParking",
            "WeatherObserved",
        ]);
        assert_eq!(c.domains, BTreeSet::from(["Parking", "Weather"]));
        assert_eq!(c.themes, BTreeSet::from([DataTheme::Envi, DataTheme::Tran]));
        assert!(c.unclassified.is_empty());
    }

    #[test]
    fn an_unknown_type_is_unclassified_never_guessed() {
        let c = classify(["Vehicle", "AirQualityObservedX"]);
        assert!(c.subjects.is_empty() && c.domains.is_empty());
        assert!(c.themes.is_empty() && c.keywords.is_empty());
        assert_eq!(
            c.unclassified,
            BTreeSet::from(["AirQualityObservedX".to_owned(), "Vehicle".to_owned()])
        );
        assert_eq!(classify([]), Classification::default());
    }

    #[test]
    fn every_row_names_a_domain_with_keywords_and_no_type_twice() {
        let mut seen = BTreeSet::new();
        for (name, domain, _) in SUBJECTS {
            assert!(seen.insert(name), "{name} twice");
            assert!(
                DOMAINS.iter().any(|(d, ..)| d == domain),
                "{domain} has no keywords"
            );
        }
    }
}
