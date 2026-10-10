//! The routes of the access document (EP-55…EP-60): what the caller may do, and the check of one
//! request before it is sent.

use crate::app::admit;
use crate::app::json_response;
use crate::app::sha256_hex;
use crate::app::text_response;
use crate::app::typed_json_response;
use crate::app::Gateway;
use crate::app::{anonymous_on, subject_from, PORTAL_CLIENT, SIMULATE_AUDIENCE};
use crate::auth::token::{self, Claims};
use crate::pdp::evaluator::Subject;
use crate::resolver::Endpoint;
use crate::{handlers, middleware::tenancy};
use axum::body::Body;
use axum::extract::{Path, Request, State};
use axum::http::HeaderMap;
use axum::http::Response;
use axum::response::IntoResponse;
use jc_core::kinds::Operation;
use jc_core::ProblemDetails;
use serde::Deserialize;
use serde_json::json;
use serde_json::Value;
use std::sync::Arc;

/// What the caller may do here, from the same PDP that enforces it (T-0163, EP-55).
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

    let accept = request
        .headers()
        .get(axum::http::header::ACCEPT)
        .and_then(|value| value.to_str().ok());
    let now = crate::pdp::now();

    // The same grants in whichever language the caller reads (EP-56, EP-57, EP-58). The
    // document is computed once per representation from the same PDP answer; a second
    // implementation of "what may this caller do" is the thing EP-60 forbids.
    match access_format(accept) {
        AccessFormat::AuthZen => {
            json_response(&handlers::access::permissions(&subject, &endpoint, now))
        }
        AccessFormat::Odrl => {
            let document = handlers::access_odrl::policy(
                &subject,
                &endpoint,
                now,
                gateway.base_url(),
                sha256_hex,
            );
            typed_json_response(&document, handlers::access_odrl::ODRL_JSON)
        }
        AccessFormat::Turtle => {
            let document = handlers::access_odrl::policy(
                &subject,
                &endpoint,
                now,
                gateway.base_url(),
                sha256_hex,
            );
            text_response(
                handlers::access_odrl::turtle(&document),
                handlers::access_odrl::TURTLE,
            )
        }
        AccessFormat::GrantAst => {
            let document = handlers::access_ucast::grant_ast(&subject, &endpoint, now);
            typed_json_response(&document, handlers::access_ucast::GRANT_AST_JSON)
        }
    }
}

/// Which representation of the access surface the caller asked for (EP-56, EP-57, EP-58).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AccessFormat {
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

/// One prospective request, answered yes or no (T-0163, R51).
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

    json_response(&handlers::access::check(
        &subject,
        &endpoint,
        action,
        query
            .get("resource")
            .and_then(|resource| resource.get("type"))
            .and_then(Value::as_str),
        crate::pdp::now(),
    ))
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
