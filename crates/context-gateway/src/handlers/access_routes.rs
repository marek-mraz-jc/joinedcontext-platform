//! The routes of the access document (EP-55…EP-60): what the caller may do, and the check of one
//! request before it is sent.

use crate::app::admit;
use crate::app::json_response;
use crate::app::sha256_hex;
use crate::app::text_response;
use crate::app::typed_json_response;
use crate::app::Gateway;
use crate::{handlers, middleware::tenancy};
use axum::body::Body;
use axum::extract::{Path, Request, State};
use axum::http::Response;
use axum::response::IntoResponse;
use jc_core::ProblemDetails;
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
