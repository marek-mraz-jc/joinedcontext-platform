//! `kind: Group`: the people a binding names at once, declared in `users/groups/` of the
//! Organization repository (T-0852, PF-62, PF-64, Architecture/12 §2a).
//!
//! Membership is configuration, not a console setting: the reconciler owns the Keycloak group of
//! each manifest, adds and prunes members to match, and reports what somebody changed by hand as
//! drift (PF-63). A member is named by their Keycloak user id, never an e-mail or a name, so
//! Git history holds nothing an erasure has to remove (PF-126, ADR-N-061); this kind checks the
//! shape of the id and nothing about who exists.

use crate::envelope::{Kind, ObjectMeta, Scope};
use crate::error::{Error, Result};
use crate::names;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// One member of a [`Group`][crate::kinds::Group] (PF-62).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Member {
    /// The person's Keycloak user id, the `sub` of their token: a lower-case UUID such as
    /// `3f2a9c1e-7b4d-4e8a-9c61-0d5e2b7f8a14` (PF-126).
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
    /// Each member is a Keycloak user id, and nobody is in the group twice.
    pub fn validate(&self) -> Result<()> {
        let mut seen: Vec<&str> = Vec::with_capacity(self.members.len());
        for member in &self.members {
            let user = member.user.as_str();
            validate_person_id("spec.members[].user", user)?;
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

    /// Every member's Keycloak user id: what the reconciler writes into the Keycloak group.
    pub fn members(&self) -> impl Iterator<Item = &str> {
        self.members.iter().map(|member| member.user.as_str())
    }
}

/// Whether `value` is a Keycloak user id: a UUID in its lower-case hyphenated form
/// (`8-4-4-4-12` hex digits), the form Keycloak puts in a token's `sub` (PF-126).
pub fn is_person_id(value: &str) -> bool {
    let bytes = value.as_bytes();
    bytes.len() == 36
        && bytes.iter().enumerate().all(|(at, byte)| match at {
            8 | 13 | 18 | 23 => *byte == b'-',
            _ => byte.is_ascii_digit() || (b'a'..=b'f').contains(byte),
        })
}

/// Refuses a person named by anything but their Keycloak user id: an e-mail, a username or a
/// display name in a manifest stays in Git history for good (PF-126, ADR-N-061).
pub(crate) fn validate_person_id(field: &'static str, value: &str) -> Result<()> {
    if is_person_id(value) {
        return Ok(());
    }
    let reason = if value.contains('@') {
        "a person is named by their Keycloak user id (a UUID such as \
         3f2a9c1e-7b4d-4e8a-9c61-0d5e2b7f8a14), never by e-mail: Git keeps every e-mail for good \
         (PF-126)"
    } else {
        "a person is named by their Keycloak user id, a lower-case UUID such as \
         3f2a9c1e-7b4d-4e8a-9c61-0d5e2b7f8a14 (PF-126)"
    };
    Err(Error::Name {
        field,
        value: value.to_owned(),
        reason,
    })
}
