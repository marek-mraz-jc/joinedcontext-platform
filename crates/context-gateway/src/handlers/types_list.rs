//! The type list a download selects by when the caller named none (T-5507). A caller's own
//! `/types` is the view's answer (T-5391).

use crate::app::{Gateway, MAX_BODY};
use crate::handlers::writes::names_no_tenant;
use crate::middleware::tenancy;
use axum::body::Body;
use axum::http::header::ACCEPT;
use axum::http::{HeaderMap, HeaderValue, Response, StatusCode};
use axum::response::IntoResponse;
use jc_core::ProblemDetails;
use serde_json::Value;

/// Every entity type the pinned tenant holds, as the broker's own `EntityTypeList` reports it.
///
/// Used only as the selector of last resort for a file download. It can only narrow: the
/// broker's view decides what this caller may see of it (T-3651). An empty list
/// means an empty space, a space with no tenant at the broker included, which is an empty file
/// rather than an error.
///
/// Asked on the gateway's own account (T-3790): no grant gives a caller the type list (5.7.5),
/// and the broker's engine refuses it to them; the entity query that follows is the caller's.
pub(super) async fn dataset_types(
    gateway: &Gateway,
    slug: &str,
    headers: &HeaderMap,
) -> Result<Vec<String>, Box<Response<Body>>> {
    let mut headers = headers.clone();
    tenancy::as_gateway(&mut headers);
    headers.insert(ACCEPT, HeaderValue::from_static("application/json"));
    // The broker took 3-5 s for this list on dev's helsinki space (T-5629): held for a while.
    let space = headers
        .get(tenancy::TENANT)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    if let Some(held) = space.as_deref().and_then(|s| gateway.held_types.get(s)) {
        return Ok(held.to_vec());
    }
    let answer = gateway
        .broker
        .send(
            axum::http::Method::GET,
            "/ngsi-ld/v1/types",
            headers,
            Body::empty(),
        )
        .await
        .map_err(|error| Box::new(ProblemDetails::from(error).into_response()))?;

    let (parts, body) = answer.into_parts();
    let bytes = axum::body::to_bytes(body, MAX_BODY)
        .await
        .map_err(|_| Box::new(ProblemDetails::internal().into_response()))?;
    // A space nobody has written to has no tenant at the broker, and holds no type (EP-22).
    if parts.status == StatusCode::NOT_FOUND && names_no_tenant(&bytes) {
        return Ok(Vec::new());
    }
    if !parts.status.is_success() {
        return Err(super::broker_failed(slug, parts.status, Body::from(bytes)).await);
    }
    let list: Value = serde_json::from_slice(&bytes)
        .map_err(|_| Box::new(ProblemDetails::internal().into_response()))?;

    let types: Vec<String> = list["typeList"]
        .as_array()
        .map(|types| {
            types
                .iter()
                .filter_map(|entry| entry.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default();
    if let Some(space) = &space {
        gateway.held_types.put(space, types.clone());
    }
    Ok(types)
}
