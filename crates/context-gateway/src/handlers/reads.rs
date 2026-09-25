//! The reads every export surface shares: the types a dataset holds, one query through the
//! PDP, a temporal query, and the capped page walk the downloads and the OGC items are made of.

use crate::app::Gateway;
use crate::app::MAX_BODY;
use crate::app::MAX_COLLECTED;
use crate::app::PAGE;
use crate::pdp::evaluator::{Subject, Verdict};
use crate::pdp::{geo, projection, temporal};
use crate::resolver::Endpoint;
use crate::translators::tabular;
use crate::{middleware::tenancy, query};
use axum::body::Body;
use axum::extract::Request;
use axum::http::{HeaderMap, Response, StatusCode};
use axum::response::IntoResponse;
use jc_core::kinds::Operation;
use jc_core::ProblemDetails;
use serde_json::Value;

/// Every entity type the pinned tenant holds, as the broker's own `EntityTypeList` reports it.
///
/// Used only as the selector of last resort for a file download. It can only narrow: the PDP
/// has already decided what this caller may see, and every constraint it produced is applied
/// either upstream or on the way back. An empty list means an empty space, which is an empty
/// file rather than an error.
async fn dataset_types(
    gateway: &Gateway,
    slug: &str,
    headers: &HeaderMap,
) -> Result<Vec<String>, Box<Response<Body>>> {
    let answer = gateway
        .broker
        .send(
            axum::http::Method::GET,
            "/ngsi-ld/v1/types",
            headers.clone(),
            Body::empty(),
        )
        .await
        .map_err(|error| Box::new(ProblemDetails::from(error).into_response()))?;

    let (parts, body) = answer.into_parts();
    if !parts.status.is_success() {
        return Err(broker_failed(slug, parts.status, body).await);
    }
    let bytes = axum::body::to_bytes(body, MAX_BODY)
        .await
        .map_err(|_| Box::new(ProblemDetails::internal().into_response()))?;
    let list: Value = serde_json::from_slice(&bytes)
        .map_err(|_| Box::new(ProblemDetails::internal().into_response()))?;

    Ok(list["typeList"]
        .as_array()
        .map(|types| {
            types
                .iter()
                .filter_map(|entry| entry.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default())
}

/// Queries the entities behind an endpoint through the PDP, projected (GW11, R9).
///
/// This is the read half of the `file.*` downloads, shared by every representation that is a
/// different rendering of the same dataset. The NGSI-LD surface does not come through here:
/// it forwards the caller's own query, and an unselected one stays the `400` the
/// specification asks for.
pub(crate) async fn query_entities(
    gateway: &Gateway,
    endpoint: &Endpoint,
    subject: &Subject,
    params: &[(String, String)],
    request: &mut Request,
) -> Result<(Value, bool), Box<Response<Body>>> {
    let slug = endpoint.slug.as_str();
    let verdict = gateway.pdp.decide(
        subject,
        Operation::QueryEntity,
        &query::requested(params),
        endpoint,
    );
    let Verdict::Rewrite(constraints) = verdict else {
        return Err(Box::new(ProblemDetails::forbidden().into_response()));
    };
    if constraints.empty {
        return Ok((Value::Array(Vec::new()), true));
    }
    tenancy::pin_tenant(request, &endpoint.space)
        .map_err(|_| Box::new(ProblemDetails::internal().into_response()))?;

    // Nothing the caller sent and nothing the grants added selects anything, and a query that
    // selects nothing is a `400` upstream (CIM 009 5.7.2). A download is not a query though:
    // asking for `file.geojson` is asking for the dataset, so the selector is every type the
    // space holds (EP-09, EP-07). It can only narrow, never widen: the PDP has already spoken.
    let fallback = if query::selects(&constraints) {
        Vec::new()
    } else {
        let types = dataset_types(gateway, slug, request.headers()).await?;
        // A space holding nothing is an empty file, not a `400` and not a second broker call.
        if types.is_empty() {
            return Ok((Value::Array(Vec::new()), constraints.restricted));
        }
        types
    };

    let target = format!(
        "/ngsi-ld/v1/entities?{}",
        query::upstream(params, &constraints, &fallback)
    );
    let answer = gateway
        .broker
        .send(
            axum::http::Method::GET,
            &target,
            request.headers().clone(),
            Body::empty(),
        )
        .await
        .map_err(|error| Box::new(ProblemDetails::from(error).into_response()))?;

    let (parts, body) = answer.into_parts();
    if !parts.status.is_success() {
        return Err(broker_failed(slug, parts.status, body).await);
    }
    let bytes = axum::body::to_bytes(body, MAX_BODY)
        .await
        .map_err(|_| Box::new(ProblemDetails::internal().into_response()))?;
    let mut entities: Value = serde_json::from_slice(&bytes)
        .map_err(|_| Box::new(ProblemDetails::internal().into_response()))?;

    let areas = geo::Areas::of(&constraints.geo_grants, constraints.geo_caller.as_deref());
    if let Value::Array(list) = &mut entities {
        list.retain(|entity| {
            projection::permitted(entity, &constraints)
                && areas.as_ref().is_none_or(|areas| areas.admits(entity))
        });
    }
    projection::project_by_type(&mut entities, &constraints);
    Ok((entities, constraints.restricted))
}

/// The history of one entity, decided and projected exactly as its current state is (T-0438).
///
/// The temporal tree is a second upstream path, so it goes through the same four steps the
/// entity path goes through and in the same order: the PDP decides, the tenant is pinned, the
/// grants' own window and attribute set are what is forwarded, and what comes back is filtered
/// by the id patterns and the geo grants and then projected. Skipping any of them would make
/// the history a way around the projection the instant answer applies (EP-07, R9).
///
/// Two differences from [`query_entities`], both of them the temporal representation's own.
/// The answer is one entity whose attributes are arrays of instances rather than a list of
/// entities, so a caller who may not read it gets `404` rather than an empty page. And the
/// grants' windows are applied to the instances after the projection, because the window the
/// broker was given is the hull of several grants and what falls in the gaps between them was
/// never granted (GW26).
pub(crate) async fn query_temporal(
    gateway: &Gateway,
    endpoint: &Endpoint,
    subject: &Subject,
    urn: &str,
    params: &[(String, String)],
    request: &mut Request,
) -> Result<(Value, bool), Box<Response<Body>>> {
    let verdict = gateway.pdp.decide(
        subject,
        Operation::RetrieveTemporal,
        &query::requested(params),
        endpoint,
    );
    let Verdict::Rewrite(constraints) = verdict else {
        return Err(Box::new(ProblemDetails::forbidden().into_response()));
    };
    // A window no grant reaches is genuinely no history, and it is answered here rather than
    // asked of the broker without one (GW26).
    if constraints.empty {
        return Ok((Value::Null, true));
    }
    tenancy::pin_tenant(request, &endpoint.space)
        .map_err(|_| Box::new(ProblemDetails::internal().into_response()))?;

    // By id, so without the grants' `type` (CIM 009 6.19.3.1): `projection::permitted` below
    // judges the type of what comes back (T-2987).
    let target = format!(
        "/ngsi-ld/v1/temporal/entities/{}?{}",
        query::encode(urn),
        query::upstream_by_id(params, &constraints, urn)
    );
    let answer = gateway
        .broker
        .send(
            axum::http::Method::GET,
            &target,
            request.headers().clone(),
            Body::empty(),
        )
        .await
        .map_err(|error| Box::new(ProblemDetails::from(error).into_response()))?;

    let (parts, body) = answer.into_parts();
    // An entity with no history and one the broker refuses both mean "no observations here",
    // and the caller learns nothing from the difference (R20).
    if !parts.status.is_success() {
        return Ok((Value::Null, constraints.restricted));
    }
    let bytes = axum::body::to_bytes(body, MAX_BODY)
        .await
        .map_err(|_| Box::new(ProblemDetails::internal().into_response()))?;
    let mut entity: Value = serde_json::from_slice(&bytes)
        .map_err(|_| Box::new(ProblemDetails::internal().into_response()))?;
    // The temporal query form answers a list even when it holds one entity.
    if let Some(first) = entity.as_array().and_then(|list| list.first()).cloned() {
        entity = first;
    }

    let areas = geo::Areas::of(&constraints.geo_grants, constraints.geo_caller.as_deref());
    if !projection::permitted(&entity, &constraints)
        || !areas.as_ref().is_none_or(|areas| areas.admits(&entity))
    {
        return Ok((Value::Null, constraints.restricted));
    }
    projection::project_by_type(&mut entity, &constraints);
    temporal::keep_windows(&mut entity, &constraints.temporal_windows);
    Ok((entity, constraints.restricted))
}

/// The gateway's own answer to a broker that failed a read behind one of its representations
/// (T-2340, EP-26, R22).
///
/// A download, an OGC page and a SensorThings collection are the gateway's documents, and what a
/// failing broker prints is whatever it was holding: a DSN with its password, a host inside the
/// cluster, a source path, the tenant. None of it reaches the caller, most of whom need no token
/// to ask; the operator reads it in the log beside the endpoint's slug. A `400` is the broker
/// refusing the query this request became and a `404` is a miss; a `5xx` keeps its status, as on
/// the NGSI-LD surface (T-2260). Any other refusal (the gateway's own credentials, its rate at
/// the broker) is the gateway failing rather than the caller, so it is a `502`.
async fn broker_failed(slug: &str, status: StatusCode, body: Body) -> Box<Response<Body>> {
    let held = axum::body::to_bytes(body, MAX_BODY)
        .await
        .unwrap_or_default();
    tracing::warn!(
        slug,
        status = status.as_u16(),
        broker = %String::from_utf8_lossy(&held),
        "the broker failed a read behind a representation; its own explanation is not passed on"
    );
    let problem = match status {
        StatusCode::BAD_REQUEST => ProblemDetails::bad_request()
            .with_detail("the context broker refused the query this request asks for"),
        StatusCode::NOT_FOUND => ProblemDetails::not_found(),
        failed => ProblemDetails::new(
            if failed.is_server_error() {
                failed.as_u16()
            } else {
                StatusCode::BAD_GATEWAY.as_u16()
            },
            "broker-failure",
            "The context broker could not answer",
        )
        .with_detail("the context broker behind this endpoint failed to answer this request"),
    };
    Box::new(problem.into_response())
}

/// The answer is bigger than this endpoint allows one download to be (EP-44).
pub(crate) fn too_large() -> Response<Body> {
    let mut response = ProblemDetails::new(413, "payload-too-large", "Payload Too Large")
        .with_detail(tabular::TooLarge.to_string())
        .into_response();
    *response.status_mut() = StatusCode::PAYLOAD_TOO_LARGE;
    response
}

/// Every entity the query reaches, read from the broker one page at a time (EP-44).
///
/// A file representation answers with the whole result set rather than one broker page,
/// so it pages until the broker runs out or the endpoint's row ceiling is reached. The
/// ceiling is a refusal and not a truncation: a short CSV looks exactly like a complete
/// one, and a caller who cannot tell will act on half the data.
pub(crate) async fn paged_entities(
    gateway: &Gateway,
    endpoint: &Endpoint,
    subject: &Subject,
    params: &[(String, String)],
    request: &mut Request,
    limits: &tabular::Limits,
) -> Result<(Value, bool), Box<Response<Body>>> {
    let slug = endpoint.slug.as_str();
    let verdict = gateway.pdp.decide(
        subject,
        Operation::QueryEntity,
        &query::requested(params),
        endpoint,
    );
    let Verdict::Rewrite(constraints) = verdict else {
        return Err(Box::new(ProblemDetails::forbidden().into_response()));
    };
    if constraints.empty {
        return Ok((Value::Array(Vec::new()), true));
    }
    tenancy::pin_tenant(request, &endpoint.space)
        .map_err(|_| Box::new(ProblemDetails::internal().into_response()))?;

    // The caller's own window is a ceiling on the download, not the page size: paging is
    // the gateway's business and the caller asked for a file, not for a page of one.
    let wanted = query::first(params, "limit")
        .and_then(|limit| limit.parse::<u64>().ok())
        .unwrap_or(u64::from(limits.max_rows))
        .min(u64::from(limits.max_rows));
    let windowless: Vec<(String, String)> = params
        .iter()
        .filter(|(name, _)| name != "limit" && name != "offset")
        .cloned()
        .collect();
    // The same selector of last resort as `query_entities`: a download names a dataset.
    let fallback = if query::selects(&constraints) {
        Vec::new()
    } else {
        let types = dataset_types(gateway, slug, request.headers()).await?;
        if types.is_empty() {
            return Ok((Value::Array(Vec::new()), constraints.restricted));
        }
        types
    };
    let narrowed = query::upstream(&windowless, &constraints, &fallback);
    let areas = geo::Areas::of(&constraints.geo_grants, constraints.geo_caller.as_deref());

    let mut collected: Vec<Value> = Vec::new();
    let mut held = 0u64;
    let mut offset = 0usize;
    loop {
        let target = format!("/ngsi-ld/v1/entities?{narrowed}&limit={PAGE}&offset={offset}");
        let answer = gateway
            .broker
            .send(
                axum::http::Method::GET,
                &target,
                request.headers().clone(),
                Body::empty(),
            )
            .await
            .map_err(|error| Box::new(ProblemDetails::from(error).into_response()))?;

        let (parts, body) = answer.into_parts();
        if !parts.status.is_success() {
            return Err(broker_failed(slug, parts.status, body).await);
        }
        let bytes = axum::body::to_bytes(body, MAX_BODY)
            .await
            .map_err(|_| Box::new(ProblemDetails::internal().into_response()))?;
        let page: Value = serde_json::from_slice(&bytes)
            .map_err(|_| Box::new(ProblemDetails::internal().into_response()))?;
        let page = match page {
            Value::Array(entities) => entities,
            entity if entity.is_object() => vec![entity],
            _ => Vec::new(),
        };

        let fetched = page.len();
        // What the broker sent, whether or not the entity survives the grants: it was read,
        // parsed and held either way, and this is a ceiling on the gateway's memory rather
        // than a statement about the file (T-0810). A download past it is refused, because a
        // 413 is a better answer than the 500 an out-of-memory gateway gives everyone.
        held += bytes.len() as u64;
        collected.extend(page.into_iter().filter(|entity| {
            projection::permitted(entity, &constraints)
                && areas.as_ref().is_none_or(|areas| areas.admits(entity))
        }));
        if collected.len() as u64 > wanted || held > MAX_COLLECTED {
            return Err(Box::new(too_large()));
        }
        // A short page is the last page; a full one may not be.
        if fetched < PAGE {
            break;
        }
        offset += PAGE;
    }

    let mut entities = Value::Array(collected);
    projection::project_by_type(&mut entities, &constraints);
    Ok((entities, constraints.restricted))
}
