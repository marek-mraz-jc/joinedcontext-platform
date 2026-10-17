//! Each Endpoint's DCAT record and the organization's catalogue a harvester reads (EP-01, EP-84).

use crate::app::Gateway;
use crate::auth::subject::admit;
use crate::handlers::{endpoint_surface, schema_routes, space_surface};
use crate::middleware::tenancy;
use crate::pdp::evaluator::Subject;
use axum::body::Body;
use axum::extract::{Path, Request, State};
use axum::http::{HeaderValue, Response};
use axum::response::IntoResponse;
use axum::routing::get;
use axum::Router;
use jc_core::kinds::Audience;
use jc_core::ProblemDetails;
use std::sync::Arc;

/// The DCAT-AP record of one endpoint, in the representation the caller asked for
/// (T-0337, T-0338, EP-27, EP-68, EP-69).
///
/// The record is built on the schema catalogue the endpoint serves (the broker's, T-3645), so
/// the digests it names are the ones `schema/index.json` names and the artifacts' own `ETag`s.
/// Everything is the granted projection: `admit` has already refused a caller the endpoint
/// does not serve, and the catalogue is read as this caller reads it.
pub(crate) async fn endpoint_record(
    State(gateway): State<Arc<Gateway>>,
    Path(slug): Path<String>,
    mut request: Request,
) -> Response<Body> {
    tenancy::strip_client_headers(&mut request);
    let (endpoint, subject) = match admit(&gateway, &slug, None, request.headers()) {
        Ok(admitted) => admitted,
        Err(problem) => return *problem,
    };

    // The catalogue the caller's `schema/` routes serve, so the digests are their ETags (EP-68).
    let index = schema_routes::index_for(&gateway, &endpoint, &endpoint.slug, &subject).await;
    let index =
        crate::handlers::mappings::with_mappings(&gateway, &endpoint, &subject, index).await;
    let space = gateway.resolver.resolve_space(&endpoint.space);
    let space = space.as_deref();

    let accept = request
        .headers()
        .get(axum::http::header::ACCEPT)
        .and_then(|value| value.to_str().ok());
    let base = gateway.base_url();
    let format = space_surface::negotiate(accept);
    let body = match format {
        space_surface::Format::JsonLd => {
            serde_json::to_string(&endpoint_surface::dataset(&endpoint, space, &index, base))
                .unwrap_or_default()
        }
        space_surface::Format::Turtle => {
            endpoint_surface::dataset_turtle(&endpoint, space, &index, base)
        }
        space_surface::Format::Html => {
            endpoint_surface::dataset_html(&endpoint, space, &index, base)
        }
    };
    match HeaderValue::from_str(format.media_type()) {
        Ok(media) => ([(axum::http::header::CONTENT_TYPE, media)], body).into_response(),
        Err(_) => ProblemDetails::internal().into_response(),
    }
}

/// The records of every public Endpoint as the anonymous caller reads each one, which is all
/// the feed may carry (EP-84, EP-69): no token is read, so no grant widens what it lists.
// ponytail: one broker hop per public Endpoint, in turn; join them if the feed's latency
// ever shows on an organization with many Endpoints.
async fn public_records(gateway: &Gateway) -> Vec<serde_json::Value> {
    let anonymous = Subject::anonymous();
    let mut records = Vec::new();
    for endpoint in gateway.resolver.endpoints() {
        if endpoint.audience != Audience::Public {
            continue;
        }
        let index = schema_routes::index_for(gateway, &endpoint, &endpoint.slug, &anonymous).await;
        let index =
            crate::handlers::mappings::with_mappings(gateway, &endpoint, &anonymous, index).await;
        let space = gateway.resolver.resolve_space(&endpoint.space);
        records.push(endpoint_surface::dataset(
            &endpoint,
            space.as_deref(),
            &index,
            gateway.base_url(),
        ));
    }
    records
}

async fn catalog_feed(gateway: &Gateway) -> serde_json::Value {
    endpoint_surface::feed(
        public_records(gateway).await,
        gateway.base_url(),
        &gateway.org_domain,
    )
}

/// `GET /catalog.jsonld`: the organization's DCAT-AP catalogue as JSON-LD (EP-84).
pub(crate) async fn catalog_feed_jsonld(State(gateway): State<Arc<Gateway>>) -> Response<Body> {
    let body = serde_json::to_string(&catalog_feed(&gateway).await).unwrap_or_default();
    (
        [(axum::http::header::CONTENT_TYPE, "application/ld+json")],
        body,
    )
        .into_response()
}

/// `GET /catalog.ttl`: the same catalogue as Turtle (EP-84).
pub(crate) async fn catalog_feed_turtle(State(gateway): State<Arc<Gateway>>) -> Response<Body> {
    let body = endpoint_surface::feed_turtle(&catalog_feed(&gateway).await);
    ([(axum::http::header::CONTENT_TYPE, "text/turtle")], body).into_response()
}

/// This surface's routes; `app::router` merges them under its guards.
pub(crate) fn routes() -> Router<Arc<Gateway>> {
    Router::new()
        // Both spellings, because EP-01 writes the base URL with the trailing slash and
        // every client that stores a base URL drops it.
        .route("/api/endpoint/{slug}", get(endpoint_record))
        .route("/api/endpoint/{slug}/", get(endpoint_record))
        // The organization's catalogue for harvesters: every public Endpoint's record (EP-84).
        .route("/catalog.jsonld", get(catalog_feed_jsonld))
        .route("/catalog.ttl", get(catalog_feed_turtle))
}
