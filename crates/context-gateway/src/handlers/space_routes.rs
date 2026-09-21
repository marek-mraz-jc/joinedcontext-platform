//! The Context Space catalogue and the record of one space under `/cs` (SP-03).

use crate::app::admit_space;
use crate::app::authenticate;
use crate::app::discoverable;
use crate::app::json_response;
use crate::app::Gateway;
use crate::handlers::space_surface;
use crate::resolver::Space;
use axum::body::Body;
use axum::extract::{Path, Request, State};
use axum::http::{HeaderValue, Response};
use axum::response::IntoResponse;
use jc_core::ProblemDetails;
use std::sync::Arc;

/// The catalog of spaces this caller may discover (SP-11).
pub(crate) async fn space_catalog(
    State(gateway): State<Arc<Gateway>>,
    request: Request,
) -> Response<Body> {
    let headers = request.headers();
    // A token is minted for one resource, so a token that names space A does not verify
    // against space B. That is not an error here, it is the narrowing: a space whose
    // authentication the caller cannot satisfy is a space they cannot discover.
    let visible: Vec<Arc<Space>> = gateway
        .resolver
        .spaces()
        .into_iter()
        .filter(|space| {
            authenticate(&gateway, &space.endpoint, headers)
                .is_ok_and(|subject| discoverable(&gateway, space, &subject))
        })
        .collect();

    let catalog = space_surface::catalog(&visible, gateway.base_url());
    let mut response = json_response(&catalog);
    response.headers_mut().insert(
        axum::http::header::CONTENT_TYPE,
        HeaderValue::from_static(space_surface::JSON_LD),
    );
    response
}

/// The DCAT-AP record of one space, in the representation the caller asked for (SP-10).
pub(crate) async fn space_record(
    State(gateway): State<Arc<Gateway>>,
    Path(name): Path<String>,
    request: Request,
) -> Response<Body> {
    // `admit_space` has already answered 404 for a space this caller cannot discover, which is
    // the same answer a name that was never created gets (SP-06, SP-11).
    let (space, _subject) = match admit_space(&gateway, &name, None, request.headers()) {
        Ok(admitted) => admitted,
        Err(problem) => return *problem,
    };

    let accept = request
        .headers()
        .get(axum::http::header::ACCEPT)
        .and_then(|value| value.to_str().ok());
    let base = gateway.base_url();
    let format = space_surface::negotiate(accept);
    let body = match format {
        space_surface::Format::JsonLd => {
            serde_json::to_string(&space_surface::dataset(&space, base)).unwrap_or_default()
        }
        space_surface::Format::Turtle => space_surface::dataset_turtle(&space, base),
        space_surface::Format::Html => space_surface::dataset_html(&space, base),
    };
    match HeaderValue::from_str(format.media_type()) {
        Ok(media) => ([(axum::http::header::CONTENT_TYPE, media)], body).into_response(),
        Err(_) => ProblemDetails::internal().into_response(),
    }
}
