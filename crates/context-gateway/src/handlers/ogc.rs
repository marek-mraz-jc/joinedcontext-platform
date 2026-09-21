//! The OGC API Features surface of an endpoint, one handler over its own resource tree (EP-29).

use crate::app::accept_language;
use crate::app::admit;
use crate::app::read_only;
use crate::app::typed_json_response;
use crate::app::Gateway;
use crate::app::PAGE;
use crate::handlers::reads::query_entities;
use crate::handlers::schema;
use crate::middleware::response::RESULTS_RESTRICTED;
use crate::pdp::evaluator::Subject;
use crate::resolver::{Endpoint, Space};
use crate::translators::{cql2, ogc};
use crate::{middleware::tenancy, query};
use axum::body::Body;
use axum::extract::{Path, Request, State};
use axum::http::{HeaderMap, HeaderValue, Method, Response, StatusCode};
use axum::response::IntoResponse;
use jc_core::kinds::Representation;
use jc_core::ProblemDetails;
use serde_json::Value;
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

/// Every OGC API - Features request of one endpoint (T-0159, EP-29, EP-30, EP-31, EP-32, EP-39).
///
/// One handler over the whole Core resource tree, for the same reason the NGSI-LD surface has
/// one: the four steps before the branch — strip, resolve, refuse a write, translate the
/// parameters — are the steps that make the representation safe, and a second entry point is
/// a second place to forget one of them.
pub(crate) async fn ogc_features(
    State(gateway): State<Arc<Gateway>>,
    Path(params): Path<HashMap<String, String>>,
    mut request: Request,
) -> Response<Body> {
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

    // EP-39: this representation has no write half at all. `OPTIONS` is answered rather than
    // refused, because that is how a client is supposed to discover the fact.
    let method = request.method().clone();
    if method == Method::OPTIONS {
        return read_only(StatusCode::NO_CONTENT);
    }
    if ![Method::GET, Method::HEAD].contains(&method) {
        return read_only(StatusCode::METHOD_NOT_ALLOWED);
    }

    let base = format!("{}{}", gateway.base_url(), endpoint.base_path);
    let raw_query = request.uri().query().unwrap_or_default().to_owned();
    let caller = query::parse(&raw_query);
    if let Some(wanted) = query::first(&caller, "crs") {
        if let Err(problem) = ogc::crs(wanted) {
            return bad_parameter(&problem);
        }
    }

    let segments: Vec<&str> = params
        .get("rest")
        .map(|rest| rest.split('/').filter(|part| !part.is_empty()).collect())
        .unwrap_or_default();

    match segments.as_slice() {
        [] => {
            let space = gateway.resolver.resolve_space(&endpoint.space);
            let (title, description) = ogc_titles(space.as_deref(), &endpoint, request.headers());
            typed_json_response(&ogc::landing(&base, &title, &description), ogc::JSON)
        }
        ["conformance"] => typed_json_response(&ogc::conformance(), ogc::JSON),
        // EP-40: the description is generated from what this caller may actually see, so the
        // collections it lists are the collections the rest of the document leads to.
        ["api"] => {
            let space = gateway.resolver.resolve_space(&endpoint.space);
            let (title, description) = ogc_titles(space.as_deref(), &endpoint, request.headers());
            match ogc_sample(&gateway, &endpoint, &subject, &mut request, &[]).await {
                Ok(sampled) => {
                    let types: Vec<String> = sampled.keys().cloned().collect();
                    typed_json_response(
                        &ogc::api_document(&base, &title, &description, &types),
                        ogc::OPENAPI,
                    )
                }
                Err(problem) => *problem,
            }
        }
        ["collections"] => {
            let space = gateway.resolver.resolve_space(&endpoint.space);
            let (_, description) = ogc_titles(space.as_deref(), &endpoint, request.headers());
            match ogc_sample(&gateway, &endpoint, &subject, &mut request, &[]).await {
                Ok(sampled) => {
                    let types: Vec<String> = sampled.keys().cloned().collect();
                    typed_json_response(&ogc::collections(&base, &types, &description), ogc::JSON)
                }
                Err(problem) => *problem,
            }
        }
        ["collections", name] => {
            let space = gateway.resolver.resolve_space(&endpoint.space);
            let (_, description) = ogc_titles(space.as_deref(), &endpoint, request.headers());
            let name = (*name).to_owned();
            match ogc_sample(
                &gateway,
                &endpoint,
                &subject,
                &mut request,
                std::slice::from_ref(&name),
            )
            .await
            {
                // EP-31: a type with no geometry is not a Feature Collection, and an ungranted
                // one is indistinguishable from it (R20).
                Ok(sampled) => match sampled.get(&name) {
                    Some(extent) => typed_json_response(
                        &ogc::collection(&base, &name, &description, Some(extent)),
                        ogc::JSON,
                    ),
                    None => ProblemDetails::not_found().into_response(),
                },
                Err(problem) => *problem,
            }
        }
        ["collections", name, "items"] => {
            ogc_items(
                &gateway,
                &endpoint,
                &subject,
                &mut request,
                name,
                &base,
                &raw_query,
            )
            .await
        }
        ["collections", name, "items", id] => {
            ogc_item(&gateway, &endpoint, &subject, &mut request, name, id, &base).await
        }
        _ => ProblemDetails::not_found().into_response(),
    }
}

/// One page of one collection (EP-33, EP-34, EP-36, EP-37, EP-38).
async fn ogc_items(
    gateway: &Gateway,
    endpoint: &Endpoint,
    subject: &Subject,
    request: &mut Request,
    name: &str,
    base: &str,
    raw_query: &str,
) -> Response<Body> {
    let caller = query::parse(raw_query);
    let limit = query::first(&caller, "limit")
        .and_then(|limit| limit.parse::<usize>().ok())
        .unwrap_or(ogc::DEFAULT_LIMIT)
        .clamp(1, PAGE);
    let offset = query::first(&caller, "next")
        .and_then(ogc::offset_of)
        .unwrap_or_default();

    let mut upstream = vec![
        ("type".to_owned(), name.to_owned()),
        ("limit".to_owned(), limit.to_string()),
        ("offset".to_owned(), offset.to_string()),
    ];
    for (parameter, translate) in [
        (
            "bbox",
            ogc::bbox as fn(&str) -> Result<Vec<(String, String)>, ogc::ParamError>,
        ),
        ("datetime", ogc::datetime),
    ] {
        if let Some(raw) = query::first(&caller, parameter) {
            match translate(raw) {
                Ok(translated) => upstream.extend(translated),
                Err(problem) => return bad_parameter(&problem),
            }
        }
    }

    // EP-35: the CQL2 filter, compiled into the same three NGSI-LD parameters. A predicate the
    // subset does not cover is a `400` naming the operator, never a filter half applied.
    if let Some(raw) = query::first(&caller, "filter") {
        if let Some(lang) = query::first(&caller, "filter-lang") {
            if lang != cql2::LANG {
                return bad_parameter(&ogc::ParamError {
                    parameter: "filter-lang",
                    detail: format!("this endpoint reads {} only", cql2::LANG),
                });
            }
        }
        let compiled = match cql2::compile(raw) {
            Ok(compiled) => compiled,
            Err(problem) => return bad_parameter(&problem),
        };
        // NGSI-LD carries one `geoQ` and one `temporalQ`, so a filter that brings its own
        // cannot be combined with the parameter that means the same thing. Applying both
        // would drop one of them, and dropping one returns more than the caller asked for.
        for (from_filter, parameter) in [
            (!compiled.geo.is_empty(), "bbox"),
            (!compiled.temporal.is_empty(), "datetime"),
        ] {
            if from_filter && query::first(&caller, parameter).is_some() {
                return bad_parameter(&ogc::ParamError {
                    parameter: "filter",
                    detail: format!(
                        "{parameter} and a filter predicate of the same kind cannot both be \
                         applied; write the whole condition in one of them"
                    ),
                });
            }
        }
        upstream.extend(compiled.geo);
        upstream.extend(compiled.temporal);
        if let Some(q) = compiled.q {
            upstream.push(("q".to_owned(), q));
        }
    }

    let (entities, restricted) =
        match query_entities(gateway, endpoint, subject, &upstream, request).await {
            Ok(answer) => answer,
            Err(problem) => return *problem,
        };

    let timestamp = crate::pdp::now().to_rfc3339();
    let page = ogc::items(
        base,
        name,
        &entities,
        limit,
        offset,
        raw_query,
        &timestamp,
        accept_language(request.headers()),
    );
    let mut response = typed_json_response(&page, ogc::GEOJSON);
    response
        .headers_mut()
        .insert("content-crs", HeaderValue::from_static(ogc::CRS84));
    if restricted {
        response
            .headers_mut()
            .insert(RESULTS_RESTRICTED, HeaderValue::from_static("true"));
    }
    response
}

/// One feature by its entity URN (EP-33).
async fn ogc_item(
    gateway: &Gateway,
    endpoint: &Endpoint,
    subject: &Subject,
    request: &mut Request,
    name: &str,
    id: &str,
    base: &str,
) -> Response<Body> {
    let upstream = vec![
        ("type".to_owned(), name.to_owned()),
        ("id".to_owned(), id.to_owned()),
        ("limit".to_owned(), "1".to_owned()),
    ];
    let (entities, restricted) =
        match query_entities(gateway, endpoint, subject, &upstream, request).await {
            Ok(answer) => answer,
            Err(problem) => return *problem,
        };

    // An entity that does not exist, one the grants withhold and one with no geometry all
    // answer the same 404, so a probe over the URN space learns nothing (R20, EP-33).
    let Some(feature) = entities
        .as_array()
        .and_then(|list| list.first())
        .and_then(|entity| ogc::feature(base, name, entity, accept_language(request.headers())))
    else {
        return ProblemDetails::not_found().into_response();
    };
    let mut response = typed_json_response(&feature, ogc::GEOJSON);
    response
        .headers_mut()
        .insert("content-crs", HeaderValue::from_static(ogc::CRS84));
    if restricted {
        response
            .headers_mut()
            .insert(RESULTS_RESTRICTED, HeaderValue::from_static("true"));
    }
    response
}

/// The collections of an endpoint and what each of them spans, from one bounded page (EP-31,
/// EP-32).
///
/// The manifest cannot answer either question: whether a type carries a geometry and what its
/// data spans are properties of the data. Both come from one projected page, so a GIS client's
/// discovery costs one broker call and not one per type.
// ponytail: a page, not a scan. A type whose geometries all sit past `PAGE` entities is not
// listed; the upgrade path is the cached extent query EP-32 names with its `maxAgeSeconds`.
async fn ogc_sample(
    gateway: &Gateway,
    endpoint: &Endpoint,
    subject: &Subject,
    request: &mut Request,
    wanted: &[String],
) -> Result<BTreeMap<String, ogc::Extent>, Box<Response<Body>>> {
    let visible = schema::visible(subject, endpoint, crate::pdp::now());
    let mut types = schema::visible_types(endpoint, &visible);
    if !wanted.is_empty() {
        types.retain(|name| wanted.contains(name));
        // A type the models do not declare is still a type the space may hold, and the PDP
        // decides either way; the models only ever narrow the discovery list.
        if types.is_empty() {
            types = wanted.to_vec();
        }
    }

    let mut upstream = vec![("limit".to_owned(), PAGE.to_string())];
    if !types.is_empty() {
        upstream.push(("type".to_owned(), types.join(",")));
    }
    let (entities, _) = query_entities(gateway, endpoint, subject, &upstream, request).await?;

    let mut grouped: BTreeMap<String, Vec<Value>> = BTreeMap::new();
    for entity in entities.as_array().into_iter().flatten() {
        if let Some(name) = entity.get("type").and_then(Value::as_str) {
            grouped
                .entry(name.to_owned())
                .or_default()
                .push(entity.clone());
        }
    }
    Ok(grouped
        .into_iter()
        .filter_map(|(name, group)| {
            let extent = ogc::extent_of(&Value::Array(group));
            // EP-31: only a type that actually carries a geometry is a Feature Collection.
            extent.bbox.is_some().then_some((name, extent))
        })
        .collect())
}

/// The endpoint's title and description in the caller's language (EP-32).
fn ogc_titles(space: Option<&Space>, endpoint: &Endpoint, headers: &HeaderMap) -> (String, String) {
    let accept = headers
        .get(axum::http::header::ACCEPT_LANGUAGE)
        .and_then(|value| value.to_str().ok());
    let Some(space) = space else {
        return (endpoint.space.clone(), String::new());
    };
    let locale = space.default_locale.as_deref();
    let title = ogc::localized(&space.title, accept, locale);
    let description = ogc::localized(&space.description, accept, locale);
    (
        if title.is_empty() {
            endpoint.space.clone()
        } else {
            title.to_owned()
        },
        description.to_owned(),
    )
}

/// A parameter the representation cannot honour, naming it so the client can fix it (EP-34).
pub(crate) fn bad_parameter(problem: &ogc::ParamError) -> Response<Body> {
    let mut body = ProblemDetails::bad_request()
        .with_detail(problem.detail.clone())
        .into_response();
    *body.status_mut() = StatusCode::BAD_REQUEST;
    let _ = HeaderValue::from_str(problem.parameter)
        .map(|value| body.headers_mut().insert("x-parameter", value));
    body
}
