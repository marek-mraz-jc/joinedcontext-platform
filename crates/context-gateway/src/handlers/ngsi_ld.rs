//! The ETSI resource tree under an Endpoint and a Context Space: what a request passes on its way
//! to the broker (T-0005, EP-01, GW1).
//!
//! One handler serves the whole ETSI resource tree, because every request on it goes
//! through the same six steps and skipping one of them for a "simple" path is how a
//! gateway leaks:
//!
//! 1. remove everything the client claimed about its identity or tenancy (GW20),
//! 2. resolve the slug, or answer 404 without saying whether it exists (EP-03),
//! 3. name the operation, or answer 404: an unmapped path is not an NGSI-LD operation,
//! 4. ask the PDP, and refuse on anything but a rewrite (GW1),
//! 5. check a write payload whole, and narrow a read's query to the grants (GW11, GW17),
//! 6. forward with the tenant pinned, then project the answer back down (R9, R22).

use crate::app::{Gateway, MAX_BODY};
use crate::auth::subject::Subject;
use crate::auth::subject::{admit, admit_space, principal_of};
use crate::handlers::respond::{json_response, typed_json_response};
use crate::handlers::schema;
use crate::handlers::writes::{
    accepted_payload, batch_entry_id, batch_result, body_length, empty_space, filled_units,
    judge_write, merged_batch, names_no_tenant, owned_guarded, quality_judged, refusal_entries,
    refused, unsupported_media_type, with_filled_units, write_share, HeldDelete, HeldWrite, Judged,
    Refused,
};
use crate::middleware::response::RESULTS_RESTRICTED;
use crate::pdp::evaluator::Constraints;
use crate::pdp::write_guard;
use crate::proxy::{self};
use crate::resolver::{DeclaredTypes, Endpoint};
use crate::{middleware::tenancy, operations, query};
use axum::body::Body;
use axum::extract::{Path, Request, State};
use axum::http::header::{AUTHORIZATION, CONTENT_LENGTH, CONTENT_LOCATION, LINK, LOCATION};
use axum::http::{HeaderMap, HeaderValue, Method, Response, StatusCode};
use axum::response::IntoResponse;
use axum::routing::{any, get};
use axum::Router;
use jc_core::kinds::{Operation, Representation};
use jc_core::ProblemDetails;
use serde_json::Value;
use std::sync::Arc;

/// The ETSI resource tree under an endpoint slug, with every refusal rendered the way an
/// NGSI-LD client reads it.
pub(crate) async fn ngsi_ld(
    State(gateway): State<Arc<Gateway>>,
    Path((slug, _rest)): Path<(String, String)>,
    mut request: Request,
) -> Response<Body> {
    // Before routing, before authentication, before anything reads a header (EP-21).
    tenancy::strip_client_headers(&mut request);
    let admitted = admit(
        &gateway,
        &slug,
        Some(Representation::NgsiLd),
        request.headers(),
    );
    let prefix = format!("/api/endpoint/{slug}/ngsi-ld/v1");
    let view = |endpoint: &Endpoint| endpoint.slug.clone();
    as_ngsi_ld_error(serve_ngsi_ld_view(&gateway, admitted, &prefix, view, request).await).await
}

/// The entry document at the NGSI-LD access URL (EP-90): where the entities are, the types this
/// caller may read with a query for each, and the access and schema documents. The gateway's
/// own answer, not a CIM 009 resource; the types are the schema surface's projection, so it
/// names none the grants withhold.
pub(crate) async fn ngsi_ld_entry(
    State(gateway): State<Arc<Gateway>>,
    Path(slug): Path<String>,
    mut request: Request,
) -> Response<Body> {
    tenancy::strip_client_headers(&mut request);
    let (endpoint, subject) = match admit(
        &gateway,
        &slug,
        Some(Representation::NgsiLd),
        request.headers(),
    ) {
        Ok(admitted) => admitted,
        Err(problem) => return as_ngsi_ld_error(*problem).await,
    };
    let root = format!("{}{}", gateway.base_url(), endpoint.base_path);
    let visible = schema::visible(&subject, &endpoint, crate::pdp::now());
    let types: Vec<Value> = schema::visible_types(&endpoint, &visible)
        .into_iter()
        .map(|name| {
            let url = format!("{root}/ngsi-ld/v1/entities?type={}", query::encode(&name));
            serde_json::json!({ "type": name, "query": url })
        })
        .collect();
    json_response(&serde_json::json!({
        "entities": format!("{root}/ngsi-ld/v1/entities"),
        "types": types,
        "access": format!("{root}/access"),
        "schema": format!("{root}/schema/index.json"),
        "openapi": format!("{root}/openapi.json"),
    }))
}

/// The same tree under a context space name (SP-03).
///
/// A stock NGSI-LD client pointed at `/cs/{space}` works unmodified, and it works through
/// the same handler an endpoint slug goes through: the only difference between the two
/// surfaces is the name in the path and the audience the token has to carry (SP-01, R15).
pub(crate) async fn space_ngsi_ld(
    State(gateway): State<Arc<Gateway>>,
    Path((space, _rest)): Path<(String, String)>,
    mut request: Request,
) -> Response<Body> {
    tenancy::strip_client_headers(&mut request);
    let admitted = admit_space(
        &gateway,
        &space,
        Some(Representation::NgsiLd),
        request.headers(),
    )
    .map(|(space, subject)| (Arc::clone(&space.endpoint), subject));
    let prefix = format!("/cs/{space}/ngsi-ld/v1");
    // The space surface's view is the one the Portal writes per Context Space (plan 5.1).
    let view = |endpoint: &Endpoint| format!("cs-{}", endpoint.space);
    as_ngsi_ld_error(serve_ngsi_ld_view(&gateway, admitted, &prefix, view, request).await).await
}

/// The error types of CIM 009 clause 5.5.2 the gateway's own refusals map to, by status.
///
/// The clause defines no type for 401, 403 (other than the two query-size ones), 415, 502 or
/// 503, so those keep the joinedcontext type URI: a made-up ETSI URI would be a lie a
/// client could act on, and a joinedcontext one is at least documented. Its only 503 type,
/// `LdContextNotAvailable`, means an `@context` that could not be fetched, which the gateway
/// never fetches: a busy gateway or a failed broker is not that (T-4438).
fn ngsi_ld_error_type(status: u16) -> Option<&'static str> {
    Some(match status {
        400 => "https://uri.etsi.org/ngsi-ld/errors/BadRequestData",
        404 => "https://uri.etsi.org/ngsi-ld/errors/ResourceNotFound",
        409 => "https://uri.etsi.org/ngsi-ld/errors/AlreadyExists",
        422 => "https://uri.etsi.org/ngsi-ld/errors/OperationNotSupported",
        500 => "https://uri.etsi.org/ngsi-ld/errors/InternalError",
        _ => return None,
    })
}

/// Re-renders a problem document the way CIM 009 clause 5.5.3 wants it (T-0272).
///
/// The clause is explicit: errors on the NGSI-LD surface use `application/json`, not the
/// RFC 7807 media type, and carry `type`, `title` and `detail`. The broker already answers
/// that way; this makes the gateway's own refusals indistinguishable from it, so a client
/// that keys on `type` learns the same thing whichever of the two refused. The Portal API
/// keeps `application/problem+json`: a different surface, a different contract.
pub(crate) async fn as_ngsi_ld_error(response: Response<Body>) -> Response<Body> {
    let is_problem = response
        .headers()
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.starts_with(jc_core::PROBLEM_JSON));
    if !is_problem {
        return response;
    }

    let (mut parts, body) = response.into_parts();
    let Ok(bytes) = axum::body::to_bytes(body, 64 * 1024).await else {
        return ProblemDetails::internal().into_response();
    };
    let Ok(mut problem) = serde_json::from_slice::<Value>(&bytes) else {
        return Response::from_parts(parts, Body::from(bytes));
    };

    if let Some(members) = problem.as_object_mut() {
        if let Some(etsi) = ngsi_ld_error_type(parts.status.as_u16()) {
            // The joinedcontext slug stays as `code`, so the hint and the Portal's translation
            // still have their key once `type` is the ETSI one (T-3243, API/00 §4).
            let code = members
                .get("type")
                .and_then(Value::as_str)
                .and_then(|uri| uri.strip_prefix(jc_core::PROBLEM_TYPE_BASE))
                .map(str::to_owned);
            if let Some(code) = code {
                members.insert("code".to_owned(), Value::String(code));
            }
            members.insert("type".to_owned(), Value::String(etsi.to_owned()));
        }
        // Clause 6.3.3: `detail` is one of the three terms an error carries; a refusal that
        // names no reason still says so in a sentence rather than omitting the member.
        if !members.contains_key("detail") {
            let title = members.get("title").cloned().unwrap_or(Value::Null);
            members.insert("detail".to_owned(), title);
        }
    }
    parts.headers.insert(
        axum::http::header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    match serde_json::to_vec(&problem) {
        Ok(rendered) => proxy::with_body(parts, rendered),
        Err(_) => Response::from_parts(parts, Body::from(bytes)),
    }
}

/// GW31 and GW33: a query the specification refuses is refused as the caller sent it (a type or
/// `q` that does not parse), and so is one that names no selector, an `id` list or `idPattern`
/// alone included (CIM 009 5.7.2.4), with the way to the entry document. A grant decides which
/// of the well-formed answers a caller sees, never which status code the surface returns.
fn query_shape_refusal(params: &[(String, String)]) -> Option<Response<Body>> {
    if let Some(why) = query::malformed(params) {
        return Some(
            ProblemDetails::bad_request()
                .with_detail(why)
                .into_response(),
        );
    }
    query::unselected(params).then(|| {
        ProblemDetails::bad_request()
            .with_detail(
                "a query names at least one of type, attrs, q or georel; an id list or \
                 idPattern alone is not a selector (CIM 009 5.7.2.4). The entry at \
                 /ngsi-ld/v1/ lists the types you may query here, or retrieve one entity at \
                 /ngsi-ld/v1/entities/{id}",
            )
            .into_response()
    })
}

/// A broker's `5xx` as the caller gets it: the gateway's own document, and the broker's text
/// in the operator's log. What a broker says when it fails is whatever it was holding: an entity
/// id in a message, a fragment of its own query, the name of an attribute an endpoint hides
/// (T-2260). The broker's `Retry-After` is passed on: when to ask again is no secret of its own
/// (T-4438).
pub(crate) async fn broker_failure(
    status: StatusCode,
    retry_after: Option<HeaderValue>,
    body: Body,
) -> Response<Body> {
    let held = axum::body::to_bytes(body, MAX_BODY)
        .await
        .unwrap_or_default();
    let (kind, detail) = explanation(&held);
    tracing::warn!(
        status = status.as_u16(),
        broker_type = %kind,
        broker_detail = %detail,
        "the broker failed; its own explanation is not passed on"
    );
    if timed_out(status, &held) {
        return query_timeout().into_response();
    }
    let mut answer = ProblemDetails::new(
        status.as_u16(),
        "broker-failure",
        "The context broker could not answer",
    )
    .with_detail("the context broker behind this endpoint failed to answer this request")
    .into_response();
    if let Some(retry_after) = retry_after {
        answer.headers_mut().insert("retry-after", retry_after);
    }
    answer
}

/// The longest piece of the broker's explanation the gateway logs: its body can hold up to 8 MiB
/// of entity values, which never belong in a log line (F-SEC1-019).
const LOGGED_EXPLANATION: usize = 300;

/// The `type` and `detail` of the broker's problem document, each cut to [`LOGGED_EXPLANATION`]
/// characters; a body that is no problem document logs neither.
fn explanation(body: &[u8]) -> (String, String) {
    let problem = serde_json::from_slice::<Value>(body).unwrap_or(Value::Null);
    let member = |name: &str| {
        problem
            .get(name)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .chars()
            .take(LOGGED_EXPLANATION)
            .collect()
    };
    (member("type"), member("detail"))
}

/// The broker's `500` for a query its database cancelled at the statement timeout (Antares names
/// it in the detail, CIM 009 5.5.2 InternalError). Asking again asks the same query, so the caller
/// gets a `504` that says to ask for less instead of a broker failure to retry (T-5616).
pub(crate) fn timed_out(status: StatusCode, body: &[u8]) -> bool {
    status == StatusCode::INTERNAL_SERVER_ERROR
        && serde_json::from_slice::<Value>(body).is_ok_and(|problem| {
            problem.get("detail").and_then(Value::as_str) == Some("database statement timeout")
        })
}

/// The answer to a query the broker did not finish in time.
pub(crate) fn query_timeout() -> ProblemDetails {
    ProblemDetails::new(504, "query-timeout", "Query Timeout")
        .with_detail("the context broker did not finish this query in time")
}

/// The NGSI-LD operation a request stands for, or the answer to one that stands for none: Set
/// Attribute Value names the PATCH it is offered as (422), anything else is not found.
fn operation_or_answer(
    method: &Method,
    path: &str,
    details: bool,
) -> Result<Operation, Box<Response<Body>>> {
    operations::operation_of(method, path, details).ok_or_else(|| {
        Box::new(match operations::sets_attribute_value(method, path) {
            Some(attribute) => ProblemDetails::for_status(422)
                .with_detail(format!(
                    "Set Attribute Value is not offered through an Endpoint; write the value \
                     with PATCH /ngsi-ld/v1/entities/{{id}}/attrs/{attribute}"
                ))
                .into_response(),
            None => ProblemDetails::not_found().into_response(),
        })
    })
}

/// The request body, up to the organization's limit, or the answer that names the limit.
pub(crate) async fn read_body(
    gateway: &Gateway,
    body: Body,
) -> Result<Vec<u8>, Box<Response<Body>>> {
    let limit = gateway.max_request_body();
    axum::body::to_bytes(body, limit)
        .await
        .map(Vec::from)
        .map_err(|_| {
            Box::new(
                ProblemDetails::bad_request()
                    .with_detail(format!(
                        "the request body is unreadable or larger than the {} MiB this \
                         organization accepts (spec.limits.gateway.maxRequestBodyMegabytes); \
                         send less at once, or ask an organization admin to raise it in \
                         Organization settings",
                        limit / (1024 * 1024)
                    ))
                    .into_response(),
            )
        })
}

/// Reserves one of the day's creates of a form Endpoint (EP-97), per caller as the rate limit
/// keys them, so one sender takes a tenth at most (T-3285). `Some(now)` is the reservation to
/// release when the broker does not take the create.
pub(crate) fn reserve_create(
    gateway: &Gateway,
    endpoint: &Endpoint,
    caller: &str,
    operation: Operation,
) -> Result<Option<u64>, Box<Response<Body>>> {
    let Some(per_day) = endpoint
        .creates
        .as_ref()
        .and_then(|creates| creates.per_day)
    else {
        return Ok(None);
    };
    if operation != Operation::CreateEntity {
        return Ok(None);
    }
    let now = crate::creates::now();
    let Err((retry_after, spent)) = gateway
        .creates
        .reserve(&endpoint.slug, caller, per_day, now)
    else {
        return Ok(Some(now));
    };
    let detail = match spent {
        crate::creates::Spent::Form => {
            tracing::info!(slug = %endpoint.slug, "the endpoint's creates for today are spent");
            "this form has taken all the entries it accepts today; retry after the seconds \
             Retry-After names"
        }
        crate::creates::Spent::Caller => {
            tracing::info!(slug = %endpoint.slug, "one caller's share of today's creates is spent");
            "you have sent this form as many entries as one sender may today; retry after the \
             seconds Retry-After names"
        }
    };
    let mut refusal = ProblemDetails::new(429, "too-many-requests", "Too Many Requests")
        .with_detail(detail)
        .into_response();
    refusal
        .headers_mut()
        .insert("retry-after", HeaderValue::from(retry_after));
    Err(Box::new(refusal))
}

/// Who spends a form's daily creates: the signed-in principal, else the caller's address
/// (T-4590). Every surface keys the same way, so one person is one share whichever surface they
/// write through, and two people behind one address are two.
pub(crate) fn create_key(subject: &Subject, headers: &HeaderMap) -> String {
    match principal_of(subject) {
        principal if principal != "anonymous" => format!("principal:{principal}"),
        _ => crate::middleware::rate_limit::address_key(headers),
    }
}

/// The longest view id the broker accepts (`[a-z0-9-]{1,63}`, ADR-0023).
pub(crate) const MAX_VIEW_ID: usize = 63;

/// The answer for an Endpoint the broker holds no view of: its slug is longer than a view id can
/// be, so the Portal's writer refused the view and the Endpoint's status says so. Nobody decides
/// for it, so nothing is answered (fail closed).
pub(crate) fn no_view(endpoint: &Endpoint) -> Response<Body> {
    tracing::error!(slug = %endpoint.slug, "no broker view serves this endpoint's access surface");
    ProblemDetails::new(
        503,
        "no-view",
        "The access surface of this endpoint is not served",
    )
    .with_detail("no context broker view serves this endpoint; its status in the Portal says why")
    .into_response()
}

/// Whether the broker holds a view `view` of an Endpoint (plan 5.1, T-3643): every Endpoint has
/// one, unless its slug is longer than a view id can be (the writer refuses it).
pub(crate) fn has_view(view: &str) -> bool {
    (1..=MAX_VIEW_ID).contains(&view.len())
}

/// The broker's view of an Endpoint (plan 5.1): its slug, or `cs-{space}` for a space's
/// canonical surface, the one whose root is `/cs/{segment}` (T-3643).
pub(crate) fn view_of(endpoint: &Endpoint) -> String {
    if endpoint.base_path.starts_with("/cs/") {
        format!("cs-{}", endpoint.space)
    } else {
        endpoint.slug.clone()
    }
}

/// One NGSI-LD request through the broker's view of the Endpoint (plan 5.1 and 5.2, T-3643):
/// `{prefix}{rest}` goes to `/x/views/{view}/ngsi-ld/v1{rest}` with the caller's query as sent,
/// and the broker's engine decides and narrows it under the `x-jc-*` subject the gateway
/// verified. The gateway keeps what is not a decision: the identifier check, the domain gate,
/// the media type, the core context, a form's minted ids and daily creates, the model's units
/// and relationships. An Endpoint without a view answers `503 no-view`, as its other surfaces do.
pub(crate) async fn serve_ngsi_ld_view(
    gateway: &Gateway,
    admitted: Result<(Arc<Endpoint>, Subject), Box<Response<Body>>>,
    prefix: &str,
    view: impl Fn(&Endpoint) -> String,
    request: Request,
) -> Response<Body> {
    let (endpoint, subject) = match admitted {
        Ok(admitted) => admitted,
        Err(problem) => return *problem,
    };
    let method = request.method().clone();
    let uri = request.uri().clone();
    let path = uri
        .path()
        .strip_prefix(prefix)
        .unwrap_or_default()
        .to_owned();
    let params = query::parse(uri.query().unwrap_or_default());
    let details = query::first(&params, "details") == Some("true");
    let operation = match operation_or_answer(&method, &path, details) {
        Ok(operation) => operation,
        Err(answer) => return *answer,
    };
    let view = view(&endpoint);
    let mut request = request;
    if let Err(answer) = answered_here(
        gateway,
        (&endpoint, &subject),
        operation,
        &view,
        &params,
        &mut request,
    )
    .await
    {
        return *answer;
    }
    let _share = match write_share(gateway, &endpoint, operation, body_length(&request)).await {
        Ok(share) => share,
        Err(answer) => return *answer,
    };
    // The audit line names the principal the gateway established (PF-46); the decision is the
    // broker's, and its answer says what it was.
    tracing::info!(
        slug = %endpoint.slug,
        space = %endpoint.space,
        %view,
        principal = %principal_of(&subject),
        agreement = subject.agreement.as_deref().unwrap_or_default(),
        operation = %operation.as_str(),
        "forwarded to the broker's view"
    );
    match view_request(gateway, &endpoint, &subject, &path, operation, &mut request).await {
        Ok(()) => {}
        Err(answer) => return *answer,
    }

    let (mut parts, body) = request.into_parts();
    let mut sent = match read_body(gateway, body).await {
        Ok(sent) => sent,
        Err(answer) => return *answer,
    };
    let mut divided: Option<(Vec<String>, Refused)> = None;
    // A view of another model takes no write (the broker's 405, `mapping::READS`), so no body is
    // judged against the source model first (T-3799).
    let mapped_write = endpoint.view_mapping.is_some() && operation.is_write();
    let payload = if mapped_write {
        Ok(Vec::new())
    } else {
        view_payload(
            gateway,
            (&endpoint, &subject),
            (&path, &params, operation),
            &mut parts.headers,
            &mut sent,
            &mut divided,
        )
        .await
    };
    let units_filled = match payload {
        Ok(filled) => filled,
        Err(answer) => return *answer,
    };

    let target = view_target(&view, &path, uri.query());
    let request = Request::from_parts(parts, Body::from(sent));
    let answer =
        match send_reserved(gateway, &endpoint, &subject, operation, &target, request).await {
            Ok(answer) => answer,
            Err(answer) => return *answer,
        };
    let answer = match map_view_answer(answer, operation).await {
        Ok(answer) => answer,
        Err(answer) => return *answer,
    };
    let mut answer = match divided {
        Some((forwarded, refused)) => merged_batch(answer, forwarded, refused).await,
        None => answer,
    };
    caller_links(
        answer.headers_mut(),
        &format!("/x/views/{view}/ngsi-ld/v1"),
        prefix,
    );
    caller_answer(answer, (&endpoint, &subject), operation, &units_filled).await
}

/// What the gateway answers itself before a view request is judged: an Endpoint without a
/// view (`503 no-view`), a query of a shape no view answers, and the Endpoint's type list as
/// its reads show the caller (T-4080). `Ok` goes on to the view.
async fn answered_here(
    gateway: &Gateway,
    (endpoint, subject): (&Endpoint, &Subject),
    operation: Operation,
    view: &str,
    params: &[(String, String)],
    request: &mut Request,
) -> Result<(), Box<Response<Body>>> {
    if !has_view(view) {
        return Err(Box::new(no_view(endpoint)));
    }
    if operation == Operation::QueryEntity {
        if let Some(refusal) = query_shape_refusal(params) {
            return Err(Box::new(refusal));
        }
    }
    // A caller without a read grant, and the details form, are the view's to answer.
    if operation != Operation::RetrieveEntityTypes
        || request.method() != Method::GET
        || view != endpoint.slug
    {
        return Ok(());
    }
    match crate::handlers::types_list::listed_types(gateway, endpoint, subject, request).await? {
        Some(list) => Err(Box::new(typed_json_response(&list, "application/json"))),
        None => Ok(()),
    }
}

/// The view's answer as the caller reads it: the restriction header under the name the
/// specification gives, a read logged where the Endpoint serves personal attributes (DM-128),
/// and the units the gateway filled in.
async fn caller_answer(
    mut answer: Response<Body>,
    (endpoint, subject): (&Endpoint, &Subject),
    operation: Operation,
    units_filled: &[String],
) -> Response<Body> {
    // The broker marks a narrowed answer in its own header, and only when the caller opted in
    // (`x-jc-restricted`); the caller reads it under the name the specification gives (R22).
    let headers = answer.headers_mut();
    if let Some(restricted) = headers.remove(ANTARES_RESTRICTED) {
        headers.insert(RESULTS_RESTRICTED, restricted);
    }
    if !operation.is_write() && answer.status().is_success() {
        answer = logged_read(answer, endpoint, subject).await;
    }
    with_filled_units(answer, units_filled)
}

/// The broker's address of one view request, with the caller's query, its length capped.
fn view_target(view: &str, path: &str, query: Option<&str>) -> String {
    match query {
        Some(query) if !query.is_empty() => {
            format!("/x/views/{view}/ngsi-ld/v1{path}?{}", capped_query(query))
        }
        _ => format!("/x/views/{view}/ngsi-ld/v1{path}"),
    }
}

/// Sends one view request to the broker under a create reservation of the caller's daily
/// creates (EP-97): a create the broker did not answer with success gives its reservation back.
async fn send_reserved(
    gateway: &Gateway,
    endpoint: &Endpoint,
    subject: &Subject,
    operation: Operation,
    target: &str,
    request: Request,
) -> Result<Response<Body>, Box<Response<Body>>> {
    let (parts, body) = request.into_parts();
    // After `pin_caller` the token is gone from the headers: a signed-in caller is keyed by
    // who they are, not by the address they share (T-4590).
    let caller = create_key(subject, &parts.headers);
    let reserved = reserve_create(gateway, endpoint, &caller, operation)?;
    let sent = gateway
        .broker
        .send(parts.method, target, parts.headers, body)
        .await;
    let succeeded = sent
        .as_ref()
        .is_ok_and(|answer| answer.status().is_success());
    if let Some(now) = reserved.filter(|_| !succeeded) {
        gateway.creates.release(&endpoint.slug, &caller, now);
    }
    sent.map_err(|error| Box::new(ProblemDetails::from(error).into_response()))
}

/// The broker's answer to a view request, mapped where the caller must not read it as sent: a
/// failure of the broker, a refusal naming the platform's slug, and the miss of a space with no
/// tenant yet. `Err` is the final answer; `Ok` goes on to the caller's decoration.
async fn map_view_answer(
    answer: Response<Body>,
    operation: Operation,
) -> Result<Response<Body>, Box<Response<Body>>> {
    if answer.status().is_server_error() {
        let (parts, body) = answer.into_parts();
        let retry = parts.headers.get("retry-after").cloned();
        return Err(Box::new(broker_failure(parts.status, retry, body).await));
    }
    if answer.status() == StatusCode::FORBIDDEN {
        return Err(Box::new(with_platform_slug(answer).await));
    }
    // A space nobody has written to yet has no tenant at the broker, and the caller was
    // admitted to it: a listing answers with nothing, a read of one thing with the plain miss,
    // and the broker's wording, which names the tenant, never reaches the caller (EP-22, R20).
    if answer.status() != StatusCode::NOT_FOUND || operation.is_write() {
        return Ok(answer);
    }
    let (parts, body) = answer.into_parts();
    let held = axum::body::to_bytes(body, MAX_BODY)
        .await
        .unwrap_or_default();
    if !names_no_tenant(&held) {
        return Err(Box::new(proxy::with_body(parts, held)));
    }
    Err(Box::new(match empty_space(operation) {
        Some(empty) => json_response(&empty),
        None => ProblemDetails::not_found().into_response(),
    }))
}

/// DM-128: a read through an Endpoint that serves personal attributes leaves one line with the
/// reader, the Endpoint and the Entities' URNs; the time is the line's own, and no value is
/// written. An answer too large to list is refused, never served unlogged.
async fn logged_read(
    answer: Response<Body>,
    endpoint: &Endpoint,
    subject: &Subject,
) -> Response<Body> {
    let personal = endpoint.personal_attributes();
    if personal.is_empty() {
        return answer;
    }
    let (parts, body) = answer.into_parts();
    let Ok(held) = axum::body::to_bytes(body, MAX_BODY).await else {
        let mut refused = ProblemDetails::new(413, "payload-too-large", "Payload Too Large")
            .with_detail("the answer holds personal data and is too large to log who read it; ask for fewer Entities")
            .into_response();
        *refused.status_mut() = StatusCode::PAYLOAD_TOO_LARGE;
        return refused;
    };
    log_personal_read(endpoint, subject, &personal, &read_entities(&held));
    proxy::with_body(parts, held)
}

/// The DM-128 line of one read of `personal` attributes: the reader, the Endpoint and the
/// Entities' URNs, never a value.
pub(crate) fn log_personal_read(
    endpoint: &Endpoint,
    subject: &Subject,
    personal: &[String],
    entities: &[String],
) {
    tracing::info!(
        target: "personal_read",
        slug = %endpoint.slug,
        space = %endpoint.space,
        principal = %principal_of(subject),
        attributes = %personal.join(","),
        entities = %entities.join(","),
        "personal attributes read"
    );
}

/// The URNs of the Entities an NGSI-LD answer holds: one Entity, a list of them, or a GeoJSON
/// FeatureCollection, and every Entity `join=inline` nested in a Relationship's `entity` or a
/// ListRelationship's `entityList`, at any `joinLevel` (F-SEC1-025).
fn read_entities(body: &[u8]) -> Vec<String> {
    let Ok(answer) = serde_json::from_slice::<Value>(body) else {
        return Vec::new();
    };
    let items = match &answer {
        Value::Array(items) => items.as_slice(),
        Value::Object(members) => match members.get("features").and_then(Value::as_array) {
            Some(features) => features.as_slice(),
            None => std::slice::from_ref(&answer),
        },
        _ => &[],
    };
    let mut ids = Vec::new();
    for item in items {
        inlined(item, &mut ids);
    }
    ids
}

/// Collects the id of `entity` and of every Entity inlined in its attributes.
fn inlined(entity: &Value, ids: &mut Vec<String>) {
    let Some(members) = entity.as_object() else {
        return;
    };
    if let Some(id) = members.get("id").and_then(Value::as_str) {
        ids.push(id.to_owned());
    }
    // A GeoJSON Feature carries the attributes under `properties`.
    let feature = members.get("properties").and_then(Value::as_object);
    for attribute in members
        .values()
        .chain(feature.into_iter().flat_map(|p| p.values()))
    {
        // An attribute with several instances is a list of them.
        for instance in std::slice::from_ref(attribute)
            .iter()
            .chain(attribute.as_array().into_iter().flatten())
        {
            if let Some(nested) = instance.get("entity") {
                inlined(nested, ids);
            }
            for nested in instance
                .get("entityList")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                inlined(nested, ids);
            }
        }
    }
}

/// The engine's refusal keeps the broker's type (plan O-8) and gains the platform's slug as
/// `code` with its hint, the members the NGSI-LD surface carries them in (API/00 §4, T-3243), so
/// a client keyed on the slug still says who can give access.
pub(crate) async fn with_platform_slug(answer: Response<Body>) -> Response<Body> {
    let (parts, body) = answer.into_parts();
    let held = axum::body::to_bytes(body, MAX_BODY)
        .await
        .unwrap_or_default();
    let Ok(mut problem) = serde_json::from_slice::<Value>(&held) else {
        return proxy::with_body(parts, held);
    };
    let denied = problem["type"] == ANTARES_ACCESS_DENIED;
    if let Some(members) = problem.as_object_mut().filter(|_| denied) {
        members
            .entry("code")
            .or_insert_with(|| Value::from("forbidden"));
        if let Some(forbidden) = jc_core::error::problem_type("forbidden") {
            members
                .entry("hint")
                .or_insert_with(|| Value::from(forbidden.hint));
        }
    }
    proxy::with_body(parts, problem.to_string().into_bytes())
}

/// The type of the broker's refusal (Antares `configuration.md`, `ANTARES_POLICY`).
const ANTARES_ACCESS_DENIED: &str = "urn:antares:error:AccessDenied";

/// The broker's narrowing marker (Antares `configuration.md`, `ANTARES_POLICY`).
pub(crate) const ANTARES_RESTRICTED: &str = "antares-results-restricted";

/// The broker names its view root in `Location`, `Content-Location` and `Link` (views.md 2);
/// the caller reaches the same resource under their own prefix, and the mount is internal.
pub(crate) fn caller_links(headers: &mut HeaderMap, root: &str, prefix: &str) {
    for name in [LOCATION, CONTENT_LOCATION, LINK] {
        let rewritten: Vec<HeaderValue> = headers
            .get_all(&name)
            .iter()
            .map(|value| {
                value
                    .to_str()
                    .ok()
                    .and_then(|text| HeaderValue::from_str(&text.replace(root, prefix)).ok())
                    .unwrap_or_else(|| value.clone())
            })
            .collect();
        if rewritten.is_empty() {
            continue;
        }
        headers.remove(&name);
        for value in rewritten {
            headers.append(&name, value);
        }
    }
}

/// The caller's query as sent, with the two costs one request may put on the shared broker
/// capped as the gateway's own pipeline caps them: `lastN` (GW26) and `joinLevel` (AG-25). The
/// broker refuses a `joinLevel` past 10 and pages a temporal read (S17, T-3643); these caps are
/// tighter and silent, as callers already rely on. Every other pair goes on byte for byte.
pub(crate) fn capped_query(raw: &str) -> String {
    raw.split('&')
        .map(|pair| match pair.split_once('=') {
            Some(("lastN", n)) if n.parse::<u64>().is_ok_and(|n| n > query::LAST_N_CAP) => {
                format!("lastN={}", query::LAST_N_CAP)
            }
            Some(("joinLevel", n)) if n.parse::<u64>().is_ok_and(|n| n > query::JOIN_LEVEL_CAP) => {
                format!("joinLevel={}", query::JOIN_LEVEL_CAP)
            }
            _ => pair.to_owned(),
        })
        .collect::<Vec<_>>()
        .join("&")
}

/// The checks of a view request that need no body: the domain gate on a write (PF-41), a
/// federated read's write refused (T-3999), the identifier (PF-43), then the tenant and the
/// verified caller pinned for the hop (T-3641), with the token a `caller` registration of the
/// space receives (PF-48, PF-113).
/// A token the gateway exchanged for a federated read reads entities; it never writes and never
/// touches a subscription, which would outlive it (T-3999, PF-114).
fn federated_refusal(
    endpoint: &Endpoint,
    subject: &Subject,
    operation: Operation,
) -> Option<Response<Body>> {
    if subject.via.as_deref() != Some(tenancy::GATEWAY_CLIENT) {
        return None;
    }
    let subscription = matches!(
        operation,
        Operation::CreateSubscription
            | Operation::UpdateSubscription
            | Operation::RetrieveSubscription
            | Operation::QuerySubscription
            | Operation::DeleteSubscription
    );
    if !operation.is_write() && !subscription {
        return None;
    }
    tracing::info!(slug = %endpoint.slug, %operation, "refused: a federated read's token");
    Some(
        ProblemDetails::forbidden()
            .with_detail("a token exchanged for a federated read only reads entities")
            .into_response(),
    )
}

/// Nobody anonymous registers, changes or removes a subscription, whatever a public Policy
/// grants: a stored subscription holds broker resources and notifies an address the caller
/// chose, so it needs a caller the platform can name (User-Guide/05, F-BNT1-003).
fn anonymous_subscription_refusal(
    subject: &Subject,
    operation: Operation,
) -> Option<Response<Body>> {
    let changes = matches!(
        operation,
        Operation::CreateSubscription
            | Operation::UpdateSubscription
            | Operation::DeleteSubscription
    );
    (changes && principal_of(subject) == "anonymous").then(|| {
        let mut answer = ProblemDetails::new(401, "unauthorized", "Unauthorized")
            .with_detail(
                "sign in to register or change a subscription; anonymous callers only read",
            )
            .into_response();
        answer.headers_mut().insert(
            axum::http::header::WWW_AUTHENTICATE,
            axum::http::HeaderValue::from_static("Bearer"),
        );
        answer
    })
}

/// The longest `JC-Reason` the gateway takes, in characters once decoded (DM-113).
const MAX_REASON: usize = 500;

/// A delete of context data, and every write of it from the Portal's UI, names why in
/// `JC-Reason` (ADR-N-054 2.3, DM-113); without it the answer is `400 reason-required`. The UI is
/// the `portal-ui` client of a verified person's token, never a header the caller types. A
/// subscription is no write of context data and needs none.
fn reason_refusal(
    operation: Operation,
    subject: &Subject,
    headers: &axum::http::HeaderMap,
) -> Option<Response<Body>> {
    let deletes = matches!(
        operation,
        Operation::DeleteAttrs
            | Operation::DeleteEntity
            | Operation::DeleteBatch
            | Operation::DeleteAttrsTemporal
            | Operation::DeleteAttrInstanceTemporal
            | Operation::DeleteTemporal
    );
    let subscription = matches!(
        operation,
        Operation::CreateSubscription
            | Operation::UpdateSubscription
            | Operation::DeleteSubscription
    );
    let ui_write = operation.is_write() && !subscription && tenancy::provenance(subject).0 == "ui";
    // The reason travels percent-encoded: `%20%20` is as blank as no reason (DM-113: 1 to 500).
    let given = headers
        .get(tenancy::REASON)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|r| (1..=MAX_REASON).contains(&query::decode(r).trim().chars().count()));
    ((deletes || ui_write) && !given).then(|| {
        let detail = if deletes {
            "a delete names why in JC-Reason"
        } else {
            "a write from the Portal names why in JC-Reason"
        };
        ProblemDetails::new(400, "reason-required", "Reason Required")
            .with_detail(detail)
            .into_response()
    })
}

pub(crate) async fn view_request(
    gateway: &Gateway,
    endpoint: &Endpoint,
    subject: &Subject,
    path: &str,
    operation: Operation,
    request: &mut Request,
) -> Result<(), Box<Response<Body>>> {
    if let Some(refusal) = anonymous_subscription_refusal(subject, operation) {
        tracing::info!(slug = %endpoint.slug, %operation, "refused: an anonymous subscription");
        return Err(Box::new(refusal));
    }
    if operation.is_write() {
        if let Some(refusal) = gateway.domain_gate.refusal(&gateway.org_domain) {
            tracing::info!(slug = %endpoint.slug, %operation, "write refused: the domain is not verified");
            return Err(Box::new(refusal.into_response()));
        }
    }
    if let Some(refusal) = federated_refusal(endpoint, subject, operation) {
        return Err(Box::new(refusal));
    }
    let federated = if operation.is_write() {
        None
    } else {
        gateway
            .federation_token(endpoint, request.headers().get(AUTHORIZATION))
            .await?
    };
    if let Some(raw) = operations::addressed_entity(path) {
        if let Err(refusal) = write_guard::check_identifier(&query::decode(raw), None) {
            return Err(Box::new(ProblemDetails::from(refusal).into_response()));
        }
    }
    if let Some(refusal) = reason_refusal(operation, subject, request.headers()) {
        tracing::info!(slug = %endpoint.slug, %operation, "write refused: no reason given");
        return Err(Box::new(refusal));
    }
    tenancy::pin_caller(request, &endpoint.space, subject).map_err(|problem| {
        tracing::error!(space = %endpoint.space, %problem, "endpoint names an unusable space");
        Box::new(ProblemDetails::internal().into_response())
    })?;
    if let Some(federated) = federated {
        request.headers_mut().insert(AUTHORIZATION, federated);
    }
    // ADR-N-054 2.8: the broker keeps every change through an audited Endpoint (DM-136)
    if operation.is_write() && gateway.audited(&endpoint.slug) {
        request.headers_mut().insert(
            tenancy::AUDIT,
            axum::http::HeaderValue::from_static("changes"),
        );
    }
    // ADR-N-054 2.4: the broker answers `versions=all` only on this grant (DM-114)
    if crate::pdp::evaluator::grants_action(
        subject,
        jc_core::kinds::PlatformAction::ReadVersions,
        &endpoint.policies,
        crate::pdp::now(),
    ) {
        request.headers_mut().insert(
            tenancy::READ_VERSIONS,
            axum::http::HeaderValue::from_static("true"),
        );
    }
    Ok(())
}

/// Why a subscription's notification endpoint may not be stored, read off its body: the target
/// [`crate::notification_target::refusal`] judges. A body that is no JSON, or a fragment that
/// names no endpoint, is the broker's to answer.
fn subscription_target_refusal(sent: &[u8], private_hosts: &[String]) -> Option<ProblemDetails> {
    let payload: Value = serde_json::from_slice(sent).ok()?;
    match payload.get("notification")?.get("endpoint")?.get("uri")? {
        Value::String(uri) => crate::notification_target::refusal(uri, private_hosts),
        _ => Some(
            ProblemDetails::bad_request()
                .with_detail("the notification endpoint's uri is a string"),
        ),
    }
}

/// The payload of a view request as the gateway keeps it: the media type (CIM 009 6.3.2), the
/// core context (T-3287), a form's minted id and one entity per create (EP-97), the model's
/// units (DM-06) and relationships (DM-70, DM-71). Returns the units it filled; a batch the
/// relationships divide leaves its refused members in `divided`.
pub(crate) async fn view_payload(
    gateway: &Gateway,
    (endpoint, subject): (&Endpoint, &Subject),
    (path, params, operation): (&str, &[(String, String)], Operation),
    headers: &mut HeaderMap,
    sent: &mut Vec<u8>,
    divided: &mut Option<(Vec<String>, Refused)>,
) -> Result<Vec<String>, Box<Response<Body>>> {
    let answer = |problem: ProblemDetails| Box::new(problem.into_response());
    if operation.is_write() && !sent.is_empty() && !accepted_payload(headers) {
        return Err(answer(unsupported_media_type()));
    }
    if let Err(why) = crate::context_guard::check_link(headers)
        .and_then(|()| crate::context_guard::check_body(sent))
    {
        tracing::info!(slug = %endpoint.slug, "refused: a context other than the core one");
        return Err(answer(ProblemDetails::bad_request().with_detail(why)));
    }
    // A subscription is a standing query, not an entity: the view intersects it with the grant
    // at match time and `pre_notify` decides each notification under the subscriber (T-3499).
    // What is the gateway's is where it delivers to (G11, T-3653).
    if matches!(
        operation,
        Operation::CreateSubscription | Operation::UpdateSubscription
    ) {
        return match subscription_target_refusal(sent, &gateway.private_hosts) {
            Some(problem) => {
                tracing::info!(slug = %endpoint.slug, "subscription refused: its notification endpoint");
                Err(answer(problem))
            }
            None => Ok(Vec::new()),
        };
    }
    let before = sent.len();
    one_entity_per_create(endpoint, operation, sent)?;
    // What the Endpoint asks of a write whatever the grants are: an id that is an NGSI-LD URN
    // of its type (PF-43), the entity of the path (PF-44), a type of the space's model (DM-61),
    // no access control inside the data (GW29). With no constraint the grant checks pass; the
    // broker's view and its engine decide the grants. A batch keeps what passes (GW18).
    if operation.is_write() && !sent.is_empty() {
        judged_write(endpoint, path, operation, sent, divided)?;
    }
    if operation.is_write() {
        owned_guarded(sent, path).map_err(|problem| answer(*problem))?;
    }
    let declared = endpoint.declared_types.as_ref();
    let mut filled = Vec::new();
    if let Some(declared) = declared.filter(|_| operation.is_write() && !sent.is_empty()) {
        filled = model_judged(
            gateway,
            (endpoint, subject),
            (path, params, operation),
            declared,
            sent,
            divided,
        )?;
    }
    if let Some(rules) = declared
        .map(|declared| &declared.relationships)
        .filter(|rules| !rules.is_empty())
    {
        relationships_held(
            gateway,
            (endpoint, subject),
            (path, params, operation),
            (&*headers, rules),
            sent,
            divided,
        )
        .await?;
    }
    // Last, so every check above judged the value the caller sent and only the broker sees it
    // minimised (DM-121): MCP writes pass here too.
    if operation.is_write() && !sent.is_empty() {
        if let Some(schema) = declared.and_then(|declared| declared.schema.as_deref()) {
            crate::handlers::writes::minimised(gateway, endpoint, path, schema, sent)
                .await
                .map_err(|problem| answer(*problem))?;
        }
    }
    if sent.len() != before {
        headers.remove(CONTENT_LENGTH);
        headers.insert(CONTENT_LENGTH, HeaderValue::from(sent.len() as u64));
    }
    Ok(filled)
}

/// A form's minted id on a create, and one entity per create on an Endpoint that counts its
/// creates (EP-97): the batch forms are refused there.
fn one_entity_per_create(
    endpoint: &Endpoint,
    operation: Operation,
    sent: &mut Vec<u8>,
) -> Result<(), Box<Response<Body>>> {
    let Some(creates) = &endpoint.creates else {
        return Ok(());
    };
    match operation {
        Operation::CreateEntity if creates.mint_ids && !sent.is_empty() => {
            *sent = crate::creates::minted(sent)
                .map_err(|problem| Box::new(problem.into_response()))?;
        }
        Operation::CreateBatch | Operation::UpsertBatch | Operation::UpsertTemporal => {
            return Err(Box::new(
                ProblemDetails::bad_request()
                    .with_detail("this endpoint takes one entity per create, at POST /entities")
                    .into_response(),
            ));
        }
        _ => {}
    }
    Ok(())
}

/// The Endpoint's own judgement of a write body (PF-43, PF-44, DM-61, GW29): a batch keeps the
/// members that pass, and its refused members wait in `divided` for the merged answer (GW18).
fn judged_write(
    endpoint: &Endpoint,
    path: &str,
    operation: Operation,
    sent: &mut Vec<u8>,
    divided: &mut Option<(Vec<String>, Refused)>,
) -> Result<(), Box<Response<Body>>> {
    let unconstrained = Constraints::default();
    match judge_write(sent, path, operation, &unconstrained, endpoint) {
        Err(problem) => Err(Box::new(problem.into_response())),
        Ok(Judged::Whole) => Ok(()),
        Ok(Judged::Divided { permitted, refused }) => {
            if permitted.is_empty() {
                return Err(Box::new(batch_result(Vec::new(), refusal_entries(refused))));
            }
            let ids = permitted.iter().filter_map(batch_entry_id).collect();
            *sent = serde_json::to_vec(&permitted).map_err(|error| {
                tracing::error!(%error, "the permitted part of a batch does not serialize");
                Box::new(ProblemDetails::internal().into_response())
            })?;
            *divided = Some((ids, refused));
            Ok(())
        }
    }
}

/// A write body judged against the space's model: each quantity in its model's unit (DM-06),
/// returned as the units it filled, and the model's quality rules (DM-88). A caller the
/// Endpoint's Policy refuses learns that alone, never a rule's name or bound.
fn model_judged(
    gateway: &Gateway,
    (endpoint, subject): (&Endpoint, &Subject),
    (path, params, operation): (&str, &[(String, String)], Operation),
    declared: &DeclaredTypes,
    sent: &mut Vec<u8>,
    divided: &mut Option<(Vec<String>, Refused)>,
) -> Result<Vec<String>, Box<Response<Body>>> {
    let filled = filled_units(sent, path, &declared.units).map_err(|problem| {
        tracing::info!(slug = %endpoint.slug, "write refused: a quantity is not in its model's unit");
        Box::new(problem.into_response())
    })?;
    // The broker decides a view's grants after this; a caller the Endpoint's Policy
    // refuses learns that, never a rule's name or bound (DM-88), which a refusal or a
    // 207's member names.
    let earlier = divided.as_ref().map_or(0, |(_, refused)| refused.len());
    let judged = quality_judged(sent, path, operation, endpoint, divided);
    let refused_here =
        judged.is_err() || divided.as_ref().map_or(0, |(_, refused)| refused.len()) > earlier;
    if refused_here
        && gateway
            .pdp
            .decide(subject, operation, &query::requested(params), endpoint)
            .is_deny()
    {
        return Err(Box::new(refused(operation, path)));
    }
    judged?;
    Ok(filled)
}

/// A write or a delete held against the relationships of the space's model (DM-70, DM-71): a
/// write that breaks one is refused, and so is a delete of an entity a relationship keeps.
async fn relationships_held(
    gateway: &Gateway,
    (endpoint, subject): (&Endpoint, &Subject),
    (path, params, operation): (&str, &[(String, String)], Operation),
    (headers, rules): (&HeaderMap, &crate::relationships::RelationshipRules),
    sent: &mut Vec<u8>,
    divided: &mut Option<(Vec<String>, Refused)>,
) -> Result<(), Box<Response<Body>>> {
    if operation.is_write() {
        let held = HeldWrite {
            gateway,
            subject,
            endpoint,
            headers,
            path,
            params,
            operation,
            rules,
        };
        held.check(sent, divided).await.inspect_err(|_| {
            tracing::info!(slug = %endpoint.slug, "write refused: it breaks a relationship of the space's model");
        })?;
    }
    if !matches!(operation, Operation::DeleteEntity | Operation::DeleteBatch) {
        return Ok(());
    }
    let held = HeldDelete {
        gateway,
        subject,
        endpoint,
        headers,
        rules,
    };
    let refused = if operation == Operation::DeleteEntity {
        match operations::addressed_entity(path).map(query::decode) {
            Some(id) => held.entity(&id).await,
            None => Ok(()),
        }
    } else {
        held.batch(sent, divided).await.map(|_| ())
    };
    refused.inspect_err(|_| {
        tracing::info!(slug = %endpoint.slug, "delete refused: a relationship keeps it");
    })
}

/// This surface's routes; `app::router` merges them under its guards.
pub(crate) fn routes() -> Router<Arc<Gateway>> {
    Router::new()
        // The NGSI-LD access URL the DCAT record advertises, both spellings (EP-90).
        .route("/api/endpoint/{slug}/ngsi-ld/v1", get(ngsi_ld_entry))
        .route("/api/endpoint/{slug}/ngsi-ld/v1/", get(ngsi_ld_entry))
        .route("/api/endpoint/{slug}/ngsi-ld/v1/{*rest}", any(ngsi_ld))
        .route("/cs/{space}/ngsi-ld/v1/{*rest}", any(space_ngsi_ld))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// DM-128: the URNs of one Entity, a list and a FeatureCollection; nothing from what is none.
    #[test]
    fn a_read_names_the_entities_of_every_answer_shape() {
        let ids = |body: &str| read_entities(body.as_bytes());
        assert_eq!(ids(r#"{"id":"urn:a:1","type":"T"}"#), ["urn:a:1"]);
        assert_eq!(
            ids(r#"[{"id":"urn:a:1"},{"type":"T"},{"id":"urn:a:2"}]"#),
            ["urn:a:1", "urn:a:2"]
        );
        assert_eq!(
            ids(r#"{"type":"FeatureCollection","features":[{"id":"urn:a:3"}]}"#),
            ["urn:a:3"]
        );
        assert!(ids("not json").is_empty());
        assert!(ids("7").is_empty());
    }

    /// F-SEC1-025: an Entity `join=inline` nests in a Relationship, a ListRelationship or one
    /// instance of several, at any level, is a read of that Entity too.
    #[test]
    fn a_read_names_every_entity_inlined_by_a_join() {
        let body = r#"[{"id":"urn:a:1","type":"T",
            "owner":{"type":"Relationship","object":"urn:p:1",
                "entity":{"id":"urn:p:1","type":"Person",
                    "employer":{"type":"Relationship","object":"urn:o:1",
                        "entity":{"id":"urn:o:1","type":"Org"}}}},
            "riders":{"type":"ListRelationship","objectList":[],
                "entityList":[{"id":"urn:p:2","type":"Person"}]},
            "seen":[{"type":"Relationship","datasetId":"urn:d:1","object":"urn:p:3",
                "entity":{"id":"urn:p:3","type":"Person"}}]}]"#;
        assert_eq!(
            read_entities(body.as_bytes()),
            ["urn:a:1", "urn:p:1", "urn:o:1", "urn:p:2", "urn:p:3"]
        );
        let feature = r#"{"type":"FeatureCollection","features":[{"id":"urn:a:4",
            "properties":{"owner":{"type":"Relationship","object":"urn:p:4",
                "entity":{"id":"urn:p:4","type":"Person"}}}}]}"#;
        assert_eq!(read_entities(feature.as_bytes()), ["urn:a:4", "urn:p:4"]);
    }

    /// F-SEC1-019: the log holds the broker's `type` and a cut `detail`, never the rest of a body
    /// that may carry entity values.
    #[test]
    fn a_broker_failure_logs_only_its_type_and_a_cut_detail() {
        let long = "x".repeat(10_000);
        let body = serde_json::json!({"type": "urn:t:Internal", "detail": long, "entity": {"rider": "jane"}});
        let (kind, detail) = explanation(body.to_string().as_bytes());
        assert_eq!(kind, "urn:t:Internal");
        assert_eq!(detail.chars().count(), LOGGED_EXPLANATION);
        assert_eq!(
            explanation(b"rider jane doe"),
            (String::new(), String::new())
        );
    }

    fn answered(status: StatusCode, body: &'static str) -> Response<Body> {
        let mut answer = Response::new(Body::from(body));
        *answer.status_mut() = status;
        answer
    }

    async fn text(answer: Response<Body>) -> String {
        let held = axum::body::to_bytes(answer.into_body(), MAX_BODY)
            .await
            .unwrap_or_default();
        String::from_utf8_lossy(&held).into_owned()
    }

    /// T-5508: a broker failure, a refusal and a tenant's miss are final answers the broker's
    /// wording never reaches the caller through; a success and a write's miss go on.
    #[tokio::test]
    async fn map_view_answer_keeps_the_broker_wording_from_the_caller() {
        let read = Operation::RetrieveEntity;
        let failed = map_view_answer(answered(StatusCode::BAD_GATEWAY, "tenant t1"), read).await;
        let failed = failed.err().map(|answer| answer.status());
        assert!(failed.is_some_and(|status| status.is_server_error()));
        let refused = map_view_answer(answered(StatusCode::FORBIDDEN, "{}"), read).await;
        assert_eq!(
            refused.err().map(|answer| answer.status()),
            Some(StatusCode::FORBIDDEN)
        );

        let tenant =
            r#"{"type":"https://uri.etsi.org/ngsi-ld/errors/NonexistentTenant","detail":"t1"}"#;
        let missed = map_view_answer(answered(StatusCode::NOT_FOUND, tenant), read).await;
        let missed = missed.err().map(|answer| *answer);
        assert_eq!(
            missed.as_ref().map(Response::status),
            Some(StatusCode::NOT_FOUND)
        );
        assert!(!text(missed.unwrap_or_default()).await.contains("t1"));
        let listed = map_view_answer(
            answered(StatusCode::NOT_FOUND, tenant),
            Operation::QueryEntity,
        )
        .await;
        assert_eq!(
            listed.err().map(|answer| answer.status()),
            Some(StatusCode::OK)
        );
        let plain = map_view_answer(answered(StatusCode::NOT_FOUND, "gone"), read).await;
        let plain = plain.err().map(|answer| answer.status());
        assert_eq!(plain, Some(StatusCode::NOT_FOUND));

        let written = map_view_answer(
            answered(StatusCode::NOT_FOUND, tenant),
            Operation::UpdateAttrs,
        )
        .await;
        assert!(written.is_ok());
        assert!(map_view_answer(answered(StatusCode::OK, "{}"), read)
            .await
            .is_ok());
    }

    /// T-5508, EP-97: an Endpoint counting its creates refuses the batch forms and mints the
    /// id a form names; one without creates leaves the body alone.
    #[test]
    fn one_entity_per_create_refuses_a_batch_where_creates_count() {
        let mut endpoint = crate::mcp::catalog::tests::fixture();
        let mut sent = br#"[{"id":"urn:ngsi-ld:T:1","type":"T"}]"#.to_vec();
        assert!(one_entity_per_create(&endpoint, Operation::CreateBatch, &mut sent).is_ok());
        endpoint.creates = Some(jc_core::kinds::Creates {
            mint_ids: true,
            ..Default::default()
        });
        let refused = one_entity_per_create(&endpoint, Operation::UpsertBatch, &mut sent);
        let refused = refused.err().map(|answer| answer.status());
        assert_eq!(refused, Some(StatusCode::BAD_REQUEST));

        let mut one = br#"{"id":"urn:ngsi-ld:T:mine","type":"T"}"#.to_vec();
        assert!(one_entity_per_create(&endpoint, Operation::CreateEntity, &mut one).is_ok());
        assert!(!String::from_utf8_lossy(&one).contains("mine"));
    }

    /// T-5508, PF-43, EP-61: a write whose id is no NGSI-LD URN is refused whole; a batch keeps
    /// its passing members and holds the one writing a hidden attribute in `divided`.
    #[test]
    fn judged_write_refuses_a_foreign_id_and_divides_a_batch() {
        let mut endpoint = crate::mcp::catalog::tests::fixture();
        endpoint.hidden_attributes.insert("secret".to_owned());
        let mut divided = None;
        let mut sent = br#"{"id":"not-a-urn","type":"T"}"#.to_vec();
        let refused = judged_write(
            &endpoint,
            "/entities",
            Operation::CreateEntity,
            &mut sent,
            &mut divided,
        );
        let refused = refused.err().map(|answer| answer.status());
        assert_eq!(refused, Some(StatusCode::BAD_REQUEST));

        let mut batch =
            br#"[{"id":"urn:ngsi-ld:Thing:one","type":"Thing"},{"id":"urn:ngsi-ld:Thing:two","type":"Thing","secret":{"value":1}}]"#.to_vec();
        let path = "/entityOperations/create";
        let kept = judged_write(
            &endpoint,
            path,
            Operation::CreateBatch,
            &mut batch,
            &mut divided,
        );
        assert!(kept.is_ok());
        assert_eq!(
            divided.as_ref().map(|(ids, _)| ids.clone()),
            Some(vec!["urn:ngsi-ld:Thing:one".to_owned()])
        );
        assert!(!String::from_utf8_lossy(&batch).contains("secret"));
    }

    /// T-5508: the broker's address keeps the caller's query and no empty `?`.
    #[test]
    fn view_target_keeps_the_query() {
        assert_eq!(
            view_target("v", "/entities", None),
            "/x/views/v/ngsi-ld/v1/entities"
        );
        assert_eq!(
            view_target("v", "/entities", Some("")),
            "/x/views/v/ngsi-ld/v1/entities"
        );
        assert_eq!(
            view_target("v", "/entities", Some("type=T")),
            "/x/views/v/ngsi-ld/v1/entities?type=T"
        );
    }

    /// T-5508: an Endpoint whose view id no broker view carries is the `503 no-view` before any
    /// hop; a read of entities goes on to the view.
    #[tokio::test]
    async fn answered_here_refuses_an_endpoint_without_a_view() {
        let gateway = Gateway::new(
            crate::proxy::Broker::new("http://localhost:9"),
            Box::new(crate::pdp::PolicyPdp),
            "example.org",
        );
        let endpoint = crate::mcp::catalog::tests::fixture();
        let subject = Subject::anonymous();
        let mut request = Request::new(Body::empty());
        let read = Operation::RetrieveEntity;
        let long = "v".repeat(MAX_VIEW_ID + 1);
        let refused = answered_here(
            &gateway,
            (&endpoint, &subject),
            read,
            &long,
            &[],
            &mut request,
        )
        .await;
        let refused = refused.err().map(|answer| answer.status());
        assert_eq!(refused, Some(StatusCode::SERVICE_UNAVAILABLE));
        let goes_on = answered_here(
            &gateway,
            (&endpoint, &subject),
            read,
            "view",
            &[],
            &mut request,
        )
        .await;
        assert!(goes_on.is_ok());
    }

    /// T-5508, R22: the broker's restriction header reaches the caller under the name the
    /// specification gives, and never under the broker's own.
    #[tokio::test]
    async fn caller_answer_renames_the_restriction_header() {
        let endpoint = crate::mcp::catalog::tests::fixture();
        let subject = Subject::anonymous();
        let mut answer = answered(StatusCode::OK, "{}");
        answer
            .headers_mut()
            .insert(ANTARES_RESTRICTED, HeaderValue::from_static("true"));
        let write = Operation::UpdateAttrs;
        let answer = caller_answer(answer, (&endpoint, &subject), write, &[]).await;
        assert!(answer.headers().get(ANTARES_RESTRICTED).is_none());
        assert_eq!(
            answer.headers().get(RESULTS_RESTRICTED),
            Some(&HeaderValue::from_static("true"))
        );
    }
}
