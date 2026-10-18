//! `kind: McpServer`, one named MCP server over chosen Endpoints (MF-53, ADR-N-043).
//!
//! The gateway serves it at `/api/mcp/{project}/{name}` with the hub's catalogue narrowed to its
//! members and to the Endpoints its `select` matches (EP-92…EP-96, MF-55, ADR-N-043 §7). Every rule one manifest can be judged by is judged here; whether each
//! member exists, serves MCP and admits at least the server's audience needs the whole repository
//! and is `jcctl validate`'s.

use crate::envelope::{Kind, ObjectMeta, Scope, TypedRef};
use crate::error::{Error, Result};
use crate::kinds::catalog::DataTheme;
use crate::kinds::endpoint::Audience;
use crate::names;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

/// The most member Endpoints one server may name (ADR-N-043 §2.7).
pub const MAX_MEMBERS: usize = 10;

/// The most values one `select` field may hold (MF-55).
pub const MAX_SELECT_VALUES: usize = 50;

/// The most knowledge sources one server attaches (MF-57).
pub const MAX_KNOWLEDGE_SOURCES: usize = 20;

/// `{project}/{source}` as its two parts, `{source}` as `namespace`'s.
pub fn knowledge_source_id<'a>(named: &'a str, namespace: &'a str) -> (&'a str, &'a str) {
    named.split_once('/').unwrap_or((namespace, named))
}

/// How a caller reaches an Endpoint, the levels of the per-caller catalogue (T-4503).
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "kebab-case")]
pub enum AccessLevel {
    /// Anyone, signed in or not.
    Public,
    /// Every member of the organization.
    Organization,
    /// The members of the projects the Endpoint admits.
    Project,
    /// The ServiceAccounts a grant names.
    Grant,
}

impl AccessLevel {
    /// Whether a server of `audience` may select this level: a public server public Endpoints
    /// only, an organization server public and organization ones (MF-55, ADR-N-043 §7.3).
    pub fn fits(self, audience: Audience) -> bool {
        match audience {
            Audience::Public => self == Self::Public,
            Audience::Organization => matches!(self, Self::Public | Self::Organization),
            Audience::ProjectList => true,
        }
    }
}

impl std::fmt::Display for AccessLevel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Public => "public",
            Self::Organization => "organization",
            Self::Project => "project",
            Self::Grant => "grant",
        })
    }
}

/// Which Endpoints a server selects beside its members (MF-55). An Endpoint is selected when it
/// matches every field given, one value of a field being enough; `select: {}` selects every
/// Endpoint the caller's token and the server's audience admit. The gateway resolves it per
/// call from the caller's own reach, so it only narrows.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Select {
    /// Project names.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub projects: Option<Vec<String>>,
    /// Context Spaces as `{project}/{space}`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spaces: Option<Vec<String>>,
    /// Smart Data Models domains of the Endpoint's classification, e.g. `Environment` (T-4498).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub domains: Option<Vec<String>>,
    /// EU data themes of that classification, e.g. `ENVI`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub themes: Option<Vec<DataTheme>>,
    /// How the caller reaches the Endpoint; no wider than the server's audience admits.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub access_level: Option<Vec<AccessLevel>>,
}

/// One named MCP server (MF-53).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct McpServerSpec {
    /// The Endpoints the server reads, up to ten, by typed reference (`kind: Endpoint`); a
    /// member of another project names its project in `namespace`. At least one without
    /// `select`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub members: Vec<TypedRef>,
    /// The Endpoints it selects beside its members (MF-55).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub select: Option<Select>,
    /// Who may connect: `public`, `organization` or `project-list`. No wider than the narrowest
    /// member's own audience; `public` takes the red lane (EP-16).
    pub audience: Audience,
    /// The projects a `project-list` server admits; the server's own project when empty.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allowed_projects: Vec<String>,
    /// The `KnowledgeSource`s its `search_documents` searches, up to twenty, each `{source}` of
    /// its own project or `{project}/{source}`; each a `visibility: public` source (MF-57).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub knowledge_sources: Vec<String>,
}

impl Kind for McpServerSpec {
    const KIND: &'static str = "McpServer";
    const PLURAL: &'static str = "mcpservers";
    /// A server belongs to the organization or to one project (ADR-N-043 §2.1).
    const SCOPE: Scope = Scope::OrganizationOrProject;
    const PATH_TEMPLATE: &'static str = "mcp/{name}.yaml";
    const PROJECT_PATH_TEMPLATE: Option<&'static str> = Some("projects/{project}/mcp/{name}.yaml");

    fn validate_spec(&self, meta: &ObjectMeta) -> Result<()> {
        names::validate_dns1123_label(&meta.name)?;
        self.validate(meta.namespace.as_deref().unwrap_or_default())
    }
}

impl McpServerSpec {
    /// Every rule of MF-53 that one manifest can be judged by; `namespace` is the server's own.
    pub fn validate(&self, namespace: &str) -> Result<()> {
        if self.members.len() > MAX_MEMBERS {
            return Err(invalid(
                "members",
                format!(
                    "a server names at most {MAX_MEMBERS} member Endpoints, not {}",
                    self.members.len()
                ),
            ));
        }
        match &self.select {
            None if self.members.is_empty() => {
                return Err(invalid(
                    "members",
                    "a server names at least one member Endpoint or a `select`".into(),
                ))
            }
            None => {}
            Some(select) => select.validate(self.audience)?,
        }
        let mut seen = BTreeSet::new();
        for member in &self.members {
            if member.kind != "Endpoint" {
                return Err(invalid(
                    "members[].kind",
                    format!("a member is an Endpoint, not a {}", member.kind),
                ));
            }
            names::validate_dns1123_label(&member.name)?;
            let project = member.namespace.as_deref().unwrap_or(namespace);
            if project.is_empty() || project == crate::envelope::ORG_NAMESPACE {
                return Err(invalid(
                    "members[].namespace",
                    format!(
                        "member `{}` names no project: an Endpoint lives in a project, so a server of the organization names each member's",
                        member.name
                    ),
                ));
            }
            if !seen.insert((project.to_owned(), member.name.clone())) {
                return Err(invalid(
                    "members",
                    format!("Endpoint `{project}/{}` is named twice", member.name),
                ));
            }
        }
        if self.knowledge_sources.len() > MAX_KNOWLEDGE_SOURCES {
            return Err(invalid(
                "knowledgeSources",
                format!(
                    "a server attaches at most {MAX_KNOWLEDGE_SOURCES} knowledge sources, not {}",
                    self.knowledge_sources.len()
                ),
            ));
        }
        let mut attached = BTreeSet::new();
        for named in &self.knowledge_sources {
            let (project, source) = knowledge_source_id(named, namespace);
            names::validate_dns1123_label(source)?;
            if project.is_empty() || project == crate::envelope::ORG_NAMESPACE {
                return Err(invalid(
                    "knowledgeSources",
                    format!("`{named}` names no project: a server of the organization names each source as `{{project}}/{{source}}`"),
                ));
            }
            names::validate_dns1123_label(project)?;
            if !attached.insert((project.to_owned(), source.to_owned())) {
                return Err(invalid(
                    "knowledgeSources",
                    format!("knowledge source `{project}/{source}` is named twice"),
                ));
            }
        }
        if self.audience == Audience::ProjectList {
            for project in &self.allowed_projects {
                names::validate_dns1123_label(project)?;
            }
        } else if !self.allowed_projects.is_empty() {
            return Err(invalid(
                "allowedProjects",
                format!(
                    "only a `project-list` server names projects; this one is `{}`",
                    self.audience
                ),
            ));
        }
        Ok(())
    }

    /// The attached knowledge sources as `(project, source)`, one without a project in
    /// `namespace` (MF-57).
    pub fn knowledge_source_ids<'a>(
        &'a self,
        namespace: &'a str,
    ) -> impl Iterator<Item = (&'a str, &'a str)> {
        self.knowledge_sources
            .iter()
            .map(move |named| knowledge_source_id(named, namespace))
    }

    /// The members as `(project, name)`, a member without `namespace` in `namespace`.
    pub fn member_ids<'a>(
        &'a self,
        namespace: &'a str,
    ) -> impl Iterator<Item = (&'a str, &'a str)> {
        self.members.iter().map(move |member| {
            (
                member.namespace.as_deref().unwrap_or(namespace),
                member.name.as_str(),
            )
        })
    }
}

impl Select {
    /// The rules of MF-55 one manifest can be judged by; whether a project or space exists is
    /// `jcctl validate`'s.
    pub fn validate(&self, audience: Audience) -> Result<()> {
        fn bounded<T>(field: &str, values: &Option<Vec<T>>) -> Result<()> {
            match values {
                Some(values) if values.is_empty() || values.len() > MAX_SELECT_VALUES => {
                    Err(select_invalid(
                        field,
                        format!(
                            "a select field holds one to {MAX_SELECT_VALUES} values, not {}",
                            values.len()
                        ),
                    ))
                }
                _ => Ok(()),
            }
        }
        bounded("projects", &self.projects)?;
        bounded("spaces", &self.spaces)?;
        bounded("domains", &self.domains)?;
        bounded("themes", &self.themes)?;
        bounded("accessLevel", &self.access_level)?;
        for project in self.projects.iter().flatten() {
            names::validate_dns1123_label(project)?;
        }
        for space in self.spaces.iter().flatten() {
            let Some((project, name)) = space.split_once('/') else {
                return Err(select_invalid(
                    "spaces",
                    format!("`{space}` is not `{{project}}/{{space}}`"),
                ));
            };
            names::validate_dns1123_label(project)?;
            names::validate_dns1123_label(name)?;
        }
        for domain in self.domains.iter().flatten() {
            if !crate::classification::is_domain(domain) {
                return Err(select_invalid(
                    "domains",
                    format!("`{domain}` is no Smart Data Models domain of the classification"),
                ));
            }
        }
        for level in self.access_level.iter().flatten() {
            if !level.fits(audience) {
                return Err(select_invalid(
                    "accessLevel",
                    format!(
                        "a `{audience}` server selects no `{level}` Endpoint: a selector never reaches past the server's audience"
                    ),
                ));
            }
        }
        Ok(())
    }
}

/// How wide an audience is: a server is no wider than its narrowest member (MF-53).
pub fn breadth(audience: Audience) -> u8 {
    match audience {
        Audience::ProjectList => 0,
        Audience::Organization => 1,
        Audience::Public => 2,
    }
}

fn invalid(field: &str, reason: String) -> Error {
    Error::Invalid {
        field: format!("spec.{field}"),
        reason: format!("{reason} (MF-53)"),
    }
}

fn select_invalid(field: &str, reason: String) -> Error {
    Error::Invalid {
        field: format!("spec.select.{field}"),
        reason: format!("{reason} (MF-55)"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn member(name: &str, namespace: Option<&str>) -> TypedRef {
        TypedRef {
            kind: "Endpoint".into(),
            name: name.into(),
            namespace: namespace.map(str::to_owned),
        }
    }

    fn spec(members: Vec<TypedRef>, audience: Audience) -> McpServerSpec {
        McpServerSpec {
            members,
            select: None,
            audience,
            allowed_projects: Vec::new(),
            knowledge_sources: Vec::new(),
        }
    }

    fn selecting(select: &str, audience: Audience) -> Result<()> {
        let mut server = spec(Vec::new(), audience);
        server.select = Some(serde_json::from_str(select).map_err(|e| Error::Invalid {
            field: "spec.select".into(),
            reason: e.to_string(),
        })?);
        server.validate("helsinki")
    }

    #[test]
    fn a_select_alone_names_no_member() {
        assert!(selecting("{}", Audience::Public).is_ok());
        assert!(selecting(
            r#"{"themes":["ENVI","TRAN"],"domains":["Environment"],"spaces":["praha/ovzdusie"]}"#,
            Audience::Organization
        )
        .is_ok());
        let none = spec(Vec::new(), Audience::Public).validate("helsinki");
        assert!(none.unwrap_err().to_string().contains("or a `select`"));
    }

    #[test]
    fn a_selector_never_reaches_past_the_servers_audience() {
        for level in ["organization", "project", "grant"] {
            let err = selecting(
                &format!(r#"{{"accessLevel":["{level}"]}}"#),
                Audience::Public,
            )
            .unwrap_err()
            .to_string();
            assert!(err.contains("spec.select.accessLevel"), "{err}");
        }
        assert!(selecting(r#"{"accessLevel":["public"]}"#, Audience::Public).is_ok());
        assert!(selecting(
            r#"{"accessLevel":["public","organization"]}"#,
            Audience::Organization
        )
        .is_ok());
        for level in ["project", "grant"] {
            assert!(selecting(
                &format!(r#"{{"accessLevel":["{level}"]}}"#),
                Audience::Organization
            )
            .is_err());
        }
        assert!(selecting(r#"{"accessLevel":["grant"]}"#, Audience::ProjectList).is_ok());
    }

    #[test]
    fn a_select_names_known_codes_in_bounded_lists() {
        // an unknown theme code is refused by the kind's own type
        assert!(selecting(r#"{"themes":["SPACE"]}"#, Audience::Public).is_err());
        assert!(selecting(r#"{"domains":["Astrology"]}"#, Audience::Public)
            .unwrap_err()
            .to_string()
            .contains("spec.select.domains"));
        assert!(selecting(r#"{"spaces":["ovzdusie"]}"#, Audience::Public)
            .unwrap_err()
            .to_string()
            .contains("{project}/{space}"));
        assert!(selecting(r#"{"projects":[]}"#, Audience::Public).is_err());
        let many: Vec<String> = (0..51).map(|i| format!("\"p{i}\"")).collect();
        assert!(selecting(
            &format!(r#"{{"projects":[{}]}}"#, many.join(",")),
            Audience::Public
        )
        .is_err());
        assert!(selecting(r#"{"favourites":true}"#, Audience::Public).is_err());
    }

    #[test]
    fn one_to_ten_distinct_endpoint_members_of_a_project() {
        assert!(spec(
            vec![member("bikes", None), member("air", Some("praha"))],
            Audience::Public
        )
        .validate("helsinki")
        .is_ok());
        assert!(spec(Vec::new(), Audience::Public)
            .validate("helsinki")
            .is_err());
        let eleven = (0..11).map(|i| member(&format!("e{i}"), None)).collect();
        assert!(spec(eleven, Audience::Public).validate("helsinki").is_err());
        let twice = spec(
            vec![member("bikes", None), member("bikes", Some("helsinki"))],
            Audience::Public,
        );
        assert!(twice
            .validate("helsinki")
            .unwrap_err()
            .to_string()
            .contains("named twice"));
        let policy = TypedRef {
            kind: "Policy".into(),
            ..member("p", None)
        };
        assert!(spec(vec![policy], Audience::Public)
            .validate("helsinki")
            .is_err());
    }

    #[test]
    fn a_server_of_the_organization_names_each_members_project() {
        let bare = spec(vec![member("bikes", None)], Audience::Organization);
        assert!(bare
            .validate("org")
            .unwrap_err()
            .to_string()
            .contains("names no project"));
        assert!(spec(
            vec![member("bikes", Some("helsinki"))],
            Audience::Organization
        )
        .validate("org")
        .is_ok());
    }

    /// MF-57: up to twenty distinct knowledge sources, each with its project, the organization's
    /// server naming it.
    #[test]
    fn a_server_attaches_up_to_twenty_distinct_knowledge_sources_of_named_projects() {
        let attaching = |sources: &[&str], namespace: &str| {
            let mut server = spec(vec![member("bikes", Some("helsinki"))], Audience::Public);
            server.knowledge_sources = sources.iter().map(|s| (*s).to_owned()).collect();
            server.validate(namespace)
        };
        assert!(attaching(&["web", "bbsk/web"], "banskabystrica").is_ok());
        let ids: Vec<(String, String)> = {
            let mut server = spec(Vec::new(), Audience::Public);
            server.knowledge_sources = vec!["web".into(), "bbsk/web".into()];
            server
                .knowledge_source_ids("banskabystrica")
                .map(|(p, s)| (p.to_owned(), s.to_owned()))
                .collect()
        };
        assert_eq!(
            ids,
            [
                ("banskabystrica".into(), "web".into()),
                ("bbsk".into(), "web".into())
            ]
        );
        assert!(attaching(&["web", "banskabystrica/web"], "banskabystrica")
            .unwrap_err()
            .to_string()
            .contains("named twice"));
        assert!(attaching(&["web"], "org")
            .unwrap_err()
            .to_string()
            .contains("names no project"));
        assert!(attaching(&["helsinki/web"], "org").is_ok());
        for bad in ["Web", "bbsk/", "/web", "a/b/c", "../web"] {
            assert!(attaching(&[bad], "banskabystrica").is_err(), "{bad}");
        }
        let many: Vec<String> = (0..21).map(|i| format!("s{i}")).collect();
        let many: Vec<&str> = many.iter().map(String::as_str).collect();
        assert!(attaching(&many, "banskabystrica").is_err());
    }

    #[test]
    fn only_a_project_list_server_names_projects() {
        let mut listed = spec(vec![member("bikes", None)], Audience::ProjectList);
        listed.allowed_projects = vec!["espoo".into()];
        assert!(listed.validate("helsinki").is_ok());
        let mut public = spec(vec![member("bikes", None)], Audience::Public);
        public.allowed_projects = vec!["espoo".into()];
        assert!(public.validate("helsinki").is_err());
    }

    #[test]
    fn breadth_orders_the_audiences() {
        assert!(breadth(Audience::ProjectList) < breadth(Audience::Organization));
        assert!(breadth(Audience::Organization) < breadth(Audience::Public));
    }
}
