//! T-1186, DM-60: the Data Structure Definition keywords and the observation id.

use jc_core::qb::{self, Component};
use jc_core::urn::Urn;
use serde_json::json;

#[test]
fn an_observation_id_is_the_dsd_and_its_dimension_values_and_fits_the_urn_scheme() {
    let district = "urn:ngsi-ld:District:banskabystrica.sk:population:okres-banska-bystrica";
    let local = qb::observation_local_id("PopulationObservation", &[district, "15-19", "2024"])
        .expect("a valid cell");
    assert_eq!(
        local,
        "PopulationObservation~okres-banska-bystrica~15-19~2024"
    );
    // The whole id is an ordinary entity URN of the observation's type (PF-42).
    let urn = Urn::new(
        "PopulationObservation",
        "banskabystrica.sk",
        "banskabystrica-kpi",
        &local,
    )
    .expect("the id is a valid URN");
    assert_eq!(urn.local_id(), local);
}

#[test]
fn two_different_cells_never_share_an_id() {
    // With `-` between the parts these two would both be `…-15-19-2024`.
    let one = qb::observation_local_id("PopulationObservation", &["15-19", "2024"]).expect("cell");
    let other =
        qb::observation_local_id("PopulationObservation", &["15", "19-2024"]).expect("cell");
    assert_ne!(one, other);
}

#[test]
fn an_observation_id_is_refused_without_dimensions_or_with_a_value_it_cannot_carry() {
    assert!(qb::observation_local_id("PopulationObservation", &[]).is_err());
    assert!(qb::observation_local_id("PopulationObservation", &["a~b"]).is_err());
    assert!(qb::observation_local_id("PopulationObservation", &[""]).is_err());
    // Outside the local id alphabet (PF-42): the id would not be a URN.
    assert!(qb::observation_local_id("PopulationObservation", &["Banská Bystrica"]).is_err());
    assert!(
        qb::observation_local_id("population", &["2024"]).is_err(),
        "the DSD is an entity type"
    );
    let long = "9".repeat(200);
    assert!(qb::observation_local_id("PopulationObservation", &[long.as_str()]).is_err());
}

#[test]
fn a_dsd_and_its_components_are_read_from_the_projected_schema() {
    let dsd = json!({
        "x-qb-dsd": true,
        "properties": {
            "ageBand": { "type": "string", "x-qb-component": "dimension" },
            "population": { "type": "integer", "x-qb-component": "measure" },
            "name": { "type": "string" },
            "odd": { "type": "string", "x-qb-component": "attribute" }
        }
    });
    assert!(qb::is_dsd(&dsd));
    assert!(
        !qb::is_dsd(&json!({ "x-qb-dsd": "true" })),
        "only the boolean the generator writes"
    );
    assert!(!qb::is_dsd(&json!({ "properties": {} })));
    let slot = |name: &str| dsd["properties"][name].clone();
    assert_eq!(Component::of(&slot("ageBand")), Some(Component::Dimension));
    assert_eq!(Component::of(&slot("population")), Some(Component::Measure));
    assert_eq!(Component::of(&slot("name")), None);
    assert_eq!(
        Component::of(&slot("odd")),
        None,
        "a role the platform does not know is no role"
    );
}
