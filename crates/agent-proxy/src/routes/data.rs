//! Data route forwarding to Context Gateway (/v1/data/*).

use crate::audit::{log_request, AuditEntry};
use crate::auth::authenticate_data;
use crate::delegation::GrantError;
use crate::ProxyState;
use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, Method, Request, StatusCode};
use axum::response::{IntoResponse, Response};
use std::time::Instant;

const FORBIDDEN_CLIENT_HEADERS: &[&str] = &[
    "authorization",
    "cookie",
    "ngsild-tenant",
    "x-userinfo",
    "x-access-token",
    "x-allowed-scope-ids",
    "x-endpoint-slug",
    "x-consumer-identity",
];

/// The MCP tools that only read: the gateway's and the broker's (Antares `docs/src/mcp.md`).
/// Named rather than the writes, so a write tool under a name nobody listed is refused, not
/// passed (T-3806).
const READ_TOOLS: &[&str] = &[
    "batch_query",
    "batch_query_temporal",
    "describe_access",
    "describe_schema",
    "get_attribute",
    "get_attribute_value",
    "get_entity",
    "get_subscription",
    "get_type",
    "list_attribute_details",
    "list_attributes",
    "list_subscriptions",
    "list_type_details",
    "list_types",
    "query_entities",
    "query_temporal",
    "retrieve_temporal",
];

/// Whether an MCP body, one message or a JSON-RPC batch, calls no tool but a read one. A body
/// that does not parse reads nothing the proxy can vouch for.
fn reads_only(body: &[u8]) -> bool {
    let reads = |message: &serde_json::Value| {
        message.get("method").and_then(|m| m.as_str()) != Some("tools/call")
            || message
                .get("params")
                .and_then(|p| p.get("name"))
                .and_then(|n| n.as_str())
                .is_some_and(|tool| READ_TOOLS.contains(&tool))
    };
    match serde_json::from_slice::<serde_json::Value>(body) {
        Ok(serde_json::Value::Array(messages)) => messages.iter().all(reads),
        Ok(message @ serde_json::Value::Object(_)) => reads(&message),
        _ => false,
    }
}

/// `/v1/data/{*rest}`: the run's primary endpoint.
pub async fn handler(
    State(state): State<ProxyState>,
    method: Method,
    headers: HeaderMap,
    Path(rest): Path<String>,
    req: Request<Body>,
) -> Response {
    forward(state, method, headers, None, rest, req).await
}

/// `/v1/data/endpoints/{slug}/{*rest}`: any endpoint of the run by its slug, with a token for
/// that endpoint and no other (AP-44, AG-75). A slug the run does not name is refused before a
/// token is asked for.
pub async fn endpoint_handler(
    State(state): State<ProxyState>,
    method: Method,
    headers: HeaderMap,
    Path((slug, rest)): Path<(String, String)>,
    req: Request<Body>,
) -> Response {
    forward(state, method, headers, Some(slug), rest, req).await
}

async fn forward(
    state: ProxyState,
    method: Method,
    headers: HeaderMap,
    slug: Option<String>,
    rest: String,
    req: Request<Body>,
) -> Response {
    let start = Instant::now();
    let run = match authenticate_data(
        &headers,
        &state.runs,
        &state.config,
        &state.data_credentials,
    )
    .await
    {
        Ok(r) => r,
        Err(p) => return (*p).into_response(),
    };

    // Before the Portal is asked about a stale slug, so a loop of unknown slugs is counted too
    // (F-R2PLT1-004).
    if let Some(refusal) = super::over_rate(&state, &run).await {
        return refusal;
    }

    if super::escapes(&rest) {
        return jc_core::ProblemDetails::forbidden()
            .with_detail("path traversal not permitted")
            .into_response();
    }

    // A slug the cached run does not name is checked once more against the Portal's record: the
    // assistant adds an endpoint to a conversation and calls it at once (AG-75). So is a run
    // cached with no primary yet: a conversation starts with none, the identity hand-over caches
    // it that way, and the first endpoint the assistant opens is its primary (T-3044).
    let stale = match &slug {
        Some(slug) => !run.slugs().contains(&slug.as_str()),
        None => run.endpoint_slug.is_empty(),
    };
    let run = if stale {
        state.runs.resolve_fresh(&run.id).await.unwrap_or(run)
    } else {
        run
    };
    let slug = match slug {
        None if run.endpoint_slug.is_empty() => {
            return jc_core::ProblemDetails::conflict()
                .with_detail("this run reads no endpoint yet: open one in the conversation first")
                .into_response()
        }
        None => run.endpoint_slug.clone(),
        Some(slug) if run.slugs().contains(&slug.as_str()) => slug,
        Some(slug) => {
            return jc_core::ProblemDetails::forbidden()
                .with_detail(format!("endpoint '{slug}' is not an endpoint of this run"))
                .into_response()
        }
    };

    if !run.allows_write {
        if matches!(method, Method::PATCH | Method::PUT | Method::DELETE) {
            return jc_core::ProblemDetails::forbidden()
                .with_detail("write operations not permitted on read-only run")
                .into_response();
        }
        if method == Method::POST
            && rest != "mcp"
            && !rest.ends_with("/query")
            && rest != "entityOperations/query"
        {
            return jc_core::ProblemDetails::forbidden()
                .with_detail("mutating POST requests not permitted on read-only run")
                .into_response();
        }
    }

    let audit_path = if slug == run.endpoint_slug {
        rest.clone()
    } else {
        format!("endpoints/{slug}/{rest}")
    };

    // The run reads as the person who started it, with their exchanged token, and with nothing
    // else: no grant is a 401 and never the proxy's own token (ADR-N-038 §3.3, AG-94).
    let token = match state.credentials.get_run_token(&run.id).await {
        Ok(t) => t,
        Err(error @ (GrantError::Missing | GrantError::Refused(_))) => {
            tracing::info!(run = %run.id, %error, "a data call of a run without its person's grant");
            return jc_core::ProblemDetails::unauthorized()
                .with_detail(match error {
                    GrantError::Missing => "this run holds no identity of the person who started it: it was not started from a signed-in session, or its grant has ended (ADR-N-038)",
                    _ => "the session of the person who started this run has ended, and so has the run's access to data (ADR-N-038)",
                })
                .into_response();
        }
        Err(error) => {
            // The message names the realm. A run learns that the proxy has no token and nothing
            // else, the same answer `portal_bearer` gives for the Portal (AG-52, T-2271).
            tracing::warn!(%error, slug = %slug, "no token for the run");
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                jc_core::ProblemDetails::new(503, "upstream-unavailable", "Upstream Unavailable")
                    .with_detail("the proxy could not obtain a token for this endpoint"),
            )
                .into_response();
        }
    };

    // The query string travels with the path: `type`, `attrs`, `q`, `limit`, `offset` and
    // `options=keyValues` are the read, not decoration on it. Without them every page is the
    // first page and every entity is normalized.
    let target_url = format!(
        "{}/api/endpoint/{}/{}{}",
        state.config.gateway_base.as_str().trim_end_matches('/'),
        slug,
        rest.trim_start_matches('/'),
        req.uri()
            .query()
            .map(|query| format!("?{query}"))
            .unwrap_or_default()
    );

    let body_bytes = match super::body::bounded(req.into_body()).await {
        Ok(bytes) => bytes,
        Err(refusal) => return *refusal,
    };

    // AG-25: a read-only run's MCP body passes only when every message in it reads.
    if rest == "mcp" && !run.allows_write && !reads_only(&body_bytes) {
        return jc_core::ProblemDetails::forbidden()
            .with_detail("mutation tools not permitted on read-only run")
            .into_response();
    }

    let mut client_req = state.http.request(method.clone(), &target_url);
    for (k, v) in headers.iter() {
        let name = k.as_str().to_lowercase();
        if !FORBIDDEN_CLIENT_HEADERS.contains(&name.as_str())
            && !name.starts_with("x-jc-")
            && name != "host"
        {
            client_req = client_req.header(k, v);
        }
    }
    client_req = client_req.bearer_auth(token).body(body_bytes);

    let upstream_resp = match client_req.send().await {
        Ok(r) => r,
        Err(e) => return super::upstream_unavailable(super::GATEWAY, &e),
    };

    let status =
        StatusCode::from_u16(upstream_resp.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    if let Some(refusal) = super::refused_redirect(
        status,
        super::location_of(&upstream_resp),
        "context gateway",
    ) {
        return refusal;
    }
    let mut resp_builder = Response::builder().status(status);

    for (k, v) in upstream_resp.headers().iter() {
        if k.as_str().to_lowercase() != "set-cookie" {
            resp_builder = resp_builder.header(k, v);
        }
    }

    // Past the cap the run gets the refusal, and the audit line says 502 like any other outage.
    let (resp_bytes, refusal) = match super::upstream_body(
        super::GATEWAY,
        upstream_resp,
        super::GATEWAY_ANSWER_BYTES,
    )
    .await
    {
        Ok(bytes) => (bytes, None),
        Err(refusal) => (Vec::new(), Some(*refusal)),
    };
    log_request(&AuditEntry {
        run_id: &run.id,
        user: &run.created_by,
        upstream: "context-gateway",
        method: method.as_str(),
        path: &audit_path,
        status: if refusal.is_some() {
            502
        } else {
            status.as_u16()
        },
        bytes: resp_bytes.len(),
        duration_ms: start.elapsed().as_millis(),
    });
    if let Some(refusal) = refusal {
        return refusal;
    }

    resp_builder
        .body(Body::from(resp_bytes))
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
}
