//! `kind: Blueprint`, the parameterized template regular users author through
//! (T-0135, CC-23…CC-27, CC-59).
//!
//! A blueprint is one manifest: the parameter schema the form is generated from, the
//! templates that expand into manifests, and the two authorization members the gallery
//! filters on. Keeping them in one object is what lets `spec.version` pin a blueprint
//! (CC-26) and what makes expansion a pure function of `(blueprint, parameters)` (CC-25) —
//! there is no sibling file that could have changed underneath a version.

use crate::envelope::{Kind, ObjectMeta, Scope};
use crate::error::{Error, Result};
use crate::kinds::data_model::SemVer;
use crate::names;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

/// The review lane a blueprint's changes take (CC-59, CC-63).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum RiskClass {
    /// Auto-approved by the policy bot; still committed, attributed and revertible (CC-64).
    Green,
    /// One domain approver (CC-34).
    Yellow,
    /// The full approval chain: cross-domain, public exposure, federation, deletion (CC-63).
    Red,
}

/// One template, rendering exactly one manifest (CC-23).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct BlueprintTemplate {
    /// Stable name of this template; it names the rendered file and appears in diagnostics.
    pub name: String,
    /// The minijinja (Jinja2) source, rendered in a sandbox with no filesystem, no
    /// environment and strict undefined values (ADR-N-005).
    pub template: String,
}

/// `spec` of a Blueprint (CC-23…CC-27, CC-59).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct BlueprintSpec {
    /// Blueprint version; an upgrade never silently rewrites instances (CC-26).
    pub version: SemVer,
    /// Gallery grouping, e.g. `alerting` or `onboarding` (CC-30).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub category: Option<String>,
    /// The merge lane changes from this blueprint take (CC-59, CC-63).
    pub risk_class: RiskClass,
    /// Roles that may see and run this blueprint (CC-59, CC-30).
    pub allowed_roles: Vec<String>,
    /// JSON Schema draft-07 of the parameters: the complete user-facing surface (CC-24).
    pub parameter_schema: serde_json::Value,
    /// The templates this blueprint expands into (CC-23).
    pub templates: Vec<BlueprintTemplate>,
}

impl Kind for BlueprintSpec {
    const KIND: &'static str = "Blueprint";
    const PLURAL: &'static str = "blueprints";
    const SCOPE: Scope = Scope::Organization;
    const PATH_TEMPLATE: &'static str = "blueprints/{name}/blueprint.yaml";

    fn validate_spec(&self, _meta: &ObjectMeta) -> Result<()> {
        self.validate()
    }
}

impl BlueprintSpec {
    /// Validates roles, parameter schema shape and templates (CC-24, CC-59).
    pub fn validate(&self) -> Result<()> {
        if self.allowed_roles.is_empty() {
            return Err(Error::Name {
                field: "spec.allowedRoles",
                value: String::new(),
                reason: "a blueprint nobody may run cannot reach the gallery; name at least one role (CC-59)",
            });
        }
        for role in &self.allowed_roles {
            names::validate_dns1123_label(role)?;
        }
        if let Some(category) = &self.category {
            names::validate_dns1123_label(category)?;
        }

        // CC-24: the parameters are a form, so the schema is an object schema. Anything else
        // (a bare `true`, an array schema, a string) has no fields to render.
        let schema = self.parameter_schema.as_object().ok_or(Error::Name {
            field: "spec.parameterSchema",
            value: self.parameter_schema.to_string(),
            reason: "parameter schema must be a JSON Schema draft-07 object (CC-24)",
        })?;
        if schema.get("type").and_then(serde_json::Value::as_str) != Some("object") {
            return Err(Error::Name {
                field: "spec.parameterSchema.type",
                value: schema
                    .get("type")
                    .map(ToString::to_string)
                    .unwrap_or_default(),
                reason: "parameter schema must declare `type: object`: parameters are the fields of one form (CC-24)",
            });
        }

        if self.templates.is_empty() {
            return Err(Error::Name {
                field: "spec.templates",
                value: String::new(),
                reason: "a blueprint that expands to nothing is not a blueprint (CC-23)",
            });
        }
        let mut seen = BTreeSet::new();
        for template in &self.templates {
            names::validate_dns1123_label(&template.name)?;
            if !seen.insert(&template.name) {
                return Err(Error::Name {
                    field: "spec.templates[].name",
                    value: template.name.clone(),
                    reason: "template names must be unique: they name the rendered manifests",
                });
            }
            if template.template.trim().is_empty() {
                return Err(Error::Name {
                    field: "spec.templates[].template",
                    value: template.name.clone(),
                    reason: "template body must not be empty",
                });
            }
        }
        Ok(())
    }
}
