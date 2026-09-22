//! Git forge route mediation for Gitea (/v1/forge/*).

use crate::audit::{log_request, AuditEntry};
use crate::auth::authenticate;
use crate::ProxyState;
use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, Method, Request, StatusCode};
use axum::response::{IntoResponse, Response};
use std::time::Instant;

/// The methods a `contents/` write uses: Gitea creates with POST, updates with PUT and removes
/// with DELETE, and all three take `branch`, `author`, `committer` and `message`.
const CONTENT_WRITES: [Method; 3] = [Method::POST, Method::PUT, Method::DELETE];

/// The request body as a JSON object, or the refusal to send back (AG-64, T-2362).
///
/// Four forge operations do not forward the body as it arrived: a `contents/` write gains the
/// run's branch, its author, its committer and the `Co-Proposed-By` trailer, a new branch is
/// checked against the run's own, and a pull request has its `head` forced to it. A body this
/// proxy cannot read as an object is a body it cannot pin to the run, so it stops here rather
/// than reaching the forge with the platform's token and no branch at all, on whichever ref
/// Gitea calls default. An empty body is an empty object: a `DELETE` that carries everything in
/// its query is still pinned, because the fields are inserted into what it then has.
fn object_body(bytes: &[u8]) -> Result<serde_json::Map<String, serde_json::Value>, Box<Response>> {
    if bytes.iter().all(u8::is_ascii_whitespace) {
        return Ok(serde_json::Map::new());
    }
    match serde_json::from_slice::<serde_json::Value>(bytes) {
        Ok(serde_json::Value::Object(map)) => Ok(map),
        Ok(_) => Err(Box::new(
            jc_core::ProblemDetails::new(400, "invalid-body", "Bad Request")
                .with_detail("this forge operation takes a JSON object as its body")
                .into_response(),
        )),
        Err(error) => Err(Box::new(
            jc_core::ProblemDetails::new(400, "invalid-body", "Bad Request")
                .with_detail(format!("the body is not JSON: {error}"))
                .into_response(),
        )),
    }
}

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

    // The body is only read where an operation rewrites or checks it; a read forwards what
    // arrived, untouched.
    let mut pinned: Option<serde_json::Map<String, serde_json::Value>> = None;

    if let Some(file_path) = rest.strip_prefix("contents/") {
        // Nothing encoded, no backslash, no dot or empty segment (T-0817, `super::escapes`).
        if super::escapes(file_path) || !file_path.starts_with(&run.path_prefix) {
            return jc_core::ProblemDetails::forbidden()
                .with_detail("file path outside assigned application directory")
                .into_response();
        }
        // A workflow is the Portal's, never a run's: a run that wrote one could rewrite what the
        // forge runs and reach the lane's secret before anybody reviewed it (AP-100, ADR-N-028).
        if file_path.split('/').any(|segment| segment == ".gitea") {
            return jc_core::ProblemDetails::forbidden()
                .with_detail("a run never writes under .gitea/: the build is the Portal's (AP-100)")
                .into_response();
        }
        if CONTENT_WRITES.contains(&method) {
            let mut map = match object_body(&body_bytes) {
                Ok(map) => map,
                Err(refusal) => return *refusal,
            };
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
                let message = format!("{msg}{trailer}");
                map.insert("message".to_string(), serde_json::json!(message));
            }
            pinned = Some(map);
        }
    } else if rest == "branches" && method == Method::POST {
        let map = match object_body(&body_bytes) {
            Ok(map) => map,
            Err(refusal) => return *refusal,
        };
        if map.get("new_branch_name").and_then(|n| n.as_str()) != Some(run.branch.as_str()) {
            return jc_core::ProblemDetails::forbidden()
                .with_detail("cannot create branches other than run assigned branch")
                .into_response();
        }
        pinned = Some(map);
    } else if rest == "pulls" && method == Method::POST {
        let mut map = match object_body(&body_bytes) {
            Ok(map) => map,
            Err(refusal) => return *refusal,
        };
        map.insert("head".to_string(), serde_json::json!(run.branch));
        pinned = Some(map);
    } else if (rest.starts_with("pulls/") || rest.starts_with("commits/")) && method == Method::GET
    {
        // Read-only inspection allowed
    } else {
        return jc_core::ProblemDetails::forbidden()
            .with_detail("forge operation not permitted")
            .into_response();
    }

    // The run's one repository: its project's own in layout 2, the configuration repository
    // otherwise. Never a name the request carries, so a run reaches no other (AG-86).
    let repository = match &run.repository {
        Some(name) => {
            if name.is_empty() || name.contains(['/', '.', '%', '\\']) {
                return jc_core::ProblemDetails::forbidden()
                    .with_detail("the run names no repository this door reaches")
                    .into_response();
            }
            let owner = state
                .config
                .forge_repo
                .split_once('/')
                .map_or(state.config.forge_repo.as_str(), |(owner, _)| owner);
            format!("{owner}/{name}")
        }
        None => state.config.forge_repo.clone(),
    };
    let target_url = format!(
        "{}/api/v1/repos/{}/{}",
        state.config.forge_base.as_str().trim_end_matches('/'),
        repository,
        rest.trim_start_matches('/')
    );
    // The URL the forge will see, after its parser had its say, is inside the application
    // directory or the request stops here (T-0817).
    if rest.starts_with("contents/") {
        let inside = format!("/api/v1/repos/{repository}/contents/{}", run.path_prefix);
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

    if let Some(map) = pinned {
        client_req = client_req.json(&serde_json::Value::Object(map));
    } else if !body_bytes.is_empty() {
        client_req = client_req.body(body_bytes);
    }

    let upstream_resp = match client_req.send().await {
        Ok(r) => r,
        Err(e) => return super::upstream_unavailable(super::FORGE, &e),
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
