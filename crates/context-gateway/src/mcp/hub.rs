//! The MCP hub: one connector over every Endpoint the caller's token may read (T-2490, ADR-N-025,
//! EP-87, EP-88).
//!
//! The hub holds no data path of its own. It answers who the caller may reach, and every tool
//! call it takes goes to the named Endpoint's view on the broker, `/x/views/{slug}/mcp`, in the
//! broker's form and under the verified caller, as a call to `/api/endpoint/{slug}/mcp` does
//! (T-3808); `describe_schema`, `describe_access` and an Endpoint no view serves yet stay on the
//! Endpoint's own façade (SP-16). Nothing is remembered between calls (SP-19):
//! the agent names the Endpoint on every call, and the list is rebuilt from the token each time.

use crate::app::{self, principal_of, subject_from, Credential, Door, Gateway, HUB_PATH, MAX_BODY};
use crate::auth::token;
use crate::handlers::{endpoint_surface, schema};
use crate::mcp::endpoint_facade::{self, answered, error, refused, result, HubTool};
use crate::mcp::server::{self, McpServer};
use crate::middleware::{rate_limit, tenancy};
use crate::pdp::evaluator::Subject;
use crate::resolver::Endpoint;
use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::header::AUTHORIZATION;
use axum::http::{HeaderMap, HeaderValue, Response, StatusCode};
use axum::response::IntoResponse;
use jc_core::kinds::Representation;
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

/// A hub tool call in the form the broker's MCP surface takes it (Antares `docs/src/mcp.md`
/// section 2): the hub keeps its catalogue's names and arguments, and the broker names a few
/// of them otherwise. A `type` is a list of names, `details` is a tool of its own, and the one
/// entity or subscription a write carries is the broker's argument list. A cursor is the
/// broker's, passed back as it gave it.
fn broker_call(name: &str, mut arguments: Map<String, Value>) -> (&str, Map<String, Value>) {
    let details = arguments.remove("details") == Some(Value::Bool(true));
    match name {
        "list_types" if details => ("list_type_details", arguments),
        "list_attributes" if details => ("list_attribute_details", arguments),
        "upsert_entity" => {
            let entity = arguments.remove("entity").unwrap_or(Value::Null);
            arguments.insert("entities".to_owned(), json!([entity]));
            ("batch_upsert", arguments)
        }
        "create_subscription" => match arguments.remove("subscription") {
            Some(Value::Object(subscription)) => (name, subscription),
            _ => (name, arguments),
        },
        "get_type" => (name, arguments),
        _ => {
            if let Some(one) = arguments.get_mut("type").filter(|one| one.is_string()) {
                *one = json!([one.take()]);
            }
            if let Some(ids) = arguments.remove("ids") {
                arguments.entry("id").or_insert(ids);
            }
            (name, arguments)
        }
    }
}

/// One JSON-RPC message to the hub (EP-87).
pub async fn message(State(gateway): State<Arc<Gateway>>, mut request: Request) -> Response<Body> {
    tenancy::strip_client_headers(&mut request);

    let Reached {
        reach, decision, ..
    } = match reached(&gateway, request.headers(), Some(Representation::Mcp)) {
        Ok(reached) => reached,
        Err(refusal) => return *refusal,
    };
    let authorization = request.headers().get(AUTHORIZATION).cloned();

    let caller = rate_limit::caller_key(request.headers());
    let (_, body) = request.into_parts();
    let Ok(bytes) = axum::body::to_bytes(body, MAX_BODY).await else {
        return endpoint_facade::parse_error();
    };
    let Ok(message) = serde_json::from_slice::<Value>(&bytes) else {
        return endpoint_facade::parse_error();
    };

    let hub = Hub {
        gateway,
        reach,
        authorization,
        caller,
        server: None,
    };
    let mut response = match hub.answer(message).await {
        Answer::Json(answer) => endpoint_facade::json_response(StatusCode::OK, &answer),
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
    // The hub is its own protected resource: a client without a token is told where to get one
    // for it (AG-32, RFC 9728). It lists nothing anonymously; a public Endpoint's own URL still
    // answers without a token.
    let presented = headers
        .get(AUTHORIZATION)
        .and_then(|value| value.to_str().ok());
    let raw = match token::bearer(presented) {
        Ok(raw) => raw,
        Err(token::Rejected::NoToken) => return Err(Box::new(unauthorized(gateway))),
        Err(rejected) => return Err(Box::new(ProblemDetails::from(rejected).into_response())),
    };
    let Some(verifier) = gateway.verifier.as_ref() else {
        tracing::warn!("a token was presented to the hub but no realm is configured");
        return Err(Box::new(ProblemDetails::unauthorized().into_response()));
    };

    // Which Endpoints the token then reaches is decided per Endpoint by `reaches`.
    let served: Vec<Arc<Endpoint>> = gateway
        .resolver
        .endpoints()
        .into_iter()
        .filter(|endpoint| serving.is_none_or(|wanted| endpoint.serves(wanted)))
        .collect();
    let mut audiences = gateway.hub_audiences();
    for endpoint in &served {
        audiences.extend(gateway.audiences_for(endpoint));
    }
    audiences.sort_unstable();
    audiences.dedup();
    let claims = verifier
        .verify(raw, &audiences)
        .map_err(|rejected| Box::new(ProblemDetails::from(rejected).into_response()))?;

    // One bucket per subject across every Endpoint, spent on every hub request, so fifty
    // Endpoints are not fifty bursts (ADR-N-025 section 3).
    let subject_key = format!("subject:{}", claims.sub);
    let decision = gateway.rate_limiter.check(
        HUB_BUCKET,
        &subject_key,
        &gateway.hub_rate_limit,
        Instant::now(),
    );
    if !decision.allowed {
        tracing::info!(principal = %claims.sub, "hub rate limit reached");
        return Err(Box::new(rate_limit::too_many(
            &decision,
            "the hub's per-subject rate limit is spent; retry after the seconds the RateLimit-Reset header names",
        )));
    }

    let mut reach: Vec<(Arc<Endpoint>, Subject)> = served
        .into_iter()
        .filter(|endpoint| gateway.reaches(&claims, endpoint))
        .filter_map(|endpoint| {
            let subject = subject_from(gateway, &endpoint, &claims).ok()?;
            endpoint_facade::reads_anything(gateway, &endpoint, &subject)
                .then_some((endpoint, subject))
        })
        .collect();
    reach.sort_by(|(a, _), (b, _)| a.slug.cmp(&b.slug));
    Ok(Reached {
        principal: claims.sub,
        reach,
        decision,
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
    } = match reached(&gateway, request.headers(), None) {
        Ok(reached) => reached,
        Err(refusal) => return *refusal,
    };
    // The query as `list_endpoints` takes its arguments: a number is a number, anything else
    // stays a string and is refused by the same words.
    let arguments: Map<String, Value> =
        crate::query::parse(request.uri().query().unwrap_or_default())
            .into_iter()
            .map(|(name, value)| {
                let value = value
                    .parse::<u64>()
                    .map_or_else(|_| json!(value), |number| json!(number));
                (name, value)
            })
            .collect();
    let mut response = match page(gateway.base_url(), &reach, &arguments) {
        Ok(page) => {
            tracing::info!(principal = %principal, total = reach.len(), "endpoint list");
            endpoint_facade::json_response(StatusCode::OK, &page)
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
    base: &str,
    reach: &[(Arc<Endpoint>, Subject)],
    arguments: &Map<String, Value>,
) -> Result<Value, String> {
    if let Some(unknown) = arguments
        .keys()
        .find(|key| !matches!(key.as_str(), "limit" | "cursor"))
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
    let offset = match arguments.get("cursor") {
        None => 0,
        Some(value) => value
            .as_u64()
            .ok_or("`cursor` is the previous page's nextCursor, a whole number")?
            as usize,
    };
    let endpoints: Vec<Value> = reach
        .iter()
        .skip(offset)
        .take(limit)
        .map(|(endpoint, subject)| described(base, endpoint, subject))
        .collect();
    let mut page = json!({ "endpoints": endpoints, "total": reach.len() });
    let next = offset.saturating_add(limit);
    if next < reach.len() {
        page["nextCursor"] = json!(next);
    }
    Ok(page)
}

/// One request's view of the hub, or of a named server: who may be reached, with what.
#[derive(Clone)]
pub(crate) struct Hub {
    pub(crate) gateway: Arc<Gateway>,
    /// The caller's Endpoints and who the caller is on each: in slug order on the hub, in the
    /// order of `spec.members` on a named server.
    pub(crate) reach: Vec<(Arc<Endpoint>, Subject)>,
    /// The caller's own header, handed to the Endpoint's façade and nothing else (EP-26).
    pub(crate) authorization: Option<HeaderValue>,
    /// The key the Endpoint's own `(slug, caller)` bucket is spent under, the middleware's own.
    pub(crate) caller: String,
    /// The named server this request came in by, `None` on the hub (ADR-N-043).
    pub(crate) server: Option<McpServer>,
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
                    "protocolVersion": endpoint_facade::PROTOCOL_VERSION,
                    "capabilities": { "tools": { "listChanged": false } },
                    "serverInfo": {
                        "name": "joinedcontext-hub",
                        "version": env!("CARGO_PKG_VERSION"),
                    },
                    "instructions": "One connector over several Endpoints. Call list_endpoints \
                                     to see which ones this token reaches, then name one in the \
                                     `endpoint` argument of every other tool; each call reads or \
                                     writes that Endpoint alone, under its own Policy. To compare \
                                     two Endpoints, make two calls.",
                }),
            ),
            "ping" => result(id, json!({})),
            "tools/list" => {
                let mut tools = vec![list_endpoints_tool()];
                tools.extend(endpoint_facade::hub_tools(&self.gateway, &self.reach));
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
        let mut arguments = match params.get("arguments") {
            None => Map::new(),
            Some(Value::Object(arguments)) => arguments.clone(),
            Some(_) => {
                return Answer::Json(error(id, -32602, "`arguments` is an object"));
            }
        };
        if name == "list_endpoints" {
            return Answer::Json(self.list_endpoints(id, &arguments));
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
            None => {
                return Answer::Json(error(
                    id,
                    -32602,
                    "`endpoint` is required: name one of the slugs list_endpoints answers (EP-87)",
                ))
            }
        };
        if let Some(refusal) = misrouted(&id, &arguments) {
            return Answer::Json(refusal);
        }

        // An Endpoint outside the caller's list and one that does not exist are the same answer,
        // byte for byte (SP-20).
        let Some((endpoint, subject)) = self
            .reach
            .iter()
            .find(|(endpoint, _)| endpoint.slug == slug)
        else {
            return Answer::Json(error(id, -32602, "unknown endpoint"));
        };
        match endpoint_facade::hub_tool(&self.gateway, endpoint, subject, &self.reach, name) {
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
        match self
            .forward(endpoint, subject, name, id, &params, arguments)
            .await
        {
            Some(answer) => Answer::Json(answer),
            None => Answer::Notification,
        }
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

    /// Hands one tool call to the Endpoint's own façade, which admits the caller again from
    /// their token (EP-26). `None` is a notification.
    pub(crate) async fn forward(
        &self,
        endpoint: &Arc<Endpoint>,
        subject: &Subject,
        name: &str,
        id: Value,
        params: &Value,
        arguments: Map<String, Value>,
    ) -> Option<Value> {
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

        let mut forwarded = params.as_object().cloned().unwrap_or_default();
        if app::member_viewed(&self.gateway, endpoint, name) {
            let (tool, arguments) = broker_call(name, arguments);
            forwarded.insert("name".to_owned(), json!(tool));
            forwarded.insert("arguments".to_owned(), Value::Object(arguments));
            let message = json!({
                "jsonrpc": "2.0",
                "id": id,
                "method": "tools/call",
                "params": forwarded,
            });
            return Some(app::member_call(&self.gateway, endpoint, subject, message).await);
        }
        forwarded.insert("arguments".to_owned(), Value::Object(arguments));
        let message = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "tools/call",
            "params": forwarded,
        });
        let credential = Credential {
            authorization: self.authorization.clone(),
            door: if self.server.is_some() {
                Door::Server
            } else {
                Door::Hub
            },
        };
        endpoint_facade::handle(
            Arc::clone(&self.gateway),
            Arc::clone(endpoint),
            subject.clone(),
            credential,
            message,
        )
        .await
    }

    /// The caller's Endpoints, a page at a time (ADR-N-025 2.1).
    fn list_endpoints(&self, id: Value, arguments: &Map<String, Value>) -> Value {
        let page = match page(self.gateway.base_url(), &self.reach, arguments) {
            Ok(page) => page,
            Err(why) => return result(id, refused(&why, &Value::Null)),
        };
        let mut answer = answered(&page["endpoints"], "endpoints", false, &[]);
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

/// One Endpoint as `list_endpoints` names it: only what this caller may see of it.
fn described(base: &str, endpoint: &Endpoint, subject: &Subject) -> Value {
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
    let mut entry = json!({
        "slug": endpoint.slug,
        "title": endpoint.title,
        "space": endpoint.space,
        "types": types,
        "representations": representations,
        "surfaces": surfaces,
        "mcp": format!("{base}{}/mcp", endpoint.base_path),
    });
    // The recommended formalism of the newest major, the one describe_schema recommends.
    if let Some(major) = endpoint.models.iter().map(|model| model.major).max() {
        entry["schema"] = json!(format!(
            "{base}{}/schema/v{major}/{}",
            endpoint.base_path,
            schema::Artifact::LinkMl.file_name()
        ));
    }
    entry
}

/// The tool that answers the caller's list (ADR-N-025 2.1).
fn list_endpoints_tool() -> Value {
    json!({
        "name": "list_endpoints",
        "description": "The Endpoints this connector reaches, with their context space, the \
                        entity types you may read and the link to their data model. Name one in \
                        the `endpoint` argument of every other tool.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "limit": { "type": "integer", "minimum": 1, "maximum": 100 },
                "cursor": { "type": "integer", "minimum": 0, "description": "The previous page's nextCursor" },
            },
            "additionalProperties": false,
        },
        "outputSchema": {
            "type": "object",
            "properties": {
                "endpoints": { "type": "array" },
                "total": { "type": "integer" },
                "nextCursor": { "type": "integer" },
            },
            "required": ["endpoints", "total"],
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
    endpoint_facade::json_response(
        StatusCode::OK,
        &json!({
            "resource": format!("{}{HUB_PATH}", gateway.base_url()),
            "authorization_servers": [issuer],
            "bearer_methods_supported": ["header"],
            "scopes_supported": ["endpoint:{slug}"],
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::broker_call;
    use serde_json::{json, Map, Value};

    fn called(name: &str, arguments: Value) -> (String, Value) {
        let arguments: Map<String, Value> =
            serde_json::from_value(arguments).unwrap_or_else(|e| unreachable!("{e}"));
        let (tool, arguments) = broker_call(name, arguments);
        (tool.to_owned(), Value::Object(arguments))
    }

    #[test]
    fn each_hub_tool_reaches_the_broker_in_its_form() {
        let entity = json!({ "id": "urn:ngsi-ld:Car:1", "type": "Car" });
        assert_eq!(
            called("upsert_entity", json!({ "entity": entity })),
            ("batch_upsert".to_owned(), json!({ "entities": [entity] }))
        );
        assert_eq!(
            called("get_type", json!({ "type": "Car" })),
            ("get_type".to_owned(), json!({ "type": "Car" }))
        );
        assert_eq!(
            called("list_attributes", json!({ "details": true })),
            ("list_attribute_details".to_owned(), json!({}))
        );
        assert_eq!(
            called("list_attributes", json!({})),
            ("list_attributes".to_owned(), json!({}))
        );
        // A subscription that is not an object is left for the broker to refuse by name.
        assert_eq!(
            called("create_subscription", json!({ "subscription": "x" })),
            ("create_subscription".to_owned(), json!({}))
        );
        // `id` beside `ids` keeps the caller's `id`.
        assert_eq!(
            called("batch_query", json!({ "ids": ["a"], "id": ["b"] })),
            ("batch_query".to_owned(), json!({ "id": ["b"] }))
        );
        assert_eq!(
            called("query_temporal", json!({ "type": 7 })),
            ("query_temporal".to_owned(), json!({ "type": 7 }))
        );
    }
}
