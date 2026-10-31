//! The endpoint surfaces the gateway serves beside the ETSI resource tree.

use crate::app::MAX_BODY;
use axum::body::Body;
use axum::http::{Response, StatusCode};
use axum::response::IntoResponse;
use jc_core::ProblemDetails;

pub mod catalog;
pub(crate) mod download_cache;
pub mod endpoint_surface;
pub mod files;
pub mod mappings;
pub mod ngsi_ld;
pub mod ogc;
pub mod openapi_doc;
pub mod reads;
pub mod respond;
pub mod schema;
pub mod schema_routes;
pub mod space_routes;
pub mod space_surface;
pub mod types_list;
pub mod view_access;
pub mod writes;
pub mod ws;

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
    let (kind, detail) = ngsi_ld::explanation(&held);
    tracing::warn!(
        slug,
        status = status.as_u16(),
        broker_type = %kind,
        broker_detail = %detail,
        "the broker failed a read behind a representation; its own explanation is not passed on"
    );
    let problem = match status {
        _ if ngsi_ld::timed_out(status, &held) => ngsi_ld::query_timeout(),
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
