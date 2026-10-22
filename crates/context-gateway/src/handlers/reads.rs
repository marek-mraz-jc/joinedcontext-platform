//! The reads every export surface shares: the types a dataset holds, one query through the
//! broker's view, a temporal query, and the capped page walk the downloads are made of.

use crate::app::{Gateway, MAX_BODY, MAX_COLLECTED, PAGE};
use crate::handlers::ngsi_ld::{has_view, ANTARES_RESTRICTED};
use crate::handlers::view_access::no_view;
use crate::handlers::writes::names_no_tenant;
use crate::pdp::evaluator::Subject;
use crate::resolver::Endpoint;
use crate::translators::tabular;
use crate::{middleware::tenancy, query};
use axum::body::{Body, Bytes};
use axum::extract::Request;
use axum::http::header::ACCEPT;
use axum::http::{HeaderMap, HeaderValue, Response, StatusCode};
use axum::response::IntoResponse;
use futures_util::stream::{self, StreamExt};
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

/// The gateway's own answer to a broker that failed a read behind one of its representations
/// (T-2340, EP-26, R22).
///
/// A download is the gateway's document, and what a
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

/// How many broker pages one download reads at once (T-4082).
pub(super) const PARALLEL_PAGES: usize = 4;

/// Every entity the query reaches, read from the broker one page at a time (EP-44).
///
/// A file representation answers with the whole result set rather than one broker page,
/// so it pages until the broker runs out or the endpoint's row ceiling is reached. The
/// ceiling is a refusal and not a truncation: a short CSV looks exactly like a complete
/// one, and a caller who cannot tell will act on half the data.
///
/// `view` is the broker's view the surface reads through, the endpoint's slug. A view id the
/// broker cannot hold answers `503 no-view` (T-3652).
pub(crate) async fn paged_entities(
    gateway: &Gateway,
    endpoint: &Endpoint,
    subject: &Subject,
    (params, view): (&[(String, String)], &str),
    request: &mut Request,
    limits: &tabular::Limits,
) -> Result<(tabular::Table, bool), Box<Response<Body>>> {
    if !has_view(view) {
        return Err(Box::new(no_view(endpoint)));
    }
    paged_view(gateway, endpoint, subject, (params, view), request, limits).await
}

/// [`paged_entities`] through the broker's view (plan 5.7, T-3651): the caller's own query, paged
/// out of `/x/views/{view}/ngsi-ld/v1/entities` under the endpoint's ceilings. The engine decides
/// and narrows each page; the gateway adds no grant, filters nothing out and projects nothing,
/// so a download holds exactly what the view answers the caller and no more.
async fn paged_view(
    gateway: &Gateway,
    endpoint: &Endpoint,
    subject: &Subject,
    (params, view): (&[(String, String)], &str),
    request: &mut Request,
    limits: &tabular::Limits,
) -> Result<(tabular::Table, bool), Box<Response<Body>>> {
    let slug = endpoint.slug.as_str();
    let window = window(params)?;
    view_hop(endpoint, subject, request)?;
    // The row ceiling is the refusal (EP-44); `limit` and `offset` cut the answer (T-4591).
    let wanted = u64::from(limits.max_rows);
    let windowless: Vec<(String, String)> = params
        .iter()
        .filter(|(name, _)| name != "limit" && name != "offset")
        .cloned()
        .collect();
    let Some(types) = selector(gateway, view, &windowless, request.headers()).await? else {
        return Ok((tabular::Table::default(), false));
    };
    let narrowed = query::view_read(&windowless, &types);

    // The first page with the broker's count, then every further page the count names,
    // PARALLEL_PAGES at a time (T-4082): read one after another, a 25 000-entity download took
    // 20-28 s behind a broker answering a page in about 0.9 s. The pages stay in their order,
    // and the ceilings hold as each page arrives.
    let headers = request.headers().clone();
    let read = |target: String| {
        let headers = &headers;
        async move { view_page(gateway, slug, view, &target, headers).await }
    };
    // A `q` the view checks entity by entity cannot be counted under the broker's scan ceiling:
    // it answers TooManyResults past 10 000 stored rows (T-4324), so such a read is walked page
    // by page, as a broker that sends no count is.
    let counted = if query::first(&windowless, "q").is_some() {
        ""
    } else {
        "&count=true"
    };
    let (page, mut restricted, bytes, total) = read(format!(
        "entities?{narrowed}&limit={PAGE}&offset=0{counted}"
    ))
    .await?;
    // What the broker sent is held either way: a ceiling on the gateway's memory (T-0810).
    let mut held = bytes as u64;
    let full = page.len() >= PAGE;
    // Each page goes into the table as it arrives and its entities are dropped (T-4782).
    let mut seen: usize = 0;
    let mut table = tabular::table(&in_window(page, window, &mut seen), limits)
        .map_err(|_| Box::new(too_large()))?;
    let mut further = Vec::new();
    if full {
        match total {
            // The ceiling is known before the rest is read.
            Some(total) if total > wanted => return Err(Box::new(too_large())),
            Some(total) => further.extend(
                (PAGE..usize::try_from(total).unwrap_or(usize::MAX))
                    .step_by(PAGE)
                    .map(|offset| format!("entities?{narrowed}&limit={PAGE}&offset={offset}")),
            ),
            // A broker that sends no count is walked page by page, as before.
            None => {
                let mut offset = PAGE;
                loop {
                    let target = format!("entities?{narrowed}&limit={PAGE}&offset={offset}");
                    let (page, narrowed_page, bytes, _) = read(target).await?;
                    restricted |= narrowed_page;
                    held += bytes as u64;
                    let fetched = page.len();
                    table.append(&in_window(page, window, &mut seen));
                    if seen as u64 > wanted || held > MAX_COLLECTED {
                        return Err(Box::new(too_large()));
                    }
                    if fetched < PAGE {
                        break;
                    }
                    offset += PAGE;
                }
            }
        }
    }
    if seen as u64 > wanted || held > MAX_COLLECTED {
        return Err(Box::new(too_large()));
    }
    // Read ahead as bytes and parsed one at a time as each joins the table (T-5182).
    let ahead = |target: String| {
        let headers = &headers;
        async move { view_bytes(gateway, slug, view, &target, headers).await }
    };
    let mut pages = stream::iter(further).map(ahead).buffered(PARALLEL_PAGES);
    while let Some(page) = pages.next().await {
        let (bytes, narrowed_page, _) = page?;
        restricted |= narrowed_page;
        held += bytes.len() as u64;
        table.append(&in_window(parse_page(&bytes)?, window, &mut seen));
        if seen as u64 > wanted || held > MAX_COLLECTED {
            return Err(Box::new(too_large()));
        }
    }
    Ok((table, restricted))
}

/// The entities of `page` inside the caller's `(offset, limit)` window, `seen` the rows read
/// before it and counted on: a row outside the window counts against the ceiling and is not kept.
fn in_window(page: Vec<Value>, (offset, limit): (usize, usize), seen: &mut usize) -> Value {
    let first = *seen;
    *seen += page.len();
    Value::Array(
        page.into_iter()
            .enumerate()
            .filter(|(index, _)| {
                (first + index)
                    .checked_sub(offset)
                    .is_some_and(|at| at < limit)
            })
            .map(|(_, entity)| entity)
            .collect(),
    )
}

/// The `offset` and `limit` a download asks for (T-4591), none the whole answer; a value that is
/// no whole number is the caller's mistake, said, never a silent default.
fn window(params: &[(String, String)]) -> Result<(usize, usize), Box<Response<Body>>> {
    let read = |name: &str, none: usize| match query::first(params, name).map(str::parse) {
        Some(Err(_)) => Err(Box::new(
            ProblemDetails::new(400, "bad-request", "Bad Request")
                .with_detail(format!("{name} must be a whole number"))
                .into_response(),
        )),
        Some(Ok(value)) => Ok(value),
        None => Ok(none),
    };
    Ok((read("offset", 0)?, read("limit", usize::MAX)?))
}

/// Pins the tenant and the verified caller for a hop to the view; a caller who asked about
/// narrowing has the broker asked too (R22).
pub(super) fn view_hop(
    endpoint: &Endpoint,
    subject: &Subject,
    request: &mut Request,
) -> Result<(), Box<Response<Body>>> {
    tenancy::pin_caller(request, &endpoint.space, subject)
        .map_err(|_| Box::new(ProblemDetails::internal().into_response()))
}

/// The types a view read names when the caller named no selector: every type the space holds,
/// as a query names at least one (5.7.2); nothing when the caller named a selector; and `None`
/// for a space holding nothing, which is an empty answer rather than a `400` (EP-09). The read is
/// one request: the view decides each type in it and leaves out a type it refuses the caller,
/// refusing the read only when it refuses every one (T-4455, T-3652).
async fn selector(
    gateway: &Gateway,
    slug: &str,
    params: &[(String, String)],
    headers: &HeaderMap,
) -> Result<Option<Vec<String>>, Box<Response<Body>>> {
    if !query::unselected(params) {
        return Ok(Some(Vec::new()));
    }
    let types = dataset_types(gateway, slug, headers).await?;
    Ok((!types.is_empty()).then_some(types))
}

/// One page of entities from `/x/views/{view}/ngsi-ld/v1/{target}`, whether the broker marked
/// it narrowed, and its size. A space with no tenant at the broker yet is an empty page (EP-22);
/// the engine's refusal is the platform's `403`; any other failure is [`broker_failed`].
pub(super) async fn view_page(
    gateway: &Gateway,
    slug: &str,
    view: &str,
    target: &str,
    headers: &HeaderMap,
) -> Result<(Vec<Value>, bool, usize, Option<u64>), Box<Response<Body>>> {
    let (bytes, restricted, total) = view_bytes(gateway, slug, view, target, headers).await?;
    Ok((parse_page(&bytes)?, restricted, bytes.len(), total))
}

/// [`view_page`] unparsed: a download holds the pages it reads ahead as JSON, about a fifteenth of
/// their parsed form, and parses each as it joins the table (T-5182).
async fn view_bytes(
    gateway: &Gateway,
    slug: &str,
    view: &str,
    target: &str,
    headers: &HeaderMap,
) -> Result<(Bytes, bool, Option<u64>), Box<Response<Body>>> {
    // An entity array is read here, whatever the caller accepts: asked with its geo+json, the
    // view answers a FeatureCollection that reads as no entity (T-4111, T-4106).
    let mut headers = headers.clone();
    headers.insert(ACCEPT, HeaderValue::from_static("application/json"));
    let answer = gateway
        .broker
        .send(
            axum::http::Method::GET,
            &format!("/x/views/{view}/ngsi-ld/v1/{target}"),
            headers,
            Body::empty(),
        )
        .await
        .map_err(|error| Box::new(ProblemDetails::from(error).into_response()))?;
    let restricted = restricted_by(answer.headers());
    // The total a `count=true` read is answered with (CIM 009 6.3.13).
    let total = answer
        .headers()
        .get("NGSILD-Results-Count")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok());
    let (parts, body) = answer.into_parts();
    let bytes = axum::body::to_bytes(body, MAX_BODY)
        .await
        .map_err(|_| Box::new(ProblemDetails::internal().into_response()))?;
    if parts.status == StatusCode::NOT_FOUND && names_no_tenant(&bytes) {
        return Ok((Bytes::new(), false, Some(0)));
    }
    if parts.status == StatusCode::FORBIDDEN {
        // The broker's resource ceiling (CIM 009 TooManyResults) is no refusal by a Policy: the
        // caller asked for more than one read may hold, which is the download's own 413.
        if names_too_many_results(&bytes) {
            return Err(Box::new(too_large()));
        }
        return Err(Box::new(ProblemDetails::forbidden().into_response()));
    }
    if !parts.status.is_success() {
        return Err(broker_failed(slug, parts.status, Body::from(bytes)).await);
    }
    Ok((bytes, restricted, total))
}

/// The entities of a page [`view_bytes`] read: an array, a single entity, or none.
fn parse_page(bytes: &[u8]) -> Result<Vec<Value>, Box<Response<Body>>> {
    if bytes.is_empty() {
        return Ok(Vec::new());
    }
    let page: Value = serde_json::from_slice(bytes)
        .map_err(|_| Box::new(ProblemDetails::internal().into_response()))?;
    Ok(match page {
        Value::Array(entities) => entities,
        entity if entity.is_object() => vec![entity],
        _ => Vec::new(),
    })
}

/// Whether a broker answer is CIM 009's TooManyResults.
fn names_too_many_results(body: &[u8]) -> bool {
    serde_json::from_slice::<Value>(body).is_ok_and(|problem| {
        problem
            .get("type")
            .and_then(Value::as_str)
            .is_some_and(|kind| kind.ends_with("TooManyResults"))
    })
}

/// Whether the broker marked its answer narrowed (Antares `ANTARES_POLICY`, R22).
fn restricted_by(headers: &HeaderMap) -> bool {
    headers
        .get(ANTARES_RESTRICTED)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.eq_ignore_ascii_case("true"))
}
