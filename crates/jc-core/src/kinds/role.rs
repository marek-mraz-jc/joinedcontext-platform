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
    /// Delete a manifest; on [`PERSON`], delete the person.
    Delete,
    /// Create a person ([`PERSON`] only, ADR-N-031).
    Create,
    /// Edit a person's name, e-mail and language, and send them a password reset ([`PERSON`]
    /// only, ADR-N-031).
    Update,
    /// Disable and enable a person, end their sessions, remove their second factor ([`PERSON`]
    /// only, ADR-N-031).
    Disable,
}

/// The kind a rule names for the people of the realm (PF-91, ADR-N-031). A person lives in
/// Keycloak and never in a manifest, so no Change reaches them: they take `read`, `create`,
/// `update`, `disable` and `delete`, and those three new verbs mean nothing on any other kind.
pub const PERSON: &str = "Person";

impl Verb {
    /// The verbs that act on a person directly rather than by a Change (ADR-N-031).
    pub fn is_person_only(self) -> bool {
        matches!(self, Verb::Create | Verb::Update | Verb::Disable)
    }

    /// The verb as a manifest writes it.
    pub fn as_str(self) -> &'static str {
        match self {
            Verb::Read => "read",
            Verb::Propose => "propose",
            Verb::Approve => "approve",
            Verb::Delete => "delete",
            Verb::Create => "create",
            Verb::Update => "update",
            Verb::Disable => "disable",
        }
    }
}

/// A constraint on one spec field or on `metadata.name`; exactly one of `in`, `notIn`,
/// `equals`, `pattern` is given.
///
/// The one exception is [`BUILD_FIELD`] with no operator, on a rule of `App` with `propose`
/// alone: it names the rule as the writer of that field, and a rule carrying it authorizes
/// nothing else (AP-73).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Constraint {
    /// Dotted path from the manifest root, e.g. `spec.audience` or `metadata.name`.
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
    /// The whole value must match this regular expression, at most [`PATTERN_MAX`] characters
    /// (T-2627): `t1[0-9]{3}-.+` confines a rule to the names a journey gives what it makes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pattern: Option<String>,
}

/// The longest `pattern` a constraint may carry.
pub const PATTERN_MAX: usize = 256;

/// `pattern` anchored on both ends, so it matches the whole value and never a part of it.
fn whole_value(pattern: &str) -> std::result::Result<regex::Regex, regex::Error> {
    regex::Regex::new(&format!("^(?:{pattern})$"))
}

/// The one `status` field a role may write: the build lane's `status.build` of an App (AP-73).
pub const BUILD_FIELD: &str = "status.build";

impl Rule {
    /// Whether this is the build lane's rule: `propose` on `App` constrained to `status.build`,
    /// which writes that field and authorizes no other proposal (AP-73).
    pub fn writes_status_only(&self) -> bool {
        self.constraints.iter().any(|c| c.field == BUILD_FIELD)
    }

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

impl Rule {
    /// A rule on [`PERSON`] names that kind alone and the verbs a person takes; the person verbs
    /// appear nowhere else (ADR-N-031). A person is not a manifest, so `propose` and `approve`
    /// reach nothing there, and `create` on a `Pipeline` would read like a right it is not.
    fn validate_person(&self) -> Result<()> {
        let names_person = self.kinds.iter().any(|kind| kind == PERSON);
        if names_person && self.kinds.len() > 1 {
            return Err(Error::Name {
                field: "spec.rules[].kinds",
                value: self.kinds.join(", "),
                reason: "a rule on Person names Person alone: a person is not a manifest, so \
                         the verbs of the other kinds mean something else there (ADR-N-031)",
            });
        }
        if let Some(verb) = self.verbs.iter().find(|verb| {
            if names_person {
                matches!(verb, Verb::Propose | Verb::Approve)
            } else {
                verb.is_person_only()
            }
        }) {
            return Err(Error::Name {
                field: "spec.rules[].verbs",
                value: verb.as_str().to_owned(),
                reason: if names_person {
                    "Person takes read, create, update, disable and delete: a person is not a \
                     manifest, so there is no Change to propose or approve (ADR-N-031)"
                } else {
                    "create, update and disable are verbs of Person alone; a manifest is \
                     created and edited with propose (ADR-N-031)"
                },
            });
        }
        if !self.constraints.is_empty() && names_person {
            return Err(Error::Name {
                field: "spec.rules[].constraints",
                value: PERSON.to_owned(),
                reason: "a person has no manifest fields to constrain (ADR-N-031)",
            });
        }
        Ok(())
    }
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
                    reason: "a rule names at least one verb of read, propose, approve, delete \
                             (and on Person also create, update, disable)",
                });
            }
            rule.validate_person()?;
            for constraint in &rule.constraints {
                if constraint.field.starts_with("status.") {
                    constraint.validate_status(rule)?;
                } else {
                    constraint.validate()?;
                }
            }
        }
        Ok(())
    }
}

impl Constraint {
    /// Whether `value`, the field's text or `None` when the manifest has none, satisfies the
    /// constraint. `notIn` admits an absent value; every other operator needs one. The Portal
    /// and the forge's `roles.rego` read the operators the same way (PF-50, PF-51).
    pub fn holds(&self, value: Option<&str>) -> bool {
        if let Some(expected) = &self.equals {
            return value == Some(expected.as_str());
        }
        if let Some(pattern) = &self.pattern {
            // A pattern that does not compile was refused at validation; one that slipped past
            // it admits nothing.
            return value.is_some_and(|v| whole_value(pattern).is_ok_and(|re| re.is_match(v)));
        }
        if !self.one_of.is_empty() {
            return value.is_some_and(|v| self.one_of.iter().any(|x| x == v));
        }
        !value.is_some_and(|v| self.not_in.iter().any(|x| x == v))
    }

    /// `{ field: status.build }` alone, on a rule of `App` with `propose` alone (AP-73).
    fn validate_status(&self, rule: &Rule) -> Result<()> {
        if self.field != BUILD_FIELD {
            return Err(Error::Name {
                field: "spec.rules[].constraints[].field",
                value: self.field.clone(),
                reason: "the one status field a role writes is `status.build` (AP-73)",
            });
        }
        if !self.one_of.is_empty()
            || !self.not_in.is_empty()
            || self.equals.is_some()
            || self.pattern.is_some()
        {
            return Err(Error::Name {
                field: "spec.rules[].constraints[]",
                value: self.field.clone(),
                reason: "`status.build` names its writer and carries no `in`, `notIn`, `equals` or `pattern` (AP-73)",
            });
        }
        if rule.kinds != ["App"] || rule.verbs != [Verb::Propose] || rule.constraints.len() != 1 {
            return Err(Error::Name {
                field: "spec.rules[]",
                value: self.field.clone(),
                reason: "a rule constrained to `status.build` is `propose` on `App` alone, with no other constraint (AP-73)",
            });
        }
        Ok(())
    }

    fn validate(&self) -> Result<()> {
        let spec_field = self.field.starts_with("spec.") && self.field.len() > "spec.".len();
        if !spec_field && self.field != "metadata.name" {
            return Err(Error::Name {
                field: "spec.rules[].constraints[].field",
                value: self.field.clone(),
                reason: "a constraint names a spec field, e.g. `spec.audience`, or `metadata.name`",
            });
        }
        let operators = usize::from(!self.one_of.is_empty())
            + usize::from(!self.not_in.is_empty())
            + usize::from(self.equals.is_some())
            + usize::from(self.pattern.as_ref().is_some_and(|p| !p.is_empty()));
        if operators != 1 || self.pattern.as_ref().is_some_and(String::is_empty) {
            return Err(Error::Name {
                field: "spec.rules[].constraints[]",
                value: self.field.clone(),
                reason: "a constraint has exactly one of `in`, `notIn`, `equals`, `pattern`",
            });
        }
        if let Some(pattern) = &self.pattern {
            if pattern.chars().count() > PATTERN_MAX {
                return Err(Error::Name {
                    field: "spec.rules[].constraints[].pattern",
                    value: self.field.clone(),
                    reason: "a pattern is at most 256 characters",
                });
            }
            if whole_value(pattern).is_err() {
                return Err(Error::Name {
                    field: "spec.rules[].constraints[].pattern",
                    value: pattern.clone(),
                    reason: "a pattern is a regular expression the Portal and Conftest both read (RE2 syntax)",
                });
            }
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
    /// A group: a `Group` manifest of the organization unless `source` says otherwise (PF-64).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
    /// Who owns `group`'s members: absent for a `Group` manifest, `provider` for a group the
    /// identity provider owns, which the reconciler never manages. Only a `RoleBinding` subject
    /// carries it, and only with `group` (PF-63, PF-64).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<SubjectSource>,
}

/// Who owns the members of a subject's group (PF-64).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum SubjectSource {
    /// A group of the identity provider, such as the realm's default group; no `Group`
    /// manifest declares it and the reconciler never creates, edits or prunes it (PF-63).
    Provider,
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
            if let (Some(user), Some(_)) = (&subject.user, subject.source) {
                return Err(Error::Name {
                    field: "spec.subjects[].source",
                    value: user.clone(),
                    reason: "`source: provider` marks a group of the identity provider; a user \
                             has no source (PF-64)",
                });
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
