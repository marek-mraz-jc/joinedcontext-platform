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
use jc_dcat::profile::{self, Profile};
use std::sync::Arc;

/// The profile `?profile=` asks for: DCAT-AP when it names none, `None` when it names one this
/// build does not carry or the Organization does not serve, which answers 404 (API/08
/// section 5, T-4499).
fn profile_of(gateway: &Gateway, query: Option<&str>) -> Option<Profile> {
    let params = crate::query::parse(query.unwrap_or_default());
    match params.iter().find(|(key, _)| key == "profile") {
        None => Some(Profile::DcatAp),
        Some((_, id)) => gateway.dcat_profiles().pick(id),
    }
}

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
    let Some(profile) = profile_of(&gateway, request.uri().query()) else {
        return ProblemDetails::not_found().into_response();
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
        space_surface::Format::JsonLd => serde_json::to_string(&profile::render(
            endpoint_surface::dataset(&endpoint, space, &index, base),
            profile,
            endpoint.catalog.as_deref(),
        ))
        .unwrap_or_default(),
        space_surface::Format::Turtle => jc_dcat::turtle(&profile::render(
            endpoint_surface::dataset(&endpoint, space, &index, base),
            profile,
            endpoint.catalog.as_deref(),
        )),
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
async fn public_records(
    gateway: &Gateway,
    profile: Profile,
    project: Option<&str>,
) -> Vec<serde_json::Value> {
    let anonymous = Subject::anonymous();
    let mut records = Vec::new();
    for endpoint in gateway.resolver.endpoints() {
        // An App's own door re-serves a space for that App and carries no catalogue block: it
        // is no dataset of the publisher, and a national catalogue would harvest it as one.
        // A door over data other datasets already publish (EP-139): its record stays, the
        // feeds leave it out, so a harvester never takes the same data twice.
        if endpoint.audience != Audience::Public
            || endpoint.roles.app_client.is_some()
            || endpoint.catalog.as_deref().is_some_and(|c| !c.harvested())
            || project.is_some_and(|project| endpoint.project != project)
        {
            continue;
        }
        let index = schema_routes::index_for(gateway, &endpoint, &endpoint.slug, &anonymous).await;
        let index =
            crate::handlers::mappings::with_mappings(gateway, &endpoint, &anonymous, index).await;
        let space = gateway.resolver.resolve_space(&endpoint.space);
        records.push(profile::render(
            endpoint_surface::dataset(&endpoint, space.as_deref(), &index, gateway.base_url()),
            profile,
            endpoint.catalog.as_deref(),
        ));
    }
    records
}

async fn catalog_feed(gateway: &Gateway, profile: Profile) -> serde_json::Value {
    endpoint_surface::feed(
        public_records(gateway, profile, None).await,
        gateway.base_url(),
        &gateway.org_domain,
    )
}

/// `GET /catalog.jsonld`: the organization's DCAT-AP catalogue as JSON-LD (EP-84).
pub(crate) async fn catalog_feed_jsonld(
    State(gateway): State<Arc<Gateway>>,
    request: Request,
) -> Response<Body> {
    let Some(profile) = profile_of(&gateway, request.uri().query()) else {
        return ProblemDetails::not_found().into_response();
    };
    let body = serde_json::to_string(&catalog_feed(&gateway, profile).await).unwrap_or_default();
    (
        [(axum::http::header::CONTENT_TYPE, "application/ld+json")],
        body,
    )
        .into_response()
}

/// `GET /catalog.ttl`: the same catalogue as Turtle (EP-84).
pub(crate) async fn catalog_feed_turtle(
    State(gateway): State<Arc<Gateway>>,
    request: Request,
) -> Response<Body> {
    let Some(profile) = profile_of(&gateway, request.uri().query()) else {
        return ProblemDetails::not_found().into_response();
    };
    let body = endpoint_surface::feed_turtle(&catalog_feed(&gateway, profile).await);
    ([(axum::http::header::CONTENT_TYPE, "text/turtle")], body).into_response()
}

/// `GET /catalog/{project}.jsonld` and `.ttl`: the catalogue of one publisher, a Project that
/// names it, with the records of the project's public Endpoints (EP-127, API/08 section 6). An
/// unknown project, one without a publisher and one without a public Endpoint are the same 404,
/// so the address says nothing about a project that publishes nothing.
pub(crate) async fn publisher_catalog(
    State(gateway): State<Arc<Gateway>>,
    Path(file): Path<String>,
    request: Request,
) -> Response<Body> {
    let (project, turtle) = match file.rsplit_once('.') {
        Some((project, "jsonld")) => (project, false),
        Some((project, "ttl")) => (project, true),
        _ => return ProblemDetails::not_found().into_response(),
    };
    let (Some(profile), Some(owner)) = (
        profile_of(&gateway, request.uri().query()),
        gateway.publisher(project),
    ) else {
        return ProblemDetails::not_found().into_response();
    };
    let records = public_records(&gateway, profile, Some(project)).await;
    if records.is_empty() {
        return ProblemDetails::not_found().into_response();
    }
    let feed = profile::render_catalog(
        jc_dcat::publisher_feed(
            records,
            gateway.base_url(),
            &jc_dcat::CatalogueOwner {
                project,
                title: &owner.title,
                description: &owner.description,
                domain: owner.domain.as_deref(),
                catalog: &owner.catalog,
            },
        ),
        profile,
    );
    if turtle {
        let body = endpoint_surface::feed_turtle(&feed);
        ([(axum::http::header::CONTENT_TYPE, "text/turtle")], body).into_response()
    } else {
        let body = serde_json::to_string(&feed).unwrap_or_default();
        (
            [(axum::http::header::CONTENT_TYPE, "application/ld+json")],
            body,
        )
            .into_response()
    }
}

/// The catalogue widget's defaults per class (API/11 EP-163): the geometry, colour field, popup
/// fields and clustering its map starts from. Admitted as the Endpoint's other routes are, and an
/// attribute the Endpoint's projection hides is never named.
pub(crate) async fn widget_defaults(
    State(gateway): State<Arc<Gateway>>,
    Path(slug): Path<String>,
    mut request: Request,
) -> Response<Body> {
    tenancy::strip_client_headers(&mut request);
    let (endpoint, _) = match admit(&gateway, &slug, None, request.headers()) {
        Ok(admitted) => admitted,
        Err(problem) => return *problem,
    };
    let hidden = |path: &String| {
        let first = path.split('.').next().unwrap_or_default();
        endpoint.hidden_attributes.contains(first)
    };
    let map: std::collections::BTreeMap<_, _> = endpoint
        .map_defaults
        .iter()
        .map(|(class, defaults)| {
            let mut shown = defaults.clone();
            shown.geometry = shown.geometry.filter(|p| !hidden(p));
            shown.colour = shown.colour.filter(|p| !hidden(p));
            shown.popup.retain(|p| !hidden(p));
            (class.clone(), shown)
        })
        .collect();
    axum::Json(serde_json::json!({ "map": map })).into_response()
}

/// This surface's routes; `app::router` merges them under its guards.
pub(crate) fn routes() -> Router<Arc<Gateway>> {
    Router::new()
        // Both spellings, because EP-01 writes the base URL with the trailing slash and
        // every client that stores a base URL drops it.
        .route("/api/endpoint/{slug}", get(endpoint_record))
        .route("/api/endpoint/{slug}/", get(endpoint_record))
        .route("/api/endpoint/{slug}/widget.json", get(widget_defaults))
        // The organization's catalogue for harvesters: every public Endpoint's record (EP-84).
        .route("/catalog.jsonld", get(catalog_feed_jsonld))
        .route("/catalog.ttl", get(catalog_feed_turtle))
        // One publisher's catalogue, which a national portal registers (EP-127).
        .route("/catalog/{file}", get(publisher_catalog))
}
