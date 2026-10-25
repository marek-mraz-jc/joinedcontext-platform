//! MCP on one Endpoint and on one Context Space: JSON-RPC in, the broker's MCP surface out, a
//! federated Endpoint's members called under the caller (EP-87).

use crate::activity::Outcome;
use crate::app::{Gateway, MAX_BODY};
use crate::auth::subject::Subject;
use crate::auth::subject::{admit, admit_space, principal_of};
use crate::auth::token;
use crate::handlers::ngsi_ld::{broker_failure, view_request, MAX_VIEW_ID};
use crate::handlers::respond::json_response;
use crate::middleware::rate_limit::{self};
use crate::proxy::{self};
use crate::resolver::Endpoint;
use crate::{mcp, middleware::tenancy, query};
use axum::body::Body;
use axum::extract::{Path, Request, State};
use axum::http::header::{ACCEPT, CONTENT_LENGTH, CONTENT_TYPE};
use axum::http::{HeaderValue, Method, Response, StatusCode};
use axum::response::IntoResponse;
use jc_core::kinds::{Audience, Operation, Representation};
use jc_core::ProblemDetails;
use serde_json::Value;
use std::sync::Arc;

/// The endpoint's own MCP instance: one JSON-RPC message in, one out (T-0166, EP-24, SP-14).
///
/// Streamable HTTP without a session: no `GET` stream to open, nothing kept between calls,
/// so a grant withdrawn a second ago is already gone from the next `tools/list` (SP-19).
pub(crate) async fn mcp_message(
    State(gateway): State<Arc<Gateway>>,
    Path(slug): Path<String>,
    mut request: Request,
) -> Response<Body> {
    tenancy::strip_client_headers(&mut request);

    // AG-32: a client with no token is told where to get one, instead of being refused in
    // a way it cannot act on. A public MCP instance needs none, so it is served first.
    let public = gateway
        .resolver
        .resolve(&slug)
        .is_some_and(|endpoint| endpoint.audience == Audience::Public);
    let anonymous = !token::presents(request.headers());
    if !public && anonymous {
        return unauthorized(&gateway, &slug);
    }
    let spent = if anonymous {
        rate_limit::spend_anonymous_mcp(&gateway, request.headers())
    } else {
        rate_limit::spend_subject_mcp(&gateway, &request)
    };
    if let Err(refusal) = spent {
        return *refusal;
    }

    let (endpoint, subject) = match admit(
        &gateway,
        &slug,
        Some(Representation::Mcp),
        request.headers(),
    ) {
        Ok(admitted) => admitted,
        Err(problem) => return *problem,
    };

    let view = endpoint.slug.clone();
    mcp_answer(gateway, endpoint, subject, view, request, anonymous).await
}

/// One JSON-RPC message, whichever surface it arrived on: the broker's MCP surface of the
/// view answers it under the verified caller (plan 5.3, T-3647). The gateway keeps what the
/// broker has no tool or resource for, `describe_access`, `describe_schema` and their
/// `access://` and `schema://` documents, added to the broker's listings and answered from the
/// documents the access and schema surfaces serve, and what no view serves yet (a view
/// mapping) stays on its own façade. A caller under a data space agreement is the view's too:
/// the hop names its DID alone (map K38, T-3818). A federating space's registrations are named
/// in the answer, see [`federated`]. Without a token (`reads_only`) the caller lists and calls
/// [`mcp::tools::ANONYMOUS_TOOLS`] alone, whatever the broker's view offers (T-4168).
pub(crate) async fn mcp_answer(
    gateway: Arc<Gateway>,
    endpoint: Arc<Endpoint>,
    subject: Subject,
    view: String,
    request: Request,
    reads_only: bool,
) -> Response<Body> {
    let (parts, body) = request.into_parts();
    let caller = crate::handlers::ngsi_ld::create_key(&subject, &parts.headers);
    let Ok(bytes) = axum::body::to_bytes(body, MAX_BODY).await else {
        return mcp::jsonrpc::parse_error();
    };
    let Ok(mut message) = serde_json::from_slice::<Value>(&bytes) else {
        return mcp::jsonrpc::parse_error();
    };
    // What is no single request (a notification, a batch, a scalar) changes nothing and is
    // answered with nothing, before a hop: nothing is kept between messages (SP-19).
    if !(message.is_object() && message.get("id").is_some()) {
        return StatusCode::ACCEPTED.into_response();
    }
    let tool = message["params"]["name"].as_str().map(str::to_owned);
    let calls = message["method"] == "tools/call";
    let id = message["id"].clone();
    // Each tool call, on the project's Activity (T-5322): never its arguments or its answer.
    let note = |outcome| {
        if calls {
            crate::activity::record(
                &endpoint,
                &subject,
                tool.as_deref().unwrap_or_default(),
                outcome,
            );
        }
    };
    if reads_only && calls {
        let name = tool.as_deref().unwrap_or_default();
        if !mcp::tools::anonymous_may(name) {
            note(Outcome::Refused);
            let refusal = mcp::tools::anonymous_refusal(id, name);
            return mcp::jsonrpc::json_response(StatusCode::OK, &refusal);
        }
    }
    if calls
        && tool
            .as_deref()
            .is_some_and(|tool| mcp::describe::DESCRIBE_TOOLS.contains(&tool))
    {
        let answer =
            mcp::describe::call(&gateway, &endpoint, &subject, id, &message["params"]).await;
        note(Outcome::of(&answer));
        return mcp::jsonrpc::json_response(StatusCode::OK, &answer);
    }
    if message["method"] == "resources/read"
        && message["params"]["uri"].as_str().is_some_and(|uri| {
            mcp::describe::OWN_SCHEMES
                .iter()
                .any(|scheme| uri.starts_with(scheme))
        })
    {
        let answer =
            mcp::describe::read(&gateway, &endpoint, &subject, id, &message["params"]).await;
        return mcp::jsonrpc::json_response(StatusCode::OK, &answer);
    }
    if !(1..=MAX_VIEW_ID).contains(&view.len()) {
        // The Portal writes no view whose id the broker refuses (`[a-z0-9-]{1,63}`), so a space
        // whose name leaves no room for `cs-` has no MCP surface to forward to.
        tracing::info!(%view, "MCP refused: no broker view can carry this id");
        return ProblemDetails::new(
            503,
            "no-view",
            "The access surface of this endpoint is not served",
        )
        .with_detail(
            "this space's name is too long for a broker view, so its MCP surface is not \
                 served; a space name of at most 60 characters is",
        )
        .into_response();
    }

    let operation = mcp_operation(calls.then_some(tool.as_deref().unwrap_or_default()));
    let mut request = Request::from_parts(parts, Body::empty());
    if let Err(answer) =
        view_request(&gateway, &endpoint, &subject, "", operation, &mut request).await
    {
        note(Outcome::Refused);
        return *answer;
    }
    // A tool call is an AI agent's, acting for the caller its token names (ADR-N-054 2.3).
    tenancy::as_agent(request.headers_mut());
    let federates = member_names(&gateway, &endpoint);
    let (mut parts, _) = request.into_parts();
    let mut reserved = None;
    if calls {
        capped_arguments(&mut message);
        match mcp::writes::guarded(&gateway, &endpoint, &subject, &caller, &mut message).await {
            Ok(held) => reserved = held,
            Err(refusal) => {
                note(Outcome::Refused);
                return mcp::jsonrpc::json_response(StatusCode::OK, &refusal);
            }
        }
    }
    // The broker's Streamable HTTP refuses a request that does not accept both (406); the
    // façade took `application/json` alone, and the callers that send just that keep working.
    parts.headers.insert(
        ACCEPT,
        HeaderValue::from_static("application/json, text/event-stream"),
    );
    tracing::info!(
        slug = %endpoint.slug,
        space = %endpoint.space,
        %view,
        principal = %principal_of(&subject),
        tool = tool.as_deref().unwrap_or_default(),
        "MCP message forwarded to the broker's view"
    );
    let method = message["method"].as_str().unwrap_or_default().to_owned();
    let answer = match gateway
        .broker
        .send(
            Method::POST,
            &format!("/x/views/{view}/mcp"),
            parts.headers,
            Body::from(message.to_string()),
        )
        .await
    {
        Ok(answer) => answer,
        Err(error) => {
            note(Outcome::Failed);
            if let Some(reserved) = reserved {
                reserved.release(&gateway, &endpoint);
            }
            return ProblemDetails::from(error).into_response();
        }
    };
    // ponytail: a streamed result is not read to tell a tool error from an answer.
    note(match answer.status() {
        status if status.is_success() => Outcome::Answered,
        status if status.is_server_error() => Outcome::Failed,
        _ => Outcome::Refused,
    });
    // ponytail: a tool error inside a 200 keeps its create spent; reading every answer to
    // find one would hold streamed results, so only a refused request gives it back.
    if let Some(reserved) = reserved.filter(|_| !answer.status().is_success()) {
        reserved.release(&gateway, &endpoint);
    }
    if answer.status().is_server_error() {
        let (parts, body) = answer.into_parts();
        return broker_failure(
            parts.status,
            parts.headers.get("retry-after").cloned(),
            body,
        )
        .await;
    }
    let json = answer
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.starts_with("application/json"));
    let key = OWN_LISTINGS
        .iter()
        .find(|(listing, _)| *listing == method)
        .map(|(_, key)| *key);
    let decorated = !federates.is_empty() && FEDERATED_METHODS.contains(&method.as_str());
    // A tool list the gateway cannot read is one it cannot narrow: refused, never passed on.
    let narrowed = reads_only && method == "tools/list" && answer.status().is_success();
    if narrowed && !json {
        return unreadable_tool_list(id, &endpoint);
    }
    // A tool result is read to name its units (T-4373), but only one whose length is declared
    // and fits the buffer: a larger or unsized one goes on unread, never cut to nothing.
    let fits = answer
        .headers()
        .get(CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok()?.parse::<usize>().ok())
        .is_some_and(|length| length <= MAX_BODY);
    let units = calls && fits;
    let live = method == "initialize" && endpoint.serves(Representation::WebSocket);
    if key.is_none() && !decorated && !units && !live || !answer.status().is_success() || !json {
        return answer;
    }
    let (parts, body) = answer.into_parts();
    let held = axum::body::to_bytes(body, MAX_BODY)
        .await
        .unwrap_or_default();
    let Ok(mut listed) = serde_json::from_slice::<Value>(&held) else {
        if narrowed {
            return unreadable_tool_list(id, &endpoint);
        }
        return proxy::with_body(parts, held);
    };
    if decorated {
        federated(&mut listed, &method, &federates);
    }
    if units {
        mcp::units::readable(&mut listed);
    }
    if live {
        let address = crate::handlers::ws::ws_address(&format!(
            "{}{}/ws",
            gateway.base_url(),
            endpoint.base_path
        ));
        live_instructions(&mut listed, &address);
    }
    if let Some(key) = key {
        let kept = match key {
            "tools" => mcp::describe::tools(&gateway, &endpoint, &subject),
            _ => mcp::describe::resources(&gateway, &endpoint, &subject).await,
        };
        let items = listed
            .get_mut("result")
            .and_then(|result| result.get_mut(key))
            .and_then(Value::as_array_mut);
        if let Some(items) = items {
            items.extend(kept);
            if narrowed {
                mcp::tools::keep_anonymous(items);
            }
            if key == "tools" {
                mcp::annotations::annotate(items);
            }
        }
    }
    proxy::with_body(parts, listed.to_string().into_bytes())
}

/// The answer to an anonymous `tools/list` whose broker answer the gateway could not read, and so
/// could not narrow to the reads (T-4168).
fn unreadable_tool_list(id: Value, endpoint: &Endpoint) -> Response<Body> {
    tracing::warn!(slug = %endpoint.slug, "the broker's tool list was not JSON; refused to an anonymous caller");
    let refusal = mcp::jsonrpc::error(
        id,
        -32603,
        "this Endpoint's tool list could not be read; try again later",
    );
    mcp::jsonrpc::json_response(StatusCode::OK, &refusal)
}

/// The registrations of the endpoint's space, by name and never by address (EP-71).
fn member_names(gateway: &Gateway, endpoint: &Endpoint) -> Vec<String> {
    gateway
        .members_of(&endpoint.project, &endpoint.space)
        .into_iter()
        .map(|member| member.name)
        .collect()
}

/// Names the Endpoint's WebSocket in the server's instructions, so an agent learns that live
/// updates exist before it lists a tool (T-5045); `describe_access` carries the details.
fn live_instructions(answer: &mut Value, address: &str) {
    if let Some(Value::String(instructions)) = answer.pointer_mut("/result/instructions") {
        instructions.push_str(&format!(
            " Live updates: open {address} as a WebSocket with the subprotocol ngsi-ld-ws and \
             send x-subscribe; describe_access lists the message shape and the limits under \
             `liveUpdates`."
        ));
    }
}

/// The messages whose answer says what a federating space federates (EP-71, AG-30).
const FEDERATED_METHODS: &[&str] = &["initialize", "tools/list", "tools/call"];

/// What a federating space adds to the broker's answer of its view (EP-70, EP-71, AG-30): the
/// broker merges the space's registrations and its view says nothing of them, so the gateway
/// names them, by name only, where a model reads them: the server's instructions, each read
/// tool's description and output schema, and the result of a read that succeeded, as
/// `jc:source`. A write goes to this space alone and is described as it is.
pub(crate) fn federated(answer: &mut Value, method: &str, members: &[String]) {
    let Some(result) = answer.get_mut("result").and_then(Value::as_object_mut) else {
        return;
    };
    let names = members.join(", ");
    match method {
        "initialize" => {
            if let Some(Value::String(instructions)) = result.get_mut("instructions") {
                instructions.push_str(&format!(
                    " This space federates {names}: every read answers over their union, a \
                     result carries the names it came from as `jc:source`, and an answer can be \
                     partial when one of them does not respond."
                ));
            }
        }
        "tools/list" => {
            let tools = result.get_mut("tools").and_then(Value::as_array_mut);
            for tool in tools.into_iter().flatten().filter_map(Value::as_object_mut) {
                let read = tool
                    .get("name")
                    .and_then(Value::as_str)
                    .is_some_and(mcp::writes::is_read_tool);
                if !read {
                    continue;
                }
                if let Some(Value::String(description)) = tool.get_mut("description") {
                    description.push_str(&format!(
                        " This space federates {names}: the answer is the union of their data \
                         and can be partial."
                    ));
                }
                let properties = tool
                    .get_mut("outputSchema")
                    .and_then(|schema| schema.get_mut("properties"))
                    .and_then(Value::as_object_mut);
                if let Some(properties) = properties {
                    properties.insert(
                        "jc:source".to_owned(),
                        serde_json::json!({
                            "type": "array",
                            "items": { "type": "string" },
                            "description": "The registrations this answer is a union over, by name (EP-71)",
                        }),
                    );
                }
            }
        }
        "tools/call" if result.get("isError") != Some(&Value::Bool(true)) => {
            let structured = result
                .get_mut("structuredContent")
                .and_then(Value::as_object_mut);
            if let Some(structured) = structured {
                structured.insert("jc:source".to_owned(), Value::from(members.to_vec()));
            }
        }
        _ => {}
    }
}

/// The operation the domain gate (PF-41) weighs a message as: a tool the gateway does not know
/// as a read is a write. It stands for the class of the call; the broker gates the tool's own
/// clause.
fn mcp_operation(tool: Option<&str>) -> Operation {
    if tool.is_some_and(|tool| !mcp::writes::is_read_tool(tool)) {
        Operation::UpsertBatch
    } else {
        Operation::QueryEntity
    }
}

/// One `tools/call` of the hub or of a named server as the broker's view of the member answers
/// it (T-3808): the member's slug is its view, the tenant and the verified caller are pinned, and
/// no token travels. What fails on the way is the call's tool error, so a fan-out names the
/// member it failed on; the broker's own explanation of a failure is not passed on.
pub(crate) async fn member_call(
    gateway: &Gateway,
    endpoint: &Endpoint,
    subject: &Subject,
    mut message: Value,
) -> Value {
    let id = message["id"].clone();
    let operation = mcp_operation(message["params"]["name"].as_str());
    let mut request = Request::new(Body::empty());
    if let Err(refusal) =
        view_request(gateway, endpoint, subject, "", operation, &mut request).await
    {
        let held = axum::body::to_bytes(refusal.into_body(), MAX_BODY)
            .await
            .unwrap_or_default();
        return member_refusal(id, mcp::writes::problem_text(&held));
    }
    tenancy::as_agent(request.headers_mut());
    let (mut parts, _) = request.into_parts();
    capped_arguments(&mut message);
    // A caller without a token lists and calls reads alone here (T-4168), so a write's create
    // is always a signed-in principal's.
    let caller = crate::handlers::ngsi_ld::create_key(subject, &parts.headers);
    let reserved =
        match mcp::writes::guarded(gateway, endpoint, subject, &caller, &mut message).await {
            Ok(reserved) => reserved,
            Err(refusal) => return refusal,
        };
    parts
        .headers
        .insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    parts.headers.insert(
        ACCEPT,
        HeaderValue::from_static("application/json, text/event-stream"),
    );
    tracing::info!(
        slug = %endpoint.slug,
        space = %endpoint.space,
        principal = %principal_of(subject),
        tool = message["params"]["name"].as_str().unwrap_or_default(),
        "member call forwarded to the broker's view"
    );
    let path = format!("/x/views/{}/mcp", endpoint.slug);
    let answer = match gateway
        .broker
        .send(
            Method::POST,
            &path,
            parts.headers,
            Body::from(message.to_string()),
        )
        .await
    {
        Ok(answer) => answer,
        Err(error) => {
            tracing::warn!(slug = %endpoint.slug, %error, "the broker's view did not answer a member call");
            if let Some(reserved) = reserved {
                reserved.release(gateway, endpoint);
            }
            return member_refusal(id, None);
        }
    };
    let status = answer.status();
    let held = axum::body::to_bytes(answer.into_body(), MAX_BODY)
        .await
        .unwrap_or_default();
    if let Some(reserved) = reserved {
        let taken = status.is_success()
            && serde_json::from_slice::<Value>(&held)
                .is_ok_and(|answer| answer["result"]["isError"] != true);
        if !taken {
            reserved.release(gateway, endpoint);
        }
    }
    match serde_json::from_slice::<Value>(&held) {
        Ok(mut answer) if status.is_success() && answer.get("jsonrpc").is_some() => {
            let federates = member_names(gateway, endpoint);
            if !federates.is_empty() {
                federated(&mut answer, "tools/call", &federates);
            }
            mcp::units::readable(&mut answer);
            answer
        }
        _ if status.is_client_error() => member_refusal(id, mcp::writes::problem_text(&held)),
        _ => {
            tracing::warn!(
                slug = %endpoint.slug,
                status = status.as_u16(),
                broker = %String::from_utf8_lossy(&held),
                "the broker failed a member call; its own explanation is not passed on"
            );
            member_refusal(id, None)
        }
    }
}

/// A member call that ended before the broker's tool answered, as the tool error MCP gives it.
fn member_refusal(id: Value, why: Option<String>) -> Value {
    let why = why.unwrap_or_else(|| {
        "the context broker behind this Endpoint failed to answer this call".to_owned()
    });
    mcp::jsonrpc::result(id, mcp::jsonrpc::refused(&why, &Value::Null))
}

/// The listings the gateway adds its own entries to, by method and result member.
const OWN_LISTINGS: &[(&str, &str)] = &[("tools/list", "tools"), ("resources/list", "resources")];

/// The two costs one call may put on the shared broker, capped as [`capped_query`] caps them
/// on the NGSI-LD surface: `joinLevel` (AG-25) and `lastN` (GW26). A message whose `params` or
/// `arguments` is no object is left for the broker to refuse.
fn capped_arguments(message: &mut Value) {
    let Some(arguments) = message
        .get_mut("params")
        .and_then(|params| params.get_mut("arguments"))
        .and_then(Value::as_object_mut)
    else {
        return;
    };
    for (name, cap) in [
        ("joinLevel", query::JOIN_LEVEL_CAP),
        ("lastN", query::LAST_N_CAP),
    ] {
        if let Some(value) = arguments.get_mut(name) {
            if value.as_u64().is_some_and(|n| n > cap) {
                *value = Value::from(cap);
            }
        }
    }
}

/// The space's own MCP instance, the same façade the endpoint surface runs (SP-14).
pub(crate) async fn space_mcp_message(
    State(gateway): State<Arc<Gateway>>,
    Path(space): Path<String>,
    mut request: Request,
) -> Response<Body> {
    tenancy::strip_client_headers(&mut request);
    let anonymous = !token::presents(request.headers());
    let spent = if anonymous {
        rate_limit::spend_anonymous_mcp(&gateway, request.headers())
    } else {
        rate_limit::spend_subject_mcp(&gateway, &request)
    };
    if let Err(refusal) = spent {
        return *refusal;
    }
    let (space, subject) = match admit_space(
        &gateway,
        &space,
        Some(Representation::Mcp),
        request.headers(),
    ) {
        Ok(admitted) => admitted,
        Err(problem) => return *problem,
    };
    // The space surface's view is the one the Portal writes per Context Space (plan 5.1).
    let view = format!("cs-{}", space.endpoint.space);
    mcp_answer(
        gateway,
        Arc::clone(&space.endpoint),
        subject,
        view,
        request,
        anonymous,
    )
    .await
}

/// The protected-resource metadata of one MCP route (RFC 9728, AG-32).
///
/// Answered for every slug, whether or not one resolves: the document names the resource
/// URL the caller already typed and the realm the deployment already publishes, so it
/// discloses nothing, and a phone that has to discover its authorization server always
/// can (R20, EP-23).
pub(crate) async fn protected_resource(
    State(gateway): State<Arc<Gateway>>,
    Path(slug): Path<String>,
) -> Response<Body> {
    let readable = gateway
        .resolver
        .resolve(&slug)
        .is_some_and(|endpoint| endpoint.admits(None));
    let servers = crate::oauth::authorization_servers(&gateway, readable);
    if servers.is_empty() {
        // No realm and no anonymous OAuth server: there is no authorization server to point a
        // client at.
        return ProblemDetails::not_found().into_response();
    }
    json_response(&serde_json::json!({
        "resource": format!("{}/api/endpoint/{slug}/mcp", gateway.base_url()),
        "authorization_servers": servers,
        "bearer_methods_supported": ["header"],
        "resource_documentation": format!("{}/api/endpoint/{slug}/", gateway.base_url()),
    }))
}

/// The `401` a client needs in order to find its authorization server (RFC 9728, AG-32).
///
/// Sent for every unauthenticated call that is not on a public MCP instance, whether the
/// slug resolves or not, so the answer is the same for an endpoint that needs a login and
/// for one that does not exist (R20).
///
/// The slug is the caller's text inside a quoted-string, so only a slug shaped like one is named
/// there: a quote, a backslash or a dot segment would reshape the challenge or the URL a client
/// resolves, and such a slug never resolves anyway (T-2509). It gets the same 401 with no
/// challenge.
pub(crate) fn unauthorized(gateway: &Gateway, slug: &str) -> Response<Body> {
    let mut response = ProblemDetails::new(401, "unauthorized", "Unauthorized")
        .with_detail("this endpoint needs an access token; its authorization server is named by the resource metadata")
        .into_response();
    let shaped = (1..=128).contains(&slug.len())
        && slug
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
    if !shaped {
        return response;
    }
    let metadata = format!(
        "{}/api/endpoint/{slug}/.well-known/oauth-protected-resource",
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

#[cfg(test)]
mod tests {
    use super::*;

    /// T-5045: `initialize` names the WebSocket where a model reads first; an answer without
    /// instructions is left as it came.
    #[test]
    fn initialize_names_the_live_updates_websocket() {
        let mut answer = serde_json::json!({ "result": { "instructions": "NGSI-LD." } });
        live_instructions(&mut answer, "wss://data.example.fi/api/endpoint/s/ws");
        let said = answer["result"]["instructions"]
            .as_str()
            .unwrap_or_default();
        assert!(
            said.starts_with("NGSI-LD. Live updates: open wss://data.example.fi/api/endpoint/s/ws")
        );
        assert!(
            said.contains("ngsi-ld-ws") && said.contains("liveUpdates"),
            "{said}"
        );

        let mut bare = serde_json::json!({ "result": { "tools": [] } });
        live_instructions(&mut bare, "wss://x/ws");
        assert_eq!(bare, serde_json::json!({ "result": { "tools": [] } }));
    }

    /// T-3819: what the broker answers is not trusted to be the shape MCP says; an answer that
    /// is no object, or whose parts are not, is left as it came and never panics the request.
    #[test]
    fn a_federated_answer_of_any_shape_is_decorated_or_left_alone() {
        let members = ["transport".to_owned()];
        for method in FEDERATED_METHODS {
            for shape in [
                serde_json::json!([1, 2]),
                serde_json::json!("text"),
                serde_json::json!({ "result": [1] }),
                serde_json::json!({ "result": { "tools": [1, "x"], "structuredContent": [] } }),
                serde_json::json!({ "result": { "tools": { "a": 1 }, "instructions": 3 } }),
            ] {
                let mut answer = shape.clone();
                federated(&mut answer, method, &members);
                assert_eq!(answer, shape, "{method}");
            }
        }
        let mut answer = serde_json::json!({ "result": {
            "isError": false,
            "structuredContent": { "entities": [] },
        }});
        federated(&mut answer, "tools/call", &members);
        assert_eq!(
            answer["result"]["structuredContent"]["jc:source"],
            serde_json::json!(["transport"])
        );
    }
}
