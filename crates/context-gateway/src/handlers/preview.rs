//! The filter preview (T-2776, EP-85): one page of what an endpoint would serve if its filter
//! were the draft in the body.
//!
//! The draft replaces the saved `ModelProjection` and `hiddenAttributes` of a copy of the
//! endpoint's record, and that copy goes through the very NGSI-LD query pipeline a read goes
//! through: the same PDP, the same narrowing, the same answer projection. The Policies stay the
//! saved ones, and a projection is intersected with them (MP-02), so a draft never serves what the
//! Policies do not grant; the same draft, saved, answers the same entities through a normal read.
//!
//! A draft can still show what the saved projection does not publish yet, so only a caller the
//! space's canonical surface admits may ask (SP-06, SP-11); anybody else gets the `404` an unknown
//! slug gets.

use crate::app::{admit, admit_space, as_ngsi_ld_error, serve_ngsi_ld, Gateway};
use crate::middleware::tenancy;
use crate::pdp::evaluator::Subject;
use crate::query;
use axum::body::Body;
use axum::extract::{Path, Request, State};
use axum::http::{Method, Response};
use axum::response::IntoResponse;
use jc_core::kinds::{
    Audience, DataModelRef, ModelProjectionSpec, ProjectedClass, Projection, ProjectionFilter,
};
use jc_core::ProblemDetails;
use serde::Deserialize;
use std::sync::Arc;

/// The largest page, and the most ids one page may be restricted to.
pub const MAX_PAGE: u32 = 100;
const DEFAULT_PAGE: u32 = 50;
const MAX_BODY: usize = 64 * 1024;

/// The body of `POST /api/endpoint/{slug}/preview` (API/02 §7c).
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct Draft {
    #[serde(default)]
    projection: Option<DraftProjection>,
    #[serde(default)]
    hidden_attributes: Vec<String>,
    #[serde(rename = "type")]
    entity_type: String,
    #[serde(default)]
    id: Vec<String>,
    #[serde(default)]
    limit: Option<u32>,
    #[serde(default)]
    offset: Option<u32>,
}

/// The part of a `ModelProjection` a filter editor changes: which classes, which slots, which
/// rows. The references stay those of the saved projection, which the draft does not move.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct DraftProjection {
    classes: Vec<ProjectedClass>,
    #[serde(default)]
    filter: Option<ProjectionFilter>,
}

/// Answers one page under the draft (EP-85).
pub(crate) async fn preview(
    State(gateway): State<Arc<Gateway>>,
    Path(slug): Path<String>,
    mut request: Request,
) -> Response<Body> {
    tenancy::strip_client_headers(&mut request);
    let (endpoint, caller) = match admit(&gateway, &slug, None, request.headers()) {
        Ok(admitted) => admitted,
        Err(problem) => return *problem,
    };
    // The same answer for a caller the space does not admit as for a slug that does not exist:
    // the draft may name what the endpoint does not publish, so it is a member's view.
    if admit_space(&gateway, &endpoint.space, None, request.headers()).is_err() {
        return ProblemDetails::not_found().into_response();
    }

    let (_, body) = request.into_parts();
    let Ok(bytes) = axum::body::to_bytes(body, MAX_BODY).await else {
        return refuse("the body is larger than 64 KiB");
    };
    let draft: Draft = match serde_json::from_slice(&bytes) {
        Ok(draft) => draft,
        Err(err) => return refuse(&format!("the body is not a filter preview: {err}")),
    };
    let draft_endpoint = match apply(&endpoint, &draft) {
        Ok(record) => record,
        Err(reason) => return refuse(&reason),
    };

    // The audience decides whose view the left side is: what an anonymous caller reads on a
    // public endpoint, the caller's own reading on the others (EP-14, EP-16).
    let subject = match endpoint.audience {
        Audience::Public => Subject::anonymous(),
        _ => caller,
    };
    let prefix = format!("{}/ngsi-ld/v1", endpoint.base_path);
    let uri = format!("{prefix}/entities?{}", query_of(&draft));
    let Ok(read) = axum::http::Request::builder()
        .method(Method::GET)
        .uri(uri)
        .body(Body::empty())
    else {
        return refuse("the draft does not form a query this endpoint can serve");
    };
    as_ngsi_ld_error(
        serve_ngsi_ld(
            &gateway,
            Ok((Arc::new(draft_endpoint), subject)),
            &prefix,
            read,
        )
        .await,
    )
    .await
}

/// The endpoint's record with the draft in place of its saved projection and hidden attributes.
///
/// Every member is checked the way the manifest's is, and the reason names the member, because
/// the Portal shows it beside the part of the editor it belongs to.
fn apply(
    endpoint: &crate::resolver::Endpoint,
    draft: &Draft,
) -> Result<crate::resolver::Endpoint, String> {
    jc_core::names::validate_entity_type(&draft.entity_type)
        .map_err(|err| format!("type: {err}"))?;
    let limit = draft.limit.unwrap_or(DEFAULT_PAGE);
    if limit == 0 || limit > MAX_PAGE {
        return Err(format!("limit: a page holds 1 to {MAX_PAGE} entities"));
    }
    if draft.id.len() > MAX_PAGE as usize {
        return Err(format!("id: at most {MAX_PAGE} ids restrict one page"));
    }
    if let Some(bad) = draft
        .id
        .iter()
        .find(|id| id.is_empty() || id.contains(',') || id.chars().any(char::is_whitespace))
    {
        return Err(format!("id: '{bad}' is not an entity id"));
    }
    Projection {
        hidden_attributes: draft.hidden_attributes.clone(),
    }
    .validate()
    .map_err(|err| format!("hiddenAttributes: {err}"))?;

    let projection = match &draft.projection {
        None => None,
        Some(DraftProjection { classes, filter }) => {
            // The saved projection's references, or neutral ones: only the classes, their
            // slots and the filter reach the decision (MP-02).
            let (context_space_ref, data_model_ref) = match &endpoint.projection {
                Some(saved) => (
                    saved.context_space_ref.clone(),
                    saved.data_model_ref.clone(),
                ),
                None => ("preview".to_owned(), DataModelRef::new("preview", "1")),
            };
            let spec = ModelProjectionSpec {
                context_space_ref,
                data_model_ref,
                classes: classes.clone(),
                filter: filter.clone(),
            };
            spec.validate().map_err(|err| {
                format!(
                    "projection: {}",
                    err.to_string().replace("spec.", "projection.")
                )
            })?;
            Some(Arc::new(spec))
        }
    };

    let mut record = endpoint.clone();
    record.projection = projection;
    record.hidden_attributes = draft.hidden_attributes.iter().cloned().collect();
    Ok(record)
}

/// The query of the page: the type, the ids that align it, the window, and the total.
fn query_of(draft: &Draft) -> String {
    let mut pairs = vec![format!("type={}", query::encode(&draft.entity_type))];
    if !draft.id.is_empty() {
        pairs.push(format!("id={}", query::encode(&draft.id.join(","))));
    }
    pairs.push(format!("limit={}", draft.limit.unwrap_or(DEFAULT_PAGE)));
    pairs.push(format!("offset={}", draft.offset.unwrap_or(0)));
    pairs.push("count=true".to_owned());
    pairs.join("&")
}

fn refuse(reason: &str) -> Response<Body> {
    ProblemDetails::bad_request()
        .with_detail(reason)
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::{query_of, Draft};

    #[test]
    fn the_page_query_encodes_the_type_and_the_ids_and_always_counts() {
        let draft: Draft = serde_json::from_value(serde_json::json!({
            "type": "Event",
            "id": ["urn:ngsi-ld:Event:hel.fi:helsinki:a", "urn:ngsi-ld:Event:hel.fi:helsinki:b"],
            "offset": 50
        }))
        .expect("a draft");
        assert_eq!(
            query_of(&draft),
            "type=Event&id=urn%3Angsi-ld%3AEvent%3Ahel.fi%3Ahelsinki%3Aa%2Curn%3Angsi-ld%3AEvent%3Ahel.fi%3Ahelsinki%3Ab&limit=50&offset=50&count=true"
        );
    }
}
