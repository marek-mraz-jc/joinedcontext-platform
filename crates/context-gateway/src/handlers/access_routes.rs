//! The routes of the access document (EP-55…EP-60): what the caller may do, and the check of one
//! request before it is sent.

use crate::app::admit;
use crate::app::json_response;
use crate::app::sha256_hex;
use crate::app::text_response;
use crate::app::typed_json_response;
use crate::app::Gateway;
use crate::app::{anonymous_on, subject_from, PORTAL_CLIENT, SIMULATE_AUDIENCE};
use crate::app::{broker_failure, has_view, principal_of, view_of, MAX_BODY};
use crate::auth::token::{self, Claims};
use crate::pdp::evaluator::Subject;
use crate::resolver::Endpoint;
use crate::{handlers, middleware::tenancy, proxy};
use axum::body::Body;
use axum::body::Bytes;
use axum::extract::{Path, Request, State};
use axum::http::header::{ACCEPT, CONTENT_TYPE};
use axum::http::HeaderMap;
use axum::http::Response;
use axum::http::{HeaderValue, Method};
use axum::response::IntoResponse;
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
        AccessFormat::Odrl => typed_json_response(&document, handlers::access_odrl::ODRL_JSON),
        AccessFormat::Turtle => text_response(
            handlers::access_odrl::turtle(&document),
            handlers::access_odrl::TURTLE,
        ),
        AccessFormat::GrantAst => {
            typed_json_response(&document, handlers::access_ucast::GRANT_AST_JSON)
        }
    }
}

/// The caller's grants on `endpoint` in `format`, the document `GET …/access` serves (EP-55 to
/// EP-58), ODRL JSON-LD standing for Turtle too: the broker view's with what the gateway adds,
/// or the gateway's own for an Endpoint no view serves yet ([`has_view`]). The MCP façade's
/// `describe_access` reads this same function, so the two surfaces cannot disagree (EP-60).
pub(crate) async fn document(
    gateway: &Gateway,
    endpoint: &Endpoint,
    subject: &Subject,
    format: AccessFormat,
) -> Result<Value, Box<Response<Body>>> {
    if !has_view(endpoint, subject, &view_of(endpoint)) {
        return Ok(own_document(gateway, endpoint, subject, format));
    }
    // ODRL and Turtle are both read as ODRL JSON-LD: the gateway adds the licence's duties and
    // writes the Turtle from the result, so the two serializations still say one thing.
    let asked = match format {
        AccessFormat::AuthZen => "application/json",
        AccessFormat::Odrl | AccessFormat::Turtle => handlers::access_odrl::ODRL_JSON,
        AccessFormat::GrantAst => handlers::access_ucast::GRANT_AST_JSON,
    };
    let document = from_view(gateway, endpoint, subject, ViewCall::Document(asked)).await?;
    match format {
        AccessFormat::AuthZen => Ok(endpoint_document(document, endpoint)),
        AccessFormat::GrantAst => Ok(document),
        AccessFormat::Odrl | AccessFormat::Turtle => handlers::access_odrl::published(
            document,
            endpoint,
            gateway.base_url(),
            sha256_hex,
        )
        .ok_or_else(|| {
            tracing::error!(slug = %endpoint.slug, "the broker's ODRL document is not a Set");
            Box::new(not_a_document())
        }),
    }
}

/// The gateway's own document, for an Endpoint no view serves yet ([`has_view`]); it goes with
/// the gateway's pipeline when those gaps close (T-3646).
fn own_document(
    gateway: &Gateway,
    endpoint: &Endpoint,
    subject: &Subject,
    format: AccessFormat,
) -> Value {
    let now = crate::pdp::now();
    match format {
        AccessFormat::AuthZen => handlers::access::permissions(subject, endpoint, now),
        AccessFormat::Odrl | AccessFormat::Turtle => {
            handlers::access_odrl::policy(subject, endpoint, now, gateway.base_url(), sha256_hex)
        }
        AccessFormat::GrantAst => handlers::access_ucast::grant_ast(subject, endpoint, now),
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
        return Err(Box::new(broker_failure(parts.status, body).await));
    }
    let Ok(held) = axum::body::to_bytes(body, MAX_BODY).await else {
        return Err(Box::new(not_a_document()));
    };
    if !parts.status.is_success() {
        return Err(Box::new(proxy::with_body(parts, held.to_vec())));
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

/// A broker answer the gateway cannot read: the caller learns that the broker failed, the
/// operator's log says how.
fn not_a_document() -> Response<Body> {
    ProblemDetails::new(502, "broker-failure", "The context broker could not answer")
        .with_detail("the context broker behind this endpoint failed to answer this request")
        .into_response()
}

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
            handlers::access_odrl::ODRL_JSON => return AccessFormat::Odrl,
            handlers::access_odrl::TURTLE => return AccessFormat::Turtle,
            handlers::access_ucast::GRANT_AST_JSON => return AccessFormat::GrantAst,
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
    let Some(action) = query
        .get("action")
        .and_then(|action| action.get("name"))
        .and_then(Value::as_str)
    else {
        return ProblemDetails::bad_request()
            .with_detail("action.name is required")
            .into_response();
    };

    if !has_view(&endpoint, &subject, &view_of(&endpoint)) {
        return json_response(&handlers::access::check(
            &subject,
            &endpoint,
            action,
            query
                .get("resource")
                .and_then(|resource| resource.get("type"))
                .and_then(Value::as_str),
            crate::pdp::now(),
        ));
    }
    // The broker's engine decides, on the request as the caller wrote it (plan 5.6, T-3649).
    match from_view(&gateway, &endpoint, &subject, ViewCall::Check(bytes)).await {
        Ok(decision) => json_response(&decision),
        Err(answer) => *answer,
    }
}

/// One `access/simulate` request (EP-103): the `access/check` request with the subject it is
/// asked for.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Simulation {
    subject: Simulated,
    action: Action,
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
}

/// The decision `access/check` would give someone else, for the Portal alone (EP-103, T-3311).
///
/// The subject goes through the same admission as a real token ([`subject_from`]) and the
/// same evaluator ([`handlers::access::decide`]), so the simulator cannot disagree with the
/// gateway: it is the gateway, asked about another caller.
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
    let action = simulation.action.name.as_str();
    if !Operation::ALL
        .iter()
        .any(|operation| operation.as_str() == action)
    {
        return ProblemDetails::bad_request()
            .with_detail(format!("action.name {action:?} is not a CIM 009 operation"))
            .into_response();
    }
    let who = &simulation.subject;
    if who.service_account.is_some()
        && (who.user.is_some() || !who.groups.is_empty() || !who.roles.is_empty())
    {
        return ProblemDetails::bad_request()
            .with_detail("subject names a serviceAccount beside a person, groups or roles")
            .into_response();
    }

    let subject = simulated(&gateway, &endpoint, who);
    let entity_type = simulation
        .resource
        .as_ref()
        .and_then(|resource| resource.entity_type.as_deref());
    let answer = handlers::access::simulate(
        subject.as_ref().unwrap_or(&Subject::anonymous()),
        subject.is_some(),
        &endpoint,
        action,
        entity_type,
        crate::pdp::now(),
    );
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
        decision = %answer["decision"],
        reason = %answer["context"]["reason"],
        "access simulated"
    );
    json_response(&answer)
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
