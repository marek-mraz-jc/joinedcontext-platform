//! `kind: Group`: the people a binding names at once, declared in `users/groups/` of the
//! Organization repository (T-0852, PF-62, PF-64, Architecture/12 §2a).
//!
//! Membership is configuration, not a console setting: the reconciler owns the Keycloak group of
//! each manifest, adds and prunes members to match, and reports what somebody changed by hand as
//! drift (PF-63). A member no Keycloak user carries yet is not an error here — the binding takes
//! effect at that person's first login (PF-04, PF-49) — so this kind checks the shape of an
//! address and nothing about who exists.

use crate::envelope::{Kind, ObjectMeta, Scope};
use crate::error::{Error, Result};
use crate::names;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// One member of a [`Group`][crate::kinds::Group] (PF-62).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Member {
    /// The person's e-mail address, the name Keycloak carries them under (PF-04).
    pub user: String,
}

/// Desired specification of a [`Group`][crate::kinds::Group] resource (PF-62).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct GroupSpec {
    /// What the group is, for whoever reviews a binding that names it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Everyone in the group; empty is legal, and means the bindings that name it match nobody
    /// until somebody is added.
    #[serde(default)]
    pub members: Vec<Member>,
}

impl Kind for GroupSpec {
    const KIND: &'static str = "Group";
    const PLURAL: &'static str = "groups";
    const SCOPE: Scope = Scope::Organization;
    const PATH_TEMPLATE: &'static str = "users/groups/{name}.yaml";

    fn validate_spec(&self, meta: &ObjectMeta) -> Result<()> {
        names::validate_dns1123_label(&meta.name)?;
        self.validate()
    }
}

impl GroupSpec {
    /// Each member is an address, and nobody is in the group twice.
    pub fn validate(&self) -> Result<()> {
        let mut seen: Vec<&str> = Vec::with_capacity(self.members.len());
        for member in &self.members {
            let user = member.user.trim();
            if user.len() < 3 || !is_address(user) {
                return Err(Error::Name {
                    field: "spec.members[].user",
                    value: member.user.clone(),
                    reason: "a member is an e-mail address, the name Keycloak carries (PF-62)",
                });
            }
            if seen.contains(&user) {
                return Err(Error::Name {
                    field: "spec.members[].user",
                    value: member.user.clone(),
                    reason: "the same person is listed twice",
                });
            }
            seen.push(user);
        }
        Ok(())
    }

    /// Every member's address, trimmed: what the reconciler writes into the Keycloak group.
    pub fn members(&self) -> impl Iterator<Item = &str> {
        self.members.iter().map(|member| member.user.trim())
    }
}

/// `local@domain.tld`, checked no further: the identity provider owns what an address is, and a
/// rule of our own would refuse addresses Keycloak accepts.
pub(crate) fn is_address(value: &str) -> bool {
    let Some((local, domain)) = value.split_once('@') else {
        return false;
    };
    !local.is_empty()
        && !value.contains(char::is_whitespace)
        && domain.contains('.')
        && !domain.starts_with('.')
        && !domain.ends_with('.')
}
