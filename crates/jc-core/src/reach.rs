//! What an assistant reads: the Endpoints of one `AssistantDeployment`, resolved from its
//! connectors, its selector and its named server inside its project's reach (AG-121, AG-122,
//! ADR-N-055 §2.3).
//!
//! This is the one path: `jcctl validate`, `jc-assistant`, the gateway's `/api/mcp/d/{publicId}`
//! and the Portal's preview all call [`resolve_connectors`]; none filters a list of its own. Each
//! reader fills [`EndpointFacts`] from its own copy of the repository. A selector names no
//! Endpoint, so it only narrows what the candidates already hold.

use crate::kinds::assistant::{connector_target, AssistantDeploymentSpec};
use crate::kinds::catalog::DataTheme;
use crate::kinds::endpoint::Audience;
use crate::kinds::mcp_server::{breadth, AccessLevel, McpServerSpec, Select};
use std::collections::{BTreeMap, BTreeSet};

/// The most Endpoints one deployment resolves (AG-122).
pub const MAX_RESOLVED: usize = 50;

/// The timeout of an Endpoint that joins by the selector or the server, not as a connector.
pub const SELECTED_TIMEOUT_SECONDS: u16 = 20;

/// What the resolver knows of one Endpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EndpointFacts {
    /// The Endpoint's project (its manifest's namespace).
    pub project: String,
    /// Its `metadata.name`.
    pub name: String,
    /// Its `spec.slug`.
    pub slug: String,
    /// The Context Space it reads, by name.
    pub space: String,
    /// `spec.audience`.
    pub audience: Audience,
    /// The entity types it serves; empty when the reader does not know them.
    pub types: BTreeSet<String>,
    /// The Smart Data Models domains of their classification (T-4498).
    pub domains: BTreeSet<String>,
    /// Its EU data themes: `catalog.themes`, else the classification's (EP-78).
    pub themes: BTreeSet<DataTheme>,
}

impl EndpointFacts {
    /// The facts of Endpoint `name` of `project` from its manifest and the entity types it
    /// serves (its space's model classes): the domains and themes their classification gives,
    /// a theme set by hand in `catalog.themes` winning, as on the DCAT record (EP-78, T-4498).
    pub fn of(
        project: &str,
        name: &str,
        spec: &crate::kinds::EndpointSpec,
        types: impl IntoIterator<Item = String>,
    ) -> Self {
        let types: BTreeSet<String> = types.into_iter().collect();
        let classified = crate::classification::classify(types.iter().map(String::as_str));
        let themes = match spec.catalog.as_ref().filter(|c| !c.themes.is_empty()) {
            Some(catalog) => catalog.themes.iter().copied().collect(),
            None => classified.themes,
        };
        Self {
            project: project.to_owned(),
            name: name.to_owned(),
            slug: spec.slug.to_string(),
            space: spec.context_space_ref.name().to_owned(),
            audience: spec.audience,
            domains: classified.domains.into_iter().map(str::to_owned).collect(),
            themes,
            types,
        }
    }

    /// The access level a deployment, which has no caller of its own, sees the Endpoint at; a
    /// grant is per caller and never matches here.
    fn level(&self) -> AccessLevel {
        match self.audience {
            Audience::Public => AccessLevel::Public,
            Audience::Organization => AccessLevel::Organization,
            Audience::ProjectList => AccessLevel::Project,
        }
    }
}

impl Select {
    /// Whether `endpoint` matches every field given but `accessLevel`, one value of a field being
    /// enough (MF-55, MF-61). A field the reader knows nothing of (`types` empty) matches no
    /// value, so an unknown never widens a selection. `accessLevel` depends on who asks and is
    /// the caller's to apply.
    pub fn matches(&self, endpoint: &EndpointFacts) -> bool {
        fn any<T: PartialEq>(wanted: &Option<Vec<T>>, has: impl Fn(&T) -> bool) -> bool {
            wanted.as_ref().is_none_or(|values| values.iter().any(has))
        }
        let space = format!("{}/{}", endpoint.project, endpoint.space);
        any(&self.projects, |p| *p == endpoint.project)
            && any(&self.spaces, |s| *s == space)
            && any(&self.domains, |d| endpoint.domains.contains(d))
            && any(&self.themes, |t| endpoint.themes.contains(t))
            && any(&self.types, |t| endpoint.types.contains(t))
    }
}

/// One Endpoint a deployment reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedEndpoint {
    /// The Endpoint's project.
    pub project: String,
    /// Its `metadata.name`.
    pub name: String,
    /// Its slug, the order of the list.
    pub slug: String,
    /// The tools a connector lists; empty for one that joined by the selector or the server,
    /// which is offered its read tools.
    pub tools: Vec<String>,
    /// How long one call may take.
    pub timeout_seconds: u16,
}

/// A deployment's Endpoints.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Resolution {
    /// At most [`MAX_RESOLVED`], sorted by slug.
    pub endpoints: Vec<ResolvedEndpoint>,
    /// The slugs past [`MAX_RESOLVED`], sorted, which the Portal names (AG-122).
    pub omitted: Vec<String>,
}

/// The Endpoints deployment `deployment` of project `project` reads: (connectors ∪ select ∪
/// the server's reach) − exclude, restricted to its candidates (AG-121): its own project's
/// Endpoints, `audience: public` alone on every channel but `internal`, and the `audience:
/// public` Endpoints of `scope.projects`. `server` is the McpServer `spec.mcpServer` names,
/// with its namespace (`org` for the organization's); `None` when it names none or the reader
/// does not hold it. The same inputs give the same list.
pub fn resolve_connectors(
    project: &str,
    deployment: &AssistantDeploymentSpec,
    catalog: &[EndpointFacts],
    server: Option<(&str, &McpServerSpec)>,
) -> Resolution {
    let scope = deployment.scope_projects();
    let candidate = |e: &&EndpointFacts| {
        if e.project == project {
            !deployment.channel.is_anonymous() || e.audience == Audience::Public
        } else {
            scope.contains(&e.project) && e.audience == Audience::Public
        }
    };
    let named = |reference: &str| {
        let (other, name) = connector_target(reference);
        (other.unwrap_or(project).to_owned(), name.to_owned())
    };
    let excluded: BTreeSet<(String, String)> =
        deployment.exclude.iter().map(|e| named(e)).collect();
    let connectors: BTreeMap<(String, String), (&[String], u16)> = deployment
        .connectors
        .iter()
        .map(|c| (named(&c.endpoint), (c.tools.as_slice(), c.timeout_seconds)))
        .collect();
    let members: BTreeSet<(String, String)> = server
        .map(|(namespace, spec)| {
            spec.member_ids(namespace)
                .map(|(p, n)| (p.to_owned(), n.to_owned()))
                .collect()
        })
        .unwrap_or_default();

    let mut endpoints: Vec<ResolvedEndpoint> = catalog
        .iter()
        .filter(candidate)
        .filter_map(|e| {
            let key = (e.project.clone(), e.name.clone());
            if excluded.contains(&key) {
                return None;
            }
            if let Some((tools, timeout_seconds)) = connectors.get(&key) {
                return Some(resolved(e, tools.to_vec(), *timeout_seconds));
            }
            let selected = deployment.select.as_ref().is_some_and(|select| {
                select.matches(e)
                    && select
                        .access_level
                        .as_ref()
                        .is_none_or(|levels| levels.contains(&e.level()))
            });
            let served = server.is_some_and(|(_, spec)| {
                members.contains(&key)
                    || spec.select.as_ref().is_some_and(|select| {
                        breadth(e.audience) >= breadth(spec.audience)
                            && select.matches(e)
                            && select
                                .access_level
                                .as_ref()
                                .is_none_or(|levels| levels.contains(&e.level()))
                    })
            });
            (selected || served).then(|| resolved(e, Vec::new(), SELECTED_TIMEOUT_SECONDS))
        })
        .collect();
    endpoints.sort_by(|a, b| a.slug.cmp(&b.slug));
    endpoints.dedup_by(|a, b| a.slug == b.slug);
    let omitted = endpoints
        .split_off(endpoints.len().min(MAX_RESOLVED))
        .into_iter()
        .map(|e| e.slug)
        .collect();
    Resolution { endpoints, omitted }
}

fn resolved(e: &EndpointFacts, tools: Vec<String>, timeout_seconds: u16) -> ResolvedEndpoint {
    ResolvedEndpoint {
        project: e.project.clone(),
        name: e.name.clone(),
        slug: e.slug.clone(),
        tools,
        timeout_seconds,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn endpoint(
        project: &str,
        name: &str,
        audience: Audience,
        themes: &[DataTheme],
    ) -> EndpointFacts {
        EndpointFacts {
            project: project.into(),
            name: name.into(),
            slug: format!("{project}-{name}"),
            space: "mesto".into(),
            audience,
            types: BTreeSet::from(["OffStreetParking".to_owned()]),
            domains: BTreeSet::from(["Parking".to_owned()]),
            themes: themes.iter().copied().collect(),
        }
    }

    fn catalog() -> Vec<EndpointFacts> {
        vec![
            endpoint("bb", "parking", Audience::Public, &[DataTheme::Tran]),
            endpoint("bb", "air", Audience::Public, &[DataTheme::Envi]),
            endpoint("bb", "staff", Audience::Organization, &[DataTheme::Gove]),
            endpoint("bbsk", "roads", Audience::Public, &[DataTheme::Tran]),
            endpoint("bbsk", "budget", Audience::Organization, &[DataTheme::Econ]),
            endpoint("praha", "trams", Audience::Public, &[DataTheme::Tran]),
        ]
    }

    fn deployment(spec: serde_json::Value) -> AssistantDeploymentSpec {
        let mut base = json!({
            "publicId": "bb-doprava",
            "channel": "public",
            "allowedOrigins": ["https://www.banskabystrica.sk"],
            "rateLimit": {"requestsPerMinute": 60, "perClientPerMinute": 10},
            "budget": {"tokensPerDay": 2000000, "tokensPerConversation": 40000},
            "scope": {"projects": ["bbsk"]},
        });
        base.as_object_mut()
            .unwrap()
            .extend(spec.as_object().unwrap().clone());
        serde_json::from_value(base).expect("deployment")
    }

    fn slugs(resolution: &Resolution) -> Vec<&str> {
        resolution
            .endpoints
            .iter()
            .map(|e| e.slug.as_str())
            .collect()
    }

    #[test]
    fn without_a_selector_only_the_connectors_resolve() {
        let d = deployment(json!({
            "connectors": [{"endpoint": "parking", "tools": ["query_entities"], "timeoutSeconds": 9}]
        }));
        let r = resolve_connectors("bb", &d, &catalog(), None);
        assert_eq!(slugs(&r), ["bb-parking"]);
        assert_eq!(r.endpoints[0].tools, ["query_entities"]);
        assert_eq!(r.endpoints[0].timeout_seconds, 9);
    }

    #[test]
    fn a_selector_picks_by_publisher_and_by_category() {
        let by_publisher = deployment(json!({"select": {"projects": ["bbsk"]}}));
        assert_eq!(
            slugs(&resolve_connectors("bb", &by_publisher, &catalog(), None)),
            ["bbsk-roads"]
        );
        let by_category = deployment(json!({"select": {"themes": ["TRAN"]}}));
        let r = resolve_connectors("bb", &by_category, &catalog(), None);
        assert_eq!(slugs(&r), ["bb-parking", "bbsk-roads"]);
        assert!(r.endpoints.iter().all(|e| e.tools.is_empty()));
        assert!(r
            .endpoints
            .iter()
            .all(|e| e.timeout_seconds == SELECTED_TIMEOUT_SECONDS));
        let by_type = deployment(json!({"select": {"types": ["WeatherObserved"]}}));
        assert!(resolve_connectors("bb", &by_type, &catalog(), None)
            .endpoints
            .is_empty());
    }

    #[test]
    fn exclude_wins_over_connectors_select_and_server() {
        let d = deployment(json!({
            "connectors": [{"endpoint": "parking", "tools": ["query_entities"]}],
            "select": {},
            "exclude": ["parking", "bbsk/roads"],
        }));
        assert_eq!(
            slugs(&resolve_connectors("bb", &d, &catalog(), None)),
            ["bb-air"]
        );
    }

    #[test]
    fn a_public_deployment_never_resolves_an_internal_endpoint() {
        let d = deployment(json!({
            "connectors": [{"endpoint": "staff", "tools": ["query_entities"]}],
            "select": {},
            "mcpServer": "bb/all",
        }));
        let server: McpServerSpec = serde_json::from_value(json!({
            "audience": "project-list",
            "members": [{"kind": "Endpoint", "name": "staff"}],
            "select": {}
        }))
        .unwrap();
        let r = resolve_connectors("bb", &d, &catalog(), Some(("bb", &server)));
        assert_eq!(slugs(&r), ["bb-air", "bb-parking", "bbsk-roads"]);

        let mut internal = d.clone();
        internal.channel = crate::kinds::assistant::Channel::Internal;
        let r = resolve_connectors("bb", &internal, &catalog(), Some(("bb", &server)));
        assert_eq!(
            slugs(&r),
            ["bb-air", "bb-parking", "bb-staff", "bbsk-roads"]
        );
    }

    #[test]
    fn another_projects_endpoint_needs_the_scope_and_a_public_audience() {
        let mut d = deployment(json!({"select": {}}));
        d.channel = crate::kinds::assistant::Channel::Internal;
        let r = resolve_connectors("bb", &d, &catalog(), None);
        // praha is not in scope; bbsk's organization Endpoint is no grant to project bb.
        assert!(!slugs(&r).contains(&"praha-trams"));
        assert!(!slugs(&r).contains(&"bbsk-budget"));
        let named = deployment(json!({
            "connectors": [{"endpoint": "bbsk/budget", "tools": ["query_entities"]}]
        }));
        assert!(resolve_connectors("bb", &named, &catalog(), None)
            .endpoints
            .is_empty());
    }

    #[test]
    fn the_access_level_of_a_selector_narrows() {
        let mut d = deployment(json!({"select": {"accessLevel": ["organization"]}}));
        d.channel = crate::kinds::assistant::Channel::Internal;
        assert_eq!(
            slugs(&resolve_connectors("bb", &d, &catalog(), None)),
            ["bb-staff"]
        );
    }

    #[test]
    fn the_list_is_sorted_and_capped_with_the_rest_named() {
        let many: Vec<EndpointFacts> = (0..MAX_RESOLVED + 3)
            .rev()
            .map(|i| endpoint("bb", &format!("e{i:03}"), Audience::Public, &[]))
            .collect();
        let d = deployment(json!({"select": {}}));
        let r = resolve_connectors("bb", &d, &many, None);
        assert_eq!(r.endpoints.len(), MAX_RESOLVED);
        assert_eq!(r.endpoints[0].slug, "bb-e000");
        assert_eq!(r.omitted, ["bb-e050", "bb-e051", "bb-e052"]);
        assert_eq!(r, resolve_connectors("bb", &d, &many, None));
    }

    #[test]
    fn facts_classify_the_types_and_a_hand_set_theme_wins() {
        let spec: crate::kinds::EndpointSpec = serde_json::from_value(json!({
            "contextSpaceRef": "mesto", "slug": "zt4qm7ge2xdv6ksb3ncf5arw2y", "audience": "public"
        }))
        .unwrap();
        let facts = EndpointFacts::of("bb", "parking", &spec, ["OffStreetParking".to_owned()]);
        assert_eq!(facts.space, "mesto");
        assert_eq!(facts.slug, "zt4qm7ge2xdv6ksb3ncf5arw2y");
        assert!(facts.domains.contains("Parking"));
        assert_eq!(facts.themes, BTreeSet::from([DataTheme::Tran]));
        let mut themed = spec.clone();
        themed.catalog = Some(serde_json::from_value(json!({"themes": ["ECON"]})).unwrap());
        let facts = EndpointFacts::of("bb", "parking", &themed, ["OffStreetParking".to_owned()]);
        assert_eq!(facts.themes, BTreeSet::from([DataTheme::Econ]));
    }

    #[test]
    fn an_unknown_field_matches_nothing() {
        let mut unknown = endpoint("bb", "x", Audience::Public, &[]);
        unknown.types.clear();
        let select: Select =
            serde_json::from_value(json!({"types": ["OffStreetParking"]})).unwrap();
        assert!(!select.matches(&unknown));
        assert!(Select::default().matches(&unknown));
    }
}
