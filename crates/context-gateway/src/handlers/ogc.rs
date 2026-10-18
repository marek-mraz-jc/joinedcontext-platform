//! The OGC API Features surface of an endpoint (EP-29): the broker's `antares-ogc` surface of the
//! Endpoint's view (T-3752, ADR-0029 decision 3).
//!
//! `/api/endpoint/{slug}/ogc/features{rest}` goes to `/x/views/{view}/ogc/features{rest}` with
//! the caller's query as sent and the tenant and the verified caller pinned, like the NGSI-LD
//! surface (T-3643). The broker answers every document, filter (CQL2) and decision; the gateway
//! admits the caller, keeps the representation read-only (EP-39) and moves the broker's links
//! under the caller's URL.
//!
//! The SensorThings API surface (`sta/v1.1`, EP-12, T-4463) is the same read-only forward to the
//! broker's `antares-sta` surface of the view; the gateway's own translation is gone.
//!
//! The OData v4 surface (`odata/v4`, EP-104, T-3830) is the same forward to the broker's
//! `antares-odata` surface of the view. Its reads go as OGC's do; its writes pass the guards the
//! NGSI-LD view forward applies first (T-3939): the domain gate, the caller pinned, the body
//! bound, no hidden attribute named, a minting Endpoint's key left to the broker, the day's
//! creates counted. The broker's view still decides the operation (ADR-0023).

use crate::app::{Gateway, MAX_BODY};
use crate::auth::subject::{admit, principal_of};
use crate::handlers::ngsi_ld::{
    broker_failure, caller_links, has_view, read_body, reserve_create, view_request,
    with_platform_slug,
};
use crate::handlers::respond::read_only;
use crate::middleware::response::RESULTS_RESTRICTED;
use crate::middleware::{rate_limit, tenancy};
use crate::pdp::evaluator::Subject;
use crate::pdp::write_guard;
use crate::resolver::Endpoint;
use crate::{proxy, query};
use axum::body::Body;
use axum::extract::{Path, Request, State};
use axum::http::header::{ACCEPT, ALLOW, CONTENT_LENGTH, CONTENT_TYPE};
use axum::http::{HeaderMap, HeaderValue, Method, Response, StatusCode};
use axum::response::IntoResponse;
use axum::routing::any;
use axum::Router;
use jc_core::kinds::{Operation, Representation};
use jc_core::ProblemDetails;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;

/// The broker's narrowing marker (Antares `configuration.md`, `ANTARES_POLICY`).
const ANTARES_RESTRICTED: &str = "antares-results-restricted";

/// A read-only surface of the broker's view, forwarded as sent.
struct Surface {
    representation: Representation,
    /// The child path under the Endpoint and under `/x/views/{view}`.
    path: &'static str,
    name: &'static str,
}

const OGC: Surface = Surface {
    representation: Representation::OgcFeatures,
    path: "ogc/features",
    name: "OGC API Features",
};

const STA: Surface = Surface {
    representation: Representation::Sta,
    path: "sta/v1.1",
    name: "SensorThings API",
};

const ODATA: Surface = Surface {
    representation: Representation::Odata,
    path: "odata/v4",
    name: "OData",
};

const ICAL: Surface = Surface {
    representation: Representation::Ical,
    path: "ical/v1",
    name: "iCalendar",
};

/// Every OGC API - Features request of one endpoint (EP-29, EP-39, EP-40).
pub(crate) async fn ogc_features(
    State(gateway): State<Arc<Gateway>>,
    Path(params): Path<HashMap<String, String>>,
    request: Request,
) -> Response<Body> {
    read_surface(&gateway, &params, request, &OGC).await
}

/// Every SensorThings API request of one endpoint (EP-12, EP-13).
pub(crate) async fn sensorthings(
    State(gateway): State<Arc<Gateway>>,
    Path(params): Path<HashMap<String, String>>,
    request: Request,
) -> Response<Body> {
    read_surface(&gateway, &params, request, &STA).await
}

/// Every OData v4 request of one endpoint (EP-104): a read as OGC's, a write after the
/// Endpoint's write guards (T-3939).
pub(crate) async fn odata(
    State(gateway): State<Arc<Gateway>>,
    Path(params): Path<HashMap<String, String>>,
    request: Request,
) -> Response<Body> {
    if [Method::GET, Method::HEAD].contains(request.method()) {
        read_surface(&gateway, &params, request, &ODATA).await
    } else {
        odata_write(&gateway, &params, request).await
    }
}

/// The iCalendar feed of the view's Events (EP-125): read-only, the broker's alone.
pub(crate) async fn ical(
    State(gateway): State<Arc<Gateway>>,
    Path(params): Path<HashMap<String, String>>,
    request: Request,
) -> Response<Body> {
    read_surface(&gateway, &params, request, &ICAL).await
}

/// The caller admitted to the Endpoint for `surface`, with the client's own platform headers
/// gone first.
fn admitted(
    gateway: &Gateway,
    params: &HashMap<String, String>,
    request: &mut Request,
    surface: &Surface,
) -> Result<(Arc<Endpoint>, Subject), Box<Response<Body>>> {
    // Before routing, before authentication, before anything reads a header (EP-21).
    tenancy::strip_client_headers(request);
    let slug = params.get("slug").cloned().unwrap_or_default();
    admit(
        gateway,
        &slug,
        Some(surface.representation),
        request.headers(),
    )
}

/// The path below the surface's root, still percent-encoded: a decoded `/` or `?` in an item id
/// would be a different resource at the broker.
fn rest_of<'r>(request: &'r Request, prefix: &str) -> &'r str {
    request
        .uri()
        .path()
        .strip_prefix(prefix)
        .filter(|rest| *rest != "/")
        .unwrap_or_default()
}

async fn read_surface(
    gateway: &Gateway,
    params: &HashMap<String, String>,
    mut request: Request,
    surface: &Surface,
) -> Response<Body> {
    let (endpoint, subject) = match admitted(gateway, params, &mut request, surface) {
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
        return no_view(surface);
    }
    let prefix = format!("{}/{}", endpoint.base_path, surface.path);
    let target = target(&request, &prefix, &view, surface);
    if let Err(problem) = tenancy::pin_caller(&mut request, &endpoint.space, &subject) {
        tracing::error!(space = %endpoint.space, %problem, "endpoint names an unusable space");
        return ProblemDetails::internal().into_response();
    }
    tracing::info!(
        slug = %endpoint.slug,
        space = %endpoint.space,
        %view,
        principal = %principal_of(&subject),
        surface = surface.name,
        "request forwarded to the broker's view"
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
    answered(gateway, &endpoint, surface, answer).await
}

/// `/x/views/{view}/{surface}{rest}` with the caller's query as sent.
fn target(request: &Request, prefix: &str, view: &str, surface: &Surface) -> String {
    let root = format!("/x/views/{view}/{}", surface.path);
    let rest = rest_of(request, prefix);
    match request.uri().query() {
        Some(query) if !query.is_empty() => format!("{root}{rest}?{query}"),
        _ => format!("{root}{rest}"),
    }
}

/// The broker's answer as the caller gets it: a failure without its detail, the engine's
/// refusal with the platform's slug, every link under the Endpoint's URL.
async fn answered(
    gateway: &Gateway,
    endpoint: &Endpoint,
    surface: &Surface,
    answer: Response<Body>,
) -> Response<Body> {
    let prefix = format!("{}/{}", endpoint.base_path, surface.path);
    let root = format!("/x/views/{}/{}", endpoint.slug, surface.path);
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
    // The broker names its resources by its own address (`http://{Host}`, Antares
    // `public_base`); the caller reaches them under the Endpoint's URL.
    let from = format!("http://{}{root}", authority(gateway.broker.base()));
    let to = format!("{}{prefix}", gateway.base_url());
    // A feature also links its entity on the view's NGSI-LD surface, which the Endpoint serves
    // under the same child path: what is left of the view's mount goes under the Endpoint too, so
    // no answer names the broker (T-4403).
    let view_from = format!(
        "http://{}/x/views/{}/",
        authority(gateway.broker.base()),
        endpoint.slug
    );
    let view_to = format!("{}{}/", gateway.base_url(), endpoint.base_path);
    let (mut parts, body) = answer.into_parts();
    // ponytail: the page is held whole to move its links, so one past MAX_BODY (8 MiB) is a
    // broker failure; stream the rewrite if a `limit` ever reaches that.
    let Ok(held) = axum::body::to_bytes(body, MAX_BODY).await else {
        tracing::error!(slug = %endpoint.slug, surface = surface.name, "the broker's answer is larger than the gateway holds");
        return broker_failure(StatusCode::BAD_GATEWAY, None, Body::empty()).await;
    };
    caller_links(&mut parts.headers, &from, &to);
    caller_links(&mut parts.headers, &view_from, &view_to);
    if let Some(restricted) = parts.headers.remove(ANTARES_RESTRICTED) {
        parts.headers.insert(RESULTS_RESTRICTED, restricted);
    }
    let text = String::from_utf8_lossy(&held)
        .replace(&from, &to)
        .replace(&view_from, &view_to);
    proxy::with_body(parts, text.into_bytes())
}

/// The methods the OData surface answers; the broker's view decides which writes it takes.
const ODATA_ALLOW: &str = "GET, HEAD, OPTIONS, POST, PATCH, PUT, DELETE";

/// One OData write (or `OPTIONS`) through the Endpoint's write guards, then the broker's view
/// (EP-104, EP-97, EP-61, T-3939).
async fn odata_write(
    gateway: &Gateway,
    params: &HashMap<String, String>,
    mut request: Request,
) -> Response<Body> {
    let (endpoint, subject) = match admitted(gateway, params, &mut request, &ODATA) {
        Ok(admitted) => admitted,
        Err(problem) => return *problem,
    };
    if request.method() == Method::OPTIONS {
        let mut offered = Response::new(Body::empty());
        *offered.status_mut() = StatusCode::NO_CONTENT;
        offered
            .headers_mut()
            .insert(ALLOW, HeaderValue::from_static(ODATA_ALLOW));
        return offered;
    }
    let view = endpoint.slug.clone();
    if endpoint.view_mapping.is_some() || !has_view(&view) {
        return no_view(&ODATA);
    }
    let prefix = format!("{}/{}", endpoint.base_path, ODATA.path);
    let write = match Write::of(request.method(), rest_of(&request, &prefix)) {
        Ok(write) => write,
        Err(problem) => return problem.into_response(),
    };
    // The domain gate, a federated read's token refused on a write, the caller pinned: what the
    // NGSI-LD view forward checks before it reads a body.
    if let Err(answer) = view_request(
        gateway,
        &endpoint,
        &subject,
        "",
        write.operation,
        &mut request,
    )
    .await
    {
        return *answer;
    }
    let method = request.method().clone();
    let target = target(&request, &prefix, &view, &ODATA);
    let (parts, body) = request.into_parts();
    let sent = match read_body(gateway, body).await {
        Ok(sent) => sent,
        Err(answer) => return *answer,
    };
    let members = match members(&sent) {
        Ok(members) => members,
        Err(problem) => return problem.into_response(),
    };
    // What an Endpoint does not show cannot be changed through it (EP-61): the property the path
    // names, or a member of the body, by its NGSI-LD name.
    let named = write
        .property
        .iter()
        .cloned()
        .chain(members.iter().filter_map(|member| attribute_of(member)));
    if let Some(hidden) = named
        .map(|name| unmangle(&name))
        .find(|name| endpoint.hidden_attributes.contains(name))
    {
        let refusal = write_guard::Refusal::AttributeOutsideGrant(hidden);
        tracing::info!(slug = %endpoint.slug, %refusal, "odata write refused");
        return ProblemDetails::from(refusal).into_response();
    }
    // A form's ids are minted, never chosen (EP-97): the broker mints the key a create leaves
    // out, so a create that names it is refused.
    let mints = endpoint
        .creates
        .as_ref()
        .is_some_and(|creates| creates.mint_ids);
    if write.operation == Operation::CreateEntity && mints {
        let key = match key_of(gateway, &view, &write.set, &parts.headers).await {
            Ok(key) => key,
            Err(answer) => return *answer,
        };
        if members.contains(&key) {
            return ProblemDetails::bad_request()
                .with_detail(format!(
                    "this endpoint mints the id of what it creates; leave `{key}` out"
                ))
                .into_response();
        }
    }
    let caller = rate_limit::caller_key(&parts.headers);
    let reserved = match reserve_create(gateway, &endpoint, &caller, write.operation) {
        Ok(reserved) => reserved,
        Err(answer) => return *answer,
    };
    tracing::info!(
        slug = %endpoint.slug,
        space = %endpoint.space,
        %view,
        principal = %principal_of(&subject),
        surface = ODATA.name,
        operation = %write.operation.as_str(),
        "write forwarded to the broker's view"
    );
    let answer = gateway
        .broker
        .send(method, &target, parts.headers, Body::from(sent))
        .await;
    if let Some(now) = reserved {
        if !answer.as_ref().is_ok_and(|a| a.status().is_success()) {
            gateway.creates.release(&endpoint.slug, &caller, now);
        }
    }
    match answer {
        Ok(answer) => answered(gateway, &endpoint, &ODATA, answer).await,
        Err(error) => ProblemDetails::from(error).into_response(),
    }
}

/// An OData write as the Endpoint's guards read it: the operation it stands for (the domain
/// gate, the day's creates), the entity set, and the property or navigation below the entity.
struct Write {
    operation: Operation,
    set: String,
    property: Option<String>,
}

impl Write {
    /// `POST {Set}` is a create, `PATCH`/`PUT`/`DELETE {Set}({key})` a merge, replace or delete,
    /// and a write below it a change of one property. Any other form still goes to the broker,
    /// which answers it (405, 501); `$batch` does not, since its members are no request the
    /// gateway reads.
    fn of(method: &Method, rest: &str) -> Result<Self, Box<ProblemDetails>> {
        let mut segments = rest.trim_start_matches('/').split('/');
        let first = segments.next().unwrap_or_default();
        if first == "$batch" {
            // ponytail: a $batch would need its multipart and JSON members parsed to guard each;
            // send each write on its own until a client needs one round trip.
            return Err(Box::new(
                ProblemDetails::new(501, "odata-batch-unavailable", "Not Implemented").with_detail(
                    "a $batch is not served through an endpoint; send each write on its own",
                ),
            ));
        }
        let (set, keyed) = match first.split_once('(') {
            Some((set, _)) => (set, true),
            None => (first, false),
        };
        let property = segments
            .next()
            .filter(|segment| !segment.is_empty() && !segment.starts_with('$'))
            .map(query::decode);
        let operation = match (method, keyed, property.is_some()) {
            (&Method::POST, false, _) => Operation::CreateEntity,
            (&Method::DELETE, true, true) => Operation::DeleteAttrs,
            (_, true, true) => Operation::UpdateAttrs,
            (&Method::PUT, true, false) => Operation::ReplaceEntity,
            (&Method::DELETE, true, false) => Operation::DeleteEntity,
            _ => Operation::MergeEntity,
        };
        Ok(Self {
            operation,
            set: query::decode(set),
            property,
        })
    }
}

/// The member names of a JSON object body; none for an empty body. A body the gateway cannot
/// read is refused here, since its members could not be checked.
fn members(sent: &[u8]) -> Result<Vec<String>, Box<ProblemDetails>> {
    if sent.is_empty() {
        return Ok(Vec::new());
    }
    match serde_json::from_slice::<Value>(sent) {
        Ok(Value::Object(object)) => Ok(object.keys().cloned().collect()),
        Ok(_) => Ok(Vec::new()),
        Err(_) => Err(Box::new(
            ProblemDetails::bad_request().with_detail("request body is not JSON"),
        )),
    }
}

/// The property a body member writes: `name` itself, or the `name` of `name@odata.bind`; an
/// annotation of the entity (`@odata.type`) writes none.
fn attribute_of(member: &str) -> Option<String> {
    let name = member.split('@').next().unwrap_or_default();
    (!name.is_empty()).then(|| name.to_owned())
}

/// The NGSI-LD name an OData property stands for: every `_xHHHH_` the broker wrote for a code
/// point a CSDL identifier does not allow, decoded (Antares `antares-odata` `names::mangle`).
fn unmangle(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut rest = name;
    while let Some(at) = rest.find("_x") {
        out.push_str(rest.get(..at).unwrap_or_default());
        let after = rest.get(at + 2..).unwrap_or_default();
        let hex = after
            .find('_')
            .map(|end| after.get(..end).unwrap_or_default())
            .filter(|hex| !hex.is_empty() && hex.bytes().all(|b| b.is_ascii_hexdigit()));
        match hex.and_then(|hex| u32::from_str_radix(hex, 16).ok().and_then(char::from_u32)) {
            Some(decoded) => {
                out.push(decoded);
                rest = after.get(hex.map_or(0, str::len) + 1..).unwrap_or_default();
            }
            None => {
                out.push_str("_x");
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

/// The key property of `set` as the broker's `$metadata` declares it, asked as the caller.
async fn key_of(
    gateway: &Gateway,
    view: &str,
    set: &str,
    headers: &HeaderMap,
) -> Result<String, Box<Response<Body>>> {
    let mut asked = headers.clone();
    for name in [CONTENT_TYPE, CONTENT_LENGTH] {
        asked.remove(name);
    }
    asked.remove("odata-version");
    asked.insert(ACCEPT, HeaderValue::from_static("application/json"));
    asked.insert("odata-maxversion", HeaderValue::from_static("4.01"));
    let path = format!("/x/views/{view}/odata/v4/$metadata");
    let answer = match gateway
        .broker
        .send(Method::GET, &path, asked, Body::empty())
        .await
    {
        Ok(answer) if answer.status().is_success() => answer,
        Ok(answer) => {
            tracing::error!(%view, status = %answer.status(), "the view's $metadata is unreadable");
            return Err(Box::new(
                broker_failure(StatusCode::BAD_GATEWAY, None, Body::empty()).await,
            ));
        }
        Err(error) => return Err(Box::new(ProblemDetails::from(error).into_response())),
    };
    let held = axum::body::to_bytes(answer.into_body(), MAX_BODY)
        .await
        .unwrap_or_default();
    let csdl = serde_json::from_slice::<Value>(&held).unwrap_or_default();
    match entity_key(&csdl, set) {
        Some(key) => Ok(key),
        // An unknown set is the broker's own 404 at the write; a set without a readable key is
        // refused, never written with a key the caller chose.
        None if csdl.is_object() && !names_set(&csdl, set) => Ok(String::new()),
        None => {
            tracing::error!(%view, %set, "the view's $metadata names no key for the set");
            Err(Box::new(
                broker_failure(StatusCode::BAD_GATEWAY, None, Body::empty()).await,
            ))
        }
    }
}

/// The entity type of `set` in a CSDL JSON document (CSDL JSON 4.01 section 13.2).
fn set_type<'c>(csdl: &'c Value, set: &str) -> Option<&'c str> {
    let container = csdl.get("$EntityContainer")?.as_str()?;
    let (namespace, name) = container.rsplit_once('.')?;
    csdl.get(namespace)?
        .get(name)?
        .get(set)?
        .get("$Type")?
        .as_str()
}

fn names_set(csdl: &Value, set: &str) -> bool {
    set_type(csdl, set).is_some()
}

/// The first key property of the entity type `set` holds, its alias where it has one, following
/// `$BaseType` (CSDL JSON 4.01 sections 6.1, 6.5).
fn entity_key(csdl: &Value, set: &str) -> Option<String> {
    let mut ty = set_type(csdl, set)?;
    // A derivation chain longer than this is no model the broker writes.
    for _ in 0..8 {
        let (namespace, name) = ty.rsplit_once('.')?;
        let definition = csdl.get(namespace)?.get(name)?;
        if let Some(key) = definition
            .get("$Key")
            .and_then(Value::as_array)
            .and_then(|key| key.first())
        {
            return match key {
                Value::String(name) => Some(name.clone()),
                Value::Object(alias) => alias.keys().next().cloned(),
                _ => None,
            };
        }
        ty = definition.get("$BaseType")?.as_str()?;
    }
    None
}

/// The `host[:port]` of an upstream URL.
fn authority(base: &str) -> &str {
    let rest = base.split_once("://").map_or(base, |(_, rest)| rest);
    rest.split('/').next().unwrap_or(rest)
}

/// An Endpoint no broker view can serve yet (a view of another model, a projection with
/// `temporalQ`, a caller under a data-space agreement, a slug no view id holds): the surface
/// is the broker's alone, so there is no other answer (ADR-0029 decision 3).
fn no_view(surface: &Surface) -> Response<Body> {
    // One literal slug per surface: the catalogue test reads them from the source (API/00 §4).
    // SensorThings is an OGC standard too, so it answers the OGC slug.
    let problem = match surface.representation {
        Representation::Odata => {
            ProblemDetails::new(501, "odata-view-unavailable", "Not Implemented")
        }
        Representation::Ical => {
            ProblemDetails::new(501, "ical-view-unavailable", "Not Implemented")
        }
        _ => ProblemDetails::new(501, "ogc-view-unavailable", "Not Implemented"),
    };
    problem
        .with_detail(format!(
            "the {} surface is served from the broker's view of this endpoint, and this \
             endpoint or caller has none yet; read it through the NGSI-LD surface",
            surface.name
        ))
        .into_response()
}

/// This surface's routes; `app::router` merges them under its guards.
pub(crate) fn routes() -> Router<Arc<Gateway>> {
    Router::new()
        // The OGC surface is the broker's view (T-3752), one forward over its resource tree:
        // three spellings of the landing page, because a GIS client stores whichever one it was
        // given (EP-29).
        .route("/api/endpoint/{slug}/ogc/features", any(ogc_features))
        .route("/api/endpoint/{slug}/ogc/features/", any(ogc_features))
        .route(
            "/api/endpoint/{slug}/ogc/features/{*rest}",
            any(ogc_features),
        )
        // The SensorThings API of the view (T-4463), the same read-only forward.
        .route("/api/endpoint/{slug}/sta/v1.1", any(sensorthings))
        .route("/api/endpoint/{slug}/sta/v1.1/", any(sensorthings))
        .route("/api/endpoint/{slug}/sta/v1.1/{*rest}", any(sensorthings))
        // The OData v4 service of the view (EP-104, T-3830), its writes after the Endpoint's write guards (T-3939).
        .route("/api/endpoint/{slug}/odata/v4", any(odata))
        .route("/api/endpoint/{slug}/odata/v4/", any(odata))
        .route("/api/endpoint/{slug}/odata/v4/{*rest}", any(odata))
        // The iCalendar feed of the view's Events (EP-125, T-4573), served by the broker.
        .route("/api/endpoint/{slug}/ical/v1/{*rest}", any(ical))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unmangle_decodes_the_brokers_escapes_only() {
        assert_eq!(unmangle("contact"), "contact");
        assert_eq!(unmangle("e_x002D_mail"), "e-mail");
        assert_eq!(unmangle("_x0031_st"), "1st");
        assert_eq!(unmangle("a_xZZ_b"), "a_xZZ_b");
        assert_eq!(unmangle("a_x"), "a_x");
        assert_eq!(unmangle("a_x_b"), "a_x_b");
        assert_eq!(unmangle("_xD800_"), "_xD800_");
    }

    #[test]
    fn a_write_names_its_set_property_and_operation() {
        let of = |method: Method, rest: &str| {
            Write::of(&method, rest)
                .ok()
                .map(|w| (w.operation, w.set, w.property))
        };
        assert_eq!(
            of(Method::POST, "/Vehicle"),
            Some((Operation::CreateEntity, "Vehicle".into(), None))
        );
        assert_eq!(
            of(Method::PATCH, "/Vehicle('urn%3Ax')"),
            Some((Operation::MergeEntity, "Vehicle".into(), None))
        );
        assert_eq!(
            of(Method::PUT, "/Vehicle('x')").map(|w| w.0),
            Some(Operation::ReplaceEntity)
        );
        assert_eq!(
            of(Method::DELETE, "/Vehicle('x')").map(|w| w.0),
            Some(Operation::DeleteEntity)
        );
        assert_eq!(
            of(Method::PUT, "/Vehicle('x')/e_x002D_mail"),
            Some((
                Operation::UpdateAttrs,
                "Vehicle".into(),
                Some("e_x002D_mail".into())
            ))
        );
        assert_eq!(
            of(Method::DELETE, "/Vehicle('x')/owner/$ref").map(|w| (w.0, w.2)),
            Some((Operation::DeleteAttrs, Some("owner".into())))
        );
        assert_eq!(
            of(Method::POST, "/Vehicle('x')/$value").map(|w| w.2),
            Some(None)
        );
        assert!(Write::of(&Method::POST, "/$batch").is_err());
    }

    #[test]
    fn a_member_names_the_property_it_writes() {
        assert_eq!(attribute_of("speed"), Some("speed".into()));
        assert_eq!(attribute_of("owner@odata.bind"), Some("owner".into()));
        assert_eq!(attribute_of("@odata.type"), None);
        assert_eq!(members(b"").ok(), Some(vec![]));
        assert_eq!(members(b"[1]").ok(), Some(vec![]));
        assert!(members(b"{").is_err());
    }

    #[test]
    fn the_key_comes_from_the_type_or_its_base() {
        let csdl = serde_json::json!({
            "$EntityContainer": "jc.v1.Container",
            "jc.v1": {
                "Container": {
                    "Vehicle": {"$Collection": true, "$Type": "jc.v1.Vehicle"},
                    "Truck": {"$Collection": true, "$Type": "jc.v1.Truck"},
                    "Plate": {"$Collection": true, "$Type": "jc.v1.Plate"}
                },
                "Vehicle": {"$Kind": "EntityType", "$Key": ["id"]},
                "Truck": {"$Kind": "EntityType", "$BaseType": "jc.v1.Vehicle"},
                "Plate": {"$Kind": "EntityType", "$Key": [{"number": "plate/number"}]}
            }
        });
        assert_eq!(entity_key(&csdl, "Vehicle"), Some("id".into()));
        assert_eq!(entity_key(&csdl, "Truck"), Some("id".into()));
        assert_eq!(entity_key(&csdl, "Plate"), Some("number".into()));
        assert_eq!(entity_key(&csdl, "Nothing"), None);
        assert!(!names_set(&csdl, "Nothing"));
        let looped = serde_json::json!({
            "$EntityContainer": "n.C",
            "n": {"C": {"S": {"$Type": "n.A"}}, "A": {"$BaseType": "n.A"}}
        });
        assert_eq!(entity_key(&looped, "S"), None);
    }

    #[test]
    fn authority_is_host_and_port() {
        assert_eq!(authority("http://antares:1026"), "antares:1026");
        assert_eq!(authority("https://b.example/base"), "b.example");
        assert_eq!(authority("antares:1026"), "antares:1026");
    }
}
