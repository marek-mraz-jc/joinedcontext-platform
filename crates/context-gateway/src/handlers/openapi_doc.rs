//! The OpenAPI document of one Endpoint's NGSI-LD read surface (EP-99, T-3265).
//!
//! Generated on every request from the Endpoint and its newest model major, never written by
//! hand: the types a caller may filter by are the types the caller may see, and each type's
//! schema is the projected JSON Schema of `schema/v{major}/json-schema` (EP-47), so the document
//! describes nothing the caller could not read and cannot disagree with the data.

use crate::app::Gateway;
use crate::auth::subject::admit;
use crate::handlers::schema::{self, Visible};
use crate::handlers::schema_routes::revalidated;
use crate::middleware::tenancy;
use crate::resolver::{Endpoint, Model};
use axum::body::Body;
use axum::extract::{Path, Request, State};
use axum::http::{header::ACCEPT_LANGUAGE, HeaderMap, Response};
use axum::routing::get;
use axum::Router;
use serde_json::{json, Map, Value};
use std::sync::Arc;

/// `GET /api/endpoint/{slug}/openapi.json`.
pub(crate) async fn openapi(
    State(gateway): State<Arc<Gateway>>,
    Path(slug): Path<String>,
    mut request: Request,
) -> Response<Body> {
    tenancy::strip_client_headers(&mut request);
    let (endpoint, subject) = match admit(&gateway, &slug, None, request.headers()) {
        Ok(admitted) => admitted,
        Err(problem) => return *problem,
    };
    let visible = schema::visible(&subject, &endpoint, crate::pdp::now());
    revalidated(
        &document(&endpoint, &visible, &languages(request.headers())),
        "application/json",
        request.headers(),
    )
}

/// Points every `$ref` of the JSON Schema at the document's `components`.
fn rehome(value: &mut Value) {
    match value {
        Value::Object(map) => {
            for (key, inner) in map.iter_mut() {
                if key == "$ref" {
                    if let Value::String(target) = inner {
                        for prefix in ["#/$defs/", "#/definitions/"] {
                            if let Some(name) = target.strip_prefix(prefix) {
                                *target = format!("#/components/schemas/{name}");
                            }
                        }
                    }
                } else {
                    rehome(inner);
                }
            }
        }
        Value::Array(items) => items.iter_mut().for_each(rehome),
        _ => {}
    }
}

/// The primary language subtags the caller's `Accept-Language` lists, most wanted first
/// (T-5630): `sk-SK;q=0.9` counts as `sk`; `*`, `q=0` and a malformed entry count as none.
fn languages(headers: &HeaderMap) -> Vec<String> {
    let Some(value) = headers.get(ACCEPT_LANGUAGE).and_then(|v| v.to_str().ok()) else {
        return Vec::new();
    };
    let mut ranked: Vec<(f32, String)> = value
        .split(',')
        .take(16)
        .filter_map(|item| {
            let mut parts = item.split(';');
            let primary = parts.next()?.trim().split('-').next()?.to_ascii_lowercase();
            let quality = match parts.find_map(|p| p.trim().strip_prefix("q=")) {
                Some(q) => q.trim().parse::<f32>().ok()?,
                None => 1.0,
            };
            let known = !primary.is_empty() && primary.chars().all(|c| c.is_ascii_alphabetic());
            (known && quality > 0.0).then_some((quality, primary))
        })
        .collect();
    ranked.sort_by(|a, b| b.0.total_cmp(&a.0));
    ranked.into_iter().map(|(_, language)| language).collect()
}

/// The text of a per-locale map: the caller's first language it holds, then English, then any,
/// then `fallback`.
fn text(
    map: &std::collections::BTreeMap<String, String>,
    languages: &[String],
    fallback: &str,
) -> String {
    languages
        .iter()
        .find_map(|language| map.get(language))
        .or_else(|| map.get("en"))
        .or_else(|| map.values().next())
        .cloned()
        .unwrap_or_else(|| fallback.to_owned())
}

/// One query parameter of the entities path.
fn parameter(name: &str, description: &str, schema: Value) -> Value {
    json!({ "name": name, "in": "query", "required": false, "description": description, "schema": schema })
}

/// The document for one caller, its title and description in the caller's `languages`.
pub fn document(endpoint: &Endpoint, visible: &Visible, languages: &[String]) -> Value {
    let newest = endpoint.models.iter().map(|model| model.major).max();
    let models: Vec<&Model> = endpoint
        .models
        .iter()
        .filter(|model| Some(model.major) == newest)
        .collect();
    let mut redacted = Vec::new();
    let mut schemas = match schema::json_schema(&models, visible, &mut redacted)
        .get_mut("$defs")
        .map(Value::take)
    {
        Some(Value::Object(defs)) => defs,
        _ => Map::new(),
    };
    for definition in schemas.values_mut() {
        rehome(definition);
    }
    let types: Vec<String> = schema::visible_types(endpoint, visible)
        .into_iter()
        .filter(|class| models.iter().any(|model| model.classes.contains(class)))
        .collect();
    let described: Vec<Value> = types
        .iter()
        .filter(|class| schemas.contains_key(*class))
        .map(|class| json!({ "$ref": format!("#/components/schemas/{class}") }))
        .collect();
    let entity = if described.is_empty() {
        json!({ "type": "object", "required": ["id", "type"] })
    } else {
        json!({ "oneOf": described })
    };
    let mut type_schema = json!({ "type": "string" });
    if !types.is_empty() {
        type_schema["enum"] = json!(types);
    }
    let problem = json!({ "description": "A problem+json answer (RFC 9457)" });
    let version = models
        .first()
        .map(|model| model.version.clone())
        .unwrap_or_else(|| "1".to_owned());
    json!({
        "openapi": "3.1.0",
        "info": {
            "title": text(&endpoint.title, languages, &endpoint.slug),
            "description": text(&endpoint.description, languages, "The NGSI-LD read surface of one Endpoint."),
            "version": version,
        },
        "servers": [{ "url": endpoint.base_path }],
        "paths": {
            "/ngsi-ld/v1/entities": {
                "get": {
                    "operationId": "queryEntities",
                    "summary": "The entities of a type",
                    "parameters": [
                        parameter("type", "The entity type", type_schema),
                        parameter("q", "NGSI-LD query language, such as temperature>20", json!({ "type": "string" })),
                        parameter("attrs", "The attributes to return, comma-separated", json!({ "type": "string" })),
                        parameter("limit", "How many entities", json!({ "type": "integer", "minimum": 0, "maximum": 1000 })),
                        parameter("offset", "How many to skip", json!({ "type": "integer", "minimum": 0 })),
                        parameter("count", "Whether NGSILD-Results-Count carries the total", json!({ "type": "boolean" })),
                    ],
                    "responses": {
                        "200": {
                            "description": "The entities the caller may read",
                            "content": { "application/ld+json": { "schema": { "type": "array", "items": entity.clone() } } },
                        },
                        "400": problem.clone(),
                        "403": problem.clone(),
                    },
                },
            },
            "/ngsi-ld/v1/entities/{entityId}": {
                "get": {
                    "operationId": "retrieveEntity",
                    "summary": "One entity by its id",
                    "parameters": [{
                        "name": "entityId",
                        "in": "path",
                        "required": true,
                        "description": "The entity's URN",
                        "schema": { "type": "string" },
                    }],
                    "responses": {
                        "200": {
                            "description": "The entity, as the caller may read it",
                            "content": { "application/ld+json": { "schema": entity } },
                        },
                        "404": problem.clone(),
                    },
                },
            },
            "/ngsi-ld/v1/types": {
                "get": {
                    "operationId": "retrieveEntityTypes",
                    "summary": "The types it holds",
                    "responses": {
                        "200": {
                            "description": "The entity types the caller may see",
                            "content": { "application/ld+json": { "schema": { "type": "object" } } },
                        },
                    },
                },
            },
        },
        "components": { "schemas": schemas },
    })
}

/// This surface's routes; `app::router` merges them under its guards.
pub(crate) fn routes() -> Router<Arc<Gateway>> {
    Router::new()
        // The read surface as OpenAPI, generated per caller (EP-99).
        .route("/api/endpoint/{slug}/openapi.json", get(openapi))
}
