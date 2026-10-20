//! The one file download of an endpoint, `file.csv` (EP-08, EP-42…EP-45). The other files
//! (`file.geojson`, `file.json`, `file.xlsx`, `file.parquet`, `file.zip` and the space dump)
//! were removed by owner decision (T-4597, API/02 §5): their paths are the `404` of any child an
//! endpoint does not serve, and the data leaves through the APIs.

use crate::app::Gateway;
use crate::auth::subject::admit;
use crate::handlers::reads::paged_entities;
use crate::handlers::reads::too_large;
use crate::handlers::respond::broker_speaks_json;
use crate::middleware::response::RESULTS_RESTRICTED;
use crate::translators::tabular;
use crate::{middleware::tenancy, query};
use axum::body::Body;
use axum::extract::{Path, Request, State};
use axum::http::{HeaderValue, Response};
use axum::routing::get;
use axum::Router;
use jc_core::kinds::Representation;
use std::sync::Arc;

/// `file.csv`: the whole answer as one flat table (EP-08, EP-44, EP-45).
pub(crate) async fn file_csv(
    State(gateway): State<Arc<Gateway>>,
    Path(slug): Path<String>,
    mut request: Request,
) -> Response<Body> {
    tenancy::strip_client_headers(&mut request);
    broker_speaks_json(&mut request);
    let (endpoint, subject) = match admit(
        &gateway,
        &slug,
        Some(Representation::Csv),
        request.headers(),
    ) {
        Ok(admitted) => admitted,
        Err(problem) => return *problem,
    };

    // Admitted first, so a refused caller learns nothing from a 503 (T-4782).
    let Some(_slot) = gateway.in_flight.download() else {
        return crate::middleware::in_flight::busy(
            "the gateway builds as many downloads as it may at once",
        );
    };
    let params = query::parse(request.uri().query().unwrap_or_default());
    let limits = tabular::Limits::of(endpoint.file_limits.as_ref());
    let (entities, restricted) = match paged_entities(
        &gateway,
        &endpoint,
        &subject,
        (&params, &endpoint.slug),
        &mut request,
        &limits,
    )
    .await
    {
        Ok(answer) => answer,
        Err(problem) => return *problem,
    };

    let mut table = match tabular::table(&entities, &limits) {
        Ok(table) => table,
        Err(_) => return too_large(),
    };
    // The table owns its cells now; the broker's answer would only double what is held
    // while the file is written (T-4107).
    drop(entities);
    if query::first(&params, "humanHeaders") == Some("true") {
        tabular::humanize(&mut table);
    }
    let text = match tabular::csv(&table, &limits) {
        Ok(text) => text,
        Err(_) => return too_large(),
    };

    let mut response = Response::new(Body::from(text));
    let headers = response.headers_mut();
    headers.insert(
        axum::http::header::CONTENT_TYPE,
        HeaderValue::from_static(tabular::CSV_MEDIA_TYPE),
    );
    // The slug is base32 and the extension a literal, so the filename needs no quoting beyond
    // the quotes themselves (EP-43).
    if let Ok(disposition) =
        HeaderValue::from_str(&format!("attachment; filename=\"{}.csv\"", endpoint.slug))
    {
        headers.insert(axum::http::header::CONTENT_DISPOSITION, disposition);
    }
    if restricted {
        headers.insert(RESULTS_RESTRICTED, HeaderValue::from_static("true"));
    }
    response
}

/// This surface's routes; `app::router` merges them under its guards.
pub(crate) fn routes() -> Router<Arc<Gateway>> {
    Router::new().route("/api/endpoint/{slug}/file.csv", get(file_csv))
}
