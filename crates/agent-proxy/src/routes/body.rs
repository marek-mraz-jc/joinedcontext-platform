//! The ceiling every route reads a request body through (AG-41, T-0811).
//!
//! The proxy is one process shared by every run in the organization, so a body it buffers is
//! memory taken from all of them. Reading through [`Limited`] stops at the ceiling instead of
//! after it: a run that sends a gigabyte is refused having allocated four megabytes, which is
//! the difference between a refusal and the proxy being the thing that fell over.

use axum::body::{Body, Bytes};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use http_body_util::{BodyExt, LengthLimitError, Limited};

/// The most one request may hand this proxy. A model call with a long conversation is the
/// biggest legitimate body there is, and it fits.
pub const MAX_REQUEST_BYTES: usize = 4 * 1024 * 1024;

/// The whole body, or the refusal to send back.
///
/// `413` for a body past the ceiling and `400` for one that broke mid-read: the first is the
/// caller's decision and the second is a connection that went away.
pub async fn bounded(body: Body) -> Result<Bytes, Box<Response>> {
    match Limited::new(body, MAX_REQUEST_BYTES).collect().await {
        Ok(collected) => Ok(collected.to_bytes()),
        Err(err) if err.is::<LengthLimitError>() => Err(Box::new(
            (
                StatusCode::PAYLOAD_TOO_LARGE,
                jc_core::ProblemDetails::new(
                    413,
                    "payload-too-large",
                    format!("request body exceeds the proxy's {MAX_REQUEST_BYTES} byte limit"),
                ),
            )
                .into_response(),
        )),
        Err(_) => Err(Box::new(
            jc_core::ProblemDetails::bad_request()
                .with_detail("failed to read body")
                .into_response(),
        )),
    }
}
