//! Runtime catalogue of every manifest kind (MF-06, CC-12, Architecture/06 section 2).
//!
//! [`ResourceEnvelope`] is generic over one kind, so a
//! kind-generic consumer — the Portal resource API serving `/api/v1/projects/{project}/{plural}`,
//! `jcctl` resolving a file path, the import wizard — cannot reach the per-kind constants
//! through the type system. This module is the reflective view over them: the same
//! `KIND`/`PLURAL`/`SCOPE`/`PATH_TEMPLATE` constants, addressable by string, plus the
//! JSON Schema (draft-07) of each whole manifest.

use crate::envelope::{Kind, ResourceEnvelope, Scope};

/// One row of the kind catalogue.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KindInfo {
    /// Manifest `kind` (e.g. `"ContextSpace"`).
    pub kind: &'static str,
    /// Plural used in the resource API path (e.g. `"spaces"`).
    pub plural: &'static str,
    /// Organization- or project-scoped.
    pub scope: Scope,
    /// Repository path template with `{project}`, `{space}` and `{name}` placeholders.
    pub path_template: &'static str,
    /// Where a kind of scope [`Scope::OrganizationOrProject`] lives inside a project (PF-68);
    /// `None` for every kind that lives in one place.
    pub project_path_template: Option<&'static str>,
    /// Where it lives inside a project when no space owns it (DM-75); `None` for every kind but
    /// `DataModel`.
    pub spaceless_path_template: Option<&'static str>,
}

macro_rules! catalogue {
    ($($spec:ty),+ $(,)?) => {
        /// Every manifest kind of `apiVersion: joinedcontext.com/v1alpha1`, in catalogue order.
        pub const KINDS: &[KindInfo] = &[$(KindInfo {
            kind: <$spec as Kind>::KIND,
            plural: <$spec as Kind>::PLURAL,
            scope: <$spec as Kind>::SCOPE,
            path_template: <$spec as Kind>::PATH_TEMPLATE,
            project_path_template: <$spec as Kind>::PROJECT_PATH_TEMPLATE,
            spaceless_path_template: <$spec as Kind>::SPACELESS_PATH_TEMPLATE,
        }),+];

        /// Parses and validates a manifest of `kind` from YAML without knowing its Rust type.
        ///
        /// Returns `None` for an unknown kind, otherwise the parse-and-validate result.
        /// This is what `jcctl validate` and the Portal import wizard call: they hold a
        /// `kind` string from the file, not a type.
        pub fn validate_yaml(kind: &str, yaml: &str) -> Option<crate::Result<()>> {
            $(if kind == <$spec as Kind>::KIND {
                return Some(
                    ResourceEnvelope::<$spec>::from_yaml(yaml)
                        .map_err(|e| crate::Error::Parse(e.to_string()))
                        .and_then(|m| m.validate()),
                );
            })+
            None
        }

        /// JSON Schema (draft-07, MF-09/DM-03) of the whole manifest of `kind`.
        ///
        /// Returns `None` for an unknown kind. The Portal serves these to
        /// react-jsonschema-form and `jcctl validate` checks manifests against them.
        pub fn schema_of(kind: &str) -> Option<serde_json::Value> {
            $(if kind == <$spec as Kind>::KIND {
                let schema = schemars::schema_for!(ResourceEnvelope<$spec>);
                return serde_json::to_value(schema).ok();
            })+
            None
        }
    };
}

catalogue!(
    crate::kinds::OrganizationSpec,
    crate::kinds::ProjectSpec,
    crate::kinds::ContextSpaceSpec,
    crate::kinds::DataModelSpec,
    crate::kinds::MappingSpec,
    crate::kinds::PolicySpec,
    crate::kinds::ScopeDefinitionSpec,
    crate::kinds::EndpointSpec,
    crate::kinds::ModelProjectionSpec,
    crate::kinds::SharedSpaceReferenceSpec,
    crate::kinds::ContextSourceRegistrationSpec,
    crate::kinds::SubscriptionSpec,
    crate::kinds::ServiceAccountSpec,
    crate::kinds::PipelineSpec,
    crate::kinds::DataSourceSpec,
    crate::kinds::AppSpec,
    crate::kinds::CkanInstanceSpec,
    crate::kinds::BlueprintSpec,
    crate::kinds::AgentProfileSpec,
    crate::kinds::DataSpaceParticipantSpec,
    crate::kinds::DataOfferSpec,
    crate::kinds::DataAgreementSpec,
    crate::kinds::SyncSourceSpec,
    crate::kinds::BundleSpec,
    crate::kinds::UiSchemaSpec,
    crate::kinds::RoleSpec,
    crate::kinds::RoleBindingSpec,
    crate::kinds::GroupSpec,
    crate::kinds::EnvironmentSpec,
    crate::kinds::DashboardSpec,
    crate::kinds::LayerSpec,
);

/// Looks a kind up by its manifest `kind` name (case-sensitive, as written in the file).
pub fn by_kind(kind: &str) -> Option<&'static KindInfo> {
    KINDS.iter().find(|k| k.kind == kind)
}

/// Looks a kind up by the plural of `/api/v1/projects/{project}/{plural}`.
pub fn by_plural(plural: &str) -> Option<&'static KindInfo> {
    KINDS.iter().find(|k| k.plural == plural)
}

impl KindInfo {
    /// Renders the kind's path template for a concrete project, space and resource name.
    ///
    /// `project` is the manifest's namespace, so a kind that lives in either place lands where
    /// its namespace says (PF-68). `space` is ignored by kinds whose template has no `{space}`
    /// placeholder, and an empty one picks the spaceless path of a kind that has one (DM-75).
    pub fn repo_path(&self, project: &str, space: &str, name: &str) -> String {
        let in_project = !project.is_empty() && project != crate::envelope::ORG_NAMESPACE;
        let template = match (self.project_path_template, self.spaceless_path_template) {
            (_, Some(no_space)) if in_project && space.is_empty() => no_space,
            (Some(project_template), _) if in_project => project_template,
            _ => self.path_template,
        };
        template
            .replace("{project}", project)
            .replace("{space}", space)
            .replace("{name}", name)
    }
}
