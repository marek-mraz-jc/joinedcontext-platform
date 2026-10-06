//! `kind: McpServer`, one named MCP server over chosen Endpoints (MF-53, ADR-N-043).
//!
//! The gateway serves it at `/api/mcp/{project}/{name}` with the hub's catalogue narrowed to its
//! members (EP-92…EP-96). Every rule one manifest can be judged by is judged here; whether each
//! member exists, serves MCP and admits at least the server's audience needs the whole repository
//! and is `jcctl validate`'s.

use crate::envelope::{Kind, ObjectMeta, Scope, TypedRef};
use crate::error::{Error, Result};
use crate::kinds::endpoint::Audience;
use crate::names;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

/// The most member Endpoints one server may name (ADR-N-043 §2.7).
pub const MAX_MEMBERS: usize = 10;

/// One named MCP server (MF-53).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct McpServerSpec {
    /// The Endpoints the server reads, one to ten, by typed reference (`kind: Endpoint`); a
    /// member of another project names its project in `namespace`.
    pub members: Vec<TypedRef>,
    /// Who may connect: `public`, `organization` or `project-list`. No wider than the narrowest
    /// member's own audience; `public` takes the red lane (EP-16).
    pub audience: Audience,
    /// The projects a `project-list` server admits; the server's own project when empty.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allowed_projects: Vec<String>,
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
        if self.members.is_empty() || self.members.len() > MAX_MEMBERS {
            return Err(invalid(
                "members",
                format!(
                    "a server names one to {MAX_MEMBERS} member Endpoints, not {}",
                    self.members.len()
                ),
            ));
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
            audience,
            allowed_projects: Vec::new(),
        }
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
