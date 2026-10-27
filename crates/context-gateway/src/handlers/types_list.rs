//! An Endpoint's type list as its reads show the caller (T-4080), split from `reads` (T-4734), and the
//! type list a download selects by when the caller named none (T-5507).

use super::reads::{broker_failed, view_hop, view_page, PARALLEL_PAGES};
use crate::app::{Gateway, MAX_BODY};
use crate::auth::subject::Subject;
use crate::handlers::writes::names_no_tenant;
use crate::middleware::tenancy;
use crate::query;
use crate::resolver::Endpoint;
use axum::body::Body;
use axum::extract::Request;
use axum::http::header::ACCEPT;
use axum::http::{HeaderMap, HeaderValue, Response, StatusCode};
use axum::response::IntoResponse;
use futures_util::stream::{self, StreamExt};
use jc_core::ProblemDetails;
use serde_json::Value;

/// The type list of an Endpoint as its reads show the caller (CIM 009 5.7.1, T-4080): every
/// type the space holds that the Endpoint's view answers at least one entity of to this caller.
/// A read grant is enough to be shown it, as it is for the schema (EP-47), and each type is the
/// view's decision, so no type name outside what the caller may read is listed. `None` when the
/// view refuses the caller every type the space holds: the view then answers the operation
/// itself.
pub(crate) async fn listed_types(
    gateway: &Gateway,
    endpoint: &Endpoint,
    subject: &Subject,
    request: &mut Request,
) -> Result<Option<Value>, Box<Response<Body>>> {
    let slug = endpoint.slug.as_str();
    view_hop(endpoint, subject, request)?;
    let held = dataset_types(gateway, slug, request.headers()).await?;
    // Each type is a view read of its own: the caller pays for every probe past the one
    // request the rate limit already counted (F-SEC1-017), before any is sent.
    let caller = crate::middleware::rate_limit::caller_of(gateway, request);
    let extra = u32::try_from(held.len().saturating_sub(1)).unwrap_or(u32::MAX);
    crate::middleware::rate_limit::charge(gateway, endpoint, &caller, extra)?;
    let mut shown = Vec::new();
    let mut refused = 0;
    let headers = request.headers();
    // PARALLEL_PAGES at a time, in the space's order (T-4734): asked one after another, the
    // HSL Endpoint's list took 10-19 s on dev.
    let probes: Vec<_> = held
        .iter()
        .map(|name| async move {
            let target = format!(
                "entities?{}&limit=1",
                query::view_read(&[], std::slice::from_ref(name))
            );
            (name, view_page(gateway, slug, slug, &target, headers).await)
        })
        .collect();
    let mut probed = stream::iter(probes).buffered(PARALLEL_PAGES);
    while let Some((name, probe)) = probed.next().await {
        match probe {
            Ok((page, _, _, _)) if !page.is_empty() => shown.push(name.clone()),
            Ok(_) => {}
            // A type the view refuses this caller is a type they are not shown.
            Err(answer) if answer.status() == StatusCode::FORBIDDEN => refused += 1,
            Err(failed) => return Err(failed),
        }
    }
    if !held.is_empty() && refused == held.len() {
        return Ok(None);
    }
    Ok(Some(serde_json::json!({
        "id": format!("urn:ngsi-ld:EntityTypeList:{slug}"),
        "type": "EntityTypeList",
        "typeList": shown,
    })))
}

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
        return Err(broker_failed(slug, parts.status, Body::from(bytes)).await);
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
