//! The one file download of an endpoint, `file.csv` (EP-08, EP-42…EP-45). The other files
//! (`file.geojson`, `file.json`, `file.xlsx`, `file.parquet`, `file.zip` and the space dump)
//! were removed by owner decision (T-4597, API/02 §5): their paths are the `404` of any child an
//! endpoint does not serve, and the data leaves through the APIs.

use crate::app::Gateway;
use crate::auth::subject::{admit, Subject};
use crate::handlers::download_cache::Key;
use crate::handlers::reads::paged_entities;
use crate::handlers::reads::too_large;
use crate::handlers::respond::broker_speaks_json;
use crate::handlers::schema;
use crate::handlers::schema_routes::revalidated_text;
use crate::middleware::response::RESULTS_RESTRICTED;
use crate::resolver::Endpoint;
use crate::{middleware::tenancy, query};
use axum::body::{Body, Bytes};
use axum::extract::{Path, Request, State};
use axum::http::{HeaderValue, Response};
use axum::response::IntoResponse;
use axum::routing::get;
use axum::Router;
use jc_core::kinds::Representation;
use jc_core::ProblemDetails;
use jc_tabular as tabular;
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
    // Normalized NGSI-LD whatever the caller asked, so `options` cannot rename columns (T-5209).
    let params: Vec<(String, String)> = query::parse(request.uri().query().unwrap_or_default())
        .into_iter()
        .filter(|(name, _)| name != "options" && name != "format")
        .collect();
    let dialect = match tabular::Dialect::of(
        query::first(&params, "delimiter"),
        query::first(&params, "bom"),
        query::first(&params, "humanHeaders") == Some("true"),
    ) {
        Ok(dialect) => dialect,
        Err(bad) => {
            let problem = ProblemDetails::new(400, "bad-request", "Bad Request");
            return problem.with_detail(bad.to_string()).into_response();
        }
    };
    // A file holding personal data is built for each reader, so each read is logged (DM-128).
    let personal = endpoint.personal_attributes();
    let key = Key::of(&endpoint, &subject, &request).filter(|_| personal.is_empty());
    let build = build(
        &gateway,
        (&endpoint, &subject),
        &personal,
        &params,
        &mut request,
        dialect,
    );
    // An anonymous answer is built once for every caller asking within FRESH_FOR (T-5629).
    let (bytes, restricted) = match gateway.downloads.get_or_build(key, build).await {
        Ok(built) => built,
        Err(problem) => return *problem,
    };
    // A strong ETag over the bytes and a 304 when they still match (EP-43, T-5209).
    let mut response = revalidated_text(bytes, tabular::CSV_MEDIA_TYPE, request.headers());
    let headers = response.headers_mut();
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

/// The file `file_csv` answers with, in one of the bounded build slots (T-4782), and whether
/// the view narrowed it. A file with `personal` attributes logs its reader and rows (DM-128).
async fn build(
    gateway: &Gateway,
    (endpoint, subject): (&Endpoint, &Subject),
    personal: &[String],
    params: &[(String, String)],
    request: &mut Request,
    dialect: tabular::Dialect,
) -> Result<(Bytes, bool), Box<Response<Body>>> {
    // Admitted first, so a refused caller learns nothing from a 503 (T-4782).
    let Some(_slot) = gateway.in_flight.download() else {
        return Err(Box::new(crate::middleware::in_flight::busy(
            "the gateway builds as many downloads as it may at once",
        )));
    };
    let limits = tabular::Limits::of(endpoint.file_limits.as_ref());
    // The table is built page by page as the broker answers (T-4782).
    let (mut table, restricted) = paged_entities(
        gateway,
        endpoint,
        subject,
        (params, &endpoint.slug),
        request,
        &limits,
    )
    .await?;
    let types: Vec<String> = query::first(params, "type")
        .map(|types| types.split(',').map(str::to_owned).collect())
        .unwrap_or_default();
    let visible = schema::visible(subject, endpoint, crate::pdp::now());
    table.settle(&endpoint.csv_slots(&types, |class, name| visible.shows(class, name)));
    if !personal.is_empty() {
        let ids = table_ids(&table);
        crate::handlers::ngsi_ld::log_personal_read(endpoint, subject, personal, &ids);
    }
    if query::first(params, "humanHeaders") == Some("true") {
        tabular::humanize(&mut table);
    }
    let text = tabular::csv(&table, &limits, dialect).map_err(|_| Box::new(too_large()))?;
    Ok((Bytes::from(text), restricted))
}

/// The Entity ids of `table`'s rows, from its `id` column.
fn table_ids(table: &tabular::Table) -> Vec<String> {
    let Some(at) = table.columns.iter().position(|column| column == "id") else {
        return Vec::new();
    };
    table
        .rows
        .iter()
        .filter_map(|row| row.get(at).and_then(serde_json::Value::as_str))
        .map(str::to_owned)
        .collect()
}

/// This surface's routes; `app::router` merges them under its guards.
pub(crate) fn routes() -> Router<Arc<Gateway>> {
    Router::new().route("/api/endpoint/{slug}/file.csv", get(file_csv))
}
