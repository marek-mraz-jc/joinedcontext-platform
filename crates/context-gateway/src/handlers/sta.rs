//! The SensorThings API surface of an endpoint, one handler over its own resource tree (T-1474).

use crate::app::admit;
use crate::app::read_only;
use crate::app::typed_json_response;
use crate::app::Gateway;
use crate::app::MAX_SKIP;
use crate::app::PAGE;
use crate::handlers::ogc::bad_parameter;
use crate::handlers::reads::query_entities;
use crate::handlers::reads::query_temporal;
use crate::middleware::response::RESULTS_RESTRICTED;
use crate::pdp::evaluator::Subject;
use crate::resolver::Endpoint;
use crate::translators::{ogc, sta};
use crate::{middleware::tenancy, query};
use axum::body::Body;
use axum::extract::{Path, Request, State};
use axum::http::{HeaderValue, Method, Response, StatusCode};
use axum::response::IntoResponse;
use jc_core::kinds::Representation;
use jc_core::ProblemDetails;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;

/// Every SensorThings v1.1 request of one endpoint (T-0160, EP-12, EP-13, TS-08).
///
/// The Sensing profile's four linked entity sets are four views of one projected entity page,
/// so the handler fetches once and translates, rather than treating each set as its own query.
/// That is what keeps a `Datastream` and the `Thing` it belongs to from disagreeing about what
/// the caller may see (EP-06, EP-07).
pub(crate) async fn sensorthings(
    State(gateway): State<Arc<Gateway>>,
    Path(params): Path<HashMap<String, String>>,
    mut request: Request,
) -> Response<Body> {
    tenancy::strip_client_headers(&mut request);
    let slug = params.get("slug").cloned().unwrap_or_default();
    let (endpoint, subject) = match admit(
        &gateway,
        &slug,
        Some(Representation::Sta),
        request.headers(),
    ) {
        Ok(admitted) => admitted,
        Err(problem) => return *problem,
    };

    // EP-13: the representation is read-only, so every non-safe method is refused before a
    // path is parsed and before the broker is touched.
    let method = request.method().clone();
    if method == Method::OPTIONS {
        return read_only(StatusCode::NO_CONTENT);
    }
    if ![Method::GET, Method::HEAD].contains(&method) {
        return read_only(StatusCode::METHOD_NOT_ALLOWED);
    }

    let base = format!("{}{}", gateway.base_url(), endpoint.base_path);
    let caller = query::parse(request.uri().query().unwrap_or_default());
    let (top, skip, counted) = match sta_paging(&caller) {
        Ok(paging) => paging,
        Err(problem) => return bad_parameter(&problem),
    };
    let expand: Vec<&str> = query::first(&caller, "$expand")
        .map(|raw| raw.split(',').map(str::trim).collect())
        .unwrap_or_default();

    let segments = sta_segments(params.get("rest").map(String::as_str).unwrap_or_default());
    let addressed: Vec<(&str, Option<String>)> =
        segments.iter().map(|segment| sta_set(segment)).collect();

    // The service document is the only answer that needs no data at all.
    let Some((set, key)) = addressed.first() else {
        return typed_json_response(&sta::service_document(&base), sta::MEDIA_TYPE);
    };
    let child = addressed.get(1).map(|(set, _)| *set);
    if addressed.len() > 2 || (child.is_some() && key.is_none()) {
        return ProblemDetails::not_found().into_response();
    }
    // EP-13: a set the platform holds nothing for is empty rather than missing, so a
    // conformance suite can walk it; a set that is not in the profile at all is a 404.
    if sta::EMPTY_SETS.contains(set) && key.is_none() {
        return typed_json_response(
            &sta::collection(Vec::new(), counted.then_some(0), None),
            sta::MEDIA_TYPE,
        );
    }
    if !sta::SETS.contains(set) {
        return ProblemDetails::not_found().into_response();
    }

    // The one set that is a series rather than an instant, and so the one that comes from the
    // temporal tree instead of the entity page (T-0438, EP-12). Answered before the entity
    // query below, because a client charting a week must not be handed the single point the
    // current state carries, and because the entity query would be a second broker call for
    // an answer it cannot give.
    if let (Some("Datastreams"), Some(key), Some("Observations")) =
        (Some(*set), key.as_deref(), child)
    {
        return sta_series(
            &gateway,
            &endpoint,
            &subject,
            &mut request,
            &caller,
            &base,
            key,
        )
        .await;
    }

    let mut upstream = vec![
        ("limit".to_owned(), top.to_string()),
        ("offset".to_owned(), skip.to_string()),
    ];
    if let Some(key) = key {
        // Every id in this profile carries the entity URN in front of it, so addressing any
        // one of them is one entity query. An id that is not shaped that way names nothing.
        let Some(urn) = sta::urn_of(key) else {
            return ProblemDetails::not_found().into_response();
        };
        upstream.push(("id".to_owned(), urn.to_owned()));
    }
    if let Some(filter) = query::first(&caller, "$filter") {
        match sta::filter_to_q(filter) {
            Ok(q) => upstream.push(("q".to_owned(), q)),
            Err(problem) => {
                return bad_parameter(&ogc::ParamError {
                    parameter: "$filter",
                    detail: problem.0,
                })
            }
        }
    }

    let (entities, restricted) =
        match query_entities(&gateway, &endpoint, &subject, &upstream, &mut request).await {
            Ok(answer) => answer,
            Err(problem) => return *problem,
        };
    let page: Vec<&Value> = entities.as_array().into_iter().flatten().collect();

    let answer = match (*set, key.as_deref(), child) {
        ("Things", None, _) => sta::collection(
            page.iter()
                .filter_map(|entity| sta::thing(&base, entity, &expand))
                .collect(),
            counted.then_some(page.len()),
            None,
        ),
        ("Things", Some(_), None) => {
            match page
                .first()
                .and_then(|entity| sta::thing(&base, entity, &expand))
            {
                Some(thing) => thing,
                None => return ProblemDetails::not_found().into_response(),
            }
        }
        ("Things", Some(_), Some("Locations")) => {
            let items = page.first().and_then(|entity| sta::location(&base, entity));
            sta::collection(items.into_iter().collect(), counted.then_some(0), None)
        }
        ("Things", Some(_), Some("Datastreams")) => {
            let items = page
                .first()
                .map(|entity| sta::datastreams(&base, entity))
                .unwrap_or_default();
            sta::collection(items.clone(), counted.then_some(items.len()), None)
        }
        ("Locations", None, _) => {
            let items: Vec<Value> = page
                .iter()
                .filter_map(|entity| sta::location(&base, entity))
                .collect();
            sta::collection(items.clone(), counted.then_some(items.len()), None)
        }
        ("Datastreams" | "Observations" | "ObservedProperties", None, _) => {
            let items: Vec<Value> = page
                .iter()
                .flat_map(|entity| sta_items(set, &base, entity))
                .collect();
            sta::collection(items.clone(), counted.then_some(items.len()), None)
        }
        (set @ ("Datastreams" | "Observations" | "ObservedProperties"), Some(key), None) => {
            let Some(item) = page
                .first()
                .into_iter()
                .flat_map(|entity| sta_items(set, &base, entity))
                .find(|item| item["@iot.id"].as_str() == Some(key))
            else {
                return ProblemDetails::not_found().into_response();
            };
            item
        }
        _ => return ProblemDetails::not_found().into_response(),
    };

    let mut response = typed_json_response(&answer, sta::MEDIA_TYPE);
    if restricted {
        response
            .headers_mut()
            .insert(RESULTS_RESTRICTED, HeaderValue::from_static("true"));
    }
    response
}

/// The `Observations` of one `Datastream`: the history of one attribute (T-0438, EP-12).
///
/// The series is bounded before it is buffered, not after. `lastN` is what the broker is asked
/// for and it is the page the caller asked for plus what they skipped, so a datastream holding
/// a year of minutes costs one page either way; the same ceiling the rest of the
/// representation uses (`$top`, clamped to the gateway's own) is what bounds it.
async fn sta_series(
    gateway: &Gateway,
    endpoint: &Endpoint,
    subject: &Subject,
    request: &mut Request,
    caller: &[(String, String)],
    base: &str,
    key: &str,
) -> Response<Body> {
    // A key that is not `{urn}/{attribute}` names no datastream, so it has no observations
    // rather than every entity's.
    let Some((urn, attribute)) = sta::split_stream_id(key) else {
        return ProblemDetails::not_found().into_response();
    };
    let (top, skip, counted) = match sta_paging(caller) {
        Ok(paging) => paging,
        Err(problem) => return bad_parameter(&problem),
    };

    // One more instance than the page needs, so a full page can tell that there is another
    // one without a second query. Without the peek a `$top` request could never offer
    // `@iot.nextLink`, because the ceiling and the answer would always be the same size.
    let ceiling = top.saturating_add(skip);
    // `$skip` becomes `lastN` here, and one request asks the broker for at most
    // `LAST_N_CAP` instances (GW26). A deeper page would be cut to the cap and answer with the
    // wrong rows, so it is refused by name; a `$filter` window reaches older observations.
    let deepest = (query::LAST_N_CAP as usize).saturating_sub(1);
    if ceiling > deepest {
        return bad_parameter(&ogc::ParamError {
            parameter: "$skip",
            detail: format!(
                "$top and $skip together reach at most {deepest} observations back; narrow the series with $filter on phenomenonTime"
            ),
        });
    }
    let mut upstream = vec![
        ("attrs".to_owned(), attribute.to_owned()),
        ("lastN".to_owned(), ceiling.saturating_add(1).to_string()),
    ];
    if let Some(filter) = query::first(caller, "$filter") {
        match sta::series_filter(filter) {
            Ok(series) => {
                upstream.extend(series.window);
                if let Some(q) = series.q {
                    upstream.push(("q".to_owned(), q));
                }
            }
            Err(problem) => {
                return bad_parameter(&ogc::ParamError {
                    parameter: "$filter",
                    detail: problem.0,
                })
            }
        }
    }

    let (entity, restricted) =
        match query_temporal(gateway, endpoint, subject, urn, &upstream, request).await {
            Ok(answer) => answer,
            Err(problem) => return *problem,
        };

    let mut items = sta::temporal_observations(base, &entity, attribute);
    if sta::newest_first(query::first(caller, "$orderby")) {
        // The broker's order is its own; `$orderby` is the client's, applied to the page it
        // is given rather than asked of a parameter NGSI-LD does not have.
        items.sort_by(|left, right| {
            right["phenomenonTime"]
                .as_str()
                .cmp(&left["phenomenonTime"].as_str())
        });
    }
    let total = items.len();
    let page: Vec<Value> = items.into_iter().skip(skip).take(top).collect();
    let next = (total > skip + page.len()).then(|| {
        format!(
            "{}/Observations?$top={top}&$skip={}",
            sta::datastream_link(base, key),
            skip + top
        )
    });

    // `@iot.count` is the total, and the total is only known when the broker returned less
    // than the ceiling: a series that hit it may hold more instants than were asked for, and
    // a count that is really the ceiling is a wrong number on somebody's chart.
    let count = counted.then_some(total).filter(|total| *total <= ceiling);
    let answer = sta::collection(page, count, next);
    let mut response = typed_json_response(&answer, sta::MEDIA_TYPE);
    if restricted {
        response
            .headers_mut()
            .insert(RESULTS_RESTRICTED, HeaderValue::from_static("true"));
    }
    response
}

/// `$top`, `$skip` and `$count`: the page an STA request asked for, bounded by the gateway's
/// own ceiling. One place, because the entity page and the temporal series have to agree on it.
fn sta_paging(caller: &[(String, String)]) -> Result<(usize, usize, bool), ogc::ParamError> {
    let top = query::first(caller, "$top")
        .and_then(|top| top.parse::<usize>().ok())
        .unwrap_or(sta::DEFAULT_TOP)
        .clamp(1, PAGE);
    let skip = match query::first(caller, "$skip") {
        None => 0,
        // An offset is what the caller has already read, so it bounds what the gateway asks
        // the broker for: on a Datastream's Observations it becomes `lastN`, one history
        // instance per skipped item (T-0809). Past the ceiling it is a refusal rather than a
        // clamp, because a silently clamped offset would answer a page the caller did not ask
        // for and page through the same rows forever.
        Some(raw) => match raw.parse::<usize>() {
            Ok(skip) if skip <= MAX_SKIP => skip,
            _ => {
                return Err(ogc::ParamError {
                    parameter: "$skip",
                    detail: format!("$skip must be a whole number of items, at most {MAX_SKIP}"),
                })
            }
        },
    };
    let counted = query::first(caller, "$count").is_some_and(|value| value == "true");
    Ok((top, skip, counted))
}

/// The items of one entity for one derived set.
fn sta_items(set: &str, base: &str, entity: &Value) -> Vec<Value> {
    match set {
        "Datastreams" => sta::datastreams(base, entity),
        "Observations" => sta::observations(base, entity),
        _ => sta::observed_properties(base, entity),
    }
}

/// The path segments of an STA resource path, without splitting inside a key literal.
///
/// A `Datastream` id is `{urn}/{attribute}`, so the slash inside the quotes is part of the
/// name and not a step down the tree. Splitting naively is how `Datastreams('a/b')/Observations`
/// becomes three segments that address nothing.
fn sta_segments(rest: &str) -> Vec<String> {
    let mut segments = Vec::new();
    let mut current = String::new();
    let mut quoted = false;
    for character in rest.chars() {
        match character {
            '\'' => {
                quoted = !quoted;
                current.push(character);
            }
            '/' if !quoted => {
                if !current.is_empty() {
                    segments.push(std::mem::take(&mut current));
                }
            }
            _ => current.push(character),
        }
    }
    if !current.is_empty() {
        segments.push(current);
    }
    segments
}

/// One segment as the set it names and the key it addresses, if any.
fn sta_set(segment: &str) -> (&str, Option<String>) {
    match segment.split_once('(') {
        Some((set, rest)) => {
            let key = rest.strip_suffix(')').unwrap_or(rest);
            let key = key.strip_prefix('\'').unwrap_or(key);
            let key = key.strip_suffix('\'').unwrap_or(key);
            (set, Some(key.replace("''", "'")))
        }
        None => (segment, None),
    }
}
