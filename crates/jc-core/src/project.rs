//! A project in a repository of its own: the registry entry, the parameters and the layout
//! (layout 2, ADR-N-029; PF-85, PF-86, CC-85, CC-86, CC-88).
//!
//! The organization repository holds one registry entry per project, `projects/{slug}.yaml`,
//! a `Project` with `spec.repository`, `spec.ref` and the parameter values of this deployment.
//! The project repository holds the project file, a `Project` without a repository, with the
//! version and the parameter declarations. This module holds what turns the two into the
//! manifests of layout 1: the parameters resolved and rendered, the layout read, and a project
//! repository's manifests mounted under the registry slug. Fetching the checkouts is `jcctl`'s.

use crate::envelope::{Scope, SecretRef};
use crate::error::{Error, Result};
use crate::kinds::ProjectSpec;
use crate::names;
use regex::Regex;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::sync::LazyLock;

/// The file both kinds of repository say their layout in (CC-85).
pub const LAYOUT_FILE: &str = ".jc/layout";

/// Every layout a loader of this release reads: 1, one repository; 2, one per project.
pub const LAYOUTS: &[u32] = &[1, 2];

/// Where a project repository's own `Project` sits in it.
pub const PROJECT_FILE: &str = "project.yaml";

/// The prefix of the environment variable a mapping reads a parameter from (CC-88).
pub const ENV_PREFIX: &str = "JC_PARAM_";

/// The placeholder a project-scoped manifest may write where its project belongs (CC-82).
pub const PROJECT_PLACEHOLDER: &str = "{project}";

static PARAMETER_NAME: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[a-z][A-Za-z0-9]{0,62}$").expect("valid regex"));
static PLACEHOLDER: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\{param:([^}]*)\}").expect("valid regex"));
static ENV_READ: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"env\(\s*"(JC_PARAM_[A-Za-z0-9_]*)"\s*\)"#).expect("valid regex")
});

/// Where a registry entry's project comes from (PF-86, CC-89).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct ProjectRepository {
    /// A repository of the organization in the local forge.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// A git repository outside the forge, mirrored read-only at the pinned ref (CC-89).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// The credential that reads `url`; a local repository needs none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secret_ref: Option<SecretRef>,
}

impl ProjectRepository {
    /// Exactly one of a local name and an `https` URL without credentials in it.
    pub fn validate(&self) -> Result<()> {
        match (&self.name, &self.url) {
            (Some(name), None) => {
                names::validate_dns1123_label(name)?;
                if self.secret_ref.is_some() {
                    return Err(invalid(
                        "spec.repository.secretRef",
                        "a repository of the local forge is read with the Portal's own forge \
                         token; a secretRef goes with a url only",
                    ));
                }
                Ok(())
            }
            (None, Some(url)) => {
                validate_url(url)?;
                match &self.secret_ref {
                    Some(secret) => secret.validate("spec.repository.secretRef"),
                    None => Ok(()),
                }
            }
            _ => Err(invalid(
                "spec.repository",
                "names either a repository of the forge (`name`) or one outside it (`url`), \
                 exactly one of the two",
            )),
        }
    }
}

/// Neither the credential nor the URL is repeated: a pasted URL may carry a live password.
fn validate_url(url: &str) -> Result<()> {
    let Some(rest) = url.strip_prefix("https://") else {
        return Err(invalid(
            "spec.repository.url",
            "must be an https URL; a plain-http or ssh remote is not mirrored",
        ));
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    if authority.contains('@') {
        return Err(invalid(
            "spec.repository.url",
            "carries credentials; the URL names the repository and `secretRef` names the \
             secret that reads it (MF-24)",
        ));
    }
    if authority.is_empty() || url.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err(invalid("spec.repository.url", "is not a repository URL"));
    }
    Ok(())
}

/// A parameter entry of `spec.parameters`: a declaration in the project file, a value in a
/// registry entry (CC-88).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum Parameter {
    /// What the project file declares: a type, and optionally a default, a description and the
    /// allowed values.
    Declaration(ParameterDeclaration),
    /// What a registry entry sets for this deployment.
    Value(ParameterValue),
}

/// The declaration of one deployment knob (CC-88), a subset of JSON Schema draft-07.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct ParameterDeclaration {
    /// What the value is; `secret` takes the name of a secret, never its value.
    #[serde(rename = "type")]
    pub kind: ParameterType,
    /// The value when the registry entry sets none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<ParameterValue>,
    /// What the knob does, for the parameter form.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// The only values allowed; empty allows every value of the type.
    #[serde(default, rename = "enum", skip_serializing_if = "Vec::is_empty")]
    pub allowed: Vec<ParameterValue>,
}

/// The type of a parameter (CC-88).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum ParameterType {
    /// Text.
    String,
    /// A whole number.
    Integer,
    /// Any number.
    Number,
    /// `true` or `false`.
    Boolean,
    /// The name of a secret in the store, set per deployment; renders as a `secretRef` name.
    Secret,
}

impl ParameterType {
    fn as_str(self) -> &'static str {
        match self {
            Self::String => "string",
            Self::Integer => "integer",
            Self::Number => "number",
            Self::Boolean => "boolean",
            Self::Secret => "secret",
        }
    }
}

/// One scalar a parameter takes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum ParameterValue {
    /// `true` or `false`.
    Boolean(bool),
    /// A number, whole or not.
    Number(serde_json::Number),
    /// Text, or a secret's name.
    String(String),
}

impl ParameterValue {
    fn fits(&self, kind: ParameterType) -> bool {
        match (kind, self) {
            (ParameterType::String, Self::String(_)) => true,
            (ParameterType::Secret, Self::String(name)) => is_secret_name(name),
            (ParameterType::Integer, Self::Number(n)) => n.is_i64() || n.is_u64(),
            (ParameterType::Number, Self::Number(_)) => true,
            (ParameterType::Boolean, Self::Boolean(_)) => true,
            _ => false,
        }
    }

    fn to_json(&self) -> Value {
        match self {
            Self::Boolean(b) => Value::Bool(*b),
            Self::Number(n) => Value::Number(n.clone()),
            Self::String(s) => Value::String(s.clone()),
        }
    }

    fn to_text(&self) -> String {
        match self {
            Self::Boolean(b) => b.to_string(),
            Self::Number(n) => n.to_string(),
            Self::String(s) => s.clone(),
        }
    }
}

fn is_secret_name(name: &str) -> bool {
    names::validate_dns1123_label(name).is_ok() && !names::looks_like_a_credential(name)
}

fn invalid(field: impl Into<String>, reason: impl Into<String>) -> Error {
    Error::Invalid {
        field: field.into(),
        reason: reason.into(),
    }
}

/// Checks the parameters of a project file or of a registry entry (CC-88). `entry` says which:
/// an entry sets values and a project file declares them, and neither holds the other's.
pub(crate) fn validate_parameters(
    parameters: &BTreeMap<String, Parameter>,
    entry: bool,
) -> Result<()> {
    for (name, parameter) in parameters {
        let field = format!("spec.parameters.{name}");
        if !PARAMETER_NAME.is_match(name) {
            return Err(invalid(
                field,
                "a parameter name is an identifier: a lower-case letter, then letters and \
                 digits, at most 63 characters",
            ));
        }
        match (parameter, entry) {
            (Parameter::Value(_), true) => {}
            (Parameter::Declaration(declaration), false) => {
                validate_declaration(&field, declaration)?
            }
            (Parameter::Declaration(_), true) => {
                return Err(invalid(
                    field,
                    "a registry entry sets a value; the declaration belongs in the project's \
                     project.yaml",
                ))
            }
            (Parameter::Value(_), false) => {
                return Err(invalid(
                    field,
                    "the project file declares a parameter as { type, default }; its value for a \
                     deployment is set on the registry entry",
                ))
            }
        }
    }
    Ok(())
}

fn validate_declaration(field: &str, declaration: &ParameterDeclaration) -> Result<()> {
    let kind = declaration.kind;
    if kind == ParameterType::Secret && !declaration.allowed.is_empty() {
        return Err(invalid(
            field,
            "a secret parameter names a secret of the deployment and takes no enum",
        ));
    }
    if let Some(default) = &declaration.default {
        if !default.fits(kind) {
            return Err(mismatch(field, kind, "the default"));
        }
    }
    for allowed in &declaration.allowed {
        if !allowed.fits(kind) {
            return Err(mismatch(field, kind, "a value of its enum"));
        }
    }
    if let Some(default) = &declaration.default {
        if !declaration.allowed.is_empty() && !declaration.allowed.contains(default) {
            return Err(invalid(
                field,
                "the default is not one of the values of its enum",
            ));
        }
    }
    Ok(())
}

/// A value that does not fit its type. A secret's value is never repeated: what was typed
/// there may be the secret itself.
fn mismatch(field: &str, kind: ParameterType, what: &str) -> Error {
    let reason = if kind == ParameterType::Secret {
        format!(
            "{what} must name a secret in the store, a DNS-1123 label that is not itself a \
             credential (MF-24)"
        )
    } else {
        format!("{what} is not of type {}", kind.as_str())
    };
    invalid(field, reason)
}

/// The parameters of one deployment of a project: every declared parameter with its value,
/// the registry entry's over the default (CC-88).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Parameters {
    resolved: BTreeMap<String, (ParameterType, ParameterValue)>,
    defaults: BTreeMap<String, ParameterValue>,
}

/// Resolves the declarations of `project` (the project file) with the values of `entry` (its
/// registry entry). An undeclared value, a value that does not fit its declaration, and a
/// declared parameter with neither a value nor a default are refused by name.
pub fn resolve_parameters(project: &ProjectSpec, entry: &ProjectSpec) -> Result<Parameters> {
    if project.is_registry_entry() || !entry.is_registry_entry() {
        return Err(invalid(
            "spec.parameters",
            "resolves a project file (no spec.repository) with its registry entry (with one)",
        ));
    }
    let mut parameters = Parameters::default();
    for name in entry.parameters.keys() {
        if !project.parameters.contains_key(name) {
            return Err(invalid(
                format!("spec.parameters.{name}"),
                "the project declares no such parameter in its project.yaml",
            ));
        }
    }
    for (name, parameter) in &project.parameters {
        let Parameter::Declaration(declaration) = parameter else {
            continue;
        };
        let field = format!("spec.parameters.{name}");
        let value = match entry.parameters.get(name) {
            Some(Parameter::Value(value)) => value.clone(),
            Some(Parameter::Declaration(_)) => {
                return Err(invalid(
                    field,
                    "a registry entry sets a value, not a declaration",
                ))
            }
            None => declaration.default.clone().ok_or_else(|| {
                invalid(
                    field.clone(),
                    "has no default, and the registry entry sets no value for it",
                )
            })?,
        };
        if !value.fits(declaration.kind) {
            return Err(mismatch(&field, declaration.kind, "the value"));
        }
        if !declaration.allowed.is_empty() && !declaration.allowed.contains(&value) {
            return Err(invalid(
                field,
                "the value is not one of the values of its enum",
            ));
        }
        if let Some(default) = &declaration.default {
            parameters.defaults.insert(name.clone(), default.clone());
        }
        parameters
            .resolved
            .insert(name.clone(), (declaration.kind, value));
    }
    Ok(parameters)
}

impl Parameters {
    /// Replaces every `{param:name}` in every string of `value`, however deep. A string that is
    /// one placeholder alone takes the parameter's own JSON type; inside a longer string it
    /// takes its text. A secret parameter renders the secret's name, and only alone. A
    /// placeholder naming no parameter is refused with where it stands.
    pub fn render(&self, value: &mut Value) -> Result<()> {
        self.render_at(value, &mut String::new())
    }

    fn render_at(&self, value: &mut Value, path: &mut String) -> Result<()> {
        match value {
            Value::String(text) => {
                if let Some(rendered) = self.render_string(text, path)? {
                    *value = rendered;
                }
            }
            Value::Array(items) => {
                for (index, item) in items.iter_mut().enumerate() {
                    let length = path.len();
                    path.push_str(&format!("[{index}]"));
                    self.render_at(item, path)?;
                    path.truncate(length);
                }
            }
            Value::Object(members) => {
                for (key, item) in members.iter_mut() {
                    let length = path.len();
                    if !path.is_empty() {
                        path.push('.');
                    }
                    path.push_str(key);
                    self.render_at(item, path)?;
                    path.truncate(length);
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn render_string(&self, text: &str, path: &str) -> Result<Option<Value>> {
        if !text.contains("{param:") {
            return Ok(None);
        }
        let mut out = String::new();
        let mut last = 0;
        for capture in PLACEHOLDER.captures_iter(text) {
            let whole = capture.get(0).expect("group 0 is the match");
            let name = &capture[1];
            let (kind, parameter) = self.resolved.get(name).ok_or_else(|| {
                invalid(
                    path,
                    format!("`{{param:{name}}}` names no parameter the project declares"),
                )
            })?;
            if whole.as_str() == text {
                return Ok(Some(parameter.to_json()));
            }
            if *kind == ParameterType::Secret {
                return Err(invalid(
                    path,
                    format!(
                        "the secret parameter `{name}` renders the name of a secretRef and \
                         stands alone in its field"
                    ),
                ));
            }
            out.push_str(&text[last..whole.start()]);
            out.push_str(&parameter.to_text());
            last = whole.end();
        }
        out.push_str(&text[last..]);
        Ok(Some(Value::String(out)))
    }

    /// The environment a mapping of this project reads, `JC_PARAM_<NAME>` per parameter. A
    /// secret parameter is not in it: its value is a secret's name, resolved by a `secretRef`.
    pub fn env(&self) -> BTreeMap<String, String> {
        self.resolved
            .iter()
            .filter(|(_, (kind, _))| *kind != ParameterType::Secret)
            .map(|(name, (_, value))| (env_name(name), value.to_text()))
            .collect()
    }

    /// Every `env("JC_PARAM_…")` of a Bloblang `mapping` that [`Self::env`] does not provide,
    /// in the order they are written, each once.
    pub fn unknown_in_mapping(&self, mapping: &str) -> Vec<String> {
        let env = self.env();
        let mut unknown = Vec::new();
        for capture in ENV_READ.captures_iter(mapping) {
            let name = capture[1].to_owned();
            if !env.contains_key(&name) && !unknown.contains(&name) {
                unknown.push(name);
            }
        }
        unknown
    }

    /// Every string of `value` that is the literal value or default of a declared string
    /// parameter, with the placeholder to write there instead (CC-83, CC-88). Only a whole
    /// string is a finding, never a word inside one; a number is not, and a secret's name is
    /// not. Read before [`Self::render`], on the file as written.
    pub fn literals(&self, value: &Value) -> Vec<LiteralParameter> {
        let mut literals = Vec::new();
        self.literals_at(value, &mut String::new(), &mut literals);
        literals
    }

    fn literals_at(&self, value: &Value, path: &mut String, found: &mut Vec<LiteralParameter>) {
        match value {
            Value::String(text) if !text.is_empty() => {
                let parameter = self.resolved.iter().find(|(name, (kind, value))| {
                    *kind == ParameterType::String
                        && (value == &ParameterValue::String(text.clone())
                            || self.defaults.get(*name)
                                == Some(&ParameterValue::String(text.clone())))
                });
                if let Some((name, _)) = parameter {
                    found.push(LiteralParameter {
                        path: path.clone(),
                        parameter: name.clone(),
                        placeholder: format!("{{param:{name}}}"),
                    });
                }
            }
            Value::Array(items) => {
                for (index, item) in items.iter().enumerate() {
                    let length = path.len();
                    path.push_str(&format!("[{index}]"));
                    self.literals_at(item, path, found);
                    path.truncate(length);
                }
            }
            Value::Object(members) => {
                for (key, item) in members {
                    let length = path.len();
                    if !path.is_empty() {
                        path.push('.');
                    }
                    path.push_str(key);
                    self.literals_at(item, path, found);
                    path.truncate(length);
                }
            }
            _ => {}
        }
    }
}

/// A literal written where a parameter exists (CC-88).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiteralParameter {
    /// Where it stands, `spec.sources[0].host`.
    pub path: String,
    /// The parameter whose value it is.
    pub parameter: String,
    /// What to write instead, `{param:feedHost}`.
    pub placeholder: String,
}

/// The variable a mapping reads parameter `name` from: `feedHost` is `JC_PARAM_FEED_HOST`.
pub fn env_name(name: &str) -> String {
    let mut env = String::from(ENV_PREFIX);
    for c in name.chars() {
        if c.is_ascii_uppercase() {
            env.push('_');
        }
        env.push(c.to_ascii_uppercase());
    }
    env
}

/// The registry entry of the project `slug`: `projects/{slug}.yaml` in the organization
/// repository (PF-86). The slug is a DNS-1123 label, because it is the project's name.
pub fn registry_path(slug: &str) -> Result<String> {
    names::validate_dns1123_label(slug)?;
    Ok(format!("projects/{slug}.yaml"))
}

/// Which of the two kinds of repository a checkout is (PF-85).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RepositoryRole {
    /// The organization repository: layout 1 holds everything, layout 2 the registry.
    Organization,
    /// A project repository, which exists in layout 2 only.
    Project,
}

/// The layout a checkout follows, from the contents of its `.jc/layout`, or `None` when it has
/// none (CC-85). An organization repository without the file is layout 1, the layout every
/// repository had before the file existed; a project repository says `2`, and a number no
/// loader of this release knows is refused.
pub fn layout_of(file: Option<&str>, role: RepositoryRole) -> Result<u32> {
    let known = LAYOUTS
        .iter()
        .map(u32::to_string)
        .collect::<Vec<_>>()
        .join(", ");
    let layout = match file {
        None if role == RepositoryRole::Organization => return Ok(1),
        None => {
            return Err(invalid(
                LAYOUT_FILE,
                "a project repository carries its layout; write `2` into .jc/layout",
            ))
        }
        Some(text) => text
            .trim()
            .parse::<u32>()
            .ok()
            .filter(|layout| LAYOUTS.contains(layout))
            .ok_or_else(|| {
                invalid(
                    LAYOUT_FILE,
                    format!(
                        "names no layout this release reads (it reads {known}); run \
                         `jcctl migrate` with a release that knows it"
                    ),
                )
            })?,
    };
    if role == RepositoryRole::Project && layout != 2 {
        return Err(invalid(
            LAYOUT_FILE,
            "a project repository exists in layout 2 only; it says 2",
        ));
    }
    Ok(layout)
}

/// Mounts one manifest of a project repository under the registry slug (CC-86): a
/// project-scoped manifest takes the slug as its namespace, and the project's own `Project`
/// takes it as its name, so one repository runs as two projects under two entries. A namespace
/// or a project name that is already written must be the slug or `{project}`; an
/// organization-level kind has no place in a project repository.
pub fn mount(manifest: &mut Value, slug: &str) -> Result<()> {
    names::validate_dns1123_label(slug)?;
    let kind = manifest
        .get("kind")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let (field, value) = if kind == "Project" {
        ("name", slug)
    } else {
        match crate::registry::by_kind(&kind).map(|info| info.scope) {
            Some(Scope::Project | Scope::OrganizationOrProject) => ("namespace", slug),
            Some(Scope::Organization) => {
                return Err(invalid(
                    "kind",
                    format!(
                        "`{kind}` belongs to the organization repository, not to the project \
                         repository of `{slug}`"
                    ),
                ))
            }
            None => return Err(invalid("kind", format!("`{kind}` is not a manifest kind"))),
        }
    };
    let metadata = manifest
        .as_object_mut()
        .ok_or_else(|| invalid("metadata", "a manifest is a mapping"))?
        .entry("metadata")
        .or_insert_with(|| Value::Object(Default::default()));
    let metadata = metadata
        .as_object_mut()
        .ok_or_else(|| invalid("metadata", "is a mapping"))?;
    match metadata.get(field).and_then(Value::as_str) {
        None | Some(PROJECT_PLACEHOLDER) => {}
        Some(written) if written == slug => {}
        Some(written) => {
            return Err(invalid(
                format!("metadata.{field}"),
                format!(
                    "is `{written}`, and this repository runs as the project `{slug}`; write \
                     `{PROJECT_PLACEHOLDER}` or leave it out (CC-82)"
                ),
            ))
        }
    }
    metadata.insert(field.to_owned(), Value::String(value.to_owned()));
    Ok(())
}
