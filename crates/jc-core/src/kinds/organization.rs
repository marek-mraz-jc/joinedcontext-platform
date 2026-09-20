//! `kind: Organization` specification and validation (T-0111, PF-01, PF-02, PF-25, PF-41).

use crate::envelope::{self, Kind, ObjectMeta, Scope};
use crate::error::{Error, Result};
use crate::names;
use crate::urn::Urn;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Desired specification of an [`Organization`][crate::kinds::Organization] resource (PF-01, PF-02, PF-25, PF-41).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct OrganizationSpec {
    /// Verified internet FQDN, the `{orgDomain}` of every entity URN (PF-41).
    pub domain: String,
    /// Git repository URL of the one Org Repository in Gitea (PF-02).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git_repository_url: Option<String>,
    /// Ordered list of supported locales, most preferred first (PF-25).
    pub locales: Vec<String>,
    /// Organization fallback default locale, MUST be one of [`Self::locales`] (PF-25, PF-26).
    pub default_locale: String,
    /// Administrative, technical, data protection, or security contacts.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub contacts: Vec<Contact>,
    /// How projects are opened and who sees them (PF-61, PF-65).
    #[serde(default)]
    pub projects: ProjectsPolicy,
}

/// The organization's rules for its projects (PF-61, PF-65).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct ProjectsPolicy {
    /// Who may open a project: `anyone`, `group:<name>` or `org-admin` (PF-65).
    #[serde(default)]
    pub creation: ProjectCreation,
    /// Who holds the seeded `viewer` role at organization scope: every signed-in person of the
    /// organization (`organization`, the default) or only whoever a binding names (`members`)
    /// (PF-61).
    #[serde(default)]
    pub visibility: ProjectVisibility,
    /// The quota every project the organization opens starts from (PF-73). A project may lower a
    /// value in the yellow lane; raising one above this is the red lane and an `org-admin`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quota: Option<crate::kinds::Quotas>,
    /// How long a deleted project's name stays reserved, in days (PF-78). Absent is
    /// [`DEFAULT_NAME_COOLDOWN_DAYS`]; `0` frees the name the moment the project is gone.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name_cooldown_days: Option<u32>,
}

/// How long a deleted project's name stays reserved when the organization sets no period (PF-78).
pub const DEFAULT_NAME_COOLDOWN_DAYS: u32 = 30;

impl ProjectsPolicy {
    /// The cooling period in force: what the organization set, else the default (PF-78).
    pub fn name_cooldown_days(&self) -> u32 {
        self.name_cooldown_days
            .unwrap_or(DEFAULT_NAME_COOLDOWN_DAYS)
    }
}

/// Who may open a project (PF-65). `group:<name>` names a `Group` of the organization.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(try_from = "String", into = "String")]
pub enum ProjectCreation {
    /// Every signed-in person of the organization.
    Anyone,
    /// The members of one group.
    Group(String),
    /// Whoever holds `propose` on `Project`, the seeded `org-admin`.
    #[default]
    OrgAdmin,
}

impl TryFrom<String> for ProjectCreation {
    type Error = String;

    fn try_from(value: String) -> std::result::Result<Self, Self::Error> {
        match value.as_str() {
            "anyone" => Ok(Self::Anyone),
            "org-admin" => Ok(Self::OrgAdmin),
            other => match other.strip_prefix("group:") {
                Some(name) if names::validate_dns1123_label(name).is_ok() => {
                    Ok(Self::Group(name.to_owned()))
                }
                Some(_) => Err(format!(
                    "'{other}' names no group: write `group:<name>` with a DNS-1123 label (PF-65)"
                )),
                None => Err(format!(
                    "'{other}' is not one of anyone, group:<name>, org-admin (PF-65)"
                )),
            },
        }
    }
}

impl From<ProjectCreation> for String {
    fn from(value: ProjectCreation) -> Self {
        match value {
            ProjectCreation::Anyone => "anyone".to_owned(),
            ProjectCreation::Group(name) => format!("group:{name}"),
            ProjectCreation::OrgAdmin => "org-admin".to_owned(),
        }
    }
}

/// Who holds the seeded `viewer` role at organization scope (PF-61).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum ProjectVisibility {
    /// Every signed-in person of the organization reads every project.
    #[default]
    Organization,
    /// Only a principal a binding names reads a project.
    Members,
}

/// Contact information for an organization role.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Contact {
    /// Role of this contact.
    pub role: ContactRole,
    /// Full name of the contact person or organizational unit.
    pub name: String,
    /// Contact email address.
    pub email: String,
    /// Optional contact telephone number.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phone: Option<String>,
}

impl Contact {
    /// Creates a new contact with required role, name, and email.
    pub fn new(role: ContactRole, name: impl Into<String>, email: impl Into<String>) -> Self {
        Self {
            role,
            name: name.into(),
            email: email.into(),
            phone: None,
        }
    }
}

/// Functional responsibility of an organization contact.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum ContactRole {
    /// Administrative contact.
    Administrative,
    /// Technical contact.
    Technical,
    /// Data protection officer / GDPR contact.
    DataProtection,
    /// Information security officer contact.
    Security,
}

impl Kind for OrganizationSpec {
    const KIND: &'static str = "Organization";
    const PLURAL: &'static str = "organizations";
    const SCOPE: Scope = Scope::Organization;
    const PATH_TEMPLATE: &'static str = "org.yaml";

    fn validate_spec(&self, _meta: &ObjectMeta) -> Result<()> {
        self.validate()
    }
}

impl OrganizationSpec {
    /// Validates domain name, locale preferences, and contact details (PF-25, PF-41).
    pub fn validate(&self) -> Result<()> {
        names::validate_org_domain(&self.domain)?;
        envelope::validate_locales(&self.locales, &self.default_locale)?;

        for contact in &self.contacts {
            if contact.name.trim().is_empty() {
                return Err(Error::Name {
                    field: "contacts.name",
                    value: contact.name.clone(),
                    reason: "contact name must not be empty",
                });
            }

            // ponytail: we validate e-mail with a pragmatic check (exactly one '@', non-empty local
            // part, valid org domain). Full RFC 5322 compliance is handled upstream by Keycloak
            // when verifying real user and service account addresses.
            let parts: Vec<&str> = contact.email.split('@').collect();
            if parts.len() != 2 || parts[0].is_empty() {
                return Err(Error::Name {
                    field: "contacts.email",
                    value: contact.email.clone(),
                    reason: "contact email must have exactly one `@` and a non-empty local part",
                });
            }
            names::validate_org_domain(parts[1]).map_err(|e| match e {
                Error::Name { reason, .. } => Error::Name {
                    field: "contacts.email",
                    value: contact.email.clone(),
                    reason,
                },
                other => other,
            })?;
        }

        if let Some(quota) = self.projects.quota.as_ref() {
            quota.validate()?;
        }

        Ok(())
    }

    /// Mints a deterministic entity URN in this organization's verified domain (PF-42, PF-44).
    pub fn urn(&self, entity_type: &str, space: &str, local_id: &str) -> Result<Urn> {
        Urn::new(entity_type, &self.domain, space, local_id)
    }
}
