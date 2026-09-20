//! Data route forwarding to Context Gateway (/v1/data/*).

use crate::audit::{log_request, AuditEntry};
use crate::auth::authenticate;
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
    let run = match authenticate(&headers, &state.runs, &state.config).await {
        Ok(r) => r,
        Err(p) => return (*p).into_response(),
    };

    if super::escapes(&rest) {
        return jc_core::ProblemDetails::forbidden()
            .with_detail("path traversal not permitted")
            .into_response();
    }

    // A slug the cached run does not name is checked once more against the Portal's record: the
    // assistant adds an endpoint to a conversation and calls it at once (AG-75).
    let run = match &slug {
        Some(slug) if !run.slugs().contains(&slug.as_str()) => {
            match state.runs.resolve_fresh(&run.id).await {
                Ok(fresh) => fresh,
                Err(_) => run,
            }
        }
        _ => run,
    };
    let slug = match slug {
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

    let token = match state.credentials.get_endpoint_token(&slug).await {
        Ok(t) => t,
        Err(error) => {
            // The credential manager's message names the realm, the client and the token URL.
            // A run learns that the proxy has no token for the endpoint and nothing else, the
            // same answer `portal_bearer` gives for the Portal (AG-52, T-2271).
            tracing::warn!(%error, slug = %slug, "no token for the endpoint");
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

    // If MCP tools/call on read-only run, inspect for mutation tools
    if rest == "mcp" && !run.allows_write {
        if let Ok(v) = serde_json::from_slice::<serde_json::Value>(&body_bytes) {
            if v.get("method").and_then(|m| m.as_str()) == Some("tools/call") {
                if let Some(tool) = v
                    .get("params")
                    .and_then(|p| p.get("name"))
                    .and_then(|n| n.as_str())
                {
                    if matches!(tool, "upsert_entity" | "create_subscription") {
                        return jc_core::ProblemDetails::forbidden()
                            .with_detail("mutation tools not permitted on read-only run")
                            .into_response();
                    }
                }
            }
        }
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
    let mut resp_builder = Response::builder().status(status);

    for (k, v) in upstream_resp.headers().iter() {
        if k.as_str().to_lowercase() != "set-cookie" {
            resp_builder = resp_builder.header(k, v);
        }
    }

    let resp_bytes = upstream_resp.bytes().await.unwrap_or_default();
    log_request(&AuditEntry {
        run_id: &run.id,
        user: &run.created_by,
        upstream: "context-gateway",
        method: method.as_str(),
        path: &audit_path,
        status: status.as_u16(),
        bytes: resp_bytes.len(),
        duration_ms: start.elapsed().as_millis(),
    });

    resp_builder
        .body(Body::from(resp_bytes))
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
}
