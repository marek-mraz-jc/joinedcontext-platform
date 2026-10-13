//! The routes of the endpoint and the space `schema/` child (EP-46, SP-04): the index and one
//! artifact, revalidated by ETag.
//!
//! An Endpoint the broker has a view of is answered by the broker's LinkML surface of that view
//! (plan 5.4, T-3645): `{prefix}/schema{rest}` goes to `/x/views/{view}/linkml/schema{rest}`
//! with the tenant and the verified caller pinned, and the broker narrows every document to
//! what the caller may see (LK1). The rest is still rendered here until T-3646.

use crate::app::admit;
use crate::app::admit_space;
use crate::app::sha256_hex;
use crate::app::Gateway;
use crate::app::{broker_failure, has_view, principal_of, with_platform_slug};
use crate::handlers::schema;
use crate::middleware::tenancy;
use crate::pdp::evaluator::Subject;
use crate::resolver::{Endpoint, Model};
use axum::body::Body;
use axum::extract::{Path, Request, State};
use axum::http::{HeaderMap, HeaderValue, Response, StatusCode};
use axum::response::IntoResponse;
use jc_core::ProblemDetails;
use serde_json::Value;
use std::sync::Arc;

/// The catalogue of what this endpoint publishes about its data (T-0162, EP-46).
pub(crate) async fn schema_index(
    State(gateway): State<Arc<Gateway>>,
    Path(slug): Path<String>,
    mut request: Request,
) -> Response<Body> {
    tenancy::strip_client_headers(&mut request);
    let (endpoint, subject) = match admit(&gateway, &slug, None, request.headers()) {
        Ok(admitted) => admitted,
        Err(problem) => return *problem,
    };
    let view = endpoint.slug.clone();
    match through_view(&gateway, &endpoint, &subject, &view, request).await {
        Ok(answer) => answer,
        Err(request) => schema_index_of(&endpoint, &subject, request.headers()),
    }
}

/// The same catalogue under the space prefix, which SP-04 makes a child of every space.
pub(crate) async fn space_schema_index(
    State(gateway): State<Arc<Gateway>>,
    Path(name): Path<String>,
    mut request: Request,
) -> Response<Body> {
    tenancy::strip_client_headers(&mut request);
    let (space, subject) = match admit_space(&gateway, &name, None, request.headers()) {
        Ok(admitted) => admitted,
        Err(problem) => return *problem,
    };
    // The space surface's view is the one the Portal writes per Context Space (plan 5.1).
    let view = format!("cs-{}", space.endpoint.space);
    match through_view(&gateway, &space.endpoint, &subject, &view, request).await {
        Ok(answer) => answer,
        Err(request) => schema_index_of(&space.endpoint, &subject, request.headers()),
    }
}

/// One catalogue, whichever prefix the caller came in by (SP-03, EP-46).
fn schema_index_of(endpoint: &Endpoint, subject: &Subject, headers: &HeaderMap) -> Response<Body> {
    let visible = schema::visible(subject, endpoint, crate::pdp::now());
    let document = schema::index(endpoint, &visible, sha256_hex);
    revalidated(&document, "application/json", headers)
}

/// One schema document of one major version, projected to the grant (T-0162, EP-47, EP-49).
pub(crate) async fn schema_artifact(
    State(gateway): State<Arc<Gateway>>,
    Path((slug, version, artifact)): Path<(String, String, String)>,
    mut request: Request,
) -> Response<Body> {
    tenancy::strip_client_headers(&mut request);
    let (endpoint, subject) = match admit(&gateway, &slug, None, request.headers()) {
        Ok(admitted) => admitted,
        Err(problem) => return *problem,
    };
    let view = endpoint.slug.clone();
    match through_view(&gateway, &endpoint, &subject, &view, request).await {
        Ok(answer) => answer,
        Err(request) => {
            schema_artifact_of(&endpoint, &subject, &version, &artifact, request.headers())
        }
    }
}

/// The same document under the space prefix (SP-03, SP-04).
pub(crate) async fn space_schema_artifact(
    State(gateway): State<Arc<Gateway>>,
    Path((name, version, artifact)): Path<(String, String, String)>,
    mut request: Request,
) -> Response<Body> {
    tenancy::strip_client_headers(&mut request);
    let (space, subject) = match admit_space(&gateway, &name, None, request.headers()) {
        Ok(admitted) => admitted,
        Err(problem) => return *problem,
    };
    let view = format!("cs-{}", space.endpoint.space);
    match through_view(&gateway, &space.endpoint, &subject, &view, request).await {
        Ok(answer) => answer,
        Err(request) => schema_artifact_of(
            &space.endpoint,
            &subject,
            &version,
            &artifact,
            request.headers(),
        ),
    }
}

/// One `schema/` request through the broker's LinkML surface of `view` (plan 5.4, T-3645), or
/// the request back when no view serves this Endpoint (a projection with `temporalQ`, a slug no
/// view id can hold): those are still rendered here.
///
/// The path after `schema` goes on as the caller sent it, still percent-encoded, with `Accept`
/// and `If-None-Match`, so the broker's formalism negotiation and its `304` hold. A hidden
/// class, slot, model or cube answers the gateway's own `404`, the body of a name that does
/// not exist (LK8), whatever the broker's wording.
async fn through_view(
    gateway: &Gateway,
    endpoint: &Endpoint,
    subject: &Subject,
    view: &str,
    mut request: Request,
) -> Result<Response<Body>, Box<Request>> {
    if !has_view(view) {
        return Err(Box::new(request));
    }
    // The known prefix, not the first `/schema/`: a space or slug named `schema` holds one too.
    let prefix = format!("{}/schema", endpoint.base_path);
    let Some(rest) = request
        .uri()
        .path()
        .strip_prefix(&prefix)
        .filter(|rest| rest.starts_with('/'))
    else {
        return Ok(ProblemDetails::not_found().into_response());
    };
    let target = format!("/x/views/{view}/linkml/schema{rest}");
    if let Err(problem) = tenancy::pin_caller(&mut request, &endpoint.space, subject) {
        tracing::error!(space = %endpoint.space, %problem, "endpoint names an unusable space");
        return Ok(ProblemDetails::internal().into_response());
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
        Err(error) => return Ok(ProblemDetails::from(error).into_response()),
    };
    Ok(match answer.status() {
        status if status.is_server_error() => {
            let (parts, body) = answer.into_parts();
            broker_failure(parts.status, body).await
        }
        StatusCode::FORBIDDEN => with_platform_slug(answer).await,
        StatusCode::NOT_FOUND => ProblemDetails::not_found().into_response(),
        _ => answer,
    })
}

/// One document of the broker's LinkML surface of the Endpoint's view, read as `subject` reads
/// it (T-3645): the record, the catalogue feed and the MCP schema tool name the same bytes and
/// digests the `schema/` routes serve (EP-46, EP-68). `None` when no view serves the Endpoint;
/// the broker's status when it answers anything but `200`.
pub(crate) async fn view_document(
    gateway: &Gateway,
    endpoint: &Endpoint,
    subject: &Subject,
    rest: &str,
) -> Option<Result<Vec<u8>, StatusCode>> {
    let view = endpoint.slug.as_str();
    if !has_view(view) {
        return None;
    }
    let mut request = Request::new(Body::empty());
    if tenancy::pin_caller(&mut request, &endpoint.space, subject).is_err() {
        return Some(Err(StatusCode::INTERNAL_SERVER_ERROR));
    }
    let target = format!("/x/views/{view}/linkml/schema/{rest}");
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

/// The schema catalogue of an Endpoint as `subject` reads it: the broker's index when a view
/// serves it, else the one rendered here. A broker that does not answer gives a catalogue of
/// no model rather than digests of bytes nobody can fetch (EP-68).
pub(crate) async fn index_for(gateway: &Gateway, endpoint: &Endpoint, subject: &Subject) -> Value {
    match view_document(gateway, endpoint, subject, "index.json").await {
        Some(Ok(bytes)) => serde_json::from_slice(&bytes)
            .unwrap_or_else(|_| serde_json::json!({ "endpoint": endpoint.slug, "models": [] })),
        Some(Err(status)) => {
            tracing::warn!(slug = %endpoint.slug, %status, "the broker's schema index is unavailable");
            serde_json::json!({ "endpoint": endpoint.slug, "models": [] })
        }
        None => {
            let visible = schema::visible(subject, endpoint, crate::pdp::now());
            schema::index(endpoint, &visible, sha256_hex)
        }
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

/// One schema document, whichever prefix the caller came in by (SP-03, EP-49).
fn schema_artifact_of(
    endpoint: &Endpoint,
    subject: &Subject,
    version: &str,
    artifact: &str,
    headers: &HeaderMap,
) -> Response<Body> {
    // `schema/v2/...`: the major of the model, never its full version (DM-22).
    let Some(major) = version
        .strip_prefix('v')
        .and_then(|n| n.parse::<u32>().ok())
    else {
        return ProblemDetails::not_found().into_response();
    };
    let accept = headers
        .get(axum::http::header::ACCEPT)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("*/*");
    let Some(wanted) = schema::artifact_of(artifact, accept) else {
        return ProblemDetails::not_found().into_response();
    };

    let models: Vec<&Model> = endpoint
        .models
        .iter()
        .filter(|model| model.major == major)
        .collect();
    if models.is_empty() {
        return ProblemDetails::not_found().into_response();
    }
    let visible = schema::visible(subject, endpoint, crate::pdp::now());
    if wanted == schema::Artifact::Qb && !schema::declares_dsd(&models, &visible) {
        // A model without a Data Structure Definition has no cube, and a grant that hides the
        // DSD class hides its cube with it (DM-60, EP-47).
        return ProblemDetails::not_found().into_response();
    }
    if !wanted.is_json() {
        // SHACL, OWL, RDF, LinkML and Markdown are rendered from the projected model rather
        // than served from a committed file, so no formalism can carry a slot the grant
        // forbids (T-0284, EP-47).
        let body = schema::render(&models, wanted, &visible);
        return revalidated_text(body.into_bytes(), wanted.media_type(), headers);
    }

    let mut redacted = Vec::new();
    let document = match wanted {
        schema::Artifact::JsonSchema => schema::json_schema(&models, &visible, &mut redacted),
        _ => schema::context(&models, &visible, &mut redacted),
    };
    revalidated(&document, wanted.media_type(), headers)
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
fn revalidated_text(bytes: Vec<u8>, media_type: &str, headers: &HeaderMap) -> Response<Body> {
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
