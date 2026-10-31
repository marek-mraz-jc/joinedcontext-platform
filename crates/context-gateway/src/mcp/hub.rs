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
use crate::auth::accounts::ServiceAccounts;
use crate::auth::subject::Subject;
use crate::auth::subject::{anonymous_on, principal_of, subject_from, HUB_PATH};
use crate::auth::token::{self, Claims};
use crate::handlers::{endpoint_surface, schema};
use crate::mcp::jsonrpc::{self, answered, error, refused, result};
use crate::mcp::server::{self, McpServer};
use crate::mcp::tools::{self, HubTool};
use crate::mcp::{catalog, describe};
use crate::middleware::{rate_limit, tenancy};
use crate::resolver::Endpoint;
use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::header::AUTHORIZATION;
use axum::http::{HeaderMap, HeaderValue, Response, StatusCode};
use axum::response::IntoResponse;
use jc_core::display_order::SUB_PROPERTIES;
use jc_core::kinds::{Audience, EndpointSlug, Operation, Representation};
use jc_core::ProblemDetails;
use serde_json::{json, Map, Value};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Arguments that would choose a space some other way than `endpoint` (AG-05, SP-14).
pub(crate) const SELECTORS: &[&str] = &["space", "tenant", "contextspace", "slug"];

/// How many Endpoints one `list_endpoints` page answers unless the caller asks for fewer.
const PAGE: usize = 50;

/// The rows an MCP `list_endpoints` call without `limit` answers (T-6061): 50 public rows were
/// 24,396 bytes on dev, the largest answer a public client read after a schema.
const LIST_PAGE: usize = 20;

/// The most bytes such a default page answers; rows past it move to the next page (T-6061).
const LIST_BYTES: usize = 9_500;

/// The key the per-subject bucket is kept under. It holds a `/`, so no Endpoint slug is it.
const HUB_BUCKET: &str = HUB_PATH;

/// How long a caller's reach is reused (T-4528). Every tool call is decided again by the
/// Endpoint it goes to, so this bounds only how late a list shows a Policy's validity window
/// opening or closing.
const KEPT_FOR: Duration = Duration::from_secs(10);

/// Reaches kept at once; past it the cache starts again empty.
const KEPT: usize = 256;

/// One caller's reach, with the Endpoint table and the accounts it was decided against.
pub(crate) struct Kept {
    table: Arc<HashMap<String, Arc<Endpoint>>>,
    accounts: Arc<ServiceAccounts>,
    at: Instant,
    reach: Arc<[(Arc<Endpoint>, Subject)]>,
}

/// The reaches of the callers of a moment ago, by what their verified token says (T-4528).
pub(crate) type Reaches = Mutex<HashMap<String, Arc<Kept>>>;

/// What a reach depends on in a verified token: everything but its lifetime, so a refreshed
/// token of the same caller finds it. `None` for a data space transfer token, whose reach
/// follows its agreement and the clock, and is decided every time.
fn kept_as(claims: Option<&Claims>, serving: Option<Representation>) -> Option<String> {
    let Some(claims) = claims else {
        return Some(format!("{serving:?} anonymous"));
    };
    if claims.agreement_id.is_some() || claims.participant.is_some() {
        return None;
    }
    let sorted = |items: &[String]| {
        let mut items = items.to_vec();
        items.sort_unstable();
        items
    };
    let mut clients: Vec<(&String, Vec<String>)> = claims
        .resource_access
        .iter()
        .map(|(client, access)| (client, sorted(&access.roles)))
        .collect();
    clients.sort_unstable();
    // Debug quotes every string, so no value can pass for the next field.
    Some(format!(
        "{serving:?} {:?}",
        (
            &claims.sub,
            &claims.iss,
            &claims.azp,
            &claims.preferred_username,
            claims
                .realm_access
                .as_ref()
                .map(|access| sorted(&access.roles)),
            sorted(&claims.groups),
            clients,
            sorted(&claims.aud),
            &claims.scope,
        )
    ))
}

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
        Err(refusal) => return challenged(&gateway, *refusal),
    };
    if anonymous {
        if let Err(refusal) = rate_limit::spend_anonymous_mcp(&gateway, request.headers()) {
            return *refusal;
        }
    }

    let caller = rate_limit::caller_key(&gateway, request.headers());
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
    pub(crate) reach: Arc<[(Arc<Endpoint>, Subject)]>,
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
    // answers an anonymous caller (T-4114, EP-87); a slug outside them is the tool error an
    // unknown slug is (`Hub::resolved`, T-6031).
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
        None => rate_limit::caller_key(gateway, headers),
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

    // The same caller a moment ago, against the same Endpoints and accounts (T-4528).
    let key = kept_as(claims.as_ref(), serving);
    let table = gateway.resolver.table();
    let accounts = gateway.accounts.load_full();
    let kept = key.as_ref().and_then(|key| {
        let reaches = gateway.reaches.lock().ok()?;
        let kept = reaches.get(key)?;
        (Arc::ptr_eq(&kept.table, &table)
            && Arc::ptr_eq(&kept.accounts, &accounts)
            && kept.at.elapsed() < KEPT_FOR)
            .then(|| Arc::clone(&kept.reach))
    });
    let principal = claims
        .as_ref()
        .map_or_else(|| "anonymous".to_owned(), |claims| claims.sub.clone());
    if let Some(reach) = kept {
        return Ok(Reached {
            principal,
            reach,
            decision,
            anonymous: raw.is_none(),
        });
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
    let reach: Arc<[(Arc<Endpoint>, Subject)]> = reach.into();
    if let (Some(key), Ok(mut reaches)) = (key, gateway.reaches.lock()) {
        if reaches.len() >= KEPT {
            reaches.clear();
        }
        reaches.insert(
            key,
            Arc::new(Kept {
                table,
                accounts,
                at: Instant::now(),
                reach: Arc::clone(&reach),
            }),
        );
    }
    Ok(Reached {
        principal,
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
    let offset = offset(arguments)?;
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

    let describe = |(endpoint, subject): &(Arc<Endpoint>, Subject)| {
        let mut entry = described(gateway, endpoint, subject);
        // Each Endpoint names the broker its space lives on (ADR-N-047 §2.3).
        entry["broker"] = json!(gateway.broker.broker_of_view(&endpoint.slug));
        entry
    };
    // No filter: describe the page only, not every Endpoint reached (T-4542: 264 ms at 10,000).
    let (total, endpoints): (usize, Vec<Value>) = if wanted.is_empty() {
        let page = reach
            .iter()
            .skip(offset)
            .take(limit)
            .map(describe)
            .collect();
        (reach.len(), page)
    } else {
        let rows: Vec<Value> = reach
            .iter()
            .map(describe)
            .filter(|entry| {
                wanted
                    .iter()
                    .all(|(name, value)| matches(entry, name, value))
            })
            .collect();
        (
            rows.len(),
            rows.into_iter().skip(offset).take(limit).collect(),
        )
    };
    let mut page = json!({ "endpoints": endpoints, "total": total });
    let next = offset.saturating_add(limit);
    if next < total {
        page["nextCursor"] = json!(next.to_string());
    }
    Ok(page)
}

/// Where a page starts. The cursor this hub hands out is an offset written as a string; one an
/// older client kept as a number still reads. One past the list answers an empty page.
fn offset(arguments: &Map<String, Value>) -> Result<usize, String> {
    match arguments.get("cursor") {
        None => Some(0),
        Some(Value::String(cursor)) => cursor.parse::<usize>().ok(),
        Some(value) => value.as_u64().and_then(|n| usize::try_from(n).ok()),
    }
    .ok_or_else(|| "`cursor` is the previous page's nextCursor, passed back as given".to_owned())
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
    pub(crate) reach: Arc<[(Arc<Endpoint>, Subject)]>,
    /// The key the Endpoint's own `(slug, caller)` bucket is spent under, the middleware's own.
    pub(crate) caller: String,
    /// The named server this request came in by, `None` on the hub (ADR-N-043).
    pub(crate) server: Option<McpServer>,
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
    /// The knowledge sources this request's `search_documents` searches: the server's own, or
    /// the hub's (MF-57).
    fn knowledge_sources<'a>(
        &'a self,
        documents: &'a super::documents::Documents,
    ) -> &'a [super::documents::Source] {
        match &self.server {
            Some(server) => &server.knowledge_sources,
            None => &documents.hub,
        }
    }

    /// The resources this server offers: an assistant's `context` (EP-136), nothing elsewhere.
    fn resources(&self) -> Vec<Value> {
        self.server
            .as_ref()
            .filter(|server| server.has_context() && self.gateway.documents.is_some())
            .map(|server| {
                vec![json!({
                    "uri": server.context_uri(),
                    "name": "context",
                    "title": "What this assistant knows",
                    "description": "Its categories, its Endpoints with their entity counts, and its knowledge sources with their pages and last crawl (API/05 §1.9).",
                    "mimeType": "application/json",
                })]
            })
            .unwrap_or_default()
    }

    /// `resources/read` of the `context` resource: the assistant's own document, as any visitor
    /// reads it; any other URI is unknown (MCP `-32002`).
    async fn read_resource(&self, id: Value, params: &Value) -> Value {
        let uri = params
            .get("uri")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if let Some(slug) = uri.strip_prefix("card://") {
            return self.read_card(id, uri, slug).await;
        }
        let (Some(server), Some(documents)) = (
            self.server
                .as_ref()
                .filter(|s| s.has_context() && s.context_uri() == uri),
            self.gateway.documents.as_ref(),
        ) else {
            return error(id, -32002, "resource not found: this server offers `context` alone; resources/list names its URI");
        };
        match documents.context(&server.name).await {
            Ok(context) => result(
                id,
                json!({ "contents": [{ "uri": uri, "mimeType": "application/json", "text": context.to_string() }] }),
            ),
            Err(err) => {
                tracing::warn!(%err, server = %server.path(), "the assistant's context could not be read");
                error(
                    id,
                    -32603,
                    "what this assistant knows could not be read just now; try again shortly",
                )
            }
        }
    }

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
            "initialize" => {
                let mut initialized = match &self.server {
                    Some(named) => server::initialized(Some(named)),
                    None => json!({
                        "protocolVersion": jsonrpc::PROTOCOL_VERSION,
                        "capabilities": { "tools": { "listChanged": false } },
                        "serverInfo": {
                            "name": "joinedcontext-hub",
                            "version": env!("CARGO_PKG_VERSION"),
                        },
                        "instructions": super::PRIMER.as_str(),
                    }),
                };
                if let (Some(cards), Some(Value::String(instructions))) = (
                    self.cards_in_full().await,
                    initialized.get_mut("instructions"),
                ) {
                    instructions.push_str("\n\n");
                    instructions.push_str(&cards);
                }
                result(id, initialized)
            }
            "ping" => result(id, json!({})),
            "tools/list" => {
                let mut tools = vec![
                    super::search::tool(),
                    list_endpoints_tool(),
                    get_endpoint_tool(),
                ];
                if let Some(documents) = &self.gateway.documents {
                    tools.extend(documents.tools(self.knowledge_sources(documents)).await);
                }
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
                    });
                }
                if self.server.is_some() {
                    server::fan_out_schemas(&mut tools);
                }
                super::annotations::annotate(&mut tools);
                result(id, json!({ "tools": tools }))
            }
            "tools/call" => return self.call(id, params).await,
            "resources/list" => result(id, json!({ "resources": self.resources() })),
            "resources/read" => return Answer::Json(self.read_resource(id, &params).await),
            _ => error(id, -32601, "method not found"),
        })
    }

    async fn call(&self, id: Value, params: Value) -> Answer {
        let name = params.get("name").and_then(Value::as_str).unwrap_or("");
        let mut arguments = match self.bounded_arguments(&id, name, &params) {
            Ok(arguments) => arguments,
            Err(answer) => return *answer,
        };
        if let Some(answer) = self.own_tool(&id, name, &arguments).await {
            return answer;
        }
        let (endpoint, subject) = match self.route_call(&id, name, &params, &mut arguments).await {
            Ok(routed) => routed,
            Err(answer) => return *answer,
        };
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

    /// A call's arguments, refused before any routing when they are no object, and an anonymous
    /// caller's write refused whichever Endpoint it names (T-4168) and its reads bounded by
    /// what one anonymous call may cost the broker (T-4261).
    fn bounded_arguments(
        &self,
        id: &Value,
        name: &str,
        params: &Value,
    ) -> Result<Map<String, Value>, Box<Answer>> {
        if self.reads_only && !tools::anonymous_may(name) {
            return Err(Box::new(Answer::Json(tools::anonymous_refusal(
                id.clone(),
                name,
            ))));
        }
        let mut arguments = match params.get("arguments") {
            None => Map::new(),
            Some(Value::Object(arguments)) => arguments.clone(),
            Some(_) => {
                return Err(Box::new(Answer::Json(error(
                    id.clone(),
                    -32602,
                    "`arguments` is an object",
                ))));
            }
        };
        if self.reads_only {
            if let Err(why) = tools::anonymous_bounds(name, &mut arguments) {
                return Err(Box::new(Answer::Json(result(
                    id.clone(),
                    refused(&why, &Value::Null),
                ))));
            }
        }
        Ok(arguments)
    }

    /// The hub's own tools, answered without an Endpoint: the listing, one Endpoint's card, the
    /// search and the knowledge documents.
    async fn own_tool(
        &self,
        id: &Value,
        name: &str,
        arguments: &Map<String, Value>,
    ) -> Option<Answer> {
        let id = id.clone();
        match name {
            "list_endpoints" => Some(Answer::Json(self.list_endpoints(id, arguments))),
            "get_endpoint" => Some(Answer::Json(self.get_endpoint(id, arguments).await)),
            "search" => Some(Answer::Json(
                catalog::search(&self.gateway, &self.reach, id, arguments).await,
            )),
            _ if name == super::documents::NAME => {
                let documents = self.gateway.documents.as_ref()?;
                let sources = self.knowledge_sources(documents);
                Some(Answer::Json(
                    documents
                        .call(&self.gateway, &self.caller, sources, id, arguments)
                        .await,
                ))
            }
            _ if name == super::documents::CONTEXT_NAME => {
                let documents = self.gateway.documents.as_ref()?;
                let sources = self.knowledge_sources(documents);
                Some(Answer::Json(
                    documents
                        .around(&self.gateway, &self.caller, sources, id, arguments)
                        .await,
                ))
            }
            _ => None,
        }
    }

    /// The Endpoint one call goes to and the caller it is answered under, by its one `endpoint`
    /// argument (ADR-N-025 2.5, 2.6), with the tool granted there. A call that names none goes
    /// to a named server's members or to the caller's catalogue, answered here.
    async fn route_call(
        &self,
        id: &Value,
        name: &str,
        params: &Value,
        arguments: &mut Map<String, Value>,
    ) -> Result<(&Arc<Endpoint>, &Subject), Box<Answer>> {
        let id = id.clone();
        // The one routing argument, one string, never a list (ADR-N-025 2.5, 2.6).
        let slug = match arguments.remove("endpoint") {
            Some(Value::String(slug)) => slug,
            Some(_) => return Err(Box::new(Answer::Json(error(
                id,
                -32602,
                "`endpoint` names one Endpoint by its slug: a single string, never a list (EP-87)",
            )))),
            // A named server's read visits every member it may (ADR-N-043 §2.4).
            None if self.server.is_some() && server::FAN_OUT.contains(&name) => {
                let answer = self.fan_out(id, name, params, std::mem::take(arguments));
                return Err(Box::new(answer.await));
            }
            // Catalogue first: a read that names no Endpoint asks the catalogue (T-4501).
            None => catalog::NAME.to_owned(),
        };
        if let Some(refusal) = misrouted(&id, arguments) {
            return Err(Box::new(Answer::Json(refusal)));
        }
        // The caller's own catalogue, built from `reach` alone (T-4503).
        if catalog::names_catalog(&slug) {
            return Err(Box::new(Answer::Json(
                catalog::call(&self.gateway, &self.reach, id, name, arguments).await,
            )));
        }
        let (endpoint, subject) = self.resolved(&id, &slug)?;
        match tools::hub_tool(&self.gateway, endpoint, subject, &self.reach, name) {
            // The types list_endpoints already names this caller, when the Policy grants no
            // retrieveEntityTypes: a reader of an Endpoint may ask what it reads (T-4567).
            HubTool::Unknown | HubTool::Ungranted(_) if name == "list_types" => {
                let visible = schema::visible(subject, endpoint, crate::pdp::now());
                let types = schema::visible_types(endpoint, &visible);
                Err(Box::new(Answer::Json(result(
                    id,
                    answered(&json!(types), "entityTypes"),
                ))))
            }
            HubTool::Unknown => Err(Box::new(Answer::Json(error(id, -32602, "unknown tool")))),
            HubTool::Ungranted(refusal) => Err(Box::new(Answer::Json(result(id, refusal)))),
            HubTool::Granted => Ok((endpoint, subject)),
        }
    }

    /// The Endpoint a slug names in the caller's list. An Endpoint outside the caller's list and
    /// one that does not exist are the same answer, byte for byte (SP-20): both are searched for
    /// in the caller's list only.
    fn resolved(&self, id: &Value, slug: &str) -> Result<(&Arc<Endpoint>, &Subject), Box<Answer>> {
        match resolve(&self.reach, slug) {
            Ok(found) => Ok(found),
            // A slug outside the list is answered the same whatever it is, with no suggestion
            // that would vary with it (SP-20): an internal one and one that does not exist
            // alike, with a token or without. A tool error, never `401`: a client reads a
            // `401` on a call as its session signed out (T-6031).
            Err(_) if EndpointSlug::new(slug).is_ok() => Err(Box::new(Answer::Json(result(
                id.clone(),
                refused(OUTSIDE_REACH, &Value::Null),
            )))),
            Err(candidates) => {
                let closest = if candidates.is_empty() {
                    "none".to_owned()
                } else {
                    candidates.join(", ")
                };
                Err(Box::new(Answer::Json(error(
                    id.clone(),
                    -32602,
                    &format!("no Endpoint of yours is named `{slug}`; closest: {closest}"),
                ))))
            }
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
            let answer =
                describe::call(&self.gateway, endpoint, subject, id, &Value::Object(asked)).await;
            crate::activity::record(
                endpoint,
                subject,
                name,
                crate::activity::Outcome::of(&answer),
            );
            return answer;
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
        let answer =
            crate::mcp::endpoint::member_call(&self.gateway, endpoint, subject, message).await;
        crate::activity::record(
            endpoint,
            subject,
            name,
            crate::activity::Outcome::of(&answer),
        );
        answer
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
        // A caller naming no `limit` gets a page that fits LIST_BYTES; one naming it gets
        // that many rows, as the assistant reads its servers' first page whole.
        let fitted = !arguments.contains_key("limit");
        if fitted {
            arguments.insert("limit".to_owned(), json!(LIST_PAGE));
        }
        let mut page = match page(&self.gateway, &self.reach, &arguments) {
            Ok(page) => page,
            Err(why) => return result(id, refused(&why, &Value::Null)),
        };
        let mut rows: Vec<Value> = page["endpoints"]
            .as_array()
            .map(|entries| entries.iter().map(|entry| compact(entry, &lang)).collect())
            .unwrap_or_default();
        loop {
            let answer = endpoint_rows(&rows, &page);
            let size = serde_json::to_vec(&answer).map_or(0, |bytes| bytes.len());
            if !fitted || size <= LIST_BYTES || rows.len() <= 1 {
                return result(id, answer);
            }
            // The cut rows open the next page: the cursor is the offset of the first of them.
            rows.pop();
            let first = offset(&arguments).unwrap_or(0);
            page["nextCursor"] = json!(first.saturating_add(rows.len()).to_string());
        }
    }

    /// One Endpoint (T-4748, T-5642): its query card first, the very text `card://` reads, then
    /// what it is and every link. `detail: "attributes"` answers the card's type lines in full,
    /// every type's or the one `type` names, so an agent of a card past the token switch reads
    /// the type it queries without the other types.
    async fn get_endpoint(&self, id: Value, arguments: &Map<String, Value>) -> Value {
        let refuse = |why: &str| result(id.clone(), refused(why, &Value::Null));
        if let Some(unknown) = arguments
            .keys()
            .find(|key| !matches!(key.as_str(), "endpoint" | "detail" | "type"))
        {
            return refuse(&format!("`{unknown}` is not an argument of get_endpoint"));
        }
        let Some(named) = arguments.get("endpoint").and_then(Value::as_str) else {
            return refuse(
                "`endpoint` names one Endpoint: its slug, {project}/{name}, title or space",
            );
        };
        let detail = match arguments.get("detail") {
            None => false,
            Some(Value::String(detail)) if detail == "attributes" => true,
            Some(_) => return refuse("`detail` is \"attributes\", optionally with one `type`"),
        };
        let wanted = match arguments.get("type") {
            None => None,
            Some(Value::String(kind)) if detail => Some(kind.as_str()),
            Some(Value::String(_)) => return refuse("`type` comes with `detail: \"attributes\"`"),
            Some(_) => return refuse("`type` is one entity type name"),
        };
        let (endpoint, subject) = match resolve(&self.reach, named) {
            Ok(found) => found,
            Err(near) => {
                return refuse(&format!(
                    "no Endpoint you may read is called `{named}`; closest: {}",
                    near.join(", ")
                ))
            }
        };
        if detail {
            return match describe::type_lines(&self.gateway, endpoint, subject, wanted).await {
                Ok(Some(lines)) => {
                    let mut answer = answered(
                        &json!({ "slug": endpoint.slug, "attributes": lines }),
                        "endpoint",
                    );
                    answer["content"][0]["text"] = json!(lines);
                    result(id, answer)
                }
                Ok(None) => {
                    let visible = schema::visible(subject, endpoint, crate::pdp::now());
                    refuse(&format!(
                        "`{}` is no type of this Endpoint you may read; its types: {}",
                        wanted.unwrap_or_default(),
                        schema::visible_types(endpoint, &visible).join(", ")
                    ))
                }
                Err(why) => refuse(&why),
            };
        }
        let mut entry = described(&self.gateway, endpoint, subject);
        entry["broker"] = json!(self.gateway.broker.broker_of_view(&endpoint.slug));
        if let Some(members) = entry.as_object_mut() {
            members.remove("attributes");
        }
        let card = describe::card(&self.gateway, endpoint, subject).await;
        let mut answer = answered(&entry, "endpoint");
        // The card first, the record after it (T-4759 item 3); a card the broker could not
        // give leaves the links and says so, rather than refusing them too.
        let text = match &card {
            Ok(card) => format!("{card}\n\n{entry}"),
            Err(why) => format!("The query card could not be read: {why}.\n\n{entry}"),
        };
        answer["content"][0]["text"] = json!(text);
        if let (Ok(card), Some(structured)) = (
            card,
            answer["structuredContent"]["endpoint"].as_object_mut(),
        ) {
            structured.insert("card".to_owned(), json!(card));
        }
        result(id, answer)
    }

    /// The query cards of every Endpoint of the caller's list, when there are at most
    /// [`CARDS_ENDPOINTS`] and they fit in [`CARDS_TOKENS`] together (T-4759 item 2): a model
    /// connected to a small server knows every type before its first call. Larger lists keep the
    /// instructions short; get_endpoint gives each card.
    async fn cards_in_full(&self) -> Option<String> {
        if self.reach.is_empty() || self.reach.len() > CARDS_ENDPOINTS {
            return None;
        }
        let cards = futures_util::future::join_all(
            self.reach
                .iter()
                .filter(|(endpoint, subject)| {
                    tools::reads_anything(&self.gateway, endpoint, subject)
                })
                .map(|(endpoint, subject)| describe::card(&self.gateway, endpoint, subject)),
        )
        .await;
        // A card the broker could not give is left to get_endpoint; the others still help.
        let cards: Vec<String> = cards.into_iter().filter_map(Result::ok).collect();
        let joined = cards.join("\n\n");
        (!cards.is_empty() && jc_core::query_card::tokens(&joined) <= CARDS_TOKENS)
            .then_some(joined)
    }

    /// `card://{endpoint}` of any Endpoint of the caller's list (T-5642): the per-Endpoint
    /// server's card, so a hub client reads the same text. One outside the list is unknown.
    async fn read_card(&self, id: Value, uri: &str, slug: &str) -> Value {
        let found = self.reach.iter().find(|(endpoint, subject)| {
            endpoint.slug == slug && tools::reads_anything(&self.gateway, endpoint, subject)
        });
        let Some((endpoint, subject)) = found else {
            return error(
                id,
                -32002,
                "resource not found: `card://` names one Endpoint of list_endpoints by its slug",
            );
        };
        match describe::card(&self.gateway, endpoint, subject).await {
            Ok(card) => result(
                id,
                json!({ "contents": [{ "uri": uri, "mimeType": "text/plain", "text": card }] }),
            ),
            Err(why) => result(id, refused(&why, &Value::Null)),
        }
    }
}

/// The most Endpoints whose cards an `initialize` reads (one broker read each, T-5642).
const CARDS_ENDPOINTS: usize = 8;

/// The most tokens the cards of a server's Endpoints add to its instructions (T-4759 item 2).
const CARDS_TOKENS: usize = 2_000;

/// The `list_endpoints` answer of `rows`: one line each as text, the rows, `total` and the
/// page's `nextCursor` as structured content.
fn endpoint_rows(rows: &[Value], page: &Value) -> Value {
    let mut answer = answered(&json!(rows), "endpoints");
    answer["content"][0]["text"] = json!(lines(rows, page));
    if let Some(structured) = answer
        .get_mut("structuredContent")
        .and_then(Value::as_object_mut)
    {
        structured.insert("total".to_owned(), page["total"].clone());
        if let Some(next) = page.get("nextCursor") {
            structured.insert("nextCursor".to_owned(), next.clone());
        }
    }
    answer
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
/// without case, its `{project}/{name}` (T-4748), or its title in any language or its context
/// space when exactly one Endpoint has it. Only `reach` is searched, so a name never finds an Endpoint the caller may not read
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
    // The readable `{project}/{name}`, as a Portal or an assistant names it (T-4748).
    if let Some((project, name)) = wanted.split_once('/') {
        if let Some((endpoint, subject)) = reach.iter().find(|(e, _)| {
            !e.manifest_name.is_empty()
                && e.project.to_lowercase() == project
                && e.manifest_name.to_lowercase() == name
        }) {
            return Ok((endpoint, subject));
        }
    }
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
    // The domains of the types this caller reads, as search counts them (T-4567).
    entry["domains"] =
        json!(jc_core::classification::classify(types.iter().map(String::as_str)).domains);
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
    // The recommended formalism of the newest major, the one describe_schema recommends, and
    // the other formalisms beside it (T-4748).
    if let Some(major) = newest {
        use crate::middleware::response::Artifact;
        let at = |artifact: Artifact| {
            format!(
                "{base}{}/schema/v{major}/{}",
                endpoint.base_path,
                artifact.file_name()
            )
        };
        entry["schema"] = json!(at(Artifact::LinkMl));
        entry["schemas"] = json!({
            "index": format!("{base}{}/schema/index.json", endpoint.base_path),
            "linkml": at(Artifact::LinkMl),
            "shacl": at(Artifact::Shacl),
            "jsonSchema": at(Artifact::JsonSchema),
            "context": at(Artifact::Context),
        });
    }
    if let Some(publisher) = endpoint.catalog.as_ref().and_then(|c| c.publisher.as_ref()) {
        entry["publisher"] = json!(publisher.name);
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
                        .filter(|name| {
                            !matches!(*name, "id" | "type" | "@context")
                                && !SUB_PROPERTIES.contains(name)
                        })
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
    // The codes an agent may filter by, each with its English label (AG-124).
    let themes = jc_core::kinds::DataTheme::ALL
        .iter()
        .map(|theme| format!("{} {}", theme.code(), theme.label_in("en")))
        .collect::<Vec<_>>()
        .join("; ");
    let themes = format!("An EU data theme code: {themes}");
    json!({
        "name": "list_endpoints",
        "description": "The Endpoints you may read, open ones without a login, 20 a page \
                        (`search` narrows). One line each: slug, title, space, access, types, \
                        domain, EU theme, area, period. Name one as `endpoint` elsewhere; \
                        get_endpoint: its URLs, attributes, model.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "q": { "type": "string", "minLength": 1, "maxLength": 200, "description": "Words in the title or description, any language" },
                "type": { "type": "string", "minLength": 1, "maxLength": 200, "description": "An entity type you may read there" },
                "attribute": { "type": "string", "minLength": 1, "maxLength": 200, "description": "An attribute name of such a type" },
                "space": { "type": "string", "minLength": 1, "maxLength": 200, "description": "The context space" },
                "access": { "type": "string", "enum": ["public", "internal"] },
                "domain": { "type": "string", "minLength": 1, "maxLength": 200, "description": "A Smart Data Model domain, e.g. Environment" },
                "theme": { "type": "string", "minLength": 1, "maxLength": 200, "description": themes },
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
        "description": "One Endpoint: its NGSI-LD query card (types, attributes, units), then \
                        title, history and links (surfaces, DCAT, MCP, LinkML, SHACL, JSON Schema, \
                        context). detail attributes: all types, or one.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "endpoint": { "type": "string", "minLength": 1, "maxLength": 200, "description": "Its slug, {project}/{name}, title or space" },
                "detail": { "enum": ["attributes"] },
                "type": { "type": "string", "maxLength": 256 },
            },
            "required": ["endpoint"],
            "additionalProperties": false,
        },
        "annotations": { "readOnlyHint": true, "destructiveHint": false },
    })
}

/// A `401` of the session itself (a token that fails verification) names the hub's resource
/// metadata, which is how a client learns where to sign in again (RFC 9728, AG-32, T-6031).
fn challenged(gateway: &Gateway, mut response: Response<Body>) -> Response<Body> {
    if response.status() != StatusCode::UNAUTHORIZED {
        return response;
    }
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

/// The one answer to a slug outside the caller's list, unknown or internal alike: it names no
/// slug, so the bytes never vary with the one asked for (SP-20, T-6031).
const OUTSIDE_REACH: &str = "no Endpoint you may read has that slug: list_endpoints lists the ones you may read, and an Endpoint that is not public is read after signing in with an account its Policy grants";

/// The hub's protected-resource metadata (RFC 9728, ADR-N-025 section 4).
pub async fn protected_resource(State(gateway): State<Arc<Gateway>>) -> Response<Body> {
    // The hub answers an anonymous caller whenever one Endpoint is public (ADR-N-052 §2.2).
    let readable = gateway
        .resolver
        .endpoints()
        .iter()
        .any(|endpoint| endpoint.admits(None));
    let servers = crate::oauth::authorization_servers(&gateway, readable);
    if servers.is_empty() {
        return ProblemDetails::not_found().into_response();
    }
    jsonrpc::json_response(
        StatusCode::OK,
        &json!({
            "resource": format!("{}{HUB_PATH}", gateway.base_url()),
            "authorization_servers": servers,
            "bearer_methods_supported": ["header"],
            // The one-login scope (ADR-N-025 4b); `endpoint:{slug}` stays a narrower choice.
            "scopes_supported": ["endpoint:*"],
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hub(anonymous: bool) -> Hub {
        let gateway = Gateway::new(
            crate::proxy::Broker::new("http://localhost:9"),
            Box::new(crate::pdp::PolicyPdp),
            "example.org",
        );
        Hub {
            gateway: Arc::new(gateway),
            reach: Arc::from(Vec::new()),
            caller: "caller".to_owned(),
            server: None,
            reads_only: anonymous,
        }
    }

    fn code(answer: Option<Answer>) -> Option<i64> {
        match answer {
            Some(Answer::Json(value)) => value["error"]["code"].as_i64(),
            _ => None,
        }
    }

    /// T-5508, T-4168: an anonymous write is refused before routing, whichever Endpoint it
    /// names, and arguments that are no object are refused for every caller.
    #[test]
    fn bounded_arguments_refuses_an_anonymous_write_and_a_scalar() {
        let id = json!(1);
        let write = json!({"name": "create_entity", "arguments": {"endpoint": "x"}});
        let refused = hub(true).bounded_arguments(&id, "create_entity", &write);
        assert_eq!(code(refused.err().map(|answer| *answer)), Some(-32602));
        let scalar = json!({"name": "query_entities", "arguments": 3});
        let refused = hub(false).bounded_arguments(&id, "query_entities", &scalar);
        assert_eq!(code(refused.err().map(|answer| *answer)), Some(-32602));
        let none = json!({"name": "query_entities"});
        let kept = hub(false).bounded_arguments(&id, "query_entities", &none);
        assert!(kept.is_ok_and(|arguments| arguments.is_empty()));
    }

    /// T-5508, T-6031, SP-20: a slug outside the caller's list is one tool error, with a token
    /// or without, never a `401`; a list as `endpoint` is refused; no own tool answers a broker
    /// tool.
    #[tokio::test]
    async fn route_call_refuses_a_list_and_an_endpoint_outside_the_reach() {
        let id = json!(2);
        let slug = "aaaaaaaaaaaaaaaaaaaaaaaaaa";
        for anonymous in [true, false] {
            let outside = hub(anonymous)
                .resolved(&id, slug)
                .err()
                .map(|answer| *answer);
            let Some(Answer::Json(answer)) = outside else {
                panic!("anonymous {anonymous}: no JSON-RPC answer");
            };
            assert_eq!(answer["result"]["isError"], json!(true), "{answer}");
            assert_eq!(answer["result"]["content"][0]["text"], json!(OUTSIDE_REACH));
        }

        let mut arguments = Map::new();
        arguments.insert("endpoint".to_owned(), json!(["a", "b"]));
        let params = json!({});
        let signed_in = hub(false);
        let listed = signed_in
            .route_call(&id, "query_entities", &params, &mut arguments)
            .await;
        assert_eq!(code(listed.err().map(|answer| *answer)), Some(-32602));
        let own = hub(false)
            .own_tool(&id, "query_entities", &Map::new())
            .await;
        assert!(own.is_none());
    }
}
