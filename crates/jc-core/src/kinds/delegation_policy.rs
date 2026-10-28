//! `kind: DelegationPolicy`: the administrator's ceiling on what the people and Groups it names
//! may hand an AI client through a connector, declared in `users/delegation/` of the
//! Organization repository (AG-150, ADR-N-064, Development/04 §DelegationPolicy).
//!
//! A policy only narrows: the Portal's connector check intersects it with the person's live
//! rights and the connector's own scope on every call (AG-144), so nothing here grants a right.
//! A field left out allows no more than the organization default of [`DelegationPolicySpec`]'s
//! accessors (`data-read`, reads only, 30 days, any client), never everything. People are named
//! by their Keycloak user id, never an e-mail (ADR-N-061).

use crate::envelope::{Kind, ObjectMeta, Scope};
use crate::error::{Error, Result};
use crate::names;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// The longest expiry a policy may allow a connector: 365 days (AG-150).
pub const MAX_EXPIRY_DAYS: u32 = 365;

/// The expiry a connector of a person gets when the policy names no `maxExpiry`, and the
/// organization default for a person no policy names (ADR-N-064 §5).
pub const DEFAULT_EXPIRY_DAYS: u32 = 30;

/// One person or Group a policy binds (AG-150).
/// The `kind` tag, `Group` or `User`, selects which.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", deny_unknown_fields)]
pub enum DelegationSubject {
    /// A `Group` manifest of the organization, by `metadata.name`.
    Group {
        /// The Group's `metadata.name`, such as `air-stewards`.
        name: String,
    },
    /// A person, by their Keycloak user id: the `sub` of their token, a lower-case UUID such as
    /// `3f1c9a52-6b1e-4d0a-9a4e-0c8d1b7e2f10`, never an e-mail or a username (ADR-N-061).
    User {
        /// The Keycloak user id.
        id: String,
    },
}

/// A segment, the preset a person ticks when creating a connector (Architecture/07 §11).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum Segment {
    /// Reads of schemas, lists and data over Endpoints and spaces.
    DataRead,
    /// Models, spaces, Endpoints and projections.
    Context,
    /// Pipelines and DataSources, their samples, mapping tries and runs.
    Pipeline,
    /// Policies, Roles, RoleBindings, Groups, ServiceAccounts.
    Access,
    /// An Endpoint's publish block and CKAN instances.
    Publish,
    /// Apps, Dashboards, Layers.
    Apps,
    /// Reads of runs, health, activity and limits, and pipeline runs.
    Operate,
    /// Every tool, within the ceiling.
    Admin,
}

/// What a connector may do on a kind: the registry's verb set (API/14 §4). No verb approves,
/// merges or decides: those doors refuse every connector (CC-41).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum ConnectorVerb {
    /// Read and list manifests of the kind.
    Read,
    /// Plan and open a change of the kind, which a person approves in the Portal.
    Propose,
    /// Start a run (`Pipeline` only).
    Run,
}

/// The verbs a connector may carry on one kind.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct KindVerbs {
    /// A kind of the platform registry, such as `Pipeline`.
    pub kind: String,
    /// At least one of `read`, `propose`, `run`.
    pub verbs: Vec<ConnectorVerb>,
}

/// What happens to a connector request above the ceiling (AG-150).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum AboveCeiling {
    /// Refused with `403` naming the field.
    #[default]
    Refuse,
    /// Stored `pending` until an administrator who holds every right it asks for approves it.
    Queue,
}

/// Desired specification of a `DelegationPolicy` (AG-150).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct DelegationPolicySpec {
    /// Who the policy binds: at least one Group or user.
    pub subjects: Vec<DelegationSubject>,
    /// The projects a connector may name, at least one; `*` is refused, list them.
    pub projects: Vec<String>,
    /// The segments a connector may carry; absent: `data-read` only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub segments: Option<Vec<Segment>>,
    /// Verbs per kind; absent: `read` on the kinds the segments reach, nothing more.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kinds: Option<Vec<KindVerbs>>,
    /// The Endpoints a connector may name; absent: the projects' Endpoints the person may read.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoints: Option<Vec<String>>,
    /// Whether a connector of theirs may have writes on.
    #[serde(default)]
    pub writes: bool,
    /// The longest expiry, an ISO 8601 duration in days such as `P90D`, at most `P365D`;
    /// absent: `P30D`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_expiry: Option<String>,
    /// The OAuth clients a connector may name (a CIMD URL or a realm client id); absent: any
    /// client of the realm.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allowed_clients: Option<Vec<String>>,
    /// A request above the ceiling is refused (default) or queued for an administrator.
    #[serde(default)]
    pub above_ceiling: AboveCeiling,
}

impl Kind for DelegationPolicySpec {
    const KIND: &'static str = "DelegationPolicy";
    const PLURAL: &'static str = "delegationpolicies";
    const SCOPE: Scope = Scope::Organization;
    const PATH_TEMPLATE: &'static str = "users/delegation/{name}.yaml";

    fn validate_spec(&self, meta: &ObjectMeta) -> Result<()> {
        names::validate_dns1123_label(&meta.name)?;
        self.validate()
    }
}

fn refuse(field: &'static str, value: impl Into<String>, reason: &'static str) -> Error {
    Error::Name {
        field,
        value: value.into(),
        reason,
    }
}

/// Refuses a name that is no DNS-1123 label, naming the field it sits in.
fn label(field: &'static str, value: &str) -> Result<()> {
    names::validate_dns1123_label(value).map_err(|_| {
        refuse(
            field,
            value,
            "a lower-case DNS-1123 label of 1 to 63 characters, such as `air-quality`",
        )
    })
}

/// Refuses an empty list, a wildcard and a value listed twice.
fn distinct_names(field: &'static str, values: &[String], what: &'static str) -> Result<()> {
    if values.is_empty() {
        return Err(refuse(field, "", what));
    }
    for (at, value) in values.iter().enumerate() {
        if value.contains('*') {
            return Err(refuse(
                field,
                value.clone(),
                "a policy lists what it allows; a wildcard is refused (AG-150)",
            ));
        }
        if values[..at].contains(value) {
            return Err(refuse(field, value.clone(), "listed twice"));
        }
    }
    Ok(())
}

impl DelegationPolicySpec {
    /// Every rule of AG-150 a single manifest can be judged on; that each Group exists is
    /// `jcctl validate`'s check over the whole repository.
    pub fn validate(&self) -> Result<()> {
        self.validate_subjects()?;
        distinct_names(
            "spec.projects",
            &self.projects,
            "a policy names at least one project, such as `air`",
        )?;
        for project in &self.projects {
            label("spec.projects", project)?;
        }
        if let Some(segments) = &self.segments {
            if segments.is_empty() {
                return Err(refuse(
                    "spec.segments",
                    "",
                    "leave segments out for `data-read`, or list at least one",
                ));
            }
            if let Some(twice) = segments
                .iter()
                .enumerate()
                .find(|(at, s)| segments[..*at].contains(s))
            {
                return Err(refuse(
                    "spec.segments",
                    format!("{:?}", twice.1),
                    "listed twice",
                ));
            }
        }
        if let Some(kinds) = &self.kinds {
            self.validate_kinds(kinds)?;
        }
        if let Some(endpoints) = &self.endpoints {
            distinct_names(
                "spec.endpoints",
                endpoints,
                "leave endpoints out for the projects' Endpoints, or list at least one",
            )?;
            for endpoint in endpoints {
                label("spec.endpoints", endpoint)?;
            }
        }
        if let Some(raw) = &self.max_expiry {
            match parse_days(raw) {
                Some(days) if (1..=MAX_EXPIRY_DAYS).contains(&days) => {}
                _ => {
                    return Err(refuse(
                        "spec.maxExpiry",
                        raw.clone(),
                        "a duration in days from P1D to P365D, such as P90D (AG-150)",
                    ))
                }
            }
        }
        if let Some(clients) = &self.allowed_clients {
            self.validate_clients(clients)?;
        }
        Ok(())
    }

    fn validate_subjects(&self) -> Result<()> {
        if self.subjects.is_empty() {
            return Err(refuse(
                "spec.subjects",
                "",
                "a policy names at least one Group or user; one naming nobody binds nobody",
            ));
        }
        for (at, subject) in self.subjects.iter().enumerate() {
            match subject {
                DelegationSubject::Group { name } => label("spec.subjects[].name", name)?,
                DelegationSubject::User { id } => {
                    crate::kinds::group::validate_person_id("spec.subjects[].id", id)?
                }
            }
            if self.subjects[..at].contains(subject) {
                return Err(refuse(
                    "spec.subjects[]",
                    format!("{subject:?}"),
                    "listed twice",
                ));
            }
        }
        Ok(())
    }

    fn validate_kinds(&self, kinds: &[KindVerbs]) -> Result<()> {
        if kinds.is_empty() {
            return Err(refuse(
                "spec.kinds",
                "",
                "leave kinds out for reads only, or list at least one",
            ));
        }
        for (at, entry) in kinds.iter().enumerate() {
            if crate::registry::by_kind(&entry.kind).is_none() {
                return Err(refuse(
                    "spec.kinds[].kind",
                    entry.kind.clone(),
                    "not a kind of the platform registry, such as Pipeline or Endpoint",
                ));
            }
            if kinds[..at].iter().any(|seen| seen.kind == entry.kind) {
                return Err(refuse(
                    "spec.kinds[].kind",
                    entry.kind.clone(),
                    "listed twice",
                ));
            }
            if entry.verbs.is_empty() {
                return Err(refuse(
                    "spec.kinds[].verbs",
                    entry.kind.clone(),
                    "at least one of read, propose, run",
                ));
            }
            for (at, verb) in entry.verbs.iter().enumerate() {
                if entry.verbs[..at].contains(verb) {
                    return Err(refuse(
                        "spec.kinds[].verbs",
                        entry.kind.clone(),
                        "listed twice",
                    ));
                }
            }
            if entry.verbs.contains(&ConnectorVerb::Run) && entry.kind != "Pipeline" {
                return Err(refuse(
                    "spec.kinds[].verbs",
                    entry.kind.clone(),
                    "`run` starts a pipeline run and applies to kind Pipeline only",
                ));
            }
        }
        Ok(())
    }

    fn validate_clients(&self, clients: &[String]) -> Result<()> {
        distinct_names(
            "spec.allowedClients",
            clients,
            "leave allowedClients out for any client, or list at least one",
        )?;
        for client in clients {
            let realm_id = !client.is_empty()
                && client
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b'.');
            let cimd = client
                .strip_prefix("https://")
                .is_some_and(|rest| !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_graphic()));
            if !(realm_id || cimd) {
                return Err(refuse(
                    "spec.allowedClients",
                    client.clone(),
                    "an https CIMD URL such as \
                     https://claude.ai/oauth/mcp-oauth-client-metadata, or a realm client id",
                ));
            }
        }
        Ok(())
    }

    /// The segments a connector may carry: `data-read` when the policy names none.
    pub fn segments(&self) -> &[Segment] {
        self.segments.as_deref().unwrap_or(&[Segment::DataRead])
    }

    /// The verbs a connector may carry on `kind`: the listed ones, or `read` alone when the
    /// policy lists no kinds; nothing for a kind a list leaves out.
    pub fn verbs_for(&self, kind: &str) -> &[ConnectorVerb] {
        match &self.kinds {
            None => &[ConnectorVerb::Read],
            Some(kinds) => kinds
                .iter()
                .find(|entry| entry.kind == kind)
                .map_or(&[], |entry| entry.verbs.as_slice()),
        }
    }

    /// The longest expiry in days: `maxExpiry`, or 30 when absent (or unreadable, which
    /// [`Self::validate`] refuses).
    pub fn max_expiry_days(&self) -> u32 {
        self.max_expiry
            .as_deref()
            .and_then(parse_days)
            .map_or(DEFAULT_EXPIRY_DAYS, |days| days.min(MAX_EXPIRY_DAYS))
    }

    /// Whether `client` is one a connector of theirs may name.
    pub fn allows_client(&self, client: &str) -> bool {
        self.allowed_clients
            .as_ref()
            .is_none_or(|clients| clients.iter().any(|c| c == client))
    }

    /// Whether the policy binds the person `user` in the Groups `groups`.
    pub fn binds(&self, user: &str, groups: &[&str]) -> bool {
        self.subjects.iter().any(|subject| match subject {
            DelegationSubject::User { id } => id == user,
            DelegationSubject::Group { name } => groups.contains(&name.as_str()),
        })
    }
}

/// `P<days>D`, the one duration form a policy takes; `None` for anything else.
fn parse_days(raw: &str) -> Option<u32> {
    let digits = raw.strip_prefix('P')?.strip_suffix('D')?;
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy() -> DelegationPolicySpec {
        DelegationPolicySpec {
            subjects: vec![DelegationSubject::Group {
                name: "air-stewards".into(),
            }],
            projects: vec!["air".into()],
            segments: None,
            kinds: Some(vec![KindVerbs {
                kind: "Pipeline".into(),
                verbs: vec![ConnectorVerb::Read, ConnectorVerb::Run],
            }]),
            endpoints: None,
            writes: false,
            max_expiry: Some("P90D".into()),
            allowed_clients: Some(vec!["mcp-local".into()]),
            above_ceiling: AboveCeiling::Refuse,
        }
    }

    fn field_of(spec: &DelegationPolicySpec) -> &'static str {
        match spec.validate() {
            Err(Error::Name { field, .. }) => field,
            other => panic!("expected a field refusal, got {other:?}"),
        }
    }

    #[test]
    fn validate_accepts_a_narrow_policy_and_names_each_refused_field() {
        assert!(policy().validate().is_ok());
        let mut spec = policy();
        spec.subjects.clear();
        assert_eq!(field_of(&spec), "spec.subjects");
        let mut spec = policy();
        spec.projects = vec!["*".into()];
        assert_eq!(field_of(&spec), "spec.projects");
        let mut spec = policy();
        spec.segments = Some(vec![Segment::Admin, Segment::Admin]);
        assert_eq!(field_of(&spec), "spec.segments");
        let mut spec = policy();
        spec.kinds = Some(vec![KindVerbs {
            kind: "Endpoint".into(),
            verbs: vec![ConnectorVerb::Run],
        }]);
        assert_eq!(field_of(&spec), "spec.kinds[].verbs");
        let mut spec = policy();
        spec.endpoints = Some(vec!["Air".into()]);
        assert_eq!(field_of(&spec), "spec.endpoints");
        let mut spec = policy();
        spec.max_expiry = Some("P366D".into());
        assert_eq!(field_of(&spec), "spec.maxExpiry");
        let mut spec = policy();
        spec.allowed_clients = Some(vec!["http://x".into()]);
        assert_eq!(field_of(&spec), "spec.allowedClients");
    }

    #[test]
    fn parse_days_takes_only_whole_days() {
        assert_eq!(parse_days("P365D"), Some(365));
        assert_eq!(parse_days("PD"), None);
        assert_eq!(parse_days("P1W"), None);
        assert_eq!(parse_days("P99999999999D"), None);
    }
}
