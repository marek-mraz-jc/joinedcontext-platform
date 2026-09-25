//! The routes of the endpoint and the space `schema/` child (EP-46, SP-04): the index and one
//! artifact, revalidated by ETag.

use crate::app::admit;
use crate::app::admit_space;
use crate::app::sha256_hex;
use crate::app::Gateway;
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
    schema_index_of(&endpoint, &subject, request.headers())
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
    schema_index_of(&space.endpoint, &subject, request.headers())
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
    schema_artifact_of(&endpoint, &subject, &version, &artifact, request.headers())
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
    schema_artifact_of(
        &space.endpoint,
        &subject,
        &version,
        &artifact,
        request.headers(),
    )
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
