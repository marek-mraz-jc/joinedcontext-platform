//! The reads every export surface shares: the types a dataset holds, one query through the
//! broker's view, a temporal query, and the capped page walk the downloads are made of.

use crate::app::Gateway;
use crate::app::MAX_BODY;
use crate::app::MAX_COLLECTED;
use crate::app::PAGE;
use crate::app::{has_view, names_no_tenant, ANTARES_RESTRICTED};
use crate::handlers::view_access::no_view;
use crate::pdp::evaluator::Subject;
use crate::resolver::Endpoint;
use crate::translators::tabular;
use crate::{middleware::tenancy, query};
use axum::body::Body;
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
async fn dataset_types(
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

/// One page of the entities behind an endpoint through the broker's view (plan 5.7, T-3651).
/// The engine decides and narrows; the gateway adds no grant and projects nothing, so the
/// representation holds what the view answers the caller and no more. An endpoint without a
/// view answers `503 no-view` (T-3652).
pub(crate) async fn read_entities(
    gateway: &Gateway,
    endpoint: &Endpoint,
    subject: &Subject,
    params: &[(String, String)],
    request: &mut Request,
) -> Result<(Value, bool), Box<Response<Body>>> {
    let slug = endpoint.slug.as_str();
    if !has_view(slug) {
        return Err(Box::new(no_view(endpoint)));
    }
    view_hop(endpoint, subject, request)?;
    let Some(fallback) = view_fallback(gateway, slug, params, request.headers()).await? else {
        return Ok((Value::Array(Vec::new()), false));
    };
    if fallback.len() < 2 {
        let target = format!("entities?{}", query::view_read(params, &fallback));
        let (page, restricted, _, _) =
            view_page(gateway, slug, slug, &target, request.headers()).await?;
        return Ok((Value::Array(page), restricted));
    }
    // One type at a time (T-4078): the caller's window is cut out of the types read in turn,
    // so a page never asks the view for more than `offset + limit` rows of the space.
    let window = |name: &str, default: usize| {
        query::first(params, name)
            .and_then(|value| value.parse::<usize>().ok())
            .unwrap_or(default)
    };
    let (offset, limit) = (window("offset", 0), window("limit", DEFAULT_LIMIT));
    let windowless: Vec<(String, String)> = params
        .iter()
        .filter(|(name, _)| name != "limit" && name != "offset")
        .cloned()
        .collect();
    let wanted = offset.saturating_add(limit);
    let mut collected: Vec<Value> = Vec::new();
    let mut restricted = false;
    let mut held = 0u64;
    let mut refused = 0;
    'types: for selector in &fallback {
        let narrowed = query::view_read(&windowless, std::slice::from_ref(selector));
        let mut from = 0usize;
        loop {
            let target = format!("entities?{narrowed}&limit={PAGE}&offset={from}");
            let (page, narrowed_page, bytes, _) =
                match view_page(gateway, slug, slug, &target, request.headers()).await {
                    Ok(page) => page,
                    // A type the view refuses this caller is a type it does not show.
                    Err(answer) if answer.status() == StatusCode::FORBIDDEN => {
                        refused += 1;
                        continue 'types;
                    }
                    Err(failed) => return Err(failed),
                };
            restricted |= narrowed_page;
            let fetched = page.len();
            // A deep offset reads the rows before it: the download's memory ceiling holds (T-0810).
            held += bytes as u64;
            if held > MAX_COLLECTED {
                return Err(Box::new(too_large()));
            }
            collected.extend(page);
            if collected.len() >= wanted {
                break 'types;
            }
            if fetched < PAGE {
                break;
            }
            from += PAGE;
        }
    }
    if refused == fallback.len() {
        return Err(Box::new(ProblemDetails::forbidden().into_response()));
    }
    let page: Vec<Value> = collected.into_iter().skip(offset).take(limit).collect();
    Ok((Value::Array(page), restricted))
}

/// The page size CIM 009 6.3.10 gives a query that names none.
const DEFAULT_LIMIT: usize = 20;

/// The history of one entity through the broker's view, decided and narrowed by its engine as
/// the current state is (T-0438, plan 5.7). An entity outside the view and one with no history
/// both answer `Null`; an endpoint without a view answers `503 no-view` (T-3652).
pub(crate) async fn query_temporal(
    gateway: &Gateway,
    endpoint: &Endpoint,
    subject: &Subject,
    urn: &str,
    params: &[(String, String)],
    request: &mut Request,
) -> Result<(Value, bool), Box<Response<Body>>> {
    let slug = endpoint.slug.as_str();
    if !has_view(slug) {
        return Err(Box::new(no_view(endpoint)));
    }
    view_hop(endpoint, subject, request)?;
    let target = format!(
        "/x/views/{slug}/ngsi-ld/v1/temporal/entities/{}?{}",
        query::encode(urn),
        query::view_read_by_id(params)
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
    // The engine refusing the operation itself is a refusal here too (EP-07): this
    // representation is no softer than the NGSI-LD surface.
    if answer.status() == StatusCode::FORBIDDEN {
        return Err(Box::new(ProblemDetails::forbidden().into_response()));
    }
    let restricted = restricted_by(answer.headers());
    let (parts, body) = answer.into_parts();
    // No history and an entity outside the view both mean "no observations here", and the
    // caller learns nothing from the difference (R20).
    if !parts.status.is_success() {
        return Ok((Value::Null, restricted));
    }
    let bytes = axum::body::to_bytes(body, MAX_BODY)
        .await
        .map_err(|_| Box::new(ProblemDetails::internal().into_response()))?;
    let mut entity: Value = serde_json::from_slice(&bytes)
        .map_err(|_| Box::new(ProblemDetails::internal().into_response()))?;
    if let Some(first) = entity.as_array().and_then(|list| list.first()).cloned() {
        entity = first;
    }
    Ok((entity, restricted))
}

/// The gateway's own answer to a broker that failed a read behind one of its representations
/// (T-2340, EP-26, R22).
///
/// A download and a SensorThings collection are the gateway's documents, and what a
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
const PARALLEL_PAGES: usize = 4;

/// Every entity the query reaches, read from the broker one page at a time (EP-44).
///
/// A file representation answers with the whole result set rather than one broker page,
/// so it pages until the broker runs out or the endpoint's row ceiling is reached. The
/// ceiling is a refusal and not a truncation: a short CSV looks exactly like a complete
/// one, and a caller who cannot tell will act on half the data.
///
/// `view` is the broker's view the surface reads through: the endpoint's slug, or `cs-{space}`
/// for the space's dump (plan 5.1). A view id the broker cannot hold answers `503 no-view`
/// (T-3652).
pub(crate) async fn paged_entities(
    gateway: &Gateway,
    endpoint: &Endpoint,
    subject: &Subject,
    (params, view): (&[(String, String)], &str),
    request: &mut Request,
    limits: &tabular::Limits,
) -> Result<(Value, bool), Box<Response<Body>>> {
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
) -> Result<(Value, bool), Box<Response<Body>>> {
    let slug = endpoint.slug.as_str();
    view_hop(endpoint, subject, request)?;
    let wanted = query::first(params, "limit")
        .and_then(|limit| limit.parse::<u64>().ok())
        .unwrap_or(u64::from(limits.max_rows))
        .min(u64::from(limits.max_rows));
    let windowless: Vec<(String, String)> = params
        .iter()
        .filter(|(name, _)| name != "limit" && name != "offset")
        .cloned()
        .collect();
    let Some(fallback) = view_fallback(gateway, view, &windowless, request.headers()).await? else {
        return Ok((Value::Array(Vec::new()), false));
    };
    // The caller's own selector as one query; the selector of last resort one type at a time
    // (T-4078), each paged to its end, all under the one ceiling of the download.
    let selectors: Vec<String> = if fallback.is_empty() {
        vec![query::view_read(&windowless, &fallback)]
    } else {
        fallback
            .iter()
            .map(|name| query::view_read(&windowless, std::slice::from_ref(name)))
            .collect()
    };

    // Every selector's first page with the broker's count, then every further page the counts
    // name, PARALLEL_PAGES at a time (T-4082): read one after another, a 25 000-entity download
    // took 20-28 s behind a broker answering a page in about 0.9 s. Each selector's pages stay
    // in their order, and the ceilings hold as each page arrives.
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
    let targets: Vec<String> = selectors
        .iter()
        .map(|narrowed| format!("entities?{narrowed}&limit={PAGE}&offset=0{counted}"))
        .collect();
    let firsts: Vec<_> = stream::iter(targets)
        .map(read)
        .buffered(PARALLEL_PAGES)
        .collect()
        .await;

    let mut chunks: Vec<Vec<Value>> = Vec::with_capacity(selectors.len());
    let mut further: Vec<(usize, String)> = Vec::new();
    let mut restricted = false;
    let mut held = 0u64;
    let mut rows = 0u64;
    let mut refused = 0;
    for (index, (narrowed, first)) in selectors.iter().zip(firsts).enumerate() {
        let (page, narrowed_page, bytes, total) = match first {
            Ok(first) => first,
            // A type of the selector of last resort the view refuses this caller is a type the
            // download does not hold; the caller's own selector refused is the refusal.
            Err(answer) if !fallback.is_empty() && answer.status() == StatusCode::FORBIDDEN => {
                refused += 1;
                chunks.push(Vec::new());
                continue;
            }
            Err(failed) => return Err(failed),
        };
        restricted |= narrowed_page;
        // What the broker sent is held either way: a ceiling on the gateway's memory (T-0810).
        held += bytes as u64;
        let full = page.len() >= PAGE;
        let mut chunk = page;
        if full {
            match total {
                // The ceiling is known before the rest is read.
                Some(total) if total > wanted => return Err(Box::new(too_large())),
                Some(total) => further.extend(
                    (PAGE..usize::try_from(total).unwrap_or(usize::MAX))
                        .step_by(PAGE)
                        .map(|offset| {
                            (
                                index,
                                format!("entities?{narrowed}&limit={PAGE}&offset={offset}"),
                            )
                        }),
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
                        chunk.extend(page);
                        if rows + chunk.len() as u64 > wanted || held > MAX_COLLECTED {
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
        rows += chunk.len() as u64;
        if rows > wanted || held > MAX_COLLECTED {
            return Err(Box::new(too_large()));
        }
        chunks.push(chunk);
    }

    if refused > 0 && refused == selectors.len() {
        return Err(Box::new(ProblemDetails::forbidden().into_response()));
    }
    let (owners, targets): (Vec<usize>, Vec<String>) = further.into_iter().unzip();
    let mut pages = stream::iter(targets).map(read).buffered(PARALLEL_PAGES);
    let mut owners = owners.into_iter();
    while let (Some(page), Some(index)) = (pages.next().await, owners.next()) {
        let (page, narrowed_page, bytes, _) = page?;
        restricted |= narrowed_page;
        held += bytes as u64;
        rows += page.len() as u64;
        if rows > wanted || held > MAX_COLLECTED {
            return Err(Box::new(too_large()));
        }
        if let Some(chunk) = chunks.get_mut(index) {
            chunk.extend(page);
        }
    }
    Ok((Value::Array(chunks.concat()), restricted))
}

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
    let held = view_fallback(gateway, slug, &[], request.headers())
        .await?
        .unwrap_or_default();
    let mut shown = Vec::new();
    let mut refused = 0;
    for name in &held {
        let target = format!(
            "entities?{}&limit=1",
            query::view_read(&[], std::slice::from_ref(name))
        );
        match view_page(gateway, slug, slug, &target, request.headers()).await {
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

/// Pins the tenant and the verified caller for a hop to the view; a caller who asked about
/// narrowing has the broker asked too (R22).
fn view_hop(
    endpoint: &Endpoint,
    subject: &Subject,
    request: &mut Request,
) -> Result<(), Box<Response<Body>>> {
    tenancy::pin_caller(request, &endpoint.space, subject)
        .map_err(|_| Box::new(ProblemDetails::internal().into_response()))
}

/// The selector of last resort for a view read: nothing when the caller named a selector,
/// every type the space holds when not, and `None` for a space holding nothing, which is an
/// empty answer rather than a `400` (EP-09).
///
/// The view refuses a query whose types compose past its rule bound, and the 21 types of the
/// Helsinki space did, as a `403` (T-4078), so the callers read these types one at a time and
/// skip a type the view refuses the caller; the view decides each type (T-3652).
async fn view_fallback(
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
async fn view_page(
    gateway: &Gateway,
    slug: &str,
    view: &str,
    target: &str,
    headers: &HeaderMap,
) -> Result<(Vec<Value>, bool, usize, Option<u64>), Box<Response<Body>>> {
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
        return Ok((Vec::new(), false, 0, Some(0)));
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
    let page: Value = serde_json::from_slice(&bytes)
        .map_err(|_| Box::new(ProblemDetails::internal().into_response()))?;
    let page = match page {
        Value::Array(entities) => entities,
        entity if entity.is_object() => vec![entity],
        _ => Vec::new(),
    };
    Ok((page, restricted, bytes.len(), total))
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
