//! Named MCP servers: `/api/mcp/{project}/{name}`, the hub's catalogue narrowed to the Endpoints
//! a `kind: McpServer` names (T-3155, ADR-N-043, EP-92…EP-96).
//!
//! A server holds no identity and no grant. Who may connect is its audience; what a caller reads
//! is each member's own call with the caller's token, decided by that member's PDP exactly as a
//! call to the member's URL is. A read without `endpoint` visits every member the caller may read,
//! in parallel, and answers each member apart: nothing is filtered, sorted or counted across two
//! members' Policies (§2.4).
//!
//! A server may also `select` Endpoints by project, space, domain, theme and access level
//! (ADR-N-043 §7): the selector is matched per call against the Endpoints the caller reads, each
//! then a member call like any other, so it only narrows; a read without `endpoint` asks at most
//! ten of them.

use crate::app::{Gateway, MAX_BODY};
use crate::auth::subject::Subject;
use crate::auth::subject::{authenticate, subject_from, EDGE_AUDIENCE, HUB_PATH};
use crate::auth::token::{self, Claims};
use crate::mcp::hub::{misrouted, Answer, Hub};
use crate::mcp::jsonrpc::{self, error, refused, result};
use crate::mcp::tools::{self, HubTool};
use crate::middleware::{rate_limit, tenancy};
use crate::resolver::Endpoint;
use axum::body::Body;
use axum::extract::{Path, Request, State};
use axum::http::header::AUTHORIZATION;
use axum::http::{HeaderMap, HeaderValue, Response, StatusCode};
use axum::response::IntoResponse;
use jc_core::kinds::mcp_server::{breadth, AccessLevel, Select};
use jc_core::kinds::{Audience, Representation};
use jc_core::reach::EndpointFacts;
use jc_core::ProblemDetails;
use serde_json::{json, Map, Value};
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// The read tools a call without `endpoint` runs once per member (ADR-N-043 §2.4).
pub(crate) const FAN_OUT: &[&str] = &["query_entities", "list_types", "describe_schema"];

/// How long one member call of a fan-out may take (ADR-N-043 §2.7).
pub(crate) const MEMBER_DEADLINE: Duration = Duration::from_secs(10);

/// How long a whole fan-out may take (ADR-N-043 §2.7).
const CALL_DEADLINE: Duration = Duration::from_secs(15);

/// The most Endpoints a read without `endpoint` asks (ADR-N-043 §7.4, EP-121).
pub(crate) const MAX_FAN_OUT: usize = 10;

/// The name of the server every organization has without a manifest (EP-122).
pub const ORG_ALL: &str = "all";

/// The first segment of an assistant's own server, `/api/mcp/d/{publicId}`: an
/// AssistantDeployment's reach, read-only for every caller (T-4691).
pub const DEPLOYMENTS: &str = "d";

/// The scheme of an assistant's `context` resource, `context://{publicId}`.
pub const CONTEXT_SCHEME: &str = "context://";

/// One named server as the gateway serves it (MF-53).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpServer {
    /// The project that declares it, `org` for the organization's.
    pub project: String,
    /// `metadata.name`, the last segment of its address.
    pub name: String,
    /// `metadata.title`, what an AI client shows as the server's name.
    pub title: Option<String>,
    /// `metadata.description`, what an AI client reads as its instructions.
    pub description: Option<String>,
    /// The member Endpoints' slugs, in the order of `spec.members`.
    pub members: Vec<String>,
    /// The Endpoints it selects beside its members (MF-55, ADR-N-043 §7).
    pub select: Option<Select>,
    /// A personal connector's favourites: only these slugs are selected (UI-98, EP-123).
    pub endpoints: Option<Vec<String>>,
    /// Who may connect.
    pub audience: Audience,
    /// The projects a `project-list` server admits besides its own.
    pub allowed_projects: Vec<String>,
    /// The knowledge sources its `search_documents` searches, as `(project, source)` (MF-57).
    pub knowledge_sources: Vec<(String, String)>,
}

/// Every named server the repository declares.
pub type McpServers = Vec<McpServer>;

impl McpServer {
    /// `org/all`: every Endpoint an organization member reads, served without a manifest until
    /// the organization declares `mcp/all.yaml` (EP-122, ADR-N-043 §7.5).
    pub fn org_all() -> Self {
        Self {
            project: jc_core::envelope::ORG_NAMESPACE.to_owned(),
            name: ORG_ALL.to_owned(),
            title: Some("All data".to_owned()),
            description: Some(
                "Every Endpoint of the organization you may read. Find a dataset in the \
                 `catalog` endpoint first, then read it by its own Endpoint."
                    .to_owned(),
            ),
            members: Vec::new(),
            select: Some(Select::default()),
            endpoints: None,
            audience: Audience::Organization,
            allowed_projects: Vec::new(),
            knowledge_sources: Vec::new(),
        }
    }

    /// Whether the selector takes `endpoint`, before who the caller is: its project, space,
    /// domain and theme match, and its audience is at least as wide as the server's, so a
    /// public server selects public Endpoints only (EP-120). A member is never selected twice.
    fn selects(&self, endpoint: &Endpoint) -> bool {
        let Some(select) = &self.select else {
            return false;
        };
        if self.members.contains(&endpoint.slug)
            || breadth(endpoint.audience) < breadth(self.audience)
            || self
                .endpoints
                .as_ref()
                .is_some_and(|endpoints| !endpoints.contains(&endpoint.slug))
        {
            return false;
        }
        select.matches(&facts(endpoint))
    }

    /// Whether the selector's `accessLevel` takes an Endpoint this caller reads as `subject`.
    fn selects_level(&self, endpoint: &Endpoint, subject: &Subject) -> bool {
        let level = match super::catalog::because(endpoint, subject) {
            "public" => AccessLevel::Public,
            "grant" => AccessLevel::Grant,
            "organization" => AccessLevel::Organization,
            _ => AccessLevel::Project,
        };
        self.select
            .as_ref()
            .and_then(|select| select.access_level.as_ref())
            .is_none_or(|levels| levels.contains(&level))
    }

    /// Whether it offers the resource `context` (EP-136, API/05 §1.9): an assistant's server on
    /// an anonymous channel, whose context the assistant answers any visitor.
    pub fn has_context(&self) -> bool {
        self.project == DEPLOYMENTS && self.audience == Audience::Public
    }

    /// The URI of its `context` resource.
    pub fn context_uri(&self) -> String {
        format!("{CONTEXT_SCHEME}{}", self.name)
    }

    /// Its address under the gateway's base URL (EP-92).
    pub fn path(&self) -> String {
        format!("{HUB_PATH}/{}/{}", self.project, self.name)
    }

    /// Its Keycloak client, which is also the audience of its tokens (ADR-N-043 §2.2).
    pub fn client(&self) -> String {
        format!("mcp-{}-{}", self.project, self.name)
    }

    /// Whether a caller of one of `projects` may connect; the projects of an anonymous caller
    /// are none. A server is never wider than its narrowest member, and each member call
    /// still applies the member's own audience (EP-93).
    fn admits(&self, projects: Option<&[String]>) -> bool {
        match (self.audience, projects) {
            (Audience::Public, _) => true,
            (_, None) => false,
            (Audience::Organization, Some(_)) => true,
            (Audience::ProjectList, Some(projects)) => projects
                .iter()
                .any(|caller| *caller == self.project || self.allowed_projects.contains(caller)),
        }
    }
}

/// What a selector reads of `endpoint` (MF-55, MF-61): its types, their classification, and its
/// themes, a theme set by hand winning over the derived one as on the DCAT record (T-4498). The
/// table holds no manifest name, so the slug stands in; matching never reads it.
pub(crate) fn facts(endpoint: &Endpoint) -> EndpointFacts {
    let classified = crate::handlers::endpoint_surface::classification(endpoint);
    let themes = match endpoint.catalog.as_deref().filter(|c| !c.themes.is_empty()) {
        Some(catalog) => catalog.themes.iter().copied().collect(),
        None => classified.themes,
    };
    EndpointFacts {
        project: endpoint.project.clone(),
        name: endpoint.slug.clone(),
        slug: endpoint.slug.clone(),
        space: endpoint.space.clone(),
        audience: endpoint.audience,
        types: crate::handlers::endpoint_surface::classes(endpoint)
            .into_iter()
            .map(str::to_owned)
            .collect(),
        domains: classified.domains.into_iter().map(str::to_owned).collect(),
        themes,
    }
}

/// One JSON-RPC message to a named server (EP-92).
pub async fn message(
    State(gateway): State<Arc<Gateway>>,
    Path((project, name)): Path<(String, String)>,
    mut request: Request,
) -> Response<Body> {
    tenancy::strip_client_headers(&mut request);
    if project == super::personal::PROJECT {
        return super::personal::message(gateway, name, request).await;
    }
    let Some(server) = gateway.server(&project, &name) else {
        return ProblemDetails::not_found().into_response();
    };
    serve(gateway, server, request).await
}

/// One JSON-RPC message to `server`, already resolved: its members and selected Endpoints as
/// this caller reads them (EP-92, EP-120).
pub(crate) async fn serve(
    gateway: Arc<Gateway>,
    server: McpServer,
    request: Request,
) -> Response<Body> {
    // Members in the order the manifest names them, then the selected Endpoints by slug; one
    // that no longer serves MCP is no member.
    let members: Vec<Arc<Endpoint>> = server
        .members
        .iter()
        .filter_map(|slug| gateway.resolver.resolve(slug))
        // Never wider than its narrowest member (EP-93): a public server's internal member
        // is not served, whoever calls.
        .filter(|endpoint| breadth(endpoint.audience) >= breadth(server.audience))
        .chain(
            gateway
                .resolver
                .endpoints()
                .into_iter()
                .filter(|endpoint| server.selects(endpoint)),
        )
        .filter(|endpoint| endpoint.serves(Representation::Mcp))
        .collect();
    let level = |(endpoint, subject): &(Arc<Endpoint>, Subject)| {
        server.members.contains(&endpoint.slug) || server.selects_level(endpoint, subject)
    };

    let authorization = request.headers().get(AUTHORIZATION).cloned();
    let presented = authorization.as_ref().and_then(|value| value.to_str().ok());
    let caller = rate_limit::caller_key(&gateway, request.headers());
    let (subject_key, reach, reads_only) = match token::bearer(presented) {
        // A public server answers an anonymous caller with each member's public grants
        // (ADR-N-043 §2.2); the members' own audience decides, as on their URLs.
        Err(token::Rejected::NoToken) if server.admits(None) => {
            if let Err(refusal) = rate_limit::spend_anonymous_mcp(&gateway, request.headers()) {
                return *refusal;
            }
            let reach = members
                .into_iter()
                .filter_map(|endpoint| {
                    let subject = authenticate(&gateway, &endpoint, &HeaderMap::new()).ok()?;
                    readable(&gateway, endpoint, subject)
                })
                .filter(level)
                .collect();
            (format!("anonymous:{caller}"), reach, true)
        }
        Err(token::Rejected::NoToken) => return challenge(&gateway, &server),
        Err(rejected) => return ProblemDetails::from(rejected).into_response(),
        Ok(raw) => {
            let Some(verifier) = gateway.verifier.as_ref() else {
                tracing::warn!(
                    "a token was presented to a named MCP server but no realm is configured"
                );
                return ProblemDetails::unauthorized().into_response();
            };
            // The server's own audiences and the edge's; a token for one Endpoint or for the hub
            // is refused here (EP-94, PF-45).
            let mut audiences = gateway.server_audiences(&server);
            audiences.push(EDGE_AUDIENCE.to_owned());
            let claims = match verifier.verify(raw, &audiences) {
                Ok(claims) => claims,
                Err(rejected) => return ProblemDetails::from(rejected).into_response(),
            };
            if !server.admits(Some(&projects_of(&gateway, &claims))) {
                tracing::warn!(
                    principal = %claims.sub,
                    server = %server.path(),
                    "the token names no project this server admits"
                );
                return ProblemDetails::forbidden().into_response();
            }
            let reach = members
                .into_iter()
                .filter_map(|endpoint| {
                    let subject = subject_from(&gateway, &endpoint, &claims).ok()?;
                    readable(&gateway, endpoint, subject)
                })
                .filter(level)
                .collect();
            // A public server reads only, whoever calls (EP-141): writes stay on the member's URL.
            let reads_only = server.project == DEPLOYMENTS || server.audience == Audience::Public;
            (format!("subject:{}", claims.sub), reach, reads_only)
        }
    };

    // The server's per-subject bucket, on top of each member's own (ADR-N-043 §2.7).
    let decision = gateway.rate_limiter.check(
        &server.path(),
        &subject_key,
        &gateway.hub_rate_limit,
        Instant::now(),
    );
    if !decision.allowed {
        tracing::info!(server = %server.path(), "a named server's rate limit reached");
        return rate_limit::too_many(
            &decision,
            "the server's per-subject rate limit is spent; retry after the seconds the RateLimit-Reset header names",
        );
    }

    let (_, body) = request.into_parts();
    let Ok(bytes) = axum::body::to_bytes(body, MAX_BODY).await else {
        return jsonrpc::parse_error();
    };
    let Ok(message) = serde_json::from_slice::<Value>(&bytes) else {
        return jsonrpc::parse_error();
    };
    let hub = Hub {
        gateway,
        reach,
        caller,
        server: Some(server),
        anonymous: false,
        reads_only,
    };
    let mut response = match hub.answer(message).await {
        Answer::Json(answer) => jsonrpc::json_response(StatusCode::OK, &answer),
        Answer::Notification => StatusCode::ACCEPTED.into_response(),
        Answer::Refused(response) => response,
    };
    rate_limit::set_headers(response.headers_mut(), &decision);
    response
}

/// The member with who the caller is on it, when the caller may read anything there: a member
/// the caller may not read is silent everywhere (ADR-N-043 §2.6, SP-20).
fn readable(
    gateway: &Gateway,
    endpoint: Arc<Endpoint>,
    subject: Subject,
) -> Option<(Arc<Endpoint>, Subject)> {
    tools::reads_anything(gateway, &endpoint, &subject).then_some((endpoint, subject))
}

/// The projects a verified caller acts in: a ServiceAccount's own, a person's groups.
fn projects_of(gateway: &Gateway, claims: &Claims) -> Vec<String> {
    if let Some(project) = claims
        .azp
        .as_deref()
        .and_then(|azp| gateway.account_project(azp))
    {
        return vec![project];
    }
    claims
        .groups
        .iter()
        .map(|group| group.trim_start_matches('/').to_owned())
        .collect()
}

/// `initialize` of a named server: its title and description are what the client reads.
pub(crate) fn initialized(server: Option<&McpServer>) -> Value {
    let (name, title, description) = server.map_or(("", None, None), |server| {
        (
            server.name.as_str(),
            server.title.as_deref(),
            server.description.as_deref(),
        )
    });
    let mut instructions = description.map(str::to_owned).unwrap_or_default();
    if !instructions.is_empty() {
        instructions.push_str("\n\n");
    }
    instructions.push_str(
        "One server over several Endpoints. list_endpoints names the ones you may read. \
         query_entities, list_types and describe_schema without `endpoint` ask every one of \
         them and answer each apart in `results`, with `partial` and `failed` when one could \
         not answer; every other tool names one Endpoint in `endpoint`. For changes as they \
         happen, describe_access names an Endpoint's WebSocket in `liveUpdates`.\n\n",
    );
    instructions.push_str(&super::PRIMER);
    let mut capabilities = json!({ "tools": { "listChanged": false } });
    if server.is_some_and(McpServer::has_context) {
        instructions.push_str(
            "\n\nThe resource `context` says what this assistant knows: its categories, its \
             Endpoints with their counts and its knowledge sources. Read it first.",
        );
        capabilities["resources"] = json!({ "listChanged": false });
    }
    json!({
        "protocolVersion": jsonrpc::PROTOCOL_VERSION,
        "capabilities": capabilities,
        "serverInfo": {
            "name": name,
            "title": title.unwrap_or(name),
            "version": env!("CARGO_PKG_VERSION"),
        },
        "instructions": instructions,
    })
}

/// A named server's catalogue: on the read tools `endpoint` is optional and `cursor` may be the
/// object a fan-out answered as `nextCursor` (ADR-N-043 §2.3, §2.4).
pub(crate) fn fan_out_schemas(tools: &mut [Value]) {
    for tool in tools
        .iter_mut()
        .filter(|tool| FAN_OUT.iter().any(|name| tool["name"] == *name))
    {
        let schema = &mut tool["inputSchema"];
        if let Some(required) = schema.get_mut("required").and_then(Value::as_array_mut) {
            required.retain(|name| name != "endpoint");
        }
        if let Some(endpoint) = schema.pointer_mut("/properties/endpoint/description") {
            *endpoint = json!(
                "The Endpoint this call reads, by the slug list_endpoints answers. Leave it out \
                 to ask every Endpoint of this server; each answers apart in `results`."
            );
        }
        if let Some(cursor) = schema.pointer_mut("/properties/cursor") {
            *cursor = json!({
                "oneOf": [
                    cursor.clone(),
                    {
                        "type": "object",
                        "additionalProperties": { "type": ["string", "integer"] },
                        "description": "Without `endpoint`: the previous answer's nextCursor, \
                                        one cursor per Endpoint slug, passed back as given; only \
                                        those Endpoints are asked",
                    },
                ],
            });
        }
        // Without `endpoint` the answer is `results`, one entry per member, not the member's
        // own object: a client that validates structuredContent must accept both (T-4871).
        let Some(output) = tool.get_mut("outputSchema").and_then(Value::as_object_mut) else {
            continue;
        };
        if let Some(required) = output.remove("required") {
            output.insert(
                "anyOf".to_owned(),
                json!([{ "required": required }, { "required": ["results"] }]),
            );
        }
        if let Some(Value::Object(properties)) = output.get_mut("properties") {
            properties.insert(
                "results".to_owned(),
                json!({ "type": "array", "items": { "type": "object", "required": ["endpoint", "space"] } }),
            );
            properties.insert("partial".to_owned(), json!({ "type": "boolean" }));
            properties.insert(
                "failed".to_owned(),
                json!({ "type": "array", "items": { "type": "object", "required": ["endpoint", "reason"] } }),
            );
            properties.insert(
                "nextCursor".to_owned(),
                json!({ "type": ["string", "integer", "object"] }),
            );
        }
    }
}

impl Hub {
    /// A read without `endpoint`: once per member this caller may read, in parallel, each run
    /// exactly as a call to that member's own URL runs, answered apart (ADR-N-043 §2.4, §2.6).
    pub(crate) async fn fan_out(
        &self,
        id: Value,
        name: &str,
        params: &Value,
        arguments: Map<String, Value>,
    ) -> Answer {
        if let Some(refusal) = misrouted(&id, &arguments) {
            return Answer::Json(refusal);
        }
        let cursors = match arguments.get("cursor") {
            None | Some(Value::Number(_)) => None,
            Some(Value::Object(cursors)) => Some(cursors.clone()),
            Some(_) => {
                return Answer::Json(result(
                    id,
                    refused(
                        "`cursor` is the previous answer's nextCursor: an object of one cursor per Endpoint slug",
                        &Value::Null,
                    ),
                ))
            }
        };

        // A selector can reach thousands: past ten the agent finds its Endpoint first (EP-121).
        let asking = cursors.as_ref().map_or(self.reach.len(), Map::len);
        if asking > MAX_FAN_OUT {
            return Answer::Json(result(
                id,
                refused(
                    &format!(
                        "this server reaches {asking} Endpoints you may read, more than the {MAX_FAN_OUT} a call without `endpoint` asks: find the one you need with list_endpoints or the `catalog` endpoint, then name it in `endpoint`"
                    ),
                    &Value::Null,
                ),
            ));
        }
        let mut failed: Vec<(usize, Value)> = Vec::new();
        let mut runs = tokio::task::JoinSet::new();
        let mut asked: Vec<(usize, String)> = Vec::new();
        for (index, (endpoint, subject)) in self.reach.iter().enumerate() {
            let mut own = arguments.clone();
            match cursors.as_ref().map(|cursors| cursors.get(&endpoint.slug)) {
                None => {}
                // A member the previous page finished is not asked again.
                Some(None) => continue,
                Some(Some(offset)) => {
                    own.insert("cursor".to_owned(), offset.clone());
                }
            }
            match tools::hub_tool(&self.gateway, endpoint, subject, &self.reach, name) {
                HubTool::Unknown => return Answer::Json(error(id, -32602, "unknown tool")),
                HubTool::Ungranted(refusal) => {
                    failed.push((index, failure(&endpoint.slug, &refusal)));
                    continue;
                }
                HubTool::Granted => {}
            }
            asked.push((index, endpoint.slug.clone()));
            if self.spend(endpoint).is_err() {
                failed.push((
                    index,
                    json!({ "endpoint": endpoint.slug, "reason": "this Endpoint's rate limit is spent" }),
                ));
                continue;
            }
            let hub = self.clone();
            let (endpoint, subject) = (Arc::clone(endpoint), subject.clone());
            let (name, id, params) = (name.to_owned(), id.clone(), params.clone());
            runs.spawn(async move {
                let answer = tokio::time::timeout(
                    MEMBER_DEADLINE,
                    hub.forward(&endpoint, &subject, &name, id, &params, own),
                )
                .await;
                (index, endpoint, answer)
            });
        }
        if asked.is_empty() && failed.is_empty() && cursors.is_none() {
            // No member this caller reads grants the tool: the server never listed it.
            return Answer::Json(error(id, -32602, "unknown tool"));
        }

        let mut results: Vec<(usize, Value)> = Vec::new();
        let mut next: BTreeMap<usize, (String, Value)> = BTreeMap::new();
        let deadline = tokio::time::Instant::now() + CALL_DEADLINE;
        while let Ok(Some(joined)) = tokio::time::timeout_at(deadline, runs.join_next()).await {
            let Ok((index, endpoint, answer)) = joined else {
                continue;
            };
            match answer {
                Err(_) => failed.push((
                    index,
                    json!({ "endpoint": endpoint.slug, "reason": "no answer within ten seconds" }),
                )),
                Ok(answer) => match member_entry(&endpoint, &answer) {
                    Ok((entry, cursor)) => {
                        if let Some(cursor) = cursor {
                            next.insert(index, (endpoint.slug.clone(), cursor));
                        }
                        results.push((index, entry));
                    }
                    Err(reason) => failed.push((index, reason)),
                },
            }
        }
        // Past the call's deadline, or a run that ended without an answer: a failed member.
        runs.abort_all();
        for (index, slug) in asked {
            let settled = |entries: &[(usize, Value)]| entries.iter().any(|(at, _)| *at == index);
            if !settled(&results) && !settled(&failed) {
                failed.push((
                    index,
                    json!({ "endpoint": slug, "reason": "no answer within the call's fifteen seconds" }),
                ));
            }
        }

        results.sort_by_key(|(index, _)| *index);
        failed.sort_by_key(|(index, _)| *index);
        let mut structured = Map::new();
        structured.insert(
            "results".to_owned(),
            Value::Array(results.into_iter().map(|(_, entry)| entry).collect()),
        );
        if !failed.is_empty() {
            structured.insert("partial".to_owned(), Value::Bool(true));
            structured.insert(
                "failed".to_owned(),
                Value::Array(failed.into_iter().map(|(_, entry)| entry).collect()),
            );
        }
        if !next.is_empty() {
            structured.insert(
                "nextCursor".to_owned(),
                Value::Object(next.into_values().collect()),
            );
        }
        let answered_nothing = structured["results"].as_array().is_some_and(Vec::is_empty)
            && structured.contains_key("failed");
        let structured = Value::Object(structured);
        Answer::Json(result(
            id,
            json!({
                "isError": answered_nothing,
                "content": [{
                    "type": "text",
                    "text": serde_json::to_string(&structured).unwrap_or_else(|_| "null".to_owned()),
                }],
                "structuredContent": structured,
            }),
        ))
    }
}

/// One member's answer as a `results` entry and its `nextCursor`, or the `failed` entry that
/// names why it gave none.
fn member_entry(endpoint: &Endpoint, answer: &Value) -> Result<(Value, Option<Value>), Value> {
    if let Some(message) = answer.pointer("/error/message").and_then(Value::as_str) {
        return Err(json!({ "endpoint": endpoint.slug, "reason": message }));
    }
    let result = answer.get("result").unwrap_or(&Value::Null);
    if result.get("isError") != Some(&Value::Bool(false)) {
        return Err(failure(&endpoint.slug, result));
    }
    let mut entry = Map::new();
    entry.insert("endpoint".to_owned(), json!(endpoint.slug));
    entry.insert("space".to_owned(), json!(endpoint.space));
    let mut cursor = None;
    if let Some(Value::Object(structured)) = result.get("structuredContent") {
        for (key, value) in structured {
            if key == "nextCursor" {
                cursor = Some(value.clone());
            }
            if key != "endpoint" && key != "space" {
                entry.insert(key.clone(), value.clone());
            }
        }
    }
    Ok((Value::Object(entry), cursor))
}

/// A `failed` entry from a tool error: the member and the sentence it answered.
fn failure(slug: &str, refusal: &Value) -> Value {
    let reason = refusal
        .pointer("/content/0/text")
        .and_then(Value::as_str)
        .unwrap_or("the Endpoint gave no answer");
    json!({ "endpoint": slug, "reason": reason })
}

/// The `401` that names the server's resource metadata (RFC 9728, AG-32).
/// `401` with the server's resource metadata named, which is how a client learns to log in.
pub(crate) fn challenge(gateway: &Gateway, server: &McpServer) -> Response<Body> {
    let mut response = ProblemDetails::new(401, "unauthorized", "Unauthorized")
        .with_detail("this MCP server needs an access token; its authorization server is named by the resource metadata")
        .into_response();
    let metadata = format!(
        "{}{}/.well-known/oauth-protected-resource",
        gateway.base_url(),
        server.path()
    );
    if let Ok(challenge) =
        HeaderValue::from_str(&format!("Bearer resource_metadata=\"{metadata}\""))
    {
        response
            .headers_mut()
            .insert(axum::http::header::WWW_AUTHENTICATE, challenge);
    }
    response
}

/// A named server's protected-resource metadata (RFC 9728, ADR-N-043 §2.2, EP-95).
pub async fn protected_resource(
    State(gateway): State<Arc<Gateway>>,
    Path((project, name)): Path<(String, String)>,
) -> Response<Body> {
    // A personal address answers the same metadata for every name: it says nothing of whose
    // connectors exist (EP-123).
    let Some(server) = gateway.server(&project, &name).or_else(|| {
        (project == super::personal::PROJECT && gateway.personal.is_some())
            .then(|| super::personal::addressed(&name))
            .flatten()
    }) else {
        return ProblemDetails::not_found().into_response();
    };
    let servers = crate::oauth::authorization_servers(&gateway, server.admits(None));
    if servers.is_empty() {
        return ProblemDetails::not_found().into_response();
    }
    jsonrpc::json_response(
        StatusCode::OK,
        &json!({
            "resource": format!("{}{}", gateway.base_url(), server.path()),
            "authorization_servers": servers,
            "bearer_methods_supported": ["header"],
            "resource_name": server.title.as_deref().unwrap_or(&server.name),
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// EP-122, MF-57: the organization's built-in server selects everything an organization
    /// member reads and attaches no knowledge sources, so it lists no `search_documents`.
    #[test]
    fn org_all_is_an_organization_server_with_no_knowledge_sources() {
        let all = McpServer::org_all();
        assert_eq!((all.project.as_str(), all.name.as_str()), ("org", ORG_ALL));
        assert_eq!(all.audience, Audience::Organization);
        assert_eq!(all.select, Some(Select::default()));
        assert!(all.members.is_empty() && all.knowledge_sources.is_empty());
    }

    fn server(audience: Audience) -> McpServer {
        McpServer {
            project: "helsinki".into(),
            name: "mobility".into(),
            title: None,
            description: None,
            members: vec!["bikes".into()],
            select: None,
            endpoints: None,
            audience,
            allowed_projects: vec!["espoo".into()],
            knowledge_sources: Vec::new(),
        }
    }

    #[test]
    fn the_audience_admits_by_project_and_only_public_admits_nobody() {
        let none: &[String] = &[];
        let espoo = ["espoo".to_owned()];
        let vantaa = ["vantaa".to_owned()];
        assert!(server(Audience::Public).admits(None));
        assert!(!server(Audience::Organization).admits(None));
        assert!(server(Audience::Organization).admits(Some(none)));
        assert!(server(Audience::ProjectList).admits(Some(&espoo)));
        assert!(server(Audience::ProjectList).admits(Some(&["helsinki".to_owned()])));
        assert!(!server(Audience::ProjectList).admits(Some(&vantaa)));
        assert!(!server(Audience::ProjectList).admits(Some(none)));
    }

    #[test]
    fn a_server_is_its_own_resource_and_client() {
        let server = server(Audience::Public);
        assert_eq!(server.path(), "/api/mcp/helsinki/mobility");
        assert_eq!(server.client(), "mcp-helsinki-mobility");
    }

    #[test]
    fn the_read_tools_take_endpoint_as_an_option_and_a_cursor_per_member() {
        let mut tools = vec![
            json!({ "name": "query_entities", "inputSchema": {
                "properties": { "endpoint": { "type": "string" }, "cursor": { "type": "integer" } },
                "required": ["endpoint"],
            }}),
            json!({ "name": "upsert_entity", "inputSchema": {
                "properties": { "endpoint": { "type": "string" } },
                "required": ["endpoint"],
            }}),
        ];
        fan_out_schemas(&mut tools);
        assert_eq!(tools[0]["inputSchema"]["required"], json!([]));
        assert_eq!(
            tools[0]["inputSchema"]["properties"]["cursor"]["oneOf"][1]["type"],
            "object"
        );
        assert_eq!(tools[1]["inputSchema"]["required"], json!(["endpoint"]));
    }

    /// T-4871: a strict MCP client (the inspector, the TypeScript SDK) checks structuredContent
    /// against outputSchema; a fan-out answer and a one-Endpoint answer both pass, a fan-out
    /// answer missing `results` does not.
    #[test]
    fn the_output_schema_admits_a_fan_out_answer_and_a_member_answer() {
        let mut tools = vec![json!({
            "name": "list_types",
            "inputSchema": { "properties": { "endpoint": { "type": "string" } }, "required": ["endpoint"] },
            "outputSchema": {
                "type": "object",
                "properties": { "entityTypes": {}, "restricted": { "type": "boolean" } },
                "required": ["entityTypes"],
            },
        })];
        fan_out_schemas(&mut tools);
        let validator = jsonschema::validator_for(&tools[0]["outputSchema"]).expect("a schema");
        let fan_out = json!({
            "results": [{ "endpoint": "a", "space": "helsinki", "entityTypes": { "typeList": ["Event"] } }],
            "partial": true,
            "failed": [{ "endpoint": "b", "reason": "no answer within ten seconds" }],
            "nextCursor": { "a": "c1" },
        });
        assert!(validator.is_valid(&fan_out), "fan-out answer refused");
        assert!(validator
            .is_valid(&json!({ "entityTypes": { "typeList": ["Event"] }, "nextCursor": "c1" })));
        assert!(!validator.is_valid(&json!({ "partial": true })));
        assert!(!validator.is_valid(&json!({ "results": [{ "endpoint": "a" }] })));
    }
}
