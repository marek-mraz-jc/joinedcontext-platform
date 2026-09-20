//! Git forge route mediation for Gitea (/v1/forge/*).

use crate::audit::{log_request, AuditEntry};
use crate::auth::authenticate;
use crate::ProxyState;
use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, Method, Request, StatusCode};
use axum::response::{IntoResponse, Response};
use std::time::Instant;

pub async fn handler(
    State(state): State<ProxyState>,
    method: Method,
    headers: HeaderMap,
    Path(rest): Path<String>,
    req: Request<Body>,
) -> Response {
    let start = Instant::now();
    let run = match authenticate(&headers, &state.runs, &state.config).await {
        Ok(r) => r,
        Err(p) => return (*p).into_response(),
    };

    let body_bytes = match super::body::bounded(req.into_body()).await {
        Ok(bytes) => bytes,
        Err(refusal) => return *refusal,
    };

    let mut body_val = serde_json::from_slice::<serde_json::Value>(&body_bytes).ok();

    if let Some(file_path) = rest.strip_prefix("contents/") {
        // Nothing encoded, no backslash, no dot or empty segment (T-0817, `super::escapes`).
        if super::escapes(file_path) || !file_path.starts_with(&run.path_prefix) {
            return jc_core::ProblemDetails::forbidden()
                .with_detail("file path outside assigned application directory")
                .into_response();
        }
        if matches!(method, Method::PUT | Method::DELETE) {
            if let Some(ref mut obj) = body_val {
                if let Some(map) = obj.as_object_mut() {
                    map.insert("branch".to_string(), serde_json::json!(run.branch));
                    map.insert(
                        "author".to_string(),
                        serde_json::json!({
                            "name": format!("agent:app-builder@{}", run.project),
                            "email": format!("agent-builder@{}.local", run.project)
                        }),
                    );
                    map.insert(
                        "committer".to_string(),
                        serde_json::json!({
                            "name": format!("agent:app-builder@{}", run.project),
                            "email": format!("agent-builder@{}.local", run.project)
                        }),
                    );
                    let msg = map
                        .get("message")
                        .and_then(|m| m.as_str())
                        .unwrap_or("agent commit");
                    let trailer = format!("\n\nCo-Proposed-By: {}", run.created_by);
                    if !msg.contains("Co-Proposed-By:") {
                        map.insert(
                            "message".to_string(),
                            serde_json::json!(format!("{msg}{trailer}")),
                        );
                    }
                }
            }
        }
    } else if rest == "branches" && method == Method::POST {
        if let Some(ref map) = body_val {
            if map.get("new_branch_name").and_then(|n| n.as_str()) != Some(&run.branch) {
                return jc_core::ProblemDetails::forbidden()
                    .with_detail("cannot create branches other than run assigned branch")
                    .into_response();
            }
        }
    } else if rest == "pulls" && method == Method::POST {
        if let Some(ref mut map) = body_val {
            if let Some(obj) = map.as_object_mut() {
                obj.insert("head".to_string(), serde_json::json!(run.branch));
            }
        }
    } else if (rest.starts_with("pulls/") || rest.starts_with("commits/")) && method == Method::GET
    {
        // Read-only inspection allowed
    } else {
        return jc_core::ProblemDetails::forbidden()
            .with_detail("forge operation not permitted")
            .into_response();
    }

    let target_url = format!(
        "{}/api/v1/repos/{}/{}",
        state.config.forge_base.as_str().trim_end_matches('/'),
        state.config.forge_repo,
        rest.trim_start_matches('/')
    );
    // The URL the forge will see, after its parser had its say, is inside the application
    // directory or the request stops here (T-0817).
    if rest.starts_with("contents/") {
        let inside = format!(
            "/api/v1/repos/{}/contents/{}",
            state.config.forge_repo, run.path_prefix
        );
        if !reqwest::Url::parse(&target_url).is_ok_and(|url| url.path().starts_with(&inside)) {
            return jc_core::ProblemDetails::forbidden()
                .with_detail("file path outside assigned application directory")
                .into_response();
        }
    }

    let forge_token = state.credentials.get_forge_token();
    let mut client_req = state
        .http
        .request(method.clone(), &target_url)
        .header("Authorization", format!("token {forge_token}"))
        .header("Accept", "application/json");

    if let Some(val) = body_val {
        client_req = client_req.json(&val);
    } else if !body_bytes.is_empty() {
        client_req = client_req.body(body_bytes);
    }

    let upstream_resp = match client_req.send().await {
        Ok(r) => r,
        Err(e) => return jc_core::ProblemDetails::internal_opaque(&e.to_string()).into_response(),
    };

    let status =
        StatusCode::from_u16(upstream_resp.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    if let Some(refusal) =
        super::refused_redirect(status, super::location_of(&upstream_resp), "forge")
    {
        return refusal;
    }
    let resp_bytes = upstream_resp.bytes().await.unwrap_or_default();

    log_request(&AuditEntry {
        run_id: &run.id,
        user: &run.created_by,
        upstream: "gitea",
        method: method.as_str(),
        path: &rest,
        status: status.as_u16(),
        bytes: resp_bytes.len(),
        duration_ms: start.elapsed().as_millis(),
    });

    Response::builder()
        .status(status)
        .header("Content-Type", "application/json")
        .body(Body::from(resp_bytes))
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
}
