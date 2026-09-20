//! The named subset of a space's model an Endpoint exposes (MP-01…MP-03, T-0563).
//!
//! One LinkML per Context Space is the truth (DM-01). An Endpoint reads a subset of it, and
//! that subset is this manifest, so the second Endpoint that needs the same view references
//! it instead of retyping it. A projection says what an Endpoint is about; the Policy set
//! still says who may read it, and the gateway intersects the two (MP-02), so a projection
//! narrows and never widens.
//!
//! Two shapes are different on purpose (MP-01): a class absent from `spec.classes` is not
//! exposed at all, a class listed with no slots is exposed with identity only (`id`, `type`).

use crate::envelope::{Kind, ObjectMeta, Scope, TypedRef};
use crate::error::{Error, Result};
use crate::kinds::mapping::DataModelRef;
use crate::names;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

/// One exposed class and the slots it keeps.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct ProjectedClass {
    /// The class (NGSI-LD entity type short name) as the model names it.
    pub name: String,
    /// The slots kept. Empty means identity only: `id` and `type`, nothing else.
    #[serde(default)]
    pub slots: Vec<String>,
}

/// A residual filter in NGSI-LD query syntax, intersected like a REWRITE constraint (GW10).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct ProjectionFilter {
    /// `q` (CIM 009 clause 4.9).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub q: Option<String>,
    /// `scopeQ` (CIM 009 clause 4.19).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope_q: Option<String>,
    /// `geoQ` as one string (`georel`, `geometry`, `coordinates` joined with `;`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub geo_q: Option<String>,
    /// `temporalQ` as one string (`timerel`, `timeAt`, `endTimeAt`, `timeproperty` joined with `;`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temporal_q: Option<String>,
}

impl ProjectionFilter {
    fn is_empty(&self) -> bool {
        self.q.is_none()
            && self.scope_q.is_none()
            && self.geo_q.is_none()
            && self.temporal_q.is_none()
    }

    fn entries(&self) -> [(&'static str, Option<&String>); 4] {
        [
            ("spec.filter.q", self.q.as_ref()),
            ("spec.filter.scopeQ", self.scope_q.as_ref()),
            ("spec.filter.geoQ", self.geo_q.as_ref()),
            ("spec.filter.temporalQ", self.temporal_q.as_ref()),
        ]
    }

    /// Both filters at once: a constraint from each side, conjoined (GW10). `None` stays `None`
    /// only when neither side has one.
    fn intersect(&self, other: &Self) -> Self {
        fn both(a: Option<&String>, b: Option<&String>) -> Option<String> {
            match (a, b) {
                (Some(a), Some(b)) if a == b => Some(a.clone()),
                (Some(a), Some(b)) => Some(format!("({a});({b})")),
                (Some(a), None) | (None, Some(a)) => Some(a.clone()),
                (None, None) => None,
            }
        }
        Self {
            q: both(self.q.as_ref(), other.q.as_ref()),
            scope_q: both(self.scope_q.as_ref(), other.scope_q.as_ref()),
            geo_q: both(self.geo_q.as_ref(), other.geo_q.as_ref()),
            temporal_q: both(self.temporal_q.as_ref(), other.temporal_q.as_ref()),
        }
    }
}

/// Desired specification of a `ModelProjection` resource (MP-01).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct ModelProjectionSpec {
    /// DNS-1123 label of the owning Context Space; with `metadata.namespace` it derives the
    /// repository path (MF-06).
    pub context_space_ref: String,
    /// The model this is a subset of: one DataModel of the space at one served major (DM-22).
    pub data_model_ref: DataModelRef,
    /// The exposed classes. A class not listed is not exposed.
    pub classes: Vec<ProjectedClass>,
    /// Optional residual filter.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filter: Option<ProjectionFilter>,
}

impl Kind for ModelProjectionSpec {
    const KIND: &'static str = "ModelProjection";
    const PLURAL: &'static str = "projections";
    const SCOPE: Scope = Scope::Project;
    const PATH_TEMPLATE: &'static str = "projects/{project}/spaces/{space}/projections/{name}.yaml";

    fn validate_spec(&self, meta: &ObjectMeta) -> Result<()> {
        names::validate_dns1123_label(&meta.name)?;
        self.validate()
    }

    fn context_space(&self) -> Option<&str> {
        Some(&self.context_space_ref)
    }
}

impl ModelProjectionSpec {
    /// Validates the shape alone: names well formed, nothing listed twice, no empty filter.
    /// Whether the names exist in the model is [`Self::check_against`].
    pub fn validate(&self) -> Result<()> {
        names::validate_dns1123_label(&self.context_space_ref)?;
        self.data_model_ref.validate("spec.dataModelRef")?;
        if self.classes.is_empty() {
            // A projection of nothing is an Endpoint that exposes nothing and looks like one
            // that works; the person meant a class list and forgot it.
            return Err(Error::Name {
                field: "spec.classes",
                value: String::new(),
                reason: "a projection exposes at least one class (MP-01)",
            });
        }
        let mut seen = BTreeSet::new();
        for class in &self.classes {
            names::validate_entity_type(&class.name)?;
            if !seen.insert(class.name.as_str()) {
                return Err(Error::Name {
                    field: "spec.classes",
                    value: class.name.clone(),
                    reason: "a class is listed once; two entries would mean two slot lists",
                });
            }
            let mut slots = BTreeSet::new();
            for slot in &class.slots {
                if slot.is_empty() || !slot.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
                    return Err(Error::Name {
                        field: "spec.classes.slots",
                        value: slot.clone(),
                        reason: "a slot is a LinkML slot name (letters, digits, underscore)",
                    });
                }
                if !slots.insert(slot.as_str()) {
                    return Err(Error::Name {
                        field: "spec.classes.slots",
                        value: slot.clone(),
                        reason: "a slot is listed once",
                    });
                }
            }
        }
        if let Some(filter) = &self.filter {
            if filter.is_empty() {
                return Err(Error::Name {
                    field: "spec.filter",
                    value: String::new(),
                    reason: "a filter carries at least one of q, scopeQ, geoQ, temporalQ; leave \
                             it out otherwise",
                });
            }
            for (field, value) in filter.entries() {
                if value.is_some_and(|v| v.trim().is_empty()) {
                    return Err(Error::Name {
                        field,
                        value: String::new(),
                        reason: "an empty query string filters nothing; leave the member out",
                    });
                }
            }
        }
        Ok(())
    }

    /// Checks every class and slot against the model's own, as `jcctl plan` and CI do (MP-01):
    /// `model` maps each class name to its slot names. Every offending name is listed at once.
    pub fn check_against(&self, model: &BTreeMap<String, BTreeSet<String>>) -> Result<()> {
        let mut missing = Vec::new();
        for class in &self.classes {
            match model.get(&class.name) {
                None => missing.push(class.name.clone()),
                Some(slots) => missing.extend(
                    class
                        .slots
                        .iter()
                        .filter(|slot| !slots.contains(*slot))
                        .map(|slot| format!("{}.{slot}", class.name)),
                ),
            }
        }
        if missing.is_empty() {
            Ok(())
        } else {
            Err(Error::Name {
                field: "spec.classes",
                value: missing.join(", "),
                reason: "not in the referenced model version (MP-01)",
            })
        }
    }

    /// The projection two projections make together on one Endpoint: the classes in both,
    /// each with the slots in both, the filters conjoined (MP-02). Never wider than either.
    pub fn intersect(&self, other: &Self) -> Self {
        let theirs: BTreeMap<&str, &ProjectedClass> =
            other.classes.iter().map(|c| (c.name.as_str(), c)).collect();
        let classes = self
            .classes
            .iter()
            .filter_map(|mine| {
                theirs.get(mine.name.as_str()).map(|that| ProjectedClass {
                    name: mine.name.clone(),
                    slots: mine
                        .slots
                        .iter()
                        .filter(|slot| that.slots.contains(slot))
                        .cloned()
                        .collect(),
                })
            })
            .collect();
        let filter = match (&self.filter, &other.filter) {
            (None, None) => None,
            (Some(a), None) | (None, Some(a)) => Some(a.clone()),
            (Some(a), Some(b)) => Some(a.intersect(b)),
        };
        Self {
            context_space_ref: self.context_space_ref.clone(),
            data_model_ref: self.data_model_ref.clone(),
            classes,
            filter,
        }
    }

    /// The attributes a class keeps under this projection: `None` when the class is not exposed,
    /// the slots plus `id` and `type` otherwise (identity is never projected away).
    pub fn attributes_of(&self, class: &str) -> Option<BTreeSet<String>> {
        self.classes.iter().find(|c| c.name == class).map(|c| {
            let mut set: BTreeSet<String> = c.slots.iter().cloned().collect();
            set.insert("id".to_owned());
            set.insert("type".to_owned());
            set
        })
    }

    /// The same projection as a typed reference an Endpoint carries.
    pub fn reference(name: &str) -> TypedRef {
        TypedRef {
            kind: Self::KIND.to_owned(),
            name: name.to_owned(),
            namespace: None,
        }
    }
}
