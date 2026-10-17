//! The routes of the endpoint and the space `schema/` child (EP-46, SP-04): the index and one
//! artifact, read from the broker's LinkML surface of the Endpoint's view (plan 5.4, T-3645):
//! `{prefix}/schema{rest}` goes to `/x/views/{view}/linkml/schema{rest}` with the tenant and the
//! verified caller pinned, and the broker narrows every document to what the caller may see
//! (LK1). Nothing is rendered here (T-3646): an Endpoint no view serves has no schema child.

use crate::app::admit;
use crate::app::admit_space;
use crate::app::sha256_hex;
use crate::app::Gateway;
use crate::app::{broker_failure, has_view, principal_of, with_platform_slug};
use crate::middleware::tenancy;
use crate::pdp::evaluator::Subject;
use crate::resolver::Endpoint;
use axum::body::Body;
use axum::extract::{Path, Request, State};
use axum::http::{HeaderMap, HeaderValue, Response, StatusCode};
use axum::response::IntoResponse;
use jc_core::ProblemDetails;
use serde_json::Value;
use std::sync::Arc;

/// The catalogue of what this endpoint publishes about its data (T-0162, EP-46), linking the
/// Mappings index of every major that publishes one (EP-53).
pub(crate) async fn schema_index(
    State(gateway): State<Arc<Gateway>>,
    Path(slug): Path<String>,
    request: Request,
) -> Response<Body> {
    let mut response = endpoint_schema(&gateway, &slug, request).await;
    if response.status().is_success() || response.status() == StatusCode::NOT_MODIFIED {
        let majors: std::collections::BTreeSet<u32> = gateway
            .mappings(&slug)
            .iter()
            .map(|mapping| mapping.major)
            .collect();
        for major in majors {
            let link =
                format!("</api/endpoint/{slug}/schema/v{major}/mappings/>; rel=\"{MAPPINGS_REL}\"");
            if let Ok(value) = HeaderValue::from_str(&link) {
                response
                    .headers_mut()
                    .append(axum::http::header::LINK, value);
            }
        }
    }
    response
}

/// The relation of a schema index to a major's Mappings index (API/02 §7a).
pub const MAPPINGS_REL: &str = "https://joinedcontext.com/rel/mappings";

/// The same catalogue under the space prefix, which SP-04 makes a child of every space.
pub(crate) async fn space_schema_index(
    State(gateway): State<Arc<Gateway>>,
    Path(name): Path<String>,
    request: Request,
) -> Response<Body> {
    space_schema(&gateway, &name, request).await
}

/// One schema document of one major version, projected to the grant (T-0162, EP-47, EP-49).
pub(crate) async fn schema_artifact(
    State(gateway): State<Arc<Gateway>>,
    Path((slug, _, _)): Path<(String, String, String)>,
    request: Request,
) -> Response<Body> {
    endpoint_schema(&gateway, &slug, request).await
}

/// The same document under the space prefix (SP-03, SP-04).
pub(crate) async fn space_schema_artifact(
    State(gateway): State<Arc<Gateway>>,
    Path((name, _, _)): Path<(String, String, String)>,
    request: Request,
) -> Response<Body> {
    space_schema(&gateway, &name, request).await
}

async fn endpoint_schema(gateway: &Gateway, slug: &str, mut request: Request) -> Response<Body> {
    tenancy::strip_client_headers(&mut request);
    let (endpoint, subject) = match admit(gateway, slug, None, request.headers()) {
        Ok(admitted) => admitted,
        Err(problem) => return *problem,
    };
    let view = endpoint.slug.clone();
    through_view(gateway, &endpoint, &subject, &view, request).await
}

async fn space_schema(gateway: &Gateway, name: &str, mut request: Request) -> Response<Body> {
    tenancy::strip_client_headers(&mut request);
    let (space, subject) = match admit_space(gateway, name, None, request.headers()) {
        Ok(admitted) => admitted,
        Err(problem) => return *problem,
    };
    // The space surface's view is the one the Portal writes per Context Space (plan 5.1).
    let view = format!("cs-{}", space.endpoint.space);
    through_view(gateway, &space.endpoint, &subject, &view, request).await
}

/// One `schema/` request through the broker's LinkML surface of `view` (plan 5.4, T-3645).
///
/// The path after `schema` goes on as the caller sent it, still percent-encoded, with `Accept`
/// and `If-None-Match`, so the broker's formalism negotiation and its `304` hold. A hidden
/// class, slot, model or cube answers the gateway's own `404`, the body of a name that does
/// not exist (LK8), whatever the broker's wording, and so does an Endpoint no view serves
/// (a slug no view id can hold): there is no second renderer to fall back to (T-3646).
async fn through_view(
    gateway: &Gateway,
    endpoint: &Endpoint,
    subject: &Subject,
    view: &str,
    mut request: Request,
) -> Response<Body> {
    if !has_view(view) {
        return ProblemDetails::not_found().into_response();
    }
    // The known prefix, not the first `/schema/`: a space or slug named `schema` holds one too.
    let prefix = format!("{}/schema", endpoint.base_path);
    let Some(rest) = request
        .uri()
        .path()
        .strip_prefix(&prefix)
        .filter(|rest| rest.starts_with('/'))
    else {
        return ProblemDetails::not_found().into_response();
    };
    let target = format!("/x/views/{view}/linkml/schema{rest}");
    if let Err(problem) = tenancy::pin_caller(&mut request, &endpoint.space, subject) {
        tracing::error!(space = %endpoint.space, %problem, "endpoint names an unusable space");
        return ProblemDetails::internal().into_response();
    }
    tracing::info!(
        slug = %endpoint.slug,
        space = %endpoint.space,
        %view,
        principal = %principal_of(subject),
        "schema request forwarded to the broker's view"
    );
    let (parts, _) = request.into_parts();
    let answer = match gateway
        .broker
        .send(parts.method, &target, parts.headers, Body::empty())
        .await
    {
        Ok(answer) => answer,
        Err(error) => return ProblemDetails::from(error).into_response(),
    };
    match answer.status() {
        status if status.is_server_error() => {
            let (parts, body) = answer.into_parts();
            broker_failure(parts.status, body).await
        }
        StatusCode::FORBIDDEN => with_platform_slug(answer).await,
        StatusCode::NOT_FOUND => ProblemDetails::not_found().into_response(),
        _ => answer,
    }
}

/// One document of the broker's LinkML surface of `view`, `rest` the path under `linkml/`,
/// read as `subject` reads it (T-3645): the record, the catalogue feed, the bundle and the MCP
/// schema tool name the same bytes and digests the `schema/` routes serve (EP-46, EP-68).
/// `None` when no view serves the Endpoint; the broker's status when it answers anything but
/// `200`.
pub(crate) async fn view_document(
    gateway: &Gateway,
    endpoint: &Endpoint,
    view: &str,
    subject: &Subject,
    rest: &str,
) -> Option<Result<Vec<u8>, StatusCode>> {
    if !has_view(view) {
        return None;
    }
    let mut request = Request::new(Body::empty());
    if tenancy::pin_caller(&mut request, &endpoint.space, subject).is_err() {
        return Some(Err(StatusCode::INTERNAL_SERVER_ERROR));
    }
    let target = format!("/x/views/{view}/linkml/{rest}");
    let answer = match gateway
        .broker
        .send(
            axum::http::Method::GET,
            &target,
            request.headers().clone(),
            Body::empty(),
        )
        .await
    {
        Ok(answer) => answer,
        Err(_) => return Some(Err(StatusCode::BAD_GATEWAY)),
    };
    if answer.status() != StatusCode::OK {
        return Some(Err(answer.status()));
    }
    Some(
        axum::body::to_bytes(answer.into_body(), crate::app::MAX_BODY)
            .await
            .map(|bytes| bytes.to_vec())
            .map_err(|_| StatusCode::BAD_GATEWAY),
    )
}

/// The schema catalogue of `view` as `subject` reads it, the broker's index. A broker that does
/// not answer, or an Endpoint no view serves, gives a catalogue of no model rather than digests
/// of bytes nobody can fetch (EP-68).
pub(crate) async fn index_for(
    gateway: &Gateway,
    endpoint: &Endpoint,
    view: &str,
    subject: &Subject,
) -> Value {
    let empty = || serde_json::json!({ "endpoint": endpoint.slug, "models": [] });
    match view_document(gateway, endpoint, view, subject, "schema/index.json").await {
        Some(Ok(bytes)) => serde_json::from_slice(&bytes).unwrap_or_else(|_| empty()),
        Some(Err(status)) => {
            tracing::warn!(slug = %endpoint.slug, %status, "the broker's schema index is unavailable");
            empty()
        }
        None => empty(),
    }
}

/// The major of an index's model: a number, or the digits the broker wrote as text before
/// T-3963.
pub(crate) fn major_of(model: &Value) -> Option<u64> {
    let version = model.get("version")?;
    version
        .as_u64()
        .or_else(|| version.as_str().and_then(|text| text.parse().ok()))
}

/// A schema document with the strong `ETag` a client revalidates against (EP-51).
///
/// The document is a projection of the policy set, so it is never immutable: a grant that
/// changes changes the schema, and a client holding a stale copy has to find out. What it
/// gets instead is a digest of exactly the bytes it holds, and a 304 whenever they still
/// match.
pub(crate) fn revalidated(
    document: &Value,
    media_type: &str,
    headers: &HeaderMap,
) -> Response<Body> {
    let Ok(bytes) = serde_json::to_vec(document) else {
        tracing::error!("a schema document does not serialize");
        return ProblemDetails::internal().into_response();
    };
    revalidated_text(bytes, media_type, headers)
}

/// The same, for a document that is already the bytes the caller receives (T-0284).
pub(crate) fn revalidated_text(
    bytes: Vec<u8>,
    media_type: &str,
    headers: &HeaderMap,
) -> Response<Body> {
    let etag = format!("\"{}\"", sha256_hex(&bytes));

    let mut response = if matches_etag(headers, &etag) {
        StatusCode::NOT_MODIFIED.into_response()
    } else {
        match HeaderValue::from_str(media_type) {
            Ok(content_type) => (
                [(axum::http::header::CONTENT_TYPE, content_type)],
                Body::from(bytes),
            )
                .into_response(),
            Err(_) => {
                tracing::error!(media_type, "a schema media type is not a header value");
                return ProblemDetails::internal().into_response();
            }
        }
    };
    let headers = response.headers_mut();
    if let Ok(value) = HeaderValue::from_str(&etag) {
        headers.insert(axum::http::header::ETAG, value);
    }
    headers.insert(
        axum::http::header::CACHE_CONTROL,
        HeaderValue::from_static("no-cache"),
    );
    response
}

/// Whether `If-None-Match` names the document the gateway just built (RFC 9110 13.1.2).
fn matches_etag(headers: &HeaderMap, etag: &str) -> bool {
    let Some(presented) = headers
        .get(axum::http::header::IF_NONE_MATCH)
        .and_then(|value| value.to_str().ok())
    else {
        return false;
    };
    presented
        .split(',')
        .map(str::trim)
        .any(|candidate| candidate == "*" || candidate.trim_start_matches("W/") == etag)
}
