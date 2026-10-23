//! An Endpoint's type list as its reads show the caller (T-4080), split from `reads` (T-4734).

use super::reads::{dataset_types, view_hop, view_page, PARALLEL_PAGES};
use crate::app::Gateway;
use crate::auth::subject::Subject;
use crate::query;
use crate::resolver::Endpoint;
use axum::body::Body;
use axum::extract::Request;
use axum::http::{Response, StatusCode};
use futures_util::stream::{self, StreamExt};
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
