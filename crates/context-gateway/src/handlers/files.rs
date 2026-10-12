//! The file downloads of an endpoint: GeoJSON, JSON, CSV, XLSX and the ZIP bundle (EP-24, EP-25).

use crate::app::accept_language;
use crate::app::admit;
use crate::app::admit_space;
use crate::app::broker_speaks_json;
use crate::app::json_response;
use crate::app::Gateway;
use crate::handlers::reads::paged_entities;
use crate::handlers::reads::too_large;
use crate::handlers::{endpoint_surface, schema_routes, space_surface};
use crate::middleware::response::RESULTS_RESTRICTED;
use crate::pdp::evaluator::Subject;
use crate::resolver::Endpoint;
use crate::translators::{geojson, tabular, zip_export};
use crate::{middleware::tenancy, query};
use axum::body::Body;
use axum::extract::{Path, Request, State};
use axum::http::{HeaderValue, Response};
use axum::response::IntoResponse;
use jc_core::kinds::Representation;
use jc_core::ProblemDetails;
use std::sync::Arc;

/// The same data as a `FeatureCollection` (T-0158, EP-09, EP-10).
pub(crate) async fn file_geojson(
    State(gateway): State<Arc<Gateway>>,
    Path(slug): Path<String>,
    mut request: Request,
) -> Response<Body> {
    tenancy::strip_client_headers(&mut request);
    broker_speaks_json(&mut request);
    let (endpoint, subject) = match admit(
        &gateway,
        &slug,
        Some(Representation::GeoJson),
        request.headers(),
    ) {
        Ok(admitted) => admitted,
        Err(problem) => return *problem,
    };

    let params = query::parse(request.uri().query().unwrap_or_default());
    // A download is the whole dataset, paged out of the broker and held to this endpoint's
    // ceilings, exactly like `file.csv` (EP-44, T-1700). One broker query instead would answer
    // a page and call it a file: a truncated FeatureCollection is indistinguishable from a
    // complete one, and the caller acts on half the data.
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

    let collection = geojson::feature_collection(&entities, accept_language(request.headers()));
    // The byte ceiling is the endpoint's own, measured on what would be sent (EP-44).
    if serde_json::to_vec(&collection).is_ok_and(|bytes| bytes.len() as u64 > limits.max_bytes) {
        return too_large();
    }
    let mut response = json_response(&collection);
    response.headers_mut().insert(
        axum::http::header::CONTENT_TYPE,
        HeaderValue::from_static(geojson::MEDIA_TYPE),
    );
    if restricted {
        response
            .headers_mut()
            .insert(RESULTS_RESTRICTED, HeaderValue::from_static("true"));
    }
    response
}

/// `file.json`: the whole dataset as one JSON array of projected entities (EP-41, EP-44).
///
/// The record has advertised this download since the `json` representation was added to the
/// Endpoint kind, and the router did not serve it (T-2382). It is the NGSI-LD document a
/// caller gets from `/ngsi-ld/v1/entities`, over the whole dataset rather than one broker
/// page, bounded by the endpoint's own `fileLimits` and named as an attachment.
pub(crate) async fn file_json(
    State(gateway): State<Arc<Gateway>>,
    Path(slug): Path<String>,
    mut request: Request,
) -> Response<Body> {
    tenancy::strip_client_headers(&mut request);
    broker_speaks_json(&mut request);
    let (endpoint, subject) = match admit(
        &gateway,
        &slug,
        Some(Representation::Json),
        request.headers(),
    ) {
        Ok(admitted) => admitted,
        Err(problem) => return *problem,
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

    let Ok(bytes) = serde_json::to_vec(&entities) else {
        tracing::error!("a download does not serialize");
        return ProblemDetails::internal().into_response();
    };
    // The same ceiling the tabular downloads apply to their own bytes: a file past it is
    // refused whole, because half a JSON array is not a smaller answer, it is a broken one.
    if bytes.len() as u64 > limits.max_bytes {
        return too_large();
    }
    download_response(
        &endpoint.slug,
        Body::from(bytes),
        "application/json",
        "json",
        restricted,
    )
}

/// `file.csv`: the whole answer as one flat table (EP-08, EP-44, EP-45).
pub(crate) async fn file_csv(
    State(gateway): State<Arc<Gateway>>,
    Path(slug): Path<String>,
    request: Request,
) -> Response<Body> {
    tabular_download(gateway, slug, request, Representation::Csv).await
}

/// `file.xlsx`: the same rows as `file.csv`, in a workbook (EP-08, EP-44).
pub(crate) async fn file_xlsx(
    State(gateway): State<Arc<Gateway>>,
    Path(slug): Path<String>,
    request: Request,
) -> Response<Body> {
    tabular_download(gateway, slug, request, Representation::Xlsx).await
}

/// The two tabular representations, which differ only in how the same table is written.
async fn tabular_download(
    gateway: Arc<Gateway>,
    slug: String,
    mut request: Request,
    representation: Representation,
) -> Response<Body> {
    tenancy::strip_client_headers(&mut request);
    broker_speaks_json(&mut request);
    let (endpoint, subject) = match admit(&gateway, &slug, Some(representation), request.headers())
    {
        Ok(admitted) => admitted,
        Err(problem) => return *problem,
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

    let (body, media, extension) = match representation {
        Representation::Xlsx => {
            let metadata = workbook_metadata(&endpoint, &params, table.len());
            match tabular::xlsx(&table, &metadata, &limits) {
                Ok(bytes) => (Body::from(bytes), tabular::XLSX_MEDIA_TYPE, "xlsx"),
                Err(tabular::XlsxError::TooLarge) => return too_large(),
                Err(error) => {
                    tracing::error!(%error, "the workbook does not serialize");
                    return ProblemDetails::internal().into_response();
                }
            }
        }
        _ => match tabular::csv(&table, &limits) {
            Ok(text) => (Body::from(text), tabular::CSV_MEDIA_TYPE, "csv"),
            Err(_) => return too_large(),
        },
    };

    download_response(&endpoint.slug, body, media, extension, restricted)
}

/// One download's response: the body under its media type, named after the endpoint, with the
/// narrowing signal when the projection dropped something (EP-43, R22).
fn download_response(
    slug: &str,
    body: Body,
    media: &str,
    extension: &str,
    restricted: bool,
) -> Response<Body> {
    let mut response = Response::new(body);
    let headers = response.headers_mut();
    if let Ok(media) = HeaderValue::from_str(media) {
        headers.insert(axum::http::header::CONTENT_TYPE, media);
    }
    // The slug is base32 and the extension is one of three literals, so the filename needs
    // no quoting beyond the quotes themselves (EP-43).
    if let Ok(disposition) =
        HeaderValue::from_str(&format!("attachment; filename=\"{slug}.{extension}\""))
    {
        headers.insert(axum::http::header::CONTENT_DISPOSITION, disposition);
    }
    if restricted {
        headers.insert(RESULTS_RESTRICTED, HeaderValue::from_static("true"));
    }
    response
}

/// `file.zip`: one query in every shape, with the schemas and the catalogue record (T-0161,
/// EP-41, EP-43, EP-44, EP-51).
///
/// The bundle is assembled from the same projected answer the other file representations use, so
/// it can only ever carry what the caller was already allowed to download one format at a time.
/// Its schema directory is rendered by the code that serves `schema/`, and its catalogue record
/// is the endpoint's own, so a bundle cannot describe the data differently from the endpoint it
/// came out of.
pub(crate) async fn file_zip(
    State(gateway): State<Arc<Gateway>>,
    Path(slug): Path<String>,
    mut request: Request,
) -> Response<Body> {
    tenancy::strip_client_headers(&mut request);
    broker_speaks_json(&mut request);
    let (endpoint, subject) = match admit(
        &gateway,
        &slug,
        Some(Representation::Zip),
        request.headers(),
    ) {
        Ok(admitted) => admitted,
        Err(problem) => return *problem,
    };

    let params = query::parse(request.uri().query().unwrap_or_default());
    let space = gateway.resolver.resolve_space(&endpoint.space);
    let index = schema_routes::index_for(&gateway, &endpoint, &endpoint.slug, &subject).await;
    let dcat = endpoint_surface::dataset(&endpoint, space.as_deref(), &index, gateway.base_url());
    zip_download(
        &gateway,
        &endpoint,
        &subject,
        (&params, &endpoint.slug),
        &dcat,
        &mut request,
    )
    .await
}

/// `/cs/{space}/dump/`: the `file.zip` bundle over everything in the space the caller's grants
/// read, generated per request (SP-13, T-2391). No query narrows it: a dump is the space, and the
/// caller who wants less asks the NGSI-LD surface. The projection is the space's own, the one
/// `ngsi-ld/v1/` applies, and a caller whose grants reach nothing gets the `404` of a space that
/// does not exist (SP-06).
pub(crate) async fn space_dump(
    State(gateway): State<Arc<Gateway>>,
    Path(name): Path<String>,
    mut request: Request,
) -> Response<Body> {
    tenancy::strip_client_headers(&mut request);
    broker_speaks_json(&mut request);
    let (space, subject) = match admit_space(
        &gateway,
        &name,
        Some(Representation::Zip),
        request.headers(),
    ) {
        Ok(admitted) => admitted,
        Err(problem) => return *problem,
    };
    let dcat = space_surface::dataset(&space, gateway.base_url());
    // The space's dump reads through the space's own view, the one `/cs/{space}` uses (plan 5.1).
    let view = format!("cs-{}", space.name());
    zip_download(
        &gateway,
        &space.endpoint,
        &subject,
        (&[], &view),
        &dcat,
        &mut request,
    )
    .await
}

/// One bundle as a download (EP-41, EP-44): the projected entities paged out of the broker, the
/// schema directory of the caller's own view and the catalogue record it was cut from, under the
/// endpoint's ceilings, which refuse the whole archive with `413` rather than truncate it.
async fn zip_download(
    gateway: &Gateway,
    endpoint: &Endpoint,
    subject: &Subject,
    (params, view): (&[(String, String)], &str),
    dcat: &serde_json::Value,
    request: &mut Request,
) -> Response<Body> {
    let limits = tabular::Limits::of(endpoint.file_limits.as_ref());
    let (entities, restricted) =
        match paged_entities(gateway, endpoint, subject, (params, view), request, &limits).await {
            Ok(answer) => answer,
            Err(problem) => return *problem,
        };

    let schemas = schema_directory(gateway, endpoint, view, subject).await;

    let exported_at = crate::pdp::now().to_rfc3339();
    let query = params
        .iter()
        .map(|(name, value)| format!("{name}={value}"))
        .collect::<Vec<_>>()
        .join("&");
    let manifest = zip_export::Bundle {
        slug: &endpoint.slug,
        space: &endpoint.space,
        query: &query,
        exported_at: &exported_at,
    };

    let archive = match zip_export::bundle(&entities, &schemas, dcat, &manifest, &limits) {
        Ok(archive) => archive,
        Err(zip_export::BundleError::TooLarge(_)) => return too_large(),
        Err(error) => {
            tracing::error!(%error, "the bundle does not serialize");
            return ProblemDetails::internal().into_response();
        }
    };

    let mut response = Response::new(Body::from(archive));
    let headers = response.headers_mut();
    headers.insert(
        axum::http::header::CONTENT_TYPE,
        HeaderValue::from_static(zip_export::MEDIA_TYPE),
    );
    // A slug is base32, a space name a DNS label, and the date is digits, so the filename needs
    // no quoting beyond the quotes themselves (EP-43).
    if let Ok(disposition) = HeaderValue::from_str(&format!(
        "attachment; filename=\"{}\"",
        zip_export::file_name(&endpoint.slug, &exported_at)
    )) {
        headers.insert(axum::http::header::CONTENT_DISPOSITION, disposition);
    }
    if restricted {
        headers.insert(RESULTS_RESTRICTED, HeaderValue::from_static("true"));
    }
    response
}

/// Every schema document the caller's view publishes, as the bundle stores them (EP-51).
///
/// One directory per major version, each artifact the broker's index lists for it, read from
/// the broker's LinkML surface of the view as the caller reads `schema/` (EP-47, T-3646). A
/// document the broker does not answer is left out rather than replaced by another rendering.
async fn schema_directory(
    gateway: &Gateway,
    endpoint: &Endpoint,
    view: &str,
    subject: &Subject,
) -> Vec<(String, Vec<u8>)> {
    let index = schema_routes::index_for(gateway, endpoint, view, subject).await;
    let mut documents = Vec::new();
    for model in index["models"].as_array().into_iter().flatten() {
        let Some(major) = schema_routes::major_of(model) else {
            continue;
        };
        let names = model["artifacts"].as_object().into_iter().flatten();
        for name in names.map(|(name, _)| name) {
            let path = format!("v{major}/{name}");
            if let Some(Ok(body)) = schema_routes::view_document(
                gateway,
                endpoint,
                view,
                subject,
                &format!("schema/{path}"),
            )
            .await
            {
                documents.push((path, body));
            }
        }
    }
    documents
}

/// What the `metadata` sheet of a workbook says about the download that produced it.
fn workbook_metadata(
    endpoint: &Endpoint,
    params: &[(String, String)],
    rows: usize,
) -> Vec<(String, String)> {
    vec![
        ("space".to_owned(), endpoint.space.clone()),
        ("endpoint".to_owned(), endpoint.slug.clone()),
        ("exportedAt".to_owned(), crate::pdp::now().to_rfc3339()),
        (
            "query".to_owned(),
            params
                .iter()
                .map(|(name, value)| format!("{name}={value}"))
                .collect::<Vec<_>>()
                .join("&"),
        ),
        ("rows".to_owned(), rows.to_string()),
    ]
}
