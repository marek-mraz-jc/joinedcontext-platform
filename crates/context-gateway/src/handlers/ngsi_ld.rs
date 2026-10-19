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
use crate::pdp::evaluator::{Constraints, Subject};
use crate::pdp::write_guard;
use crate::proxy::{self};
use crate::resolver::Endpoint;
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
    tracing::warn!(
        status = status.as_u16(),
        broker = %String::from_utf8_lossy(&held),
        "the broker failed; its own explanation is not passed on"
    );
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
        .map(|bytes| bytes.to_vec())
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
        _ => crate::middleware::rate_limit::caller_key(headers),
    }
}

/// The longest view id the broker accepts (`[a-z0-9-]{1,63}`, ADR-0023).
pub(crate) const MAX_VIEW_ID: usize = 63;

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
    if !has_view(&view) {
        return crate::handlers::view_access::no_view(&endpoint);
    }
    if operation == Operation::QueryEntity {
        if let Some(refusal) = query_shape_refusal(&params) {
            return refusal;
        }
    }
    let mut request = request;
    // The Endpoint's type list as its reads show the caller (T-4080); a caller without a read
    // grant, and the details form, are the view's to answer.
    if operation == Operation::RetrieveEntityTypes && method == Method::GET && view == endpoint.slug
    {
        match crate::handlers::types_list::listed_types(gateway, &endpoint, &subject, &mut request)
            .await
        {
            Ok(Some(list)) => return typed_json_response(&list, "application/json"),
            Ok(None) => {}
            Err(answer) => return *answer,
        }
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

    // After `pin_caller` the token is gone from the headers: a signed-in caller is keyed by
    // who they are, not by the address they share (T-4590).
    let caller = create_key(&subject, &parts.headers);
    let reserved = match reserve_create(gateway, &endpoint, &caller, operation) {
        Ok(reserved) => reserved,
        Err(answer) => return *answer,
    };
    let target = match uri.query() {
        Some(query) if !query.is_empty() => {
            format!("/x/views/{view}/ngsi-ld/v1{path}?{}", capped_query(query))
        }
        _ => format!("/x/views/{view}/ngsi-ld/v1{path}"),
    };
    let answer = match gateway
        .broker
        .send(method, &target, parts.headers, Body::from(sent))
        .await
    {
        Ok(answer) => answer,
        Err(error) => {
            if let Some(now) = reserved {
                gateway.creates.release(&endpoint.slug, &caller, now);
            }
            return ProblemDetails::from(error).into_response();
        }
    };
    if let Some(now) = reserved {
        if !answer.status().is_success() {
            gateway.creates.release(&endpoint.slug, &caller, now);
        }
    }
    if answer.status().is_server_error() {
        let (parts, body) = answer.into_parts();
        return broker_failure(
            parts.status,
            parts.headers.get("retry-after").cloned(),
            body,
        )
        .await;
    }
    if answer.status() == StatusCode::FORBIDDEN {
        return with_platform_slug(answer).await;
    }
    // A space nobody has written to yet has no tenant at the broker, and the caller was
    // admitted to it: a listing answers with nothing, a read of one thing with the plain miss,
    // and the broker's wording, which names the tenant, never reaches the caller (EP-22, R20).
    if answer.status() == StatusCode::NOT_FOUND && !operation.is_write() {
        let (parts, body) = answer.into_parts();
        let held = axum::body::to_bytes(body, MAX_BODY)
            .await
            .unwrap_or_default();
        if !names_no_tenant(&held) {
            return proxy::with_body(parts, held.to_vec());
        }
        return match empty_space(operation) {
            Some(empty) => json_response(&empty),
            None => ProblemDetails::not_found().into_response(),
        };
    }
    let mut answer = match divided {
        Some((forwarded, refused)) => merged_batch(answer, forwarded, refused).await,
        None => answer,
    };
    caller_links(
        answer.headers_mut(),
        &format!("/x/views/{view}/ngsi-ld/v1"),
        prefix,
    );
    // The broker marks a narrowed answer in its own header, and only when the caller opted in
    // (`x-jc-restricted`); the caller reads it under the name the specification gives (R22).
    let headers = answer.headers_mut();
    if let Some(restricted) = headers.remove(ANTARES_RESTRICTED) {
        headers.insert(RESULTS_RESTRICTED, restricted);
    }
    if !operation.is_write() && answer.status().is_success() {
        answer = logged_read(answer, &endpoint, &subject).await;
    }
    with_filled_units(answer, &units_filled)
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
    tracing::info!(
        target: "personal_read",
        slug = %endpoint.slug,
        space = %endpoint.space,
        principal = %principal_of(subject),
        attributes = %personal.join(","),
        entities = %read_entities(&held).join(","),
        "personal attributes read"
    );
    proxy::with_body(parts, held.to_vec())
}

/// The URNs of the Entities an NGSI-LD answer holds: one Entity, a list of them, or a GeoJSON
/// FeatureCollection.
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
    items
        .iter()
        .filter_map(|item| item.get("id").and_then(Value::as_str))
        .map(str::to_owned)
        .collect()
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
        return proxy::with_body(parts, held.to_vec());
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

/// A delete of context data names why in `JC-Reason` (ADR-N-054 2.3, DM-113); without it the
/// answer is `400 reason-required`. DM-113's other half, every write from the Portal's UI,
/// turns on with T-4667, which gives the Portal's forms their reason field: enforced before
/// that, it would refuse every form the Portal sends today.
fn reason_refusal(
    operation: Operation,
    subject: &Subject,
    headers: &axum::http::HeaderMap,
) -> Option<Response<Body>> {
    // The Portal's own delete (ui/src/components/entities/filters.ts) asks no reason yet: a
    // delete through the UI is held to it with the reason field, as every UI write is (DM-113).
    if tenancy::provenance(subject).0 == "ui" {
        return None;
    }
    let deletes = matches!(
        operation,
        Operation::DeleteAttrs
            | Operation::DeleteEntity
            | Operation::DeleteBatch
            | Operation::DeleteAttrsTemporal
            | Operation::DeleteAttrInstanceTemporal
            | Operation::DeleteTemporal
    );
    let given = headers
        .get(tenancy::REASON)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|r| !r.trim().is_empty());
    (deletes && !given).then(|| {
        ProblemDetails::new(400, "reason-required", "Reason Required")
            .with_detail("a delete names why in JC-Reason")
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
    if let Some(creates) = &endpoint.creates {
        match operation {
            Operation::CreateEntity if creates.mint_ids && !sent.is_empty() => {
                *sent = crate::creates::minted(sent).map_err(|problem| answer(*problem))?;
            }
            Operation::CreateBatch | Operation::UpsertBatch | Operation::UpsertTemporal => {
                return Err(answer(ProblemDetails::bad_request().with_detail(
                    "this endpoint takes one entity per create, at POST /entities",
                )));
            }
            _ => {}
        }
    }
    // What the Endpoint asks of a write whatever the grants are: an id that is an NGSI-LD URN
    // of its type (PF-43), the entity of the path (PF-44), a type of the space's model (DM-61),
    // no access control inside the data (GW29). With no constraint the grant checks pass; the
    // broker's view and its engine decide the grants. A batch keeps what passes (GW18).
    if operation.is_write() && !sent.is_empty() {
        let unconstrained = Constraints::default();
        match judge_write(sent, path, operation, &unconstrained, endpoint) {
            Err(problem) => return Err(answer(*problem)),
            Ok(Judged::Whole) => {}
            Ok(Judged::Divided { permitted, refused }) => {
                if permitted.is_empty() {
                    return Err(Box::new(batch_result(Vec::new(), refusal_entries(refused))));
                }
                let ids = permitted.iter().filter_map(batch_entry_id).collect();
                *sent = serde_json::to_vec(&permitted).map_err(|error| {
                    tracing::error!(%error, "the permitted part of a batch does not serialize");
                    answer(ProblemDetails::internal())
                })?;
                *divided = Some((ids, refused));
            }
        }
    }
    if operation.is_write() {
        owned_guarded(sent, path).map_err(|problem| answer(*problem))?;
    }
    let declared = endpoint.declared_types.as_ref();
    let mut filled = Vec::new();
    if operation.is_write() && !sent.is_empty() {
        if let Some(declared) = declared {
            filled = filled_units(sent, path, &declared.units).map_err(|problem| {
                tracing::info!(slug = %endpoint.slug, "write refused: a quantity is not in its model's unit");
                Box::new(problem.into_response())
            })?;
            // The broker decides a view's grants after this; a caller the Endpoint's Policy
            // refuses learns that, never a rule's name or bound (DM-88), which a refusal or a
            // 207's member names.
            let earlier = divided.as_ref().map_or(0, |(_, refused)| refused.len());
            let judged = quality_judged(sent, path, operation, endpoint, divided);
            let refused_here = judged.is_err()
                || divided.as_ref().map_or(0, |(_, refused)| refused.len()) > earlier;
            if refused_here
                && gateway
                    .pdp
                    .decide(subject, operation, &query::requested(params), endpoint)
                    .is_deny()
            {
                return Err(Box::new(refused(operation, path)));
            }
            judged?;
        }
    }
    if let Some(rules) = declared
        .map(|declared| &declared.relationships)
        .filter(|rules| !rules.is_empty())
    {
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
        if matches!(operation, Operation::DeleteEntity | Operation::DeleteBatch) {
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
            })?;
        }
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
    use super::read_entities;

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
}
