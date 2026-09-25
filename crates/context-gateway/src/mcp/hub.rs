//! The MCP hub: one connector over every Endpoint the caller's token may read (T-2490, ADR-N-025,
//! EP-87, EP-88).
//!
//! The hub holds no data path of its own. It answers who the caller may reach, and every tool
//! call it takes is handed to the named Endpoint's own façade, which re-enters the NGSI-LD path
//! with the caller's token exactly as a call to `/api/endpoint/{slug}/mcp` does: the Endpoint's
//! PDP, projection and elicitation decide (SP-16). Nothing is remembered between calls (SP-19):
//! the agent names the Endpoint on every call, and the list is rebuilt from the token each time.

use crate::app::{principal_of, subject_from, Credential, Door, Gateway, HUB_PATH, MAX_BODY};
use crate::auth::token;
use crate::handlers::schema;
use crate::mcp::endpoint_facade::{self, answered, error, refused, result, HubTool};
use crate::middleware::{rate_limit, tenancy};
use crate::pdp::evaluator::Subject;
use crate::resolver::Endpoint;
use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::header::AUTHORIZATION;
use axum::http::{HeaderValue, Response, StatusCode};
use axum::response::IntoResponse;
use jc_core::kinds::Representation;
use jc_core::ProblemDetails;
use serde_json::{json, Map, Value};
use std::sync::Arc;
use std::time::Instant;

/// Arguments that would choose a space some other way than `endpoint` (AG-05, SP-14).
const SELECTORS: &[&str] = &["space", "tenant", "contextspace", "slug"];

/// How many Endpoints one `list_endpoints` page answers unless the caller asks for fewer.
const PAGE: usize = 50;

/// The key the per-subject bucket is kept under. It holds a `/`, so no Endpoint slug is it.
const HUB_BUCKET: &str = HUB_PATH;

/// One JSON-RPC message to the hub (EP-87).
pub async fn message(State(gateway): State<Arc<Gateway>>, mut request: Request) -> Response<Body> {
    tenancy::strip_client_headers(&mut request);

    // The hub is its own protected resource: a client without a token is told where to get one
    // for it (AG-32, RFC 9728). It lists nothing anonymously; a public Endpoint's own URL still
    // answers without a token.
    let authorization = request.headers().get(AUTHORIZATION).cloned();
    let presented = authorization.as_ref().and_then(|value| value.to_str().ok());
    let raw = match token::bearer(presented) {
        Ok(raw) => raw.to_owned(),
        Err(token::Rejected::NoToken) => return unauthorized(&gateway),
        Err(rejected) => return ProblemDetails::from(rejected).into_response(),
    };
    let Some(verifier) = gateway.verifier.as_ref() else {
        tracing::warn!("a token was presented to the hub but no realm is configured");
        return ProblemDetails::unauthorized().into_response();
    };

    // Every audience the hub could honour: its own, and each MCP Endpoint's (its slug, its URL,
    // the edge). Which Endpoints the token then reaches is decided per Endpoint by `reaches`.
    let served: Vec<Arc<Endpoint>> = gateway
        .resolver
        .endpoints()
        .into_iter()
        .filter(|endpoint| endpoint.serves(Representation::Mcp))
        .collect();
    let mut audiences = gateway.hub_audiences();
    for endpoint in &served {
        audiences.extend(gateway.audiences_for(endpoint));
    }
    audiences.sort_unstable();
    audiences.dedup();
    let claims = match verifier.verify(&raw, &audiences) {
        Ok(claims) => claims,
        Err(rejected) => return ProblemDetails::from(rejected).into_response(),
    };

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
        return rate_limit::too_many(
            &decision,
            "the hub's per-subject rate limit is spent; retry after the seconds the RateLimit-Reset header names",
        );
    }

    // The caller's list: every MCP Endpoint the token reaches, admits and grants a read on.
    let mut reach: Vec<(Arc<Endpoint>, Subject)> = served
        .into_iter()
        .filter(|endpoint| gateway.reaches(&claims, endpoint))
        .filter_map(|endpoint| {
            let subject = subject_from(&gateway, &endpoint, &claims).ok()?;
            endpoint_facade::reads_anything(&gateway, &endpoint, &subject)
                .then_some((endpoint, subject))
        })
        .collect();
    reach.sort_by(|(a, _), (b, _)| a.slug.cmp(&b.slug));

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
    };
    let mut response = match hub.answer(message).await {
        Answer::Json(answer) => endpoint_facade::json_response(StatusCode::OK, &answer),
        Answer::Notification => StatusCode::ACCEPTED.into_response(),
        Answer::Refused(response) => response,
    };
    rate_limit::set_headers(response.headers_mut(), &decision);
    response
}

/// One request's view of the hub: who may be reached, with what.
struct Hub {
    gateway: Arc<Gateway>,
    /// The caller's Endpoints and who the caller is on each, in slug order.
    reach: Vec<(Arc<Endpoint>, Subject)>,
    /// The caller's own header, handed to the Endpoint's façade and nothing else (EP-26).
    authorization: Option<HeaderValue>,
    /// The key the Endpoint's own `(slug, caller)` bucket is spent under, the middleware's own.
    caller: String,
}

enum Answer {
    Json(Value),
    Notification,
    /// An HTTP answer instead of a JSON-RPC one: a spent Endpoint bucket.
    Refused(Response<Body>),
}

impl Hub {
    async fn answer(&self, message: Value) -> Answer {
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
            None => {
                return Answer::Json(error(
                    id,
                    -32602,
                    "`endpoint` is required: name one of the slugs list_endpoints answers (EP-87)",
                ))
            }
        };
        if let Some(selector) = arguments
            .keys()
            .find(|key| SELECTORS.contains(&key.to_ascii_lowercase().as_str()))
        {
            return Answer::Json(error(
                id,
                -32602,
                &format!("`{selector}` is not an argument: the hub is routed by `endpoint` alone (AG-05, SP-14)"),
            ));
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

        // The Endpoint's own bucket, as a call to its URL spends it (EP-20).
        if let Some(limits) = endpoint.rate_limit.as_ref() {
            let decision = self.gateway.rate_limiter.check(
                &endpoint.slug,
                &self.caller,
                limits,
                Instant::now(),
            );
            if !decision.allowed {
                tracing::info!(slug = %endpoint.slug, "rate limit reached through the hub");
                return Answer::Refused(rate_limit::too_many(
                    &decision,
                    "the endpoint's rate limit is spent; retry after the seconds the RateLimit-Reset header names",
                ));
            }
        }

        // What a call to the Endpoint's URL logs, and which Endpoint and tool it was (ADR-N-025
        // section 3). The Endpoint's own decision line follows from the re-entry.
        tracing::info!(
            slug = %endpoint.slug,
            space = %endpoint.space,
            principal = %principal_of(subject),
            tool = %name,
            door = "hub",
            "hub call"
        );

        let mut forwarded = params.as_object().cloned().unwrap_or_default();
        forwarded.insert("arguments".to_owned(), Value::Object(arguments));
        let message = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "tools/call",
            "params": forwarded,
        });
        let credential = Credential {
            authorization: self.authorization.clone(),
            door: Door::Hub,
        };
        match endpoint_facade::handle(
            Arc::clone(&self.gateway),
            Arc::clone(endpoint),
            subject.clone(),
            credential,
            message,
        )
        .await
        {
            Some(answer) => Answer::Json(answer),
            None => Answer::Notification,
        }
    }

    /// The caller's Endpoints, a page at a time (ADR-N-025 2.1).
    fn list_endpoints(&self, id: Value, arguments: &Map<String, Value>) -> Value {
        if let Some(unknown) = arguments
            .keys()
            .find(|key| !matches!(key.as_str(), "limit" | "cursor"))
        {
            return result(
                id,
                refused(
                    &format!("`{unknown}` is not an argument of list_endpoints"),
                    &Value::Null,
                ),
            );
        }
        let limit = match arguments.get("limit") {
            None => PAGE,
            Some(value) => match value.as_u64().filter(|limit| (1..=100).contains(limit)) {
                Some(limit) => limit as usize,
                None => {
                    return result(
                        id,
                        refused("`limit` is a whole number from 1 to 100", &Value::Null),
                    )
                }
            },
        };
        let offset = match arguments.get("cursor") {
            None => 0,
            Some(value) => match value.as_u64() {
                Some(offset) => offset as usize,
                None => {
                    return result(
                        id,
                        refused(
                            "`cursor` is the previous page's nextCursor, a whole number",
                            &Value::Null,
                        ),
                    )
                }
            },
        };

        let base = self.gateway.base_url();
        let page: Vec<Value> = self
            .reach
            .iter()
            .skip(offset)
            .take(limit)
            .map(|(endpoint, subject)| described(base, endpoint, subject))
            .collect();
        let mut answer = answered(&json!(page), "endpoints", false, &[]);
        let next = offset.saturating_add(limit);
        if let Some(structured) = answer
            .get_mut("structuredContent")
            .and_then(Value::as_object_mut)
        {
            structured.insert("total".to_owned(), json!(self.reach.len()));
            if next < self.reach.len() {
                structured.insert("nextCursor".to_owned(), json!(next));
            }
        }
        result(id, answer)
    }
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
    let mut entry = json!({
        "slug": endpoint.slug,
        "title": endpoint.title,
        "space": endpoint.space,
        "types": types,
        "representations": representations,
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
