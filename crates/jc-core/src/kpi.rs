//! The `KeyPerformanceIndicator` entity (T-0582, PF-54, PF-55, Architecture/03 §2).
//!
//! An indicator is data, not configuration: one NGSI-LD entity in the project's indicator
//! space `{project}-kpi`, carrying its value, the formula that made it and the provenance that
//! makes it auditable (PL-36). This module is the one shape every writer renders and the one
//! check every admission runs: the URN names the type and a `-kpi` space, and an indicator
//! without `calculationFormula`, `derivedFrom` or `computedBy` is refused.

use crate::error::{Error, Result};
use crate::names;
use crate::urn::Urn;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

/// The entity type of every indicator.
pub const KPI_TYPE: &str = "KeyPerformanceIndicator";
/// The suffix of a project's indicator space: `{project}-kpi` (PF-54).
pub const KPI_SPACE_SUFFIX: &str = "-kpi";
/// The namespace the platform's own terms expand into.
pub const KPI_NAMESPACE: &str = "https://joinedcontext.com/ns/kpi#";
/// The NGSI-LD core context every indicator is read with.
pub const NGSI_LD_CORE_CONTEXT: &str =
    "https://uri.etsi.org/ngsi-ld/v1/ngsi-ld-core-context-v1.8.jsonld";

/// The indicator space of a project.
pub fn kpi_space(project: &str) -> String {
    format!("{project}{KPI_SPACE_SUFFIX}")
}

/// A Property in normalized NGSI-LD form.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Property<T> {
    /// Always `Property`.
    #[serde(rename = "type")]
    pub kind: PropertyKind,
    /// The value itself.
    pub value: T,
    /// A UN/CEFACT common code, on the value that has a unit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unit_code: Option<String>,
    /// When the value was observed, RFC 3339.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_at: Option<String>,
}

impl<T> Property<T> {
    /// A Property of `value`, without unit or observation time.
    pub fn new(value: T) -> Self {
        Self {
            kind: PropertyKind::Property,
            value,
            unit_code: None,
            observed_at: None,
        }
    }
}

/// The one value `type` takes on a Property.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub enum PropertyKind {
    /// `"Property"`.
    Property,
}

/// A Relationship in normalized NGSI-LD form: one URI or several.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Relationship {
    /// Always `Relationship`.
    #[serde(rename = "type")]
    pub kind: RelationshipKind,
    /// The URI or URIs the relationship points to.
    pub object: Objects,
}

/// The one value `type` takes on a Relationship.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub enum RelationshipKind {
    /// `"Relationship"`.
    Relationship,
}

/// One URI or a list of them, as NGSI-LD writes a relationship's object.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum Objects {
    /// One URI.
    One(String),
    /// A list of URIs.
    Many(Vec<String>),
}

impl Objects {
    /// Every URI, one or many.
    pub fn iter(&self) -> impl Iterator<Item = &str> {
        let slice: &[String] = match self {
            Self::One(one) => std::slice::from_ref(one),
            Self::Many(many) => many,
        };
        slice.iter().map(String::as_str)
    }
}

impl Relationship {
    /// A relationship to one URI.
    pub fn to(object: impl Into<String>) -> Self {
        Self {
            kind: RelationshipKind::Relationship,
            object: Objects::One(object.into()),
        }
    }

    /// A relationship to several URIs.
    pub fn to_all(objects: Vec<String>) -> Self {
        Self {
            kind: RelationshipKind::Relationship,
            object: Objects::Many(objects),
        }
    }
}

/// The window an indicator's value covers, ISO 8601 instants.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Period {
    /// The first instant covered, RFC 3339.
    pub start: String,
    /// The last instant covered, RFC 3339.
    pub end: String,
}

/// An NGSI-LD DateTime value.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DateTime {
    /// Always `DateTime`.
    #[serde(rename = "@type")]
    pub kind: DateTimeKind,
    /// The instant, RFC 3339.
    #[serde(rename = "@value")]
    pub value: String,
}

/// The one value `@type` takes on a DateTime.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub enum DateTimeKind {
    /// `"DateTime"`.
    DateTime,
}

/// The value of an indicator: a number, or a text for an indicator that is a grade or a state.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum IndicatorValue {
    /// A measured or computed number.
    Number(f64),
    /// A grade or a state, when the indicator is not a number.
    Text(String),
}

/// One indicator, normalized NGSI-LD (PF-54, PF-55).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct KeyPerformanceIndicator {
    /// `urn:ngsi-ld:KeyPerformanceIndicator:{orgDomain}:{project}-kpi:{name}`.
    pub id: String,
    /// Always `KeyPerformanceIndicator`.
    #[serde(rename = "type")]
    pub entity_type: String,
    /// The short name, the `{localId}` of the id.
    pub name: Property<String>,
    /// How the value is computed, e.g. `avg(pm10) over AirQualityObserved`.
    pub calculation_formula: Property<String>,
    /// The value as last computed, with its unit code and observation time.
    pub current_value: Property<IndicatorValue>,
    /// The window the value covers.
    pub calculation_period: Property<Period>,
    /// When the value was written.
    pub updated_at: Property<DateTime>,
    /// The Endpoint URN the sources were read through, and the source entity URNs when few.
    pub derived_from: Relationship,
    /// The Pipeline URN or the agent run URN that computed the value.
    pub computed_by: Relationship,
    /// The JSON-LD context, present on the wire and absent inside a broker's response body.
    #[serde(default, rename = "@context", skip_serializing_if = "Option::is_none")]
    pub context: Option<Value>,
}

impl KeyPerformanceIndicator {
    /// An indicator with its provenance, ready to validate and write.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        urn: &Urn,
        formula: &str,
        value: IndicatorValue,
        unit_code: Option<&str>,
        period: Period,
        updated_at: &str,
        derived_from: Relationship,
        computed_by: Relationship,
    ) -> Self {
        let mut current_value = Property::new(value);
        current_value.unit_code = unit_code.map(str::to_owned);
        current_value.observed_at = Some(period.end.clone());
        Self {
            id: urn.to_string(),
            entity_type: KPI_TYPE.to_owned(),
            name: Property::new(urn.local_id().to_owned()),
            calculation_formula: Property::new(formula.to_owned()),
            current_value,
            calculation_period: Property::new(period),
            updated_at: Property::new(DateTime {
                kind: DateTimeKind::DateTime,
                value: updated_at.to_owned(),
            }),
            derived_from,
            computed_by,
            context: Some(context()),
        }
    }

    /// The indicator's URN, mint-checked (PF-42).
    pub fn urn(&self) -> Result<Urn> {
        self.id.parse()
    }

    /// PF-54 and PF-55: the type, the space, the formula and the provenance.
    pub fn validate(&self) -> Result<()> {
        let urn = self.urn()?;
        if urn.entity_type() != KPI_TYPE || self.entity_type != KPI_TYPE {
            return Err(Error::Name {
                field: "type",
                value: self.entity_type.clone(),
                reason: "an indicator is a KeyPerformanceIndicator, in its id and its type",
            });
        }
        if !urn.space().ends_with(KPI_SPACE_SUFFIX) {
            return Err(Error::Name {
                field: "id",
                value: self.id.clone(),
                reason: "an indicator lives in the project's indicator space, named {project}-kpi",
            });
        }
        if self.name.value != urn.local_id() {
            return Err(Error::Name {
                field: "name",
                value: self.name.value.clone(),
                reason: "the name is the {localId} of the id",
            });
        }
        if self.calculation_formula.value.trim().is_empty() {
            return Err(Error::Name {
                field: "calculationFormula",
                value: String::new(),
                reason: "an indicator says how it was computed",
            });
        }
        if let Some(code) = &self.current_value.unit_code {
            if code.trim().is_empty() || code.len() > 8 {
                return Err(Error::Name {
                    field: "currentValue.unitCode",
                    value: code.clone(),
                    reason: "a UN/CEFACT common code, up to 8 characters",
                });
            }
        }
        if self.calculation_period.value.start.is_empty()
            || self.calculation_period.value.end.is_empty()
        {
            return Err(Error::Name {
                field: "calculationPeriod",
                value: String::new(),
                reason: "start and end are both given",
            });
        }
        for (field, relationship) in [
            ("derivedFrom", &self.derived_from),
            ("computedBy", &self.computed_by),
        ] {
            let mut any = false;
            for object in relationship.object.iter() {
                any = true;
                object.parse::<Urn>().map_err(|_| Error::Name {
                    field,
                    value: object.to_owned(),
                    reason: "every object is a platform URN (PF-42)",
                })?;
            }
            if !any {
                return Err(Error::Name {
                    field,
                    value: String::new(),
                    reason: "provenance is required: what it was derived from and what computed it (PF-55)",
                });
            }
        }
        Ok(())
    }

    /// The entity as the wire carries it.
    pub fn to_json(&self) -> Value {
        serde_json::to_value(self).unwrap_or(Value::Null)
    }

    /// An entity read back from a broker or a request body, validated.
    pub fn from_json(value: Value) -> Result<Self> {
        let indicator: Self =
            serde_json::from_value(value).map_err(|e| Error::Parse(e.to_string()))?;
        indicator.validate()?;
        Ok(indicator)
    }
}

/// The URN of a project's indicator (PF-54): type fixed, space `{project}-kpi`.
pub fn indicator_urn(org_domain: &str, project: &str, name: &str) -> Result<Urn> {
    names::validate_local_id(name)?;
    Urn::new(KPI_TYPE, org_domain, &kpi_space(project), name)
}

/// The `@context` the indicator's terms expand with: the NGSI-LD core context and the
/// platform's own terms.
pub fn context() -> Value {
    json!([
        {
            "KeyPerformanceIndicator": format!("{KPI_NAMESPACE}KeyPerformanceIndicator"),
            "name": format!("{KPI_NAMESPACE}name"),
            "calculationFormula": format!("{KPI_NAMESPACE}calculationFormula"),
            "currentValue": format!("{KPI_NAMESPACE}currentValue"),
            "calculationPeriod": format!("{KPI_NAMESPACE}calculationPeriod"),
            "updatedAt": format!("{KPI_NAMESPACE}updatedAt"),
            "derivedFrom": { "@id": format!("{KPI_NAMESPACE}derivedFrom"), "@type": "@id" },
            "computedBy": { "@id": format!("{KPI_NAMESPACE}computedBy"), "@type": "@id" }
        },
        NGSI_LD_CORE_CONTEXT
    ])
}

/// JSON Schema (draft-07) of the entity, what `jcctl schema export` writes.
pub fn schema() -> Value {
    serde_json::to_value(schemars::schema_for!(KeyPerformanceIndicator)).unwrap_or(Value::Null)
}
