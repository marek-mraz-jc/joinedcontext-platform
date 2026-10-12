//! The OGC API Features surface of an endpoint (EP-29): the broker's `antares-ogc` surface of the
//! Endpoint's view (T-3752, ADR-0029 decision 3).
//!
//! `/api/endpoint/{slug}/ogc/features{rest}` goes to `/x/views/{view}/ogc/features{rest}` with
//! the caller's query as sent and the tenant and the verified caller pinned, like the NGSI-LD
//! surface (T-3643). The broker answers every document, filter (CQL2) and decision; the gateway
//! admits the caller, keeps the representation read-only (EP-39) and moves the broker's links
//! under the caller's URL.

use crate::app::{
    admit, broker_failure, caller_links, has_view, principal_of, read_only, with_platform_slug,
    Gateway, MAX_BODY,
};
use crate::middleware::response::RESULTS_RESTRICTED;
use crate::middleware::tenancy;
use crate::proxy;
use axum::body::Body;
use axum::extract::{Path, Request, State};
use axum::http::{Method, Response, StatusCode};
use axum::response::IntoResponse;
use jc_core::kinds::Representation;
use jc_core::ProblemDetails;
use std::collections::HashMap;
use std::sync::Arc;

/// The broker's narrowing marker (Antares `configuration.md`, `ANTARES_POLICY`).
const ANTARES_RESTRICTED: &str = "antares-results-restricted";

/// Every OGC API - Features request of one endpoint (EP-29, EP-39, EP-40).
pub(crate) async fn ogc_features(
    State(gateway): State<Arc<Gateway>>,
    Path(params): Path<HashMap<String, String>>,
    mut request: Request,
) -> Response<Body> {
    // Before routing, before authentication, before anything reads a header (EP-21).
    tenancy::strip_client_headers(&mut request);
    let slug = params.get("slug").cloned().unwrap_or_default();
    let (endpoint, subject) = match admit(
        &gateway,
        &slug,
        Some(Representation::OgcFeatures),
        request.headers(),
    ) {
        Ok(admitted) => admitted,
        Err(problem) => return *problem,
    };
    // EP-39: this representation has no write half. `OPTIONS` is answered rather than refused,
    // because that is how a client discovers the fact.
    let method = request.method().clone();
    if method == Method::OPTIONS {
        return read_only(StatusCode::NO_CONTENT);
    }
    if ![Method::GET, Method::HEAD].contains(&method) {
        return read_only(StatusCode::METHOD_NOT_ALLOWED);
    }
    let view = endpoint.slug.clone();
    // A view of another model (G18) is the view root's alone: the broker hides it from every
    // other surface, OGC included.
    if endpoint.view_mapping.is_some() || !has_view(&view) {
        return no_view();
    }
    // The raw path, still percent-encoded: a decoded `/` or `?` in an item id would be a
    // different resource at the broker.
    let prefix = format!("{}/ogc/features", endpoint.base_path);
    let rest = request
        .uri()
        .path()
        .strip_prefix(&prefix)
        .filter(|rest| *rest != "/")
        .unwrap_or_default();
    let root = format!("/x/views/{view}/ogc/features");
    let target = match request.uri().query() {
        Some(query) if !query.is_empty() => format!("{root}{rest}?{query}"),
        _ => format!("{root}{rest}"),
    };
    if let Err(problem) = tenancy::pin_caller(&mut request, &endpoint.space, &subject) {
        tracing::error!(space = %endpoint.space, %problem, "endpoint names an unusable space");
        return ProblemDetails::internal().into_response();
    }
    tracing::info!(
        slug = %endpoint.slug,
        space = %endpoint.space,
        %view,
        principal = %principal_of(&subject),
        "OGC request forwarded to the broker's view"
    );
    let (parts, _) = request.into_parts();
    let answer = match gateway
        .broker
        .send(method, &target, parts.headers, Body::empty())
        .await
    {
        Ok(answer) => answer,
        Err(error) => return ProblemDetails::from(error).into_response(),
    };
    if answer.status().is_server_error() {
        let (parts, body) = answer.into_parts();
        return broker_failure(parts.status, body).await;
    }
    if answer.status() == StatusCode::FORBIDDEN {
        return with_platform_slug(answer).await;
    }
    // The broker names its resources by its own address (`http://{Host}`, Antares
    // `public_base`); the caller reaches them under the Endpoint's URL.
    let from = format!("http://{}{root}", authority(gateway.broker.base()));
    let to = format!("{}{prefix}", gateway.base_url());
    let (mut parts, body) = answer.into_parts();
    // ponytail: the page is held whole to move its links, so one past MAX_BODY (8 MiB) is a
    // broker failure; stream the rewrite if a `limit` ever reaches that.
    let Ok(held) = axum::body::to_bytes(body, MAX_BODY).await else {
        tracing::error!(slug = %endpoint.slug, "the broker's OGC answer is larger than the gateway holds");
        return broker_failure(StatusCode::BAD_GATEWAY, Body::empty()).await;
    };
    caller_links(&mut parts.headers, &from, &to);
    if let Some(restricted) = parts.headers.remove(ANTARES_RESTRICTED) {
        parts.headers.insert(RESULTS_RESTRICTED, restricted);
    }
    let text = String::from_utf8_lossy(&held).replace(&from, &to);
    proxy::with_body(parts, text.into_bytes())
}

/// The `host[:port]` of an upstream URL.
fn authority(base: &str) -> &str {
    let rest = base.split_once("://").map_or(base, |(_, rest)| rest);
    rest.split('/').next().unwrap_or(rest)
}

/// An Endpoint no broker view can serve yet (a view of another model, a projection with
/// `temporalQ`, a caller under a data-space agreement, a slug no view id holds): the OGC
/// surface is the broker's alone, so there is no other answer (ADR-0029 decision 3).
fn no_view() -> Response<Body> {
    ProblemDetails::new(501, "ogc-view-unavailable", "Not Implemented")
        .with_detail(
            "the OGC API Features surface is served from the broker's view of this endpoint, \
             and this endpoint or caller has none yet; read it through the NGSI-LD surface",
        )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn authority_is_host_and_port() {
        assert_eq!(authority("http://antares:1026"), "antares:1026");
        assert_eq!(authority("https://b.example/base"), "b.example");
        assert_eq!(authority("antares:1026"), "antares:1026");
    }
}
