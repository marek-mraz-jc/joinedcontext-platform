//! The MCP hub: one connector over every Endpoint the caller's token may read (T-2490, ADR-N-025,
//! EP-87, EP-88).
//!
//! The hub holds no data path of its own. It answers who the caller may reach, and every tool
//! call it takes goes to the named Endpoint's view on the broker, `/x/views/{slug}/mcp`, in the
//! broker's form and under the verified caller, as a call to `/api/endpoint/{slug}/mcp` does
//! (T-3808); the gateway answers `describe_schema` and `describe_access` itself, from the same
//! documents its access and schema surfaces serve (SP-16). Nothing is remembered between calls (SP-19):
//! the agent names the Endpoint on every call, and the list is rebuilt from the token each time.

use crate::app::{Gateway, MAX_BODY};
use crate::auth::subject::{anonymous_on, principal_of, subject_from, HUB_PATH};
use crate::auth::token;
use crate::handlers::{endpoint_surface, schema};
use crate::mcp::jsonrpc::{self, answered, error, refused, result};
use crate::mcp::server::{self, McpServer};
use crate::mcp::tools::{self, HubTool};
use crate::mcp::{catalog, describe};
use crate::middleware::{rate_limit, tenancy};
use crate::pdp::evaluator::Subject;
use crate::resolver::Endpoint;
use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::header::AUTHORIZATION;
use axum::http::{HeaderMap, HeaderValue, Response, StatusCode};
use axum::response::IntoResponse;
use jc_core::kinds::{Audience, EndpointSlug, Operation, Representation};
use jc_core::ProblemDetails;
use serde_json::{json, Map, Value};
use std::sync::Arc;
use std::time::Instant;

/// Arguments that would choose a space some other way than `endpoint` (AG-05, SP-14).
pub(crate) const SELECTORS: &[&str] = &["space", "tenant", "contextspace", "slug"];

/// How many Endpoints one `list_endpoints` page answers unless the caller asks for fewer.
const PAGE: usize = 50;

/// The key the per-subject bucket is kept under. It holds a `/`, so no Endpoint slug is it.
const HUB_BUCKET: &str = HUB_PATH;

/// One JSON-RPC message to the hub (EP-87).
pub async fn message(State(gateway): State<Arc<Gateway>>, mut request: Request) -> Response<Body> {
    tenancy::strip_client_headers(&mut request);

    let Reached {
        reach,
        decision,
        anonymous,
        ..
    } = match reached(&gateway, request.headers(), Some(Representation::Mcp)) {
        Ok(reached) => reached,
        Err(refusal) => return *refusal,
    };
    if anonymous {
        if let Err(refusal) = rate_limit::spend_anonymous_mcp(&gateway, request.headers()) {
            return *refusal;
        }
    }

    let caller = rate_limit::caller_key(request.headers());
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
        server: None,
        anonymous,
        reads_only: anonymous,
    };
    let mut response = match hub.answer(message).await {
        Answer::Json(answer) => jsonrpc::json_response(StatusCode::OK, &answer),
        Answer::Notification => StatusCode::ACCEPTED.into_response(),
        Answer::Refused(response) => response,
    };
    rate_limit::set_headers(response.headers_mut(), &decision);
    response
}

/// Who a token is and what it reaches: the one list the hub's `list_endpoints` and
/// `GET /api/endpoints` answer (EP-87, EP-88, EP-105).
pub(crate) struct Reached {
    pub(crate) principal: String,
    /// Every Endpoint the token reaches, admits and grants a read on, in slug order.
    pub(crate) reach: Vec<(Arc<Endpoint>, Subject)>,
    /// The per-subject bucket after this request spent from it (ADR-N-025 section 3).
    pub(crate) decision: rate_limit::Decision,
    /// No token was presented: `reach` is the public Endpoints alone (T-4114).
    pub(crate) anonymous: bool,
}

/// Verifies the bearer token against every audience the hub could honour (its own and each
/// Endpoint's, ADR-N-025 section 4: a ServiceAccount's audiences name its Endpoints, PF-47),
/// spends the per-subject bucket, and lists what the token reaches, of the Endpoints that serve
/// `serving` (`None`: any). The refusal is the HTTP answer.
pub(crate) fn reached(
    gateway: &Gateway,
    headers: &HeaderMap,
    serving: Option<Representation>,
) -> Result<Reached, Box<Response<Body>>> {
    // Without a token the hub answers over the public Endpoints alone, each as its own URL
    // answers an anonymous caller (T-4114, EP-87); a slug outside them answers `401` with the
    // hub's resource metadata (`Hub::call`), which is how a client learns to log in.
    let presented = headers
        .get(AUTHORIZATION)
        .and_then(|value| value.to_str().ok());
    let raw = match token::bearer(presented) {
        Ok(raw) => Some(raw),
        Err(token::Rejected::NoToken) => None,
        Err(rejected) => return Err(Box::new(ProblemDetails::from(rejected).into_response())),
    };

    // Which Endpoints the token then reaches is decided per Endpoint by `reaches`.
    let served: Vec<Arc<Endpoint>> = gateway
        .resolver
        .endpoints()
        .into_iter()
        .filter(|endpoint| serving.is_none_or(|wanted| endpoint.serves(wanted)))
        .collect();
    let claims = match raw {
        None => None,
        Some(raw) => {
            let Some(verifier) = gateway.verifier.as_ref() else {
                tracing::warn!("a token was presented to the hub but no realm is configured");
                return Err(Box::new(ProblemDetails::unauthorized().into_response()));
            };
            let mut audiences = gateway.hub_audiences();
            for endpoint in &served {
                audiences.extend(gateway.audiences_for(endpoint));
            }
            audiences.sort_unstable();
            audiences.dedup();
            Some(
                verifier
                    .verify(raw, &audiences)
                    .map_err(|rejected| Box::new(ProblemDetails::from(rejected).into_response()))?,
            )
        }
    };

    // One bucket per subject across every Endpoint, spent on every hub request, so fifty
    // Endpoints are not fifty bursts (ADR-N-025 section 3); an anonymous caller's is its
    // client address, the key a public Endpoint's own URL spends.
    let bucket = match &claims {
        Some(claims) => format!("subject:{}", claims.sub),
        None => rate_limit::caller_key(headers),
    };
    let decision =
        gateway
            .rate_limiter
            .check(HUB_BUCKET, &bucket, &gateway.hub_rate_limit, Instant::now());
    if !decision.allowed {
        tracing::info!(caller = %bucket, "hub rate limit reached");
        return Err(Box::new(rate_limit::too_many(
            &decision,
            "the hub's per-subject rate limit is spent; retry after the seconds the RateLimit-Reset header names",
        )));
    }

    // What the token reaches as its own caller; a public Endpoint it does not reach, as the
    // anonymous caller the Endpoint's URL would see. Nothing internal without a token.
    let mut reach: Vec<(Arc<Endpoint>, Subject)> = served
        .into_iter()
        .filter_map(|endpoint| {
            let own = claims
                .as_ref()
                .filter(|claims| gateway.reaches(claims, &endpoint))
                .and_then(|claims| subject_from(gateway, &endpoint, claims).ok())
                .filter(|subject| tools::reads_anything(gateway, &endpoint, subject));
            let subject = match own {
                Some(subject) => subject,
                None if endpoint.audience == Audience::Public => anonymous_on(&endpoint),
                None => return None,
            };
            tools::reads_anything(gateway, &endpoint, &subject).then_some((endpoint, subject))
        })
        .collect();
    reach.sort_by(|(a, _), (b, _)| a.slug.cmp(&b.slug));
    Ok(Reached {
        principal: claims.map_or_else(|| "anonymous".to_owned(), |claims| claims.sub),
        reach,
        decision,
        anonymous: raw.is_none(),
    })
}

/// `GET /api/endpoints`: the Endpoints this token reaches over every representation, the REST
/// twin of `list_endpoints` for a client that does not speak MCP (EP-105, ADR-N-025 §4a).
pub async fn endpoints(
    State(gateway): State<Arc<Gateway>>,
    mut request: Request,
) -> Response<Body> {
    tenancy::strip_client_headers(&mut request);
    let Reached {
        principal,
        reach,
        decision,
        ..
    } = match reached(&gateway, request.headers(), None) {
        Ok(reached) => reached,
        Err(refusal) => return *refusal,
    };
    // The query as `list_endpoints` takes its arguments: `limit` is a number, the filters and
    // the cursor stay text, and anything else is refused by the same words.
    let arguments: Map<String, Value> =
        crate::query::parse(request.uri().query().unwrap_or_default())
            .into_iter()
            .map(|(name, value)| {
                let value = match name.as_str() {
                    "limit" => value
                        .parse::<u64>()
                        .map_or_else(|_| json!(value), |number| json!(number)),
                    _ => json!(value),
                };
                (name, value)
            })
            .collect();
    let mut response = match page(&gateway, &reach, &arguments) {
        Ok(page) => {
            tracing::info!(principal = %principal, total = reach.len(), "endpoint list");
            jsonrpc::json_response(StatusCode::OK, &page)
        }
        Err(why) => ProblemDetails::bad_request()
            .with_detail(why)
            .into_response(),
    };
    rate_limit::set_headers(response.headers_mut(), &decision);
    response
}

/// One page of `reach`: `{endpoints, total, nextCursor?}`, or why the arguments are refused.
fn page(
    gateway: &Gateway,
    reach: &[(Arc<Endpoint>, Subject)],
    arguments: &Map<String, Value>,
) -> Result<Value, String> {
    if let Some(unknown) = arguments
        .keys()
        .find(|key| !matches!(key.as_str(), "limit" | "cursor") && !FILTERS.contains(&key.as_str()))
    {
        return Err(format!("`{unknown}` is not an argument of list_endpoints"));
    }
    let limit = match arguments.get("limit") {
        None => PAGE,
        Some(value) => value
            .as_u64()
            .filter(|limit| (1..=100).contains(limit))
            .ok_or("`limit` is a whole number from 1 to 100")? as usize,
    };
    // The cursor this hub hands out is an offset written as a string; one an older client
    // kept as a number still reads.
    let offset = match arguments.get("cursor") {
        None => Some(0),
        Some(Value::String(cursor)) => cursor.parse::<usize>().ok(),
        Some(value) => value.as_u64().and_then(|n| usize::try_from(n).ok()),
    }
    .ok_or("`cursor` is the previous page's nextCursor, passed back as given")?;
    let mut wanted = Vec::new();
    for name in FILTERS {
        match arguments.get(*name) {
            None => {}
            Some(Value::String(text)) if !text.trim().is_empty() && text.len() <= 200 => {
                wanted.push((*name, text.trim().to_lowercase()));
            }
            Some(_) => return Err(format!("`{name}` is a text of 1 to 200 characters")),
        }
    }
    if let Some((_, access)) = wanted.iter().find(|(name, _)| *name == "access") {
        if !matches!(access.as_str(), "public" | "internal") {
            return Err("`access` is `public` or `internal`".to_owned());
        }
    }

    let rows: Vec<Value> = reach
        .iter()
        .map(|(endpoint, subject)| {
            let mut entry = described(gateway, endpoint, subject);
            // Each Endpoint names the broker its space lives on (ADR-N-047 §2.3).
            entry["broker"] = json!(gateway.broker.broker_of_view(&endpoint.slug));
            entry
        })
        .filter(|entry| {
            wanted
                .iter()
                .all(|(name, value)| matches(entry, name, value))
        })
        .collect();
    let total = rows.len();
    let endpoints: Vec<Value> = rows.into_iter().skip(offset).take(limit).collect();
    let mut page = json!({ "endpoints": endpoints, "total": total });
    let next = offset.saturating_add(limit);
    if next < total {
        page["nextCursor"] = json!(next.to_string());
    }
    Ok(page)
}

/// The filters `list_endpoints` takes beside its paging (EP-111), each one text.
const FILTERS: &[&str] = &[
    "q",
    "type",
    "attribute",
    "space",
    "access",
    "domain",
    "theme",
];

/// Whether one described Endpoint passes one filter; `value` is lowercase. A type, an
/// attribute and a space match whole names, `q` any part of a title or description.
fn matches(entry: &Value, name: &str, value: &str) -> bool {
    let names = |pointer: &str| -> Vec<String> {
        match entry.pointer(pointer) {
            Some(Value::Array(items)) => items
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_lowercase)
                .collect(),
            Some(Value::Object(map)) => map
                .values()
                .flat_map(|value| match value {
                    Value::Array(items) => items.iter().filter_map(Value::as_str).collect(),
                    Value::String(text) => vec![text.as_str()],
                    _ => Vec::new(),
                })
                .map(str::to_lowercase)
                .collect(),
            Some(Value::String(text)) => vec![text.to_lowercase()],
            _ => Vec::new(),
        }
    };
    match name {
        "q" => ["/title", "/description", "/slug"]
            .iter()
            .flat_map(|pointer| names(pointer))
            .any(|text| text.contains(value)),
        "type" => names("/types").iter().any(|t| t == value),
        "attribute" => names("/attributes").iter().any(|a| a == value),
        "space" => names("/space").iter().any(|s| s == value),
        "access" => names("/access").iter().any(|a| a == value),
        "domain" => names("/domains").iter().any(|d| d == value),
        "theme" => names("/themes").iter().any(|t| t == value),
        _ => false,
    }
}

/// One request's view of the hub, or of a named server: who may be reached, with what.
#[derive(Clone)]
pub(crate) struct Hub {
    pub(crate) gateway: Arc<Gateway>,
    /// The caller's Endpoints and who the caller is on each: in slug order on the hub, in the
    /// order of `spec.members` on a named server.
    pub(crate) reach: Vec<(Arc<Endpoint>, Subject)>,
    /// The key the Endpoint's own `(slug, caller)` bucket is spent under, the middleware's own.
    pub(crate) caller: String,
    /// The named server this request came in by, `None` on the hub (ADR-N-043).
    pub(crate) server: Option<McpServer>,
    /// No token on this request: an Endpoint outside `reach` is `401`, not unknown (T-4114).
    pub(crate) anonymous: bool,
    /// No token on this request, on the hub or a named server: only
    /// [`tools::ANONYMOUS_TOOLS`] are listed and called (T-4168).
    pub(crate) reads_only: bool,
}

pub(crate) enum Answer {
    Json(Value),
    Notification,
    /// An HTTP answer instead of a JSON-RPC one: a spent Endpoint bucket.
    Refused(Response<Body>),
}

impl Hub {
    pub(crate) async fn answer(&self, message: Value) -> Answer {
        let params = message.get("params").cloned().unwrap_or(json!({}));
        let Some(id) = message.get("id").cloned() else {
            return Answer::Notification;
        };
        let Some(method) = message
            .get("method")
            .and_then(Value::as_str)
            .filter(|_| message.get("jsonrpc").and_then(Value::as_str) == Some("2.0"))
        else {
            return Answer::Json(error(
                id,
                -32600,
                "invalid request: a JSON-RPC 2.0 request names \"jsonrpc\": \"2.0\" and a method",
            ));
        };
        Answer::Json(match method {
            "initialize" if self.server.is_some() => {
                result(id, server::initialized(self.server.as_ref()))
            }
            "initialize" => result(
                id,
                json!({
                    "protocolVersion": jsonrpc::PROTOCOL_VERSION,
                    "capabilities": { "tools": { "listChanged": false } },
                    "serverInfo": {
                        "name": "joinedcontext-hub",
                        "version": env!("CARGO_PKG_VERSION"),
                    },
                    "instructions": super::PRIMER,
                }),
            ),
            "ping" => result(id, json!({})),
            "tools/list" => {
                let mut tools = vec![list_endpoints_tool(), get_endpoint_tool()];
                tools.extend(tools::hub_tools(&self.gateway, &self.reach));
                if self.reads_only {
                    tools::keep_anonymous(&mut tools);
                }
                // A member's view answers its own cursor, opaque; a member no view serves yet
                // still answers an offset (T-3808).
                for cursor in tools
                    .iter_mut()
                    .filter_map(|tool| tool.pointer_mut("/inputSchema/properties/cursor"))
                {
                    *cursor = json!({
                        "type": ["string", "integer"],
                        "maxLength": 1024,
                        "minimum": 0,
                        "description": "The previous page's nextCursor, passed back as given",
                    });
                }
                if self.server.is_some() {
                    server::fan_out_schemas(&mut tools);
                }
                result(id, json!({ "tools": tools }))
            }
            "tools/call" => return self.call(id, params).await,
            _ => error(id, -32601, "method not found"),
        })
    }

    async fn call(&self, id: Value, params: Value) -> Answer {
        let name = params.get("name").and_then(Value::as_str).unwrap_or("");
        // Before any routing, so a write is refused whichever Endpoint it names (T-4168).
        if self.reads_only && !tools::anonymous_may(name) {
            return Answer::Json(tools::anonymous_refusal(id, name));
        }
        let mut arguments = match params.get("arguments") {
            None => Map::new(),
            Some(Value::Object(arguments)) => arguments.clone(),
            Some(_) => {
                return Answer::Json(error(id, -32602, "`arguments` is an object"));
            }
        };
        // What one anonymous call costs the broker, bounded before it is routed (T-4261).
        if self.reads_only {
            if let Err(why) = tools::anonymous_bounds(name, &mut arguments) {
                return Answer::Json(result(id, refused(&why, &Value::Null)));
            }
        }
        if name == "list_endpoints" {
            return Answer::Json(self.list_endpoints(id, &arguments));
        }
        if name == "get_endpoint" {
            return Answer::Json(self.get_endpoint(id, &arguments));
        }

        // The one routing argument, one string, never a list (ADR-N-025 2.5, 2.6).
        let slug = match arguments.remove("endpoint") {
            Some(Value::String(slug)) => slug,
            Some(_) => return Answer::Json(error(
                id,
                -32602,
                "`endpoint` names one Endpoint by its slug: a single string, never a list (EP-87)",
            )),
            // A named server's read visits every member it may (ADR-N-043 §2.4).
            None if self.server.is_some() && server::FAN_OUT.contains(&name) => {
                return self.fan_out(id, name, &params, arguments).await;
            }
            // Catalogue first: a read that names no Endpoint asks the catalogue (T-4501).
            None => catalog::NAME.to_owned(),
        };
        if let Some(refusal) = misrouted(&id, &arguments) {
            return Answer::Json(refusal);
        }
        // The caller's own catalogue, built from `reach` alone (T-4503).
        if catalog::names_catalog(&slug) {
            return Answer::Json(catalog::call(
                &self.gateway,
                &self.reach,
                id,
                name,
                &arguments,
            ));
        }

        // An Endpoint outside the caller's list and one that does not exist are the same answer,
        // byte for byte (SP-20): both are searched for in the caller's list only.
        let (endpoint, subject) = match resolve(&self.reach, &slug) {
            Ok(found) => found,
            // A slug outside the list is answered the same whatever it is, with no suggestion
            // that would vary with it (SP-20): without a token the login prompt, an internal one
            // and one that does not exist alike.
            Err(_) if EndpointSlug::new(&slug).is_ok() => {
                if self.anonymous {
                    return Answer::Refused(unauthorized(&self.gateway));
                }
                return Answer::Json(error(id, -32602, "unknown endpoint"));
            }
            Err(candidates) => {
                return Answer::Json(error(
                    id,
                    -32602,
                    &format!(
                        "no Endpoint of yours is named `{slug}`; closest: {}",
                        if candidates.is_empty() {
                            "none".to_owned()
                        } else {
                            candidates.join(", ")
                        }
                    ),
                ))
            }
        };
        match tools::hub_tool(&self.gateway, endpoint, subject, &self.reach, name) {
            HubTool::Unknown => return Answer::Json(error(id, -32602, "unknown tool")),
            HubTool::Ungranted(refusal) => return Answer::Json(result(id, refusal)),
            HubTool::Granted => {}
        }

        if let Err(decision) = self.spend(endpoint) {
            return Answer::Refused(rate_limit::too_many(
                &decision,
                "the endpoint's rate limit is spent; retry after the seconds the RateLimit-Reset header names",
            ));
        }
        let forwarded = self.forward(endpoint, subject, name, id.clone(), &params, arguments);
        if !self.reads_only {
            return Answer::Json(forwarded.await);
        }
        // An anonymous read is answered within the fan-out's member deadline, or as a tool
        // error: never a stream held open on the broker's time (T-4261).
        Answer::Json(
            match tokio::time::timeout(server::MEMBER_DEADLINE, forwarded).await {
                Ok(answer) => answer,
                Err(_) => {
                    tracing::warn!(slug = %endpoint.slug, tool = %name, "anonymous call timed out");
                    result(
                        id,
                        refused(
                            "the broker did not answer within ten seconds; narrow the window or the types and ask again",
                            &Value::Null,
                        ),
                    )
                }
            },
        )
    }

    /// Spends the Endpoint's own bucket, as a call to its URL spends it (EP-20).
    pub(crate) fn spend(&self, endpoint: &Endpoint) -> Result<(), rate_limit::Decision> {
        let Some(limits) = endpoint.rate_limit.as_ref() else {
            return Ok(());
        };
        let decision =
            self.gateway
                .rate_limiter
                .check(&endpoint.slug, &self.caller, limits, Instant::now());
        if decision.allowed {
            return Ok(());
        }
        tracing::info!(slug = %endpoint.slug, "rate limit reached through the hub");
        Err(decision)
    }

    /// Hands one tool call to the Endpoint's view at the broker, or answers a describe tool.
    pub(crate) async fn forward(
        &self,
        endpoint: &Arc<Endpoint>,
        subject: &Subject,
        name: &str,
        id: Value,
        params: &Value,
        arguments: Map<String, Value>,
    ) -> Value {
        // What a call to the Endpoint's URL logs, and which Endpoint and tool it was (ADR-N-025
        // section 3, ADR-N-043 §2.8). The Endpoint's own decision line follows from the re-entry.
        let server = self.server.as_ref().map(McpServer::path);
        tracing::info!(
            slug = %endpoint.slug,
            space = %endpoint.space,
            principal = %principal_of(subject),
            tool = %name,
            door = if server.is_some() { "server" } else { "hub" },
            server = server.as_deref().unwrap_or_default(),
            "hub call"
        );

        if describe::DESCRIBE_TOOLS.contains(&name) {
            let mut asked = params.as_object().cloned().unwrap_or_default();
            asked.insert("arguments".to_owned(), Value::Object(arguments));
            return describe::call(&self.gateway, endpoint, subject, id, &Value::Object(asked))
                .await;
        }
        let mut forwarded = params.as_object().cloned().unwrap_or_default();
        // The hub's tools are the broker's by name and arguments (EP-112): the call goes on as sent.
        forwarded.insert("name".to_owned(), json!(name));
        forwarded.insert("arguments".to_owned(), Value::Object(arguments));
        let message = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "tools/call",
            "params": forwarded,
        });
        crate::mcp::endpoint::member_call(&self.gateway, endpoint, subject, message).await
    }

    /// The caller's Endpoints, a page at a time (ADR-N-025 2.1).
    /// One line per Endpoint (T-4492): what an agent picks by, not the URLs, which
    /// `get_endpoint` answers for the one it picked.
    fn list_endpoints(&self, id: Value, arguments: &Map<String, Value>) -> Value {
        let mut arguments = arguments.clone();
        let lang = match arguments.remove("lang") {
            None => "en".to_owned(),
            Some(Value::String(lang)) if (2..=8).contains(&lang.len()) => lang.to_lowercase(),
            Some(_) => {
                return result(
                    id,
                    refused("`lang` is a language tag such as sk or en", &Value::Null),
                )
            }
        };
        let page = match page(&self.gateway, &self.reach, &arguments) {
            Ok(page) => page,
            Err(why) => return result(id, refused(&why, &Value::Null)),
        };
        let rows: Vec<Value> = page["endpoints"]
            .as_array()
            .map(|entries| entries.iter().map(|entry| compact(entry, &lang)).collect())
            .unwrap_or_default();
        let mut answer = answered(&json!(rows), "endpoints");
        answer["content"][0]["text"] = json!(lines(&rows, &page));
        if let Some(structured) = answer
            .get_mut("structuredContent")
            .and_then(Value::as_object_mut)
        {
            structured.insert("total".to_owned(), page["total"].clone());
            if let Some(next) = page.get("nextCursor") {
                structured.insert("nextCursor".to_owned(), next.clone());
            }
        }
        result(id, answer)
    }

    /// One Endpoint of the caller's list, whole: today's `list_endpoints` record with its
    /// surface, DCAT, schema and MCP URLs (T-4492).
    fn get_endpoint(&self, id: Value, arguments: &Map<String, Value>) -> Value {
        if let Some(unknown) = arguments.keys().find(|key| *key != "endpoint") {
            return result(
                id,
                refused(
                    &format!("`{unknown}` is not an argument of get_endpoint"),
                    &Value::Null,
                ),
            );
        }
        let Some(named) = arguments.get("endpoint").and_then(Value::as_str) else {
            return result(
                id,
                refused(
                    "`endpoint` names one Endpoint: its slug, title or space",
                    &Value::Null,
                ),
            );
        };
        match resolve(&self.reach, named) {
            Ok((endpoint, subject)) => {
                let mut entry = described(&self.gateway, endpoint, subject);
                entry["broker"] = json!(self.gateway.broker.broker_of_view(&endpoint.slug));
                result(id, answered(&entry, "endpoint"))
            }
            Err(near) => result(
                id,
                refused(
                    &format!(
                        "no Endpoint you may read is called `{named}`; closest: {}",
                        near.join(", ")
                    ),
                    &Value::Null,
                ),
            ),
        }
    }
}

/// One `list_endpoints` row (T-4492): the title in `lang`, else English, else any.
fn compact(entry: &Value, lang: &str) -> Value {
    let titles = entry["title"].as_object();
    let title = titles
        .and_then(|t| {
            t.get(lang)
                .or_else(|| t.get("en"))
                .or_else(|| t.values().next())
        })
        .cloned()
        .unwrap_or_else(|| entry["space"].clone());
    let mut row = json!({
        "slug": entry["slug"],
        "title": title,
        "space": entry["space"],
        "access": entry["access"],
        "types": entry["types"],
        "domains": entry["domains"],
        "themes": entry["themes"],
    });
    for key in ["place", "period", "broker"] {
        if let Some(value) = entry.get(key) {
            row[key] = value.clone();
        }
    }
    row
}

/// The rows as the text an agent reads: one line each, then the paging.
fn lines(rows: &[Value], page: &Value) -> String {
    let joined = |value: &Value| {
        value
            .as_array()
            .map(|items| {
                items
                    .iter()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
                    .join(",")
            })
            .unwrap_or_default()
    };
    let mut text = String::from(
        "slug | title | space | access | types | domain | theme | place (W,S,E,N) | period\n",
    );
    for row in rows {
        let place = row["place"]
            .as_array()
            .map(|corners| {
                corners
                    .iter()
                    .map(Value::to_string)
                    .collect::<Vec<_>>()
                    .join(",")
            })
            .unwrap_or_default();
        let period = match row.get("period") {
            Some(period) => format!(
                "{}..{}",
                period["start"].as_str().unwrap_or(""),
                period["end"].as_str().unwrap_or("")
            ),
            None => String::new(),
        };
        text.push_str(&format!(
            "{} | {} | {} | {} | {} | {} | {} | {place} | {period}\n",
            row["slug"].as_str().unwrap_or(""),
            row["title"].as_str().unwrap_or(""),
            row["space"].as_str().unwrap_or(""),
            row["access"].as_str().unwrap_or(""),
            joined(&row["types"]),
            joined(&row["domains"]),
            joined(&row["themes"]),
        ));
    }
    text.push_str(&format!("total {}", page["total"]));
    if let Some(next) = page.get("nextCursor").and_then(Value::as_str) {
        text.push_str(&format!("; nextCursor {next}"));
    }
    text
}

/// The Endpoint of the caller's list that `named` names (T-4491): its slug; else, compared
/// without case, its title in any language or its context space, when exactly one Endpoint has
/// it. Only `reach` is searched, so a name never finds an Endpoint the caller may not read
/// (SP-20). On a miss or a name several Endpoints share: up to three `slug (title)` lines,
/// the ones sharing it, else the closest by name.
fn resolve<'a>(
    reach: &'a [(Arc<Endpoint>, Subject)],
    named: &str,
) -> Result<(&'a Arc<Endpoint>, &'a Subject), Vec<String>> {
    if let Some((endpoint, subject)) = reach.iter().find(|(e, _)| e.slug == named) {
        return Ok((endpoint, subject));
    }
    let wanted = named.trim().to_lowercase();
    let names = |endpoint: &Endpoint| -> Vec<String> {
        endpoint
            .title
            .values()
            .chain(std::iter::once(&endpoint.space))
            .map(|name| name.trim().to_lowercase())
            .collect()
    };
    let line = |endpoint: &Endpoint| match endpoint.title.values().next() {
        Some(title) => format!("{} ({title})", endpoint.slug),
        None => format!("{} ({})", endpoint.slug, endpoint.space),
    };
    let sharing: Vec<&(Arc<Endpoint>, Subject)> = reach
        .iter()
        .filter(|(endpoint, _)| !wanted.is_empty() && names(endpoint).contains(&wanted))
        .collect();
    if let [(endpoint, subject)] = sharing.as_slice() {
        return Ok((endpoint, subject));
    }
    if !sharing.is_empty() {
        return Err(sharing.iter().take(3).map(|(e, _)| line(e)).collect());
    }
    let mut ranked: Vec<(f64, &Endpoint)> = reach
        .iter()
        .map(|(endpoint, _)| {
            let score = names(endpoint)
                .iter()
                .chain(std::iter::once(&endpoint.slug))
                .map(|name| strsim::normalized_levenshtein(&wanted, name))
                .fold(0.0, f64::max);
            (score, endpoint.as_ref())
        })
        .collect();
    ranked.sort_by(|a, b| b.0.total_cmp(&a.0).then_with(|| a.1.slug.cmp(&b.1.slug)));
    Err(ranked.iter().take(3).map(|(_, e)| line(e)).collect())
}

/// The refusal of an argument that would choose a space some other way than `endpoint`
/// (AG-05, SP-14).
pub(crate) fn misrouted(id: &Value, arguments: &Map<String, Value>) -> Option<Value> {
    let selector = arguments
        .keys()
        .find(|key| SELECTORS.contains(&key.to_ascii_lowercase().as_str()))?;
    Some(error(
        id.clone(),
        -32602,
        &format!(
            "`{selector}` is not an argument: the hub is routed by `endpoint` alone (AG-05, SP-14)"
        ),
    ))
}

/// One Endpoint as `list_endpoints` names it: only what this caller may see of it, enough for
/// an agent to pick it from one call (EP-105, EP-111).
fn described(gateway: &Gateway, endpoint: &Endpoint, subject: &Subject) -> Value {
    let base = gateway.base_url();
    let visible = schema::visible(subject, endpoint, crate::pdp::now());
    let types = schema::visible_types(endpoint, &visible);
    let representations: Vec<&str> = endpoint
        .representations
        .iter()
        .map(Representation::as_str)
        .collect();
    // Each representation's own URL (EP-05, EP-105), the paths the DCAT record lists.
    let surfaces: Map<String, Value> = endpoint
        .representations
        .iter()
        .map(|representation| {
            let path = endpoint_surface::path_of(*representation);
            (
                representation.as_str().to_owned(),
                json!(format!("{base}{}/{path}", endpoint.base_path)),
            )
        })
        .collect();
    let newest = endpoint.models.iter().map(|model| model.major).max();
    let mut entry = json!({
        "slug": endpoint.slug,
        "title": endpoint.title,
        "description": endpoint.description,
        "space": endpoint.space,
        "access": if endpoint.audience == Audience::Public { "public" } else { "internal" },
        "types": types,
        "attributes": attributes_of(endpoint, newest, &visible, &types),
        "temporal": tools::granted(
            gateway,
            endpoint,
            subject,
            &[Operation::QueryTemporal, Operation::RetrieveTemporal],
        ),
        "representations": representations,
        "surfaces": surfaces,
        "mcp": format!("{base}{}/mcp", endpoint.base_path),
        // The Endpoint's root answers its DCAT-AP record (EP-27).
        "dcat": format!("{base}{}/", endpoint.base_path),
    });
    // What the dataset is about (T-4498), where and when, for the one-line list (T-4492).
    let catalog = endpoint_surface::classified(endpoint);
    entry["domains"] = json!(endpoint_surface::classification(endpoint).domains);
    entry["themes"] = json!(catalog
        .themes
        .iter()
        .map(|theme| theme.code())
        .collect::<Vec<_>>());
    if let Some(place) = super::catalog::bbox(endpoint) {
        entry["place"] = json!(place);
    }
    if let Some(period) = catalog
        .temporal
        .filter(|t| t.start.is_some() || t.end.is_some())
    {
        entry["period"] = json!(period);
    }
    // The recommended formalism of the newest major, the one describe_schema recommends.
    if let Some(major) = newest {
        entry["schema"] = json!(format!(
            "{base}{}/schema/v{major}/{}",
            endpoint.base_path,
            crate::middleware::response::Artifact::LinkMl.file_name()
        ));
    }
    entry
}

/// Per type the caller may read, the attribute names the newest model major describes to
/// this caller: the same narrowing `describe_schema` applies (EP-47, MP-02).
fn attributes_of(
    endpoint: &Endpoint,
    newest: Option<u32>,
    visible: &schema::Visible,
    types: &[String],
) -> Map<String, Value> {
    let Some(newest) = newest else {
        return Map::new();
    };
    let models: Vec<&crate::resolver::Model> = endpoint
        .models
        .iter()
        .filter(|model| model.major == newest)
        .collect();
    let document = schema::json_schema(&models, visible, &mut Vec::new());
    types
        .iter()
        .map(|entity_type| {
            let mut names: Vec<&str> = document
                .pointer(&format!("/$defs/{entity_type}/properties"))
                .and_then(Value::as_object)
                .map(|properties| {
                    properties
                        .keys()
                        .map(String::as_str)
                        .filter(|name| !matches!(*name, "id" | "type" | "@context"))
                        .collect()
                })
                .unwrap_or_default();
            names.sort_unstable();
            (entity_type.clone(), json!(names))
        })
        .collect()
}

/// The tool that answers the caller's list (ADR-N-025 2.1).
pub(crate) fn list_endpoints_tool() -> Value {
    json!({
        "name": "list_endpoints",
        "description": "The Endpoints this connector reaches: open ones without a login, and \
                        the ones you may read once signed in. One line each: slug, title, \
                        context space, public or internal, entity types, Smart Data Model \
                        domain, EU theme, area and period. Filter with q, type, attribute, \
                        space, access, domain and theme; name one in the `endpoint` argument \
                        of every other tool; get_endpoint gives its URLs, attributes and data \
                        model.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "q": { "type": "string", "minLength": 1, "maxLength": 200, "description": "Words in the title or description, any language" },
                "type": { "type": "string", "minLength": 1, "maxLength": 200, "description": "An entity type you may read there" },
                "attribute": { "type": "string", "minLength": 1, "maxLength": 200, "description": "An attribute name of such a type" },
                "space": { "type": "string", "minLength": 1, "maxLength": 200, "description": "The context space" },
                "access": { "type": "string", "enum": ["public", "internal"] },
                "domain": { "type": "string", "minLength": 1, "maxLength": 200, "description": "A Smart Data Model domain, e.g. Environment" },
                "theme": { "type": "string", "minLength": 1, "maxLength": 200, "description": "An EU data theme code, e.g. ENVI" },
                "lang": { "type": "string", "minLength": 2, "maxLength": 8, "description": "The language of the titles, e.g. sk; English otherwise" },
                "limit": { "type": "integer", "minimum": 1, "maximum": 100 },
                "cursor": { "type": "string", "maxLength": 20, "description": "The previous page's nextCursor, passed back as given" },
            },
            "additionalProperties": false,
        },
        "outputSchema": {
            "type": "object",
            "properties": {
                "endpoints": { "type": "array" },
                "total": { "type": "integer" },
                "nextCursor": { "type": "string" },
            },
            "required": ["endpoints", "total"],
        },
        "annotations": { "readOnlyHint": true, "destructiveHint": false },
    })
}

/// The tool that answers one Endpoint whole (T-4492).
pub(crate) fn get_endpoint_tool() -> Value {
    json!({
        "name": "get_endpoint",
        "description": "One Endpoint of list_endpoints, whole: per type the attributes you may \
                        read, whether history reads answer, each surface's URL, its DCAT record, \
                        its data model and its MCP URL.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "endpoint": { "type": "string", "minLength": 1, "maxLength": 200, "description": "Its slug, title or space" },
            },
            "required": ["endpoint"],
            "additionalProperties": false,
        },
        "annotations": { "readOnlyHint": true, "destructiveHint": false },
    })
}

/// The `401` that names the hub's resource metadata (RFC 9728, AG-32).
fn unauthorized(gateway: &Gateway) -> Response<Body> {
    let mut response = ProblemDetails::new(401, "unauthorized", "Unauthorized")
        .with_detail("the hub needs an access token; its authorization server is named by the resource metadata")
        .into_response();
    let metadata = format!(
        "{}{HUB_PATH}/.well-known/oauth-protected-resource",
        gateway.base_url()
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

/// The hub's protected-resource metadata (RFC 9728, ADR-N-025 section 4).
pub async fn protected_resource(State(gateway): State<Arc<Gateway>>) -> Response<Body> {
    let Some(issuer) = gateway.verifier.as_ref().map(|verifier| verifier.issuer()) else {
        return ProblemDetails::not_found().into_response();
    };
    jsonrpc::json_response(
        StatusCode::OK,
        &json!({
            "resource": format!("{}{HUB_PATH}", gateway.base_url()),
            "authorization_servers": [issuer],
            "bearer_methods_supported": ["header"],
            // The one-login scope (ADR-N-025 4b); `endpoint:{slug}` stays a narrower choice.
            "scopes_supported": ["endpoint:*"],
        }),
    )
}
