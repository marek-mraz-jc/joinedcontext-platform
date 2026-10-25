//! The routes of the access document (EP-55…EP-60): what the caller may do, the check of one
//! request before it is sent, and the Portal's simulation of both for someone else (EP-103).
//! The broker's engine answers every one through the Endpoint's view (plan 5.6, T-3649, T-3650):
//! the gateway admits the caller, pins them on the hop and adds what belongs to the Endpoint.

pub mod odrl;

use crate::app::{Gateway, MAX_BODY};
use crate::auth::subject::admit;
use crate::auth::subject::principal_of;
use crate::auth::subject::Subject;
use crate::auth::subject::{anonymous_on, subject_from, PORTAL_CLIENT, SIMULATE_AUDIENCE};
use crate::auth::token::{self, Claims};
use crate::handlers::ngsi_ld::{broker_failure, has_view, view_of};
use crate::handlers::respond::json_response;
use crate::handlers::respond::sha256_hex;
use crate::handlers::respond::text_response;
use crate::handlers::respond::typed_json_response;
use crate::resolver::Endpoint;
use crate::{middleware::rate_limit, middleware::tenancy, proxy};
use axum::body::Body;
use axum::body::Bytes;
use axum::extract::{Path, Request, State};
use axum::http::header::{ACCEPT, CONTENT_TYPE};
use axum::http::HeaderMap;
use axum::http::Response;
use axum::http::{HeaderValue, Method};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::Router;
use futures_util::stream::{self, StreamExt};
use jc_core::kinds::Operation;
use jc_core::ProblemDetails;
use serde::Deserialize;
use serde_json::json;
use serde_json::Value;
use std::sync::Arc;

/// What the caller may do here (T-0163, EP-55), as the broker's engine holds it: the document
/// is the view's `access` surface (plan 5.6, T-3649), read from the rule table the broker decides
/// with, so the description cannot drift from the enforcement (EP-60). The gateway adds what
/// belongs to the Endpoint and not to the grants: its `resource` (slug and space), its rate
/// (EP-56), its licence's duties and its own access URL in the ODRL `uid` (EP-57, EP-79).
pub(crate) async fn access(
    State(gateway): State<Arc<Gateway>>,
    Path(slug): Path<String>,
    mut request: Request,
) -> Response<Body> {
    tenancy::strip_client_headers(&mut request);
    let (endpoint, subject) = match admit(&gateway, &slug, None, request.headers()) {
        Ok(admitted) => admitted,
        Err(problem) => return *problem,
    };
    let format = access_format(
        request
            .headers()
            .get(axum::http::header::ACCEPT)
            .and_then(|value| value.to_str().ok()),
    );
    let document = match document(&gateway, &endpoint, &subject, format).await {
        Ok(document) => document,
        Err(answer) => return *answer,
    };
    match format {
        AccessFormat::AuthZen => json_response(&document),
        AccessFormat::Odrl => typed_json_response(&document, odrl::ODRL_JSON),
        AccessFormat::Turtle => text_response(odrl::turtle(&document), odrl::TURTLE),
        AccessFormat::GrantAst => typed_json_response(&document, GRANT_AST_JSON),
    }
}

/// The caller's grants on `endpoint` in `format`, the document `GET …/access` serves (EP-55 to
/// EP-58), ODRL JSON-LD standing for Turtle too: the broker view's with what the gateway adds. The
/// MCP façade's `describe_access` reads this same function, so the two surfaces cannot disagree
/// (EP-60).
pub(crate) async fn document(
    gateway: &Gateway,
    endpoint: &Endpoint,
    subject: &Subject,
    format: AccessFormat,
) -> Result<Value, Box<Response<Body>>> {
    if !has_view(&view_of(endpoint)) {
        return Err(Box::new(no_view(endpoint)));
    }
    // ODRL and Turtle are both read as ODRL JSON-LD: the gateway adds the licence's duties and
    // writes the Turtle from the result, so the two serializations still say one thing.
    let asked = match format {
        AccessFormat::AuthZen => "application/json",
        AccessFormat::Odrl | AccessFormat::Turtle => odrl::ODRL_JSON,
        AccessFormat::GrantAst => GRANT_AST_JSON,
    };
    let document = from_view(gateway, endpoint, subject, ViewCall::Document(asked)).await?;
    match format {
        AccessFormat::AuthZen => Ok(endpoint_document(document, endpoint)),
        AccessFormat::GrantAst => Ok(document),
        AccessFormat::Odrl | AccessFormat::Turtle => {
            odrl::published(document, endpoint, gateway.base_url(), sha256_hex).ok_or_else(|| {
                tracing::error!(slug = %endpoint.slug, "the broker's ODRL document is not a Set");
                Box::new(not_a_document())
            })
        }
    }
}

/// The view's AuthZEN document under the Endpoint's own name. The broker names the view alone
/// (a view says its Tenant only to a caller who named it); the caller here was admitted to this
/// Endpoint and its space, and the Portal's matrix shows both (EP-55). The Endpoint's rate goes
/// beside it, the same for every caller, and an Endpoint without one says nothing (EP-56).
fn endpoint_document(mut document: Value, endpoint: &Endpoint) -> Value {
    let Some(members) = document.as_object_mut() else {
        return document;
    };
    members.insert(
        "resource".to_owned(),
        json!({ "type": "endpoint", "id": endpoint.slug, "space": endpoint.space }),
    );
    if let Some(limits) = &endpoint.rate_limit {
        let mut named = serde_json::Map::new();
        named.insert(
            "requestsPerMinute".to_owned(),
            json!(limits.requests_per_minute),
        );
        if let Some(burst) = limits.burst {
            named.insert("burst".to_owned(), json!(burst));
        }
        members.insert("limits".to_owned(), Value::Object(named));
    }
    document
}

/// One call to the view's `access` surface.
enum ViewCall {
    /// `GET access`, asking for this media type.
    Document(&'static str),
    /// `POST access/check` with this AuthZEN request.
    Check(Bytes),
    /// `POST access/explain` with this AuthZEN request: the decision with the policy that made
    /// it, for the simulator alone (EP-103, T-3881).
    Explain(Bytes),
    /// `POST access/attributes` with `{type, attributes}`: the caller's decisions per Attribute
    /// (T-5527).
    Attributes(Bytes),
}

/// The view's answer as JSON, or the answer the caller gets instead: the broker's own refusal
/// as it came (an unknown view is its `404`), and the gateway's `5xx` document for a failure.
///
/// The hop carries the tenant and the verified caller ([`tenancy::pin_caller`]) and nothing of
/// the caller's request: not their token or cookie, not an encoding the gateway would then have
/// to undo.
async fn from_view(
    gateway: &Gateway,
    endpoint: &Endpoint,
    subject: &Subject,
    call: ViewCall,
) -> Result<Value, Box<Response<Body>>> {
    let (method, path, accept, body) = match call {
        ViewCall::Document(accept) => (
            Method::GET,
            format!("/x/views/{}/access", view_of(endpoint)),
            accept,
            Body::empty(),
        ),
        ViewCall::Check(bytes) => (
            Method::POST,
            format!("/x/views/{}/access/check", view_of(endpoint)),
            "application/json",
            Body::from(bytes),
        ),
        ViewCall::Explain(bytes) => (
            Method::POST,
            format!("/x/views/{}/access/explain", view_of(endpoint)),
            "application/json",
            Body::from(bytes),
        ),
        ViewCall::Attributes(bytes) => (
            Method::POST,
            format!("/x/views/{}/access/attributes", view_of(endpoint)),
            "application/json",
            Body::from(bytes),
        ),
    };
    let mut hop = Request::new(Body::empty());
    tenancy::pin_caller(&mut hop, &endpoint.space, subject).map_err(|problem| {
        tracing::error!(space = %endpoint.space, %problem, "endpoint names an unusable space");
        Box::new(ProblemDetails::internal().into_response())
    })?;
    let mut headers = std::mem::take(hop.headers_mut());
    headers.insert(ACCEPT, HeaderValue::from_static(accept));
    if method == Method::POST {
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    }
    tracing::info!(
        slug = %endpoint.slug,
        space = %endpoint.space,
        principal = %principal_of(subject),
        %path,
        "access asked of the broker's view"
    );
    let answer = gateway
        .broker
        .send(method, &path, headers, body)
        .await
        .map_err(|error| Box::new(ProblemDetails::from(error).into_response()))?;
    let (parts, body) = answer.into_parts();
    if parts.status.is_server_error() {
        return Err(Box::new(
            broker_failure(
                parts.status,
                parts.headers.get("retry-after").cloned(),
                body,
            )
            .await,
        ));
    }
    let Ok(held) = axum::body::to_bytes(body, MAX_BODY).await else {
        return Err(Box::new(not_a_document()));
    };
    if !parts.status.is_success() {
        return Err(Box::new(proxy::with_body(parts, held)));
    }
    // Every document and decision of the surface is a JSON object; anything else is not an
    // answer the gateway can add to or pass on.
    match serde_json::from_slice::<Value>(&held) {
        Ok(answer) if answer.is_object() => Ok(answer),
        _ => {
            tracing::error!(slug = %endpoint.slug, %path, "the broker's access answer is not a JSON object");
            Err(Box::new(not_a_document()))
        }
    }
}

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

/// A broker answer the gateway cannot read: the caller learns that the broker failed, the
/// operator's log says how.
fn not_a_document() -> Response<Body> {
    ProblemDetails::new(502, "broker-failure", "The context broker could not answer")
        .with_detail("the context broker behind this endpoint failed to answer this request")
        .into_response()
}

/// The media type of the condition-tree representation (EP-58).
pub const GRANT_AST_JSON: &str = "application/vnd.joinedcontext.grant-ast+json";

/// Which representation of the access surface the caller asked for (EP-56, EP-57, EP-58).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AccessFormat {
    /// The AuthZEN resource-search document, and the default.
    AuthZen,
    /// ODRL 2.2 in the `ngsi-ld:` profile, as JSON-LD.
    Odrl,
    /// The same ODRL policy as RDF.
    Turtle,
    /// The residual as a UCAST condition tree.
    GrantAst,
}

/// The first representation the `Accept` header names that this surface serves.
///
/// Read in the order the caller wrote them, so a client that prefers Turtle and will settle
/// for JSON gets Turtle. Anything else, including no header at all, is the default document:
/// the access surface always has an answer, and a 406 here would tell a caller nothing it
/// could act on.
fn access_format(accept: Option<&str>) -> AccessFormat {
    let Some(accept) = accept else {
        return AccessFormat::AuthZen;
    };
    for offer in accept.split(',') {
        match offer.split(';').next().unwrap_or_default().trim() {
            odrl::ODRL_JSON => return AccessFormat::Odrl,
            odrl::TURTLE => return AccessFormat::Turtle,
            GRANT_AST_JSON => return AccessFormat::GrantAst,
            "application/json" | "application/ld+json" => return AccessFormat::AuthZen,
            _ => {}
        }
    }
    AccessFormat::AuthZen
}

/// One prospective request, answered yes or no (T-0163, R51) by the broker's engine through the
/// view (plan 5.6, T-3649). The gateway still caps and reads the request first, so a malformed
/// one is refused the same way whoever decides.
pub(crate) async fn access_check(
    State(gateway): State<Arc<Gateway>>,
    Path(slug): Path<String>,
    mut request: Request,
) -> Response<Body> {
    tenancy::strip_client_headers(&mut request);
    let (endpoint, subject) = match admit(&gateway, &slug, None, request.headers()) {
        Ok(admitted) => admitted,
        Err(problem) => return *problem,
    };

    let (_, body) = request.into_parts();
    let Ok(bytes) = axum::body::to_bytes(body, 64 * 1024).await else {
        return ProblemDetails::bad_request().into_response();
    };
    let Ok(query) = serde_json::from_slice::<Value>(&bytes) else {
        return ProblemDetails::bad_request()
            .with_detail("request body is not JSON")
            .into_response();
    };
    if query
        .pointer("/action/name")
        .and_then(Value::as_str)
        .is_none()
    {
        return ProblemDetails::bad_request()
            .with_detail("action.name is required")
            .into_response();
    }

    if !has_view(&view_of(&endpoint)) {
        return no_view(&endpoint);
    }
    // The broker's engine decides, on the request as the caller wrote it (plan 5.6, T-3649).
    match from_view(&gateway, &endpoint, &subject, ViewCall::Check(bytes)).await {
        Ok(decision) => json_response(&decision),
        Err(answer) => *answer,
    }
}

/// The most decisions one `access/v1/evaluations` request asks (ADR-N-057 decision 3): a form
/// asks once for every option it shows, and no caller turns one request into more broker hops.
pub(crate) const MAX_EVALUATIONS: usize = 100;

/// How many of a batch's decisions are asked of the broker at once.
const EVALUATIONS_IN_FLIGHT: usize = 16;

/// `POST access/v1/evaluation`: one AuthZEN 1.0 Access Evaluation, decided by the broker's engine
/// through the view like `access/check` and a real request (EP-06, ADR-N-057 decision 3).
pub(crate) async fn access_evaluation(
    State(gateway): State<Arc<Gateway>>,
    Path(slug): Path<String>,
    request: Request,
) -> Response<Body> {
    let (endpoint, subject, asked) = match evaluation_request(&gateway, &slug, request).await {
        Ok(admitted) => admitted,
        Err(problem) => return *problem,
    };
    let item = match evaluation(&subject, &asked, &Value::Null) {
        Ok(item) => item,
        Err(problem) => return *problem,
    };
    match from_view(&gateway, &endpoint, &subject, ViewCall::Check(item)).await {
        Ok(decision) => json_response(&decision),
        Err(answer) => *answer,
    }
}

/// `POST access/v1/evaluations`: up to [`MAX_EVALUATIONS`] AuthZEN 1.0 evaluations, each item's
/// members over the request's defaults, every one decided (`execute_all`) and answered in the
/// order asked. A malformed item refuses the whole request before the broker is asked, and a
/// broker failure on any item fails it whole: no partial answer reads as a decision.
pub(crate) async fn access_evaluations(
    State(gateway): State<Arc<Gateway>>,
    Path(slug): Path<String>,
    request: Request,
) -> Response<Body> {
    let caller = rate_limit::caller_of(&gateway, &request);
    let (endpoint, subject, asked) = match evaluation_request(&gateway, &slug, request).await {
        Ok(admitted) => admitted,
        Err(problem) => return *problem,
    };
    let asks = match batch(&subject, &asked) {
        Ok(asks) => asks,
        Err(problem) => return *problem,
    };
    // The rate limit layer spent one request; each further decision is one more (EP-20).
    let extra = u32::try_from(asks.len().saturating_sub(1)).unwrap_or(u32::MAX);
    let charged = match rate_limit::charge(&gateway, &endpoint, &caller, extra) {
        Ok(charged) => charged,
        Err(refused) => return *refused,
    };
    let decisions: Vec<_> = stream::iter(asks)
        .map(|ask| from_view(&gateway, &endpoint, &subject, ViewCall::Check(ask)))
        .buffered(EVALUATIONS_IN_FLIGHT)
        .collect()
        .await;
    let mut answer = match decisions.into_iter().collect::<Result<Vec<_>, _>>() {
        Ok(decisions) => json_response(&json!({ "evaluations": decisions })),
        Err(answer) => *answer,
    };
    if let Some(charged) = charged {
        rate_limit::set_headers(answer.headers_mut(), &charged);
    }
    answer
}

/// The most Attributes `access/attributes` answers for one type: what the view reads in one ask.
pub(crate) const MAX_ATTRIBUTES: usize = 200;

/// Decisions one rate-limit request pays for in `access/attributes` (three per Attribute).
const DECISIONS_PER_CHARGE: usize = 100;

/// `type` of `GET access/attributes`.
#[derive(Deserialize)]
struct AttributesQuery {
    #[serde(rename = "type")]
    entity_type: Option<String>,
}

/// `GET access/attributes?type=T`: the caller's read, write and delete per Attribute the Endpoint
/// serves of the model class `T` (T-5527, API/02 7b), decided by the broker's engine through the
/// view in one ask, each cell as `access/check` with `resource.properties.attribute` decides it.
pub(crate) async fn access_attributes(
    State(gateway): State<Arc<Gateway>>,
    Path(slug): Path<String>,
    mut request: Request,
) -> Response<Body> {
    tenancy::strip_client_headers(&mut request);
    let caller = rate_limit::caller_of(&gateway, &request);
    let (endpoint, subject) = match admit(&gateway, &slug, None, request.headers()) {
        Ok(admitted) => admitted,
        Err(problem) => return *problem,
    };
    let refused = |detail: String| {
        ProblemDetails::bad_request()
            .with_detail(detail)
            .into_response()
    };
    let asked = axum::extract::Query::<AttributesQuery>::try_from_uri(request.uri())
        .ok()
        .and_then(|query| query.0.entity_type)
        .filter(|entity_type| !entity_type.is_empty());
    let Some(entity_type) = asked else {
        return refused("type is required: the entity type whose attributes to answer, e.g. ?type=AirQualityObserved".to_owned());
    };
    let Some(attributes) = endpoint.served_attributes(&entity_type) else {
        return refused(format!(
            "type {entity_type:?} is not a class of the model this endpoint serves"
        ));
    };
    if attributes.is_empty() || attributes.len() > MAX_ATTRIBUTES {
        return refused(format!(
            "type {entity_type:?} has {} served attributes; 1 to {MAX_ATTRIBUTES} are answered",
            attributes.len()
        ));
    }
    if !has_view(&view_of(&endpoint)) {
        return no_view(&endpoint);
    }
    // The rate limit layer spent one request; it pays for 100 decisions, each further 100 one more.
    let charges = attributes
        .len()
        .saturating_mul(3)
        .div_ceil(DECISIONS_PER_CHARGE);
    let extra = u32::try_from(charges.saturating_sub(1)).unwrap_or(u32::MAX);
    let charged = match rate_limit::charge(&gateway, &endpoint, &caller, extra) {
        Ok(charged) => charged,
        Err(refused) => return *refused,
    };
    let names: Vec<&str> = attributes.iter().map(|a| a.name.as_str()).collect();
    let ask = json!({"type": entity_type, "attributes": names}).to_string();
    let mut answer = match from_view(
        &gateway,
        &endpoint,
        &subject,
        ViewCall::Attributes(Bytes::from(ask)),
    )
    .await
    {
        Ok(table) => json_response(&table),
        Err(answer) => *answer,
    };
    if let Some(charged) = charged {
        rate_limit::set_headers(answer.headers_mut(), &charged);
    }
    answer
}

/// The evaluations of a batch request, each as the view's `access/check` reads it, or the `400`
/// (or `403`) that refuses the whole request.
fn batch(subject: &Subject, asked: &Value) -> Result<Vec<Bytes>, Box<Response<Body>>> {
    let refused = |detail: String| {
        Box::new(
            ProblemDetails::bad_request()
                .with_detail(detail)
                .into_response(),
        )
    };
    let semantic = asked.pointer("/options/evaluations_semantic");
    if semantic.is_some_and(|semantic| semantic != "execute_all") {
        return Err(refused(
            "options.evaluations_semantic: only execute_all is served".to_owned(),
        ));
    }
    let Some(items) = asked.get("evaluations").and_then(Value::as_array) else {
        return Err(refused(format!(
            "evaluations is required: a list of 1 to {MAX_EVALUATIONS} evaluations"
        )));
    };
    if items.is_empty() || items.len() > MAX_EVALUATIONS {
        return Err(refused(format!(
            "evaluations holds {} items; 1 to {MAX_EVALUATIONS} are answered",
            items.len()
        )));
    }
    if items.iter().any(|item| !item.is_object()) {
        return Err(refused(
            "every item of evaluations is a JSON object".to_owned(),
        ));
    }
    let asks = items
        .iter()
        .map(|item| evaluation(subject, asked, item))
        .collect::<Result<Vec<_>, _>>()?;
    if asks.iter().map(Bytes::len).sum::<usize>() > MAX_BATCH_BYTES {
        return Err(refused(
            "the evaluations with the request's defaults copied into each are over 1 MiB together: send a smaller context or fewer items".to_owned(),
        ));
    }
    Ok(asks)
}

/// The most bytes an evaluation request may hold: what `access/check` reads. An ask is built
/// from members of the body, so none is longer.
const MAX_EVALUATION_BYTES: usize = 64 * 1024;

/// The most bytes a batch's asks send on together, 16 checks' worth: the request's defaults are
/// copied into every item, and one request never turns into megabytes of broker bodies.
const MAX_BATCH_BYTES: usize = 16 * MAX_EVALUATION_BYTES;

/// The admitted caller and their request body, read the way `access/check` reads one.
async fn evaluation_request(
    gateway: &Gateway,
    slug: &str,
    mut request: Request,
) -> Result<(Arc<Endpoint>, Subject, Value), Box<Response<Body>>> {
    tenancy::strip_client_headers(&mut request);
    let (endpoint, subject) = admit(gateway, slug, None, request.headers())?;
    let (_, body) = request.into_parts();
    let Ok(bytes) = axum::body::to_bytes(body, MAX_EVALUATION_BYTES).await else {
        return Err(Box::new(
            ProblemDetails::bad_request()
                .with_detail("the request body is over 64 KiB")
                .into_response(),
        ));
    };
    let Ok(asked @ Value::Object(_)) = serde_json::from_slice::<Value>(&bytes) else {
        return Err(Box::new(
            ProblemDetails::bad_request()
                .with_detail("request body is not a JSON object")
                .into_response(),
        ));
    };
    if !has_view(&view_of(&endpoint)) {
        return Err(Box::new(no_view(&endpoint)));
    }
    Ok((endpoint, subject, asked))
}

/// One evaluation as the view's `access/check` reads it: `item`'s `action`, `resource` and
/// `context`, each falling back to the request's own (AuthZEN 1.0 section 7.1). The subject is the
/// caller's token alone: a `subject` that names anyone else is a 403 (only `access/simulate`
/// asks about another caller, EP-103), and none is sent on, so the engine decides for the pinned
/// caller whatever the body says.
fn evaluation(
    subject: &Subject,
    defaults: &Value,
    item: &Value,
) -> Result<Bytes, Box<Response<Body>>> {
    let member = |name: &str| item.get(name).or_else(|| defaults.get(name));
    if let Some(named) = member("subject") {
        if !names_the_caller(subject, named) {
            tracing::warn!(
                principal = %principal_of(subject),
                "an access evaluation named a subject other than the caller"
            );
            return Err(Box::new(
                ProblemDetails::forbidden()
                    .with_detail("subject must be the caller: omit it or name your own token's")
                    .into_response(),
            ));
        }
    }
    let mut ask = serde_json::Map::new();
    for name in ["action", "resource", "context"] {
        if let Some(value) = member(name) {
            ask.insert(name.to_owned(), value.clone());
        }
    }
    if ask
        .get("action")
        .and_then(|action| action.get("name"))
        .and_then(Value::as_str)
        .is_none()
    {
        return Err(Box::new(
            ProblemDetails::bad_request()
                .with_detail("action.name is required")
                .into_response(),
        ));
    }
    Ok(Bytes::from(Value::Object(ask).to_string()))
}

/// Whether an AuthZEN `subject` (`type` and `id`) is the caller: their user, their service
/// account, their DID, or, for an anonymous caller, the role `public` the engine names them by.
fn names_the_caller(subject: &Subject, named: &Value) -> bool {
    let (Some(kind), Some(id)) = (
        named.get("type").and_then(Value::as_str),
        named.get("id").and_then(Value::as_str),
    ) else {
        return false;
    };
    let own = match kind {
        "user" => subject.user.as_deref(),
        "serviceAccount" => subject.service_account.as_deref(),
        "did" => subject.did.as_deref(),
        "role" if principal_of(subject) == "anonymous" => Some("public"),
        _ => None,
    };
    own == Some(id)
}

/// One `access/simulate` request (EP-103): the `access/check` request with the subject it is
/// asked for, or, with no `action`, the question of everything the subject is granted (T-3792).
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Simulation {
    subject: Simulated,
    #[serde(default)]
    action: Option<Action>,
    #[serde(default)]
    resource: Option<Resource>,
}

/// Who is simulated: a person with the groups and realm roles the Portal resolved for them, a
/// member of groups or roles with no person, a service account by its Keycloak client id, or,
/// with nothing named, the public.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct Simulated {
    #[serde(default)]
    user: Option<String>,
    #[serde(default)]
    groups: Vec<String>,
    #[serde(default)]
    roles: Vec<String>,
    #[serde(default)]
    service_account: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Action {
    name: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Resource {
    #[serde(default, rename = "type")]
    entity_type: Option<String>,
    #[serde(default)]
    properties: Option<ResourceProperties>,
}

/// `resource.properties`: the one Attribute (a short name or an IRI) the decision is asked about,
/// as on `access/check`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ResourceProperties {
    attribute: String,
}

/// The decision `access/check` would give someone else, for the Portal alone (EP-103, T-3311).
///
/// The subject goes through the same admission as a real token ([`subject_from`]) and is asked
/// of the broker's engine through the view, pinned as the subject's own token pins it (T-3881):
/// the simulator cannot disagree with the decision that is made, it is the same decision, asked
/// about another caller.
pub(crate) async fn access_simulate(
    State(gateway): State<Arc<Gateway>>,
    Path(slug): Path<String>,
    mut request: Request,
) -> Response<Body> {
    tenancy::strip_client_headers(&mut request);
    let Some(endpoint) = gateway.resolver.resolve(&slug) else {
        return ProblemDetails::not_found().into_response();
    };
    if let Err(problem) = the_portal(&gateway, request.headers()) {
        return problem.into_response();
    }

    let (_, body) = request.into_parts();
    let Ok(bytes) = axum::body::to_bytes(body, 64 * 1024).await else {
        return ProblemDetails::bad_request().into_response();
    };
    let simulation = match serde_json::from_slice::<Simulation>(&bytes) {
        Ok(simulation) => simulation,
        Err(err) => {
            return ProblemDetails::bad_request()
                .with_detail(format!("not a simulation request: {err}"))
                .into_response()
        }
    };
    let who = &simulation.subject;
    if who.service_account.is_some()
        && (who.user.is_some() || !who.groups.is_empty() || !who.roles.is_empty())
    {
        return ProblemDetails::bad_request()
            .with_detail("subject names a serviceAccount beside a person, groups or roles")
            .into_response();
    }

    let subject = simulated(&gateway, &endpoint, who);
    let Some(action) = simulation
        .action
        .as_ref()
        .map(|action| action.name.as_str())
    else {
        if simulation.resource.is_some() {
            return ProblemDetails::bad_request()
                .with_detail("resource needs an action")
                .into_response();
        }
        let answer = match &subject {
            // The audience refused them before any Policy is read (EP-14): nobody is asked.
            None => json!({ "admitted": false }),
            Some(subject) => match granted(&gateway, &endpoint, subject).await {
                Ok(answer) => answer,
                Err(answer) => return *answer,
            },
        };
        tracing::info!(
            caller = PORTAL_CLIENT,
            slug = %endpoint.slug,
            simulated_user = who.user.as_deref().unwrap_or_default(),
            simulated_service_account = who.service_account.as_deref().unwrap_or_default(),
            simulated_groups = ?who.groups,
            simulated_roles = ?who.roles,
            admitted = %answer["admitted"],
            "access grants simulated"
        );
        return json_response(&answer);
    };
    if !Operation::ALL
        .iter()
        .any(|operation| operation.as_str() == action)
    {
        return ProblemDetails::bad_request()
            .with_detail(format!("action.name {action:?} is not a CIM 009 operation"))
            .into_response();
    }
    let entity_type = simulation
        .resource
        .as_ref()
        .and_then(|resource| resource.entity_type.as_deref());
    let attribute = simulation
        .resource
        .as_ref()
        .and_then(|resource| resource.properties.as_ref())
        .map(|properties| properties.attribute.as_str());
    if attribute.is_some_and(str::is_empty) {
        return ProblemDetails::bad_request()
            .with_detail("resource.properties.attribute is empty: name one attribute, e.g. NO2")
            .into_response();
    }
    if !has_view(&view_of(&endpoint)) {
        return no_view(&endpoint);
    }
    let answer = match &subject {
        // The audience refused them before any Policy is read (EP-14).
        None => json!({ "decision": false, "context": { "reason": "not_admitted" } }),
        Some(subject) => {
            let mut query = json!({ "action": { "name": action } });
            if let Some(entity_type) = entity_type {
                query["resource"] = json!({ "type": entity_type });
            }
            if let Some(attribute) = attribute {
                query["resource"]["properties"] = json!({ "attribute": attribute });
            }
            let call = ViewCall::Explain(Bytes::from(query.to_string()));
            match from_view(&gateway, &endpoint, subject, call).await {
                Ok(answer) => policy_named(answer),
                Err(answer) => return *answer,
            }
        }
    };
    // The Portal's record names the administrator; this one names whom they asked about, beside
    // the Portal's account that asked (EP-103). Never a token.
    tracing::info!(
        caller = PORTAL_CLIENT,
        slug = %endpoint.slug,
        simulated_user = who.user.as_deref().unwrap_or_default(),
        simulated_service_account = who.service_account.as_deref().unwrap_or_default(),
        simulated_groups = ?who.groups,
        simulated_roles = ?who.roles,
        action,
        entity_type = entity_type.unwrap_or_default(),
        attribute = attribute.unwrap_or_default(),
        decision = %answer["decision"],
        reason = %answer["context"]["reason"],
        "access simulated"
    );
    json_response(&answer)
}

/// Everything an admitted `subject` is granted here, for the Portal's Access explorer (EP-103,
/// T-3792): the `access` document a token of theirs would read, the engine's through the view
/// ([`document`]), beside `admitted` and the manifest names of the Policies in force for them.
async fn granted(
    gateway: &Gateway,
    endpoint: &Endpoint,
    subject: &Subject,
) -> Result<Value, Box<Response<Body>>> {
    let mut answer = document(gateway, endpoint, subject, AccessFormat::AuthZen).await?;
    let names: Vec<&str> =
        crate::pdp::evaluator::effective(subject, &endpoint.policies, crate::pdp::now())
            .into_iter()
            .filter_map(|policy| {
                let index = endpoint
                    .policies
                    .iter()
                    .position(|candidate| std::ptr::eq(candidate, policy))?;
                endpoint.policy_names.get(index).map(String::as_str)
            })
            .collect();
    answer["admitted"] = json!(true);
    answer["policies"] = json!(names);
    Ok(answer)
}

/// The engine's explanation with the deciding policy under the name the Portal knows it by: the
/// engine holds a Policy as `{project}/{name}`, or `{project}/{name}@{endpoint}` where it is bound
/// to one view (the Portal's reconciler, `reconciler/views.rs`), and the Portal lists `{name}`.
/// An id of any other shape is left out rather than shown as a name nobody wrote.
fn policy_named(mut answer: Value) -> Value {
    let Some(context) = answer.get_mut("context").and_then(Value::as_object_mut) else {
        return answer;
    };
    let name = context
        .get("policy")
        .and_then(Value::as_str)
        .and_then(|id| id.split('@').next())
        .and_then(|id| id.split_once('/'))
        .map(|(_, name)| name.to_owned())
        .filter(|name| !name.is_empty() && !name.contains('/'));
    match name {
        Some(name) => context.insert("policy".to_owned(), json!(name)),
        None => context.remove("policy"),
    };
    answer
}

/// Whether the request carries the Portal's own service-account token for
/// [`SIMULATE_AUDIENCE`]: any other token, a person's token of the same client included, is a
/// `403`, and none at all a `401`.
fn the_portal(gateway: &Gateway, headers: &HeaderMap) -> Result<(), Box<ProblemDetails>> {
    let presented = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok());
    let raw = token::bearer(presented).map_err(|rejected| Box::new(rejected.into()))?;
    let Some(verifier) = &gateway.verifier else {
        tracing::warn!("a token was presented but no realm is configured");
        return Err(Box::new(ProblemDetails::unauthorized()));
    };
    let claims = verifier
        .verify(raw, &[SIMULATE_AUDIENCE.to_owned()])
        .map_err(|rejected| Box::new(ProblemDetails::from(rejected)))?;
    let own_account = format!("service-account-{PORTAL_CLIENT}");
    if claims.azp.as_deref() == Some(PORTAL_CLIENT)
        && claims
            .preferred_username
            .as_deref()
            .is_some_and(|user| user.eq_ignore_ascii_case(&own_account))
    {
        Ok(())
    } else {
        tracing::warn!(
            azp = claims.azp.as_deref().unwrap_or_default(),
            "access/simulate refused a token that is not the Portal's own account"
        );
        Err(Box::new(ProblemDetails::forbidden()))
    }
}

/// The subject a real call by `who` would be decided as, or `None` when the Endpoint's audience
/// refuses them before any Policy is read (EP-14).
fn simulated(gateway: &Gateway, endpoint: &Endpoint, who: &Simulated) -> Option<Subject> {
    let nobody = who.user.is_none()
        && who.service_account.is_none()
        && who.groups.is_empty()
        && who.roles.is_empty();
    if nobody {
        return endpoint.admits(None).then(|| anonymous_on(endpoint));
    }
    // The claims a token of theirs would carry. A member with no person is the empty user, which
    // no Policy can name; a service account is its client's own service-account user.
    let (azp, user) = match &who.service_account {
        Some(client) => (Some(client.clone()), format!("service-account-{client}")),
        None => (None, who.user.clone().unwrap_or_default()),
    };
    let claims: Claims = serde_json::from_value(json!({
        "sub": user,
        "iss": "simulated",
        "exp": 0,
        "azp": azp,
        "preferred_username": user,
        "groups": who.groups,
        "realm_access": { "roles": who.roles },
    }))
    .ok()?;
    subject_from(gateway, endpoint, &claims).ok()
}

/// This surface's routes; `app::router` merges them under its guards.
pub(crate) fn routes() -> Router<Arc<Gateway>> {
    Router::new()
        .route("/api/endpoint/{slug}/access", get(access))
        .route("/api/endpoint/{slug}/access/check", post(access_check))
        .route(
            "/api/endpoint/{slug}/access/attributes",
            get(access_attributes),
        )
        .route(
            "/api/endpoint/{slug}/access/v1/evaluation",
            post(access_evaluation),
        )
        .route(
            "/api/endpoint/{slug}/access/v1/evaluations",
            post(access_evaluations),
        )
        .route(
            "/api/endpoint/{slug}/access/simulate",
            post(access_simulate),
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// ADR-N-057 decision 3: an evaluation names the caller or nobody; the caller's own name in
    /// any other kind, or anyone else's, is not theirs.
    #[test]
    fn a_subject_is_the_callers_only_by_their_own_kind_and_id() {
        let jana = Subject {
            user: Some("jana".to_owned()),
            ..Subject::default()
        };
        assert!(names_the_caller(
            &jana,
            &json!({ "type": "user", "id": "jana" })
        ));
        for other in [
            json!({ "type": "user", "id": "peter" }),
            json!({ "type": "serviceAccount", "id": "jana" }),
            json!({ "type": "role", "id": "public" }),
            json!({ "type": "user" }),
            json!("jana"),
        ] {
            assert!(!names_the_caller(&jana, &other), "{other}");
        }
        let anonymous = Subject::anonymous();
        assert!(names_the_caller(
            &anonymous,
            &json!({ "type": "role", "id": "public" })
        ));
        assert!(!names_the_caller(
            &anonymous,
            &json!({ "type": "role", "id": "admin" })
        ));
        let account = Subject {
            service_account: Some("ingest".to_owned()),
            ..Subject::default()
        };
        assert!(names_the_caller(
            &account,
            &json!({ "type": "serviceAccount", "id": "ingest" })
        ));
    }
}
