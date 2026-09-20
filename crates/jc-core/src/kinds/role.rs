//! `kind: Role` and `kind: RoleBinding`: who may change configuration, declared in `users/`
//! of the Organization repository (T-0525, PF-49…PF-52, Architecture/12 §2a).
//!
//! A role is a list of rules (kinds × verbs, optionally constrained on spec fields); a binding
//! gives a role to humans and groups inside one scope for a validity window. Both carry no
//! secret: subjects are names, never tokens.

use crate::envelope::{Kind, ObjectMeta, Scope};
use crate::error::{Error, Result};
use crate::kinds::service_account::RoleScope;
use crate::names;
use chrono::{DateTime, Utc};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// What a rule allows on its kinds (PF-49, PF-59).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum Verb {
    /// Read and list a manifest of the kind.
    Read,
    /// Create or edit a manifest, opening a Change.
    Propose,
    /// Approve a Change of the kind.
    Approve,
    /// Delete a manifest.
    Delete,
}

/// A constraint on one spec field; exactly one of `in`, `notIn`, `equals` is given.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Constraint {
    /// Dotted path from the manifest root, e.g. `spec.audience`.
    pub field: String,
    /// The value must be one of these.
    #[serde(default, rename = "in", skip_serializing_if = "Vec::is_empty")]
    pub one_of: Vec<String>,
    /// The value must be none of these.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub not_in: Vec<String>,
    /// The value must equal this.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub equals: Option<String>,
}

impl Rule {
    /// Whether the rule grants `verb` on `kind`. `propose` implies `read`, because nobody
    /// proposes a change to what they may not see; `approve` and `delete` imply nothing, so a
    /// role that only approves reads nothing by that rule alone (PF-59). The implication lives
    /// here alone, so the Portal, jcctl's policy compiler and Conftest cannot disagree.
    pub fn grants(&self, kind: &str, verb: Verb) -> bool {
        if !self.kinds.iter().any(|k| k == kind) {
            return false;
        }
        self.verbs.contains(&verb) || (verb == Verb::Read && self.verbs.contains(&Verb::Propose))
    }
}

/// One rule: the verbs allowed on the kinds, under the constraints.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Rule {
    /// Manifest kinds the rule covers, e.g. `Pipeline`.
    pub kinds: Vec<String>,
    /// Verbs granted on those kinds.
    pub verbs: Vec<Verb>,
    /// Constraints every covered manifest must satisfy for the rule to apply.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub constraints: Vec<Constraint>,
}

/// `spec` of a Role (PF-49).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct RoleSpec {
    /// The rules; a role with none grants nothing and is refused.
    pub rules: Vec<Rule>,
}

impl Kind for RoleSpec {
    const KIND: &'static str = "Role";
    const PLURAL: &'static str = "roles";
    /// A role belongs to the organization or to one project (PF-68).
    const SCOPE: Scope = Scope::OrganizationOrProject;
    const PATH_TEMPLATE: &'static str = "users/roles/{name}.yaml";
    const PROJECT_PATH_TEMPLATE: Option<&'static str> =
        Some("projects/{project}/roles/{name}.yaml");

    fn validate_spec(&self, meta: &ObjectMeta) -> Result<()> {
        names::validate_dns1123_label(&meta.name)?;
        self.validate()?;
        if meta.namespace.as_deref() != Some(crate::envelope::ORG_NAMESPACE) {
            self.validate_in_project()?;
        }
        Ok(())
    }
}

impl RoleSpec {
    /// What a role that lives inside a project may name (PF-68).
    ///
    /// A project role reaches only the kinds that live inside a project, which is exactly the
    /// catalogue's project-scoped kinds. `Role`, `RoleBinding`, `Group`, `Organization` and
    /// `Project` are not among them, so no project role writes roles, bindings or the
    /// organization itself, and none of them reaches another project. The list is derived from
    /// the catalogue rather than written twice, so a kind added tomorrow is covered by the rule
    /// its own scope already states.
    pub fn validate_in_project(&self) -> Result<()> {
        for rule in &self.rules {
            for kind in &rule.kinds {
                let project_kind = crate::registry::by_kind(kind)
                    .is_some_and(|info| info.scope == crate::envelope::Scope::Project);
                if !project_kind {
                    return Err(Error::Name {
                        field: "spec.rules[].kinds",
                        value: kind.clone(),
                        reason: "a role inside a project names project kinds only; Role, \
                                 RoleBinding, Group, Organization and Project belong to the \
                                 organization's own roles (PF-68)",
                    });
                }
            }
        }
        Ok(())
    }

    /// Every rule names at least one kind and one verb; every constraint has one operator.
    pub fn validate(&self) -> Result<()> {
        if self.rules.is_empty() {
            return Err(Error::Name {
                field: "spec.rules",
                value: String::new(),
                reason: "a role with no rules grants nothing; name at least one (PF-49)",
            });
        }
        for rule in &self.rules {
            if rule.kinds.is_empty() {
                return Err(Error::Name {
                    field: "spec.rules[].kinds",
                    value: String::new(),
                    reason: "a rule names at least one kind",
                });
            }
            for kind in &rule.kinds {
                if !kind.starts_with(|c: char| c.is_ascii_uppercase())
                    || !kind.chars().all(|c| c.is_ascii_alphanumeric())
                {
                    return Err(Error::Name {
                        field: "spec.rules[].kinds",
                        value: kind.clone(),
                        reason: "a kind is written as in the manifest, e.g. `Pipeline`",
                    });
                }
            }
            if rule.verbs.is_empty() {
                return Err(Error::Name {
                    field: "spec.rules[].verbs",
                    value: String::new(),
                    reason: "a rule names at least one verb of read, propose, approve, delete",
                });
            }
            for constraint in &rule.constraints {
                constraint.validate()?;
            }
        }
        Ok(())
    }
}

impl Constraint {
    fn validate(&self) -> Result<()> {
        if self.field.trim().is_empty() || !self.field.starts_with("spec.") {
            return Err(Error::Name {
                field: "spec.rules[].constraints[].field",
                value: self.field.clone(),
                reason: "a constraint names a spec field, e.g. `spec.audience`",
            });
        }
        let operators = usize::from(!self.one_of.is_empty())
            + usize::from(!self.not_in.is_empty())
            + usize::from(self.equals.is_some());
        if operators != 1 {
            return Err(Error::Name {
                field: "spec.rules[].constraints[]",
                value: self.field.clone(),
                reason: "a constraint has exactly one of `in`, `notIn`, `equals`",
            });
        }
        Ok(())
    }
}

/// One human or group a binding names; exactly one of `user`, `group` (PF-49).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Subject {
    /// The user's identifier in the identity provider (username or e-mail).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    /// A group of the identity provider.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
}

/// When a binding applies; absent bounds are open.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct BindingValidity {
    /// Not in force before this instant (UTC).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub not_before: Option<DateTime<Utc>>,
    /// Not in force after this instant (UTC).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub not_after: Option<DateTime<Utc>>,
}

/// `spec` of a RoleBinding (PF-49).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct RoleBindingSpec {
    /// Who gets the role.
    pub subjects: Vec<Subject>,
    /// The `Role` (its `metadata.name`).
    pub role: String,
    /// Where the role applies: exactly one of `organization`, `project`, `contextSpace`.
    pub scope: RoleScope,
    /// When the binding applies.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub validity: Option<BindingValidity>,
}

impl Kind for RoleBindingSpec {
    const KIND: &'static str = "RoleBinding";
    const PLURAL: &'static str = "rolebindings";
    const SCOPE: Scope = Scope::Organization;
    const PATH_TEMPLATE: &'static str = "users/assignments/{name}.yaml";

    fn validate_spec(&self, meta: &ObjectMeta) -> Result<()> {
        names::validate_dns1123_label(&meta.name)?;
        self.validate()
    }
}

impl RoleBindingSpec {
    /// At least one subject with exactly one of user/group, a role name, one scope, an ordered validity.
    pub fn validate(&self) -> Result<()> {
        if self.subjects.is_empty() {
            return Err(Error::Name {
                field: "spec.subjects",
                value: String::new(),
                reason: "a binding names at least one user or group",
            });
        }
        for subject in &self.subjects {
            match (&subject.user, &subject.group) {
                (Some(name), None) | (None, Some(name)) if !name.trim().is_empty() => {}
                _ => {
                    return Err(Error::Name {
                        field: "spec.subjects[]",
                        value: format!("{subject:?}"),
                        reason: "a subject is exactly one of `user`, `group`, and not empty",
                    })
                }
            }
        }
        names::validate_dns1123_label(&self.role)?;
        self.scope.validate("spec.scope")?;
        if let Some(validity) = &self.validity {
            if let (Some(from), Some(to)) = (validity.not_before, validity.not_after) {
                if to <= from {
                    return Err(Error::Name {
                        field: "spec.validity.notAfter",
                        value: to.to_rfc3339(),
                        reason: "notAfter must be after notBefore",
                    });
                }
            }
        }
        Ok(())
    }
}

/// Whether the binding is in force at `now`.
impl BindingValidity {
    /// `true` when `now` is inside the window (bounds inclusive).
    pub fn contains(&self, now: DateTime<Utc>) -> bool {
        self.not_before.is_none_or(|from| from <= now) && self.not_after.is_none_or(|to| now <= to)
    }
}
