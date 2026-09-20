//! T-0582: the `KeyPerformanceIndicator` entity (PF-54, PF-55): its URN names the type and a
//! `-kpi` space, its provenance is required, it round-trips through serde as normalized
//! NGSI-LD, its `@context` maps every term, and the committed schema is the generated one.
use jc_core::kpi::{
    context, indicator_urn, kpi_space, schema, IndicatorValue, KeyPerformanceIndicator, Objects,
    Period, Relationship, KPI_TYPE,
};
use jc_core::Urn;
use serde_json::{json, Value};

fn indicator() -> KeyPerformanceIndicator {
    let urn = indicator_urn("hel.fi", "helsinki", "average-pm10").expect("a kpi urn");
    KeyPerformanceIndicator::new(
        &urn,
        "avg(pm10) over AirQualityObserved",
        IndicatorValue::Number(18.4),
        Some("GQ"),
        Period {
            start: "2026-09-13T07:00:00Z".into(),
            end: "2026-09-13T08:00:00Z".into(),
        },
        "2026-09-13T08:00:12Z",
        Relationship::to("urn:ngsi-ld:Endpoint:hel.fi:helsinki:helsinki-all"),
        Relationship::to("urn:ngsi-ld:AgentRun:hel.fi:helsinki:3f9c2a1e"),
    )
}

#[test]
fn the_urn_names_the_type_and_the_projects_indicator_space() {
    assert_eq!(kpi_space("helsinki"), "helsinki-kpi");
    let urn = indicator_urn("hel.fi", "helsinki", "average-pm10").expect("a kpi urn");
    assert_eq!(
        urn.to_string(),
        "urn:ngsi-ld:KeyPerformanceIndicator:hel.fi:helsinki-kpi:average-pm10"
    );
    assert_eq!(urn.entity_type(), KPI_TYPE);
    assert!(indicator_urn("hel.fi", "helsinki", "no spaces here").is_err());
    assert!(indicator_urn("hel.fi", "helsinki", "").is_err());
}

#[test]
fn a_complete_indicator_validates_and_round_trips_as_normalized_ngsi_ld() {
    let kpi = indicator();
    kpi.validate().expect("valid");
    let json = kpi.to_json();
    assert_eq!(json["type"], "KeyPerformanceIndicator");
    assert_eq!(json["name"]["value"], "average-pm10");
    assert_eq!(json["currentValue"]["type"], "Property");
    assert_eq!(json["currentValue"]["value"], 18.4);
    assert_eq!(json["currentValue"]["unitCode"], "GQ");
    assert_eq!(json["currentValue"]["observedAt"], "2026-09-13T08:00:00Z");
    assert_eq!(
        json["calculationPeriod"]["value"]["start"],
        "2026-09-13T07:00:00Z"
    );
    assert_eq!(json["updatedAt"]["value"]["@type"], "DateTime");
    assert_eq!(json["derivedFrom"]["type"], "Relationship");
    assert_eq!(
        json["derivedFrom"]["object"],
        "urn:ngsi-ld:Endpoint:hel.fi:helsinki:helsinki-all"
    );
    assert!(
        json["@context"].is_array(),
        "the context travels on the wire"
    );
    let back = KeyPerformanceIndicator::from_json(json).expect("parses and validates");
    assert_eq!(back, kpi);
}

#[test]
fn a_broker_body_without_context_and_with_many_sources_is_read_back() {
    let mut json = indicator().to_json();
    json.as_object_mut().unwrap().remove("@context");
    json["derivedFrom"]["object"] = json!([
        "urn:ngsi-ld:AirQualityObserved:hel.fi:air-quality:01",
        "urn:ngsi-ld:AirQualityObserved:hel.fi:air-quality:02"
    ]);
    let kpi = KeyPerformanceIndicator::from_json(json).expect("valid");
    assert!(kpi.context.is_none());
    assert_eq!(kpi.derived_from.object.iter().count(), 2);
    assert!(matches!(kpi.derived_from.object, Objects::Many(_)));
}

#[test]
fn an_indicator_outside_a_kpi_space_or_without_provenance_is_refused() {
    let mut wrong_space = indicator();
    wrong_space.id = "urn:ngsi-ld:KeyPerformanceIndicator:hel.fi:helsinki:average-pm10".into();
    assert!(
        wrong_space.validate().is_err(),
        "the space must end with -kpi"
    );

    let mut wrong_type = indicator();
    wrong_type.id = "urn:ngsi-ld:AirQualityObserved:hel.fi:helsinki-kpi:average-pm10".into();
    assert!(wrong_type.validate().is_err(), "the id names the type");

    let mut no_formula = indicator();
    no_formula.calculation_formula.value = "  ".into();
    assert!(no_formula.validate().is_err());

    let mut no_sources = indicator();
    no_sources.derived_from = Relationship::to_all(Vec::new());
    assert!(
        no_sources.validate().is_err(),
        "derivedFrom is required (PF-55)"
    );

    let mut foreign = indicator();
    foreign.computed_by = Relationship::to("https://example.org/not-a-platform-urn");
    assert!(foreign.validate().is_err(), "computedBy is a platform URN");

    let mut renamed = indicator();
    renamed.name.value = "other".into();
    assert!(renamed.validate().is_err(), "the name is the localId");

    let mut bad_unit = indicator();
    bad_unit.current_value.unit_code = Some("a-very-long-unit-code".into());
    assert!(bad_unit.validate().is_err());

    let unknown = json!({ "id": "urn:ngsi-ld:KeyPerformanceIndicator:hel.fi:helsinki-kpi:x", "type": "KeyPerformanceIndicator", "extra": 1 });
    assert!(
        KeyPerformanceIndicator::from_json(unknown).is_err(),
        "unknown fields are refused"
    );
}

#[test]
fn the_context_maps_every_term_and_relationships_are_ids() {
    let context = context();
    let terms = context[0].as_object().expect("the platform's terms");
    for term in [
        "KeyPerformanceIndicator",
        "name",
        "calculationFormula",
        "currentValue",
        "calculationPeriod",
        "updatedAt",
        "derivedFrom",
        "computedBy",
    ] {
        assert!(terms.contains_key(term), "{term} is mapped");
    }
    assert_eq!(terms["derivedFrom"]["@type"], "@id");
    assert_eq!(terms["computedBy"]["@type"], "@id");
    assert!(context[1]
        .as_str()
        .unwrap()
        .contains("ngsi-ld-core-context"));
    let kpi = indicator();
    let urn: Urn = kpi.id.parse().expect("a urn");
    assert_eq!(urn.space(), "helsinki-kpi");
}

/// The committed `schemas/kinds/KeyPerformanceIndicator.json` is what the generator renders,
/// like every catalogued kind's (DM-02 pattern). `UPDATE_SCHEMAS=1` writes it.
#[test]
fn the_committed_schema_is_the_generated_one() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .expect("workspace root")
        .join("schemas/kinds/KeyPerformanceIndicator.json");
    let mut generated = serde_json::to_string_pretty(&schema()).expect("serialize");
    generated.push('\n');
    if std::env::var("UPDATE_SCHEMAS").is_ok() {
        std::fs::write(&path, &generated).expect("written");
    }
    let committed = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("{}: {e} — run with UPDATE_SCHEMAS=1", path.display()));
    assert_eq!(committed, generated, "{} is stale", path.display());
    let parsed: Value = serde_json::from_str(&committed).expect("json");
    let required = parsed["required"].as_array().expect("required");
    for name in [
        "calculationFormula",
        "derivedFrom",
        "computedBy",
        "currentValue",
    ] {
        assert!(required.iter().any(|r| r == name), "{name} is required");
    }
}
