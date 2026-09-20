//! Blueprint expansion: parameters in, manifests out (T-0135, CC-23, CC-24, CC-25, CC-27).
//!
//! Expansion is a pure function. It reads nothing but the blueprint and the parameters it is
//! handed: the template engine runs with no filesystem, no environment and no host access
//! (ADR-N-005), and an undefined variable is an error rather than an empty string. The same
//! blueprint version and the same parameters therefore render byte-identical manifests
//! (CC-25), which is what makes a re-render a reviewable diff instead of noise.

use crate::loader::RawManifest;
use jc_core::kinds::Blueprint;
use minijinja::{Environment, UndefinedBehavior};
use serde_json::Value;

/// Annotation naming the blueprint a manifest was rendered from (CC-27).
pub const ANNOTATION_BLUEPRINT: &str = "joinedcontext.com/blueprint";
/// Annotation naming the blueprint version (CC-26, CC-27).
pub const ANNOTATION_VERSION: &str = "joinedcontext.com/blueprint-version";
/// Annotation carrying the parameter values as canonical JSON (CC-27, CC-32).
pub const ANNOTATION_PARAMETERS: &str = "joinedcontext.com/blueprint-parameters";

/// One rendered manifest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Expanded {
    /// `spec.templates[].name` it came from; it names the file to write.
    pub template: String,
    /// The manifest as YAML, provenance annotations included.
    pub manifest: String,
}

/// Why an expansion did not produce manifests.
#[derive(Debug, thiserror::Error)]
pub enum ExpandError {
    /// `spec.parameterSchema` is not a schema any validator can compile.
    #[error("spec.parameterSchema is not a valid JSON Schema: {0}")]
    Schema(String),
    /// The parameters do not satisfy the schema; every violation is reported at once so a
    /// form can show them together (CC-24).
    #[error("parameters do not match the blueprint's schema: {}", .0.join("; "))]
    Parameters(Vec<String>),
    /// A template did not render (unknown variable, syntax error, failed filter).
    #[error("template `{template}` did not render: {source}")]
    Render {
        /// Name of the template that failed.
        template: String,
        /// The engine's own diagnosis.
        source: minijinja::Error,
    },
    /// A template rendered something that is not a manifest.
    #[error("template `{template}` rendered something that is not a manifest: {reason}")]
    NotAManifest {
        /// Name of the template that failed.
        template: String,
        /// Parser message.
        reason: String,
    },
}

/// Expands `blueprint` with `parameters` into one manifest per template.
///
/// The parameters are validated against `spec.parameterSchema` first: a parameter set that
/// validates always renders valid manifests, so a rejection here is the only place a user
/// error surfaces (CC-24).
pub fn expand(blueprint: &Blueprint, parameters: &Value) -> Result<Vec<Expanded>, ExpandError> {
    let validator = jsonschema::draft7::new(&blueprint.spec.parameter_schema)
        .map_err(|e| ExpandError::Schema(e.to_string()))?;
    let errors: Vec<String> = validator
        .iter_errors(parameters)
        .map(|e| {
            let at = e.instance_path().to_string();
            if at.is_empty() {
                e.to_string()
            } else {
                format!("{at}: {e}")
            }
        })
        .collect();
    if !errors.is_empty() {
        return Err(ExpandError::Parameters(errors));
    }

    // Canonical JSON: serde_json keeps object members sorted, so the provenance annotation of
    // a re-render is byte-identical to the first one (CC-25, CC-27).
    let provenance = serde_json::to_string(parameters).unwrap_or_else(|_| "{}".to_string());

    let mut env = Environment::new();
    // No loader is installed, so `{% include %}` and `{% extends %}` have nowhere to reach;
    // strict undefined turns a typo in a parameter name into an error instead of a silent
    // empty string (ADR-N-005).
    env.set_undefined_behavior(UndefinedBehavior::Strict);

    let mut out = Vec::with_capacity(blueprint.spec.templates.len());
    for template in &blueprint.spec.templates {
        let rendered = env
            .render_str(&template.template, parameters)
            .map_err(|source| ExpandError::Render {
                template: template.name.clone(),
                source,
            })?;
        let mut manifest: RawManifest =
            serde_norway::from_str(&rendered).map_err(|e| ExpandError::NotAManifest {
                template: template.name.clone(),
                reason: e.to_string(),
            })?;
        annotate(
            &mut manifest,
            &blueprint.metadata.name,
            blueprint.spec.version.as_str(),
            &provenance,
        );
        let manifest =
            serde_norway::to_string(&manifest).map_err(|e| ExpandError::NotAManifest {
                template: template.name.clone(),
                reason: e.to_string(),
            })?;
        out.push(Expanded {
            template: template.name.clone(),
            manifest,
        });
    }
    Ok(out)
}

/// Writes the three provenance annotations into `metadata.annotations` (CC-27).
///
/// `RawMetadata` keeps everything but name and namespace in its `rest` map, so the
/// annotations block is created when the template did not write one.
fn annotate(manifest: &mut RawManifest, blueprint: &str, version: &str, parameters: &str) {
    let annotations = manifest
        .metadata
        .rest
        .entry("annotations")
        .or_insert_with(|| Value::Object(serde_json::Map::new()));
    if !annotations.is_object() {
        *annotations = Value::Object(serde_json::Map::new());
    }
    let annotations = annotations
        .as_object_mut()
        .expect("annotations was just made an object");
    for (key, value) in [
        (ANNOTATION_BLUEPRINT, blueprint),
        (ANNOTATION_VERSION, version),
        (ANNOTATION_PARAMETERS, parameters),
    ] {
        annotations.insert(key.to_string(), Value::String(value.to_string()));
    }
}
