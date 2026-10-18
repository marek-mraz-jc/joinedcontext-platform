//! The answers every surface writes the same way: JSON, text and a read-only refusal.

use axum::body::Body;
use axum::extract::Request;
use axum::http::header::ACCEPT;
use axum::http::{HeaderValue, Response, StatusCode};
use axum::response::IntoResponse;
use jc_core::ProblemDetails;
use serde_json::Value;

/// The lowercase hex sha256 of a body, which is what every `ETag` here is.
pub(crate) fn sha256_hex(bytes: &[u8]) -> String {
    ring::digest::digest(&ring::digest::SHA256, bytes)
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// A file route names its format in the path, so the hop to the broker asks for JSON whatever
/// the caller's `Accept` said: forwarded as it came, `Accept: text/csv` on `file.csv` was the
/// broker's `406` (EP-08, EP-44).
pub(crate) fn broker_speaks_json(request: &mut Request) {
    request
        .headers_mut()
        .insert(ACCEPT, HeaderValue::from_static("application/json"));
}

/// A JSON body under a media type of its own, for the representations that have one.
pub(crate) fn typed_json_response(payload: &Value, media_type: &'static str) -> Response<Body> {
    match serde_json::to_vec(payload) {
        Ok(bytes) => (
            [(
                axum::http::header::CONTENT_TYPE,
                HeaderValue::from_static(media_type),
            )],
            bytes,
        )
            .into_response(),
        Err(error) => {
            tracing::error!(%error, "the answer does not serialize");
            ProblemDetails::internal().into_response()
        }
    }
}

/// A text body under its own media type.
pub(crate) fn text_response(body: String, media_type: &'static str) -> Response<Body> {
    (
        [(
            axum::http::header::CONTENT_TYPE,
            HeaderValue::from_static(media_type),
        )],
        body,
    )
        .into_response()
}

/// The answer of a read-only representation to a method it does not have (EP-13, EP-39).
pub(crate) fn read_only(status: StatusCode) -> Response<Body> {
    let mut response = if status == StatusCode::NO_CONTENT {
        Response::new(Body::empty())
    } else {
        ProblemDetails::new(405, "method-not-allowed", "Method Not Allowed")
            .with_detail("this representation is read-only; write through the NGSI-LD surface")
            .into_response()
    };
    *response.status_mut() = status;
    response.headers_mut().insert(
        axum::http::header::ALLOW,
        HeaderValue::from_static("GET, HEAD, OPTIONS"),
    );
    response
}

/// A JSON body, serialized once.
pub(crate) fn json_response(payload: &Value) -> Response<Body> {
    match serde_json::to_vec(payload) {
        Ok(bytes) => (
            [(
                axum::http::header::CONTENT_TYPE,
                HeaderValue::from_static("application/json"),
            )],
            bytes,
        )
            .into_response(),
        Err(error) => {
            tracing::error!(%error, "the answer does not serialize");
            ProblemDetails::internal().into_response()
        }
    }
}
